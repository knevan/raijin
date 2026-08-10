use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};
use thiserror::Error;

use crate::download::{
    Bytes, BytesPerSecond, DownloadFailure, DownloadId, DownloadItem, DownloadKind, DownloadPart,
    DownloadStatus, FailureKind, ParseDomainEnumError, PartId, PartStatus, QueueId,
};
use crate::queue::{Queue, QueueItem};

/// Result type used by database bootstrap and repositories.
pub type DbResult<T> = Result<T, DbError>;

/// Database-layer error.
#[derive(Debug, Error)]
pub enum DbError {
    /// SQLx operation failed.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    /// Migration failed.
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    /// JSON serialization or deserialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Persisted enum value is invalid.
    #[error(transparent)]
    DomainEnum(#[from] ParseDomainEnumError),
    /// Stored integer cannot be represented by the target domain type.
    #[error("integer field `{field}` out of range: {value}")]
    IntegerOutOfRange { field: &'static str, value: i128 },
    /// Domain path cannot be safely stored in the current TEXT schema.
    #[error("path field `{field}` is not valid UTF-8")]
    NonUtf8Path { field: &'static str },
    /// Requested row does not exist.
    #[error("{entity} `{id}` not found")]
    NotFound { entity: &'static str, id: i64 },
    /// Child row does not belong to the requested parent.
    #[error("{child} `{child_id}` does not belong to {parent} `{parent_id}`")]
    ParentMismatch {
        child: &'static str,
        child_id: i64,
        parent: &'static str,
        parent_id: i64,
    },
}

/// Repository for persisted downloads.
#[derive(Debug, Clone)]
pub struct DownloadRepository {
    pool: SqlitePool,
}

impl DownloadRepository {
    /// Creates a download repository backed by the provided pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Inserts a download row.
    ///
    /// # Errors
    ///
    /// Returns an error when the row violates constraints or serialization fails.
    pub async fn add(&self, item: &DownloadItem) -> DbResult<DownloadId> {
        insert_download(&self.pool, item).await?;
        Ok(item.id)
    }

    /// Inserts a download and its parts in a single transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when any row violates constraints or serialization fails.
    pub async fn add_with_parts(
        &self,
        item: &DownloadItem,
        parts: &[DownloadPart],
    ) -> DbResult<DownloadId> {
        let mut tx = self.pool.begin().await?;
        insert_download(&mut *tx, item).await?;
        for part in parts {
            insert_part(&mut *tx, part).await?;
        }
        tx.commit().await?;
        Ok(item.id)
    }

    /// Updates all mutable persisted fields for a download.
    ///
    /// # Errors
    ///
    /// Returns an error when the row does not exist or serialization fails.
    pub async fn update(&self, item: &DownloadItem) -> DbResult<()> {
        let rows = bind_download_fields(
            sqlx::query(
                r#"
            UPDATE downloads
            SET kind = ?, url = ?, download_page = ?, headers_json = ?, file_name = ?, folder = ?,
                status = ?, total_bytes = ?, downloaded_bytes = ?, etag = ?, last_modified = ?,
                preferred_connections = ?, speed_limit_bps = ?, error_kind = ?, error_message = ?,
                created_at = ?, started_at = ?, completed_at = ?, updated_at = ?
            WHERE id = ?
            "#,
            ),
            item,
        )?
        .bind(item.id.get())
        .execute(&self.pool)
        .await?
        .rows_affected();

        ensure_affected(rows, "download", item.id.get())
    }

    /// Fetches one download by id.
    ///
    /// # Errors
    ///
    /// Returns an error when row decoding fails.
    pub async fn get(&self, id: DownloadId) -> DbResult<Option<DownloadItem>> {
        sqlx::query("SELECT * FROM downloads WHERE id = ?")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?
            .map(download_from_row)
            .transpose()
    }

    /// Lists all downloads ordered by id.
    ///
    /// # Errors
    ///
    /// Returns an error when row decoding fails.
    pub async fn list(&self) -> DbResult<Vec<DownloadItem>> {
        let rows = sqlx::query("SELECT * FROM downloads ORDER BY id")
            .fetch_all(&self.pool)
            .await?;

        rows.into_iter().map(download_from_row).collect()
    }

    /// Lists downloads that are not in a terminal state.
    ///
    /// # Errors
    ///
    /// Returns an error when row decoding fails.
    pub async fn list_unfinished(&self) -> DbResult<Vec<DownloadItem>> {
        let rows = sqlx::query(
            r#"
            SELECT * FROM downloads
            WHERE status NOT IN (?, ?)
            ORDER BY id
            "#,
        )
        .bind(DownloadStatus::Completed.as_str())
        .bind(DownloadStatus::Removed.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(download_from_row).collect()
    }

    /// Returns the next application-managed download identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to read the current maximum id.
    pub async fn next_id(&self) -> DbResult<DownloadId> {
        let max_id: Option<i64> = sqlx::query_scalar("SELECT MAX(id) FROM downloads")
            .fetch_one(&self.pool)
            .await?;
        let next_id = max_id.unwrap_or(0).checked_add(1).ok_or({
            DbError::IntegerOutOfRange {
                field: "downloads.id",
                value: i128::from(i64::MAX),
            }
        })?;

        Ok(DownloadId::new(next_id))
    }

    /// Removes a download. Parts and queue items are removed by foreign-key cascades.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to execute the delete.
    pub async fn remove(&self, id: DownloadId) -> DbResult<bool> {
        let rows = sqlx::query("DELETE FROM downloads WHERE id = ?")
            .bind(id.get())
            .execute(&self.pool)
            .await?
            .rows_affected();

        Ok(rows > 0)
    }
}

/// Repository for persisted download parts.
#[derive(Debug, Clone)]
pub struct PartRepository {
    pool: SqlitePool,
}

impl PartRepository {
    /// Creates a part repository backed by the provided pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Inserts or updates one part.
    ///
    /// # Errors
    ///
    /// Returns an error when the row violates constraints.
    pub async fn set(&self, part: &DownloadPart) -> DbResult<PartId> {
        upsert_part(&self.pool, part).await?;
        Ok(part.id)
    }

    /// Replaces every part for a download in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when any row violates constraints.
    pub async fn set_for_download(
        &self,
        download_id: DownloadId,
        parts: &[DownloadPart],
    ) -> DbResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM download_parts WHERE download_id = ?")
            .bind(download_id.get())
            .execute(&mut *tx)
            .await?;
        for part in parts {
            ensure_parent(
                part.download_id.get(),
                download_id.get(),
                "part",
                part.id.get(),
                "download",
            )?;
            insert_part(&mut *tx, part).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Lists parts for one download ordered by part index.
    ///
    /// # Errors
    ///
    /// Returns an error when row decoding fails.
    pub async fn list_for_download(&self, download_id: DownloadId) -> DbResult<Vec<DownloadPart>> {
        let rows =
            sqlx::query("SELECT * FROM download_parts WHERE download_id = ? ORDER BY part_index")
                .bind(download_id.get())
                .fetch_all(&self.pool)
                .await?;

        rows.into_iter().map(part_from_row).collect()
    }

    /// Removes all parts for a download.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to execute the delete.
    pub async fn remove_for_download(&self, download_id: DownloadId) -> DbResult<u64> {
        let rows = sqlx::query("DELETE FROM download_parts WHERE download_id = ?")
            .bind(download_id.get())
            .execute(&self.pool)
            .await?
            .rows_affected();

        Ok(rows)
    }
}

/// Repository for queues and queue items.
#[derive(Debug, Clone)]
pub struct QueueRepository {
    pool: SqlitePool,
}

impl QueueRepository {
    /// Creates a queue repository backed by the provided pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Inserts a queue.
    ///
    /// # Errors
    ///
    /// Returns an error when the row violates constraints.
    pub async fn create_queue(&self, queue: &Queue) -> DbResult<QueueId> {
        insert_queue(&self.pool, queue).await?;
        Ok(queue.id)
    }

    /// Returns next non-reserved queue id.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to read existing queue ids.
    pub async fn next_queue_id(&self) -> DbResult<QueueId> {
        let max_id: Option<i64> = sqlx::query_scalar("SELECT MAX(id) FROM queues WHERE id > 0")
            .fetch_one(&self.pool)
            .await?;
        Ok(QueueId::new(max_id.unwrap_or(0).saturating_add(1)))
    }

    /// Inserts or updates a queue.
    ///
    /// # Errors
    ///
    /// Returns an error when the row violates constraints.
    pub async fn set_queue(&self, queue: &Queue) -> DbResult<QueueId> {
        upsert_queue(&self.pool, queue).await?;
        Ok(queue.id)
    }

    /// Creates the reserved default queue when it does not already exist.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to insert the row.
    pub async fn ensure_default_queue(&self, now_ms: i64) -> DbResult<QueueId> {
        sqlx::query(
            r#"
            INSERT OR IGNORE INTO queues (id, name, max_concurrent, stop_on_empty, schedule_json, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(QueueId::MAIN.get())
        .bind("Main")
        .bind(2_i64)
        .bind(0_i64)
        .bind(Option::<String>::None)
        .bind(now_ms)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;

        Ok(QueueId::MAIN)
    }

    /// Fetches one queue by id.
    ///
    /// # Errors
    ///
    /// Returns an error when row decoding fails.
    pub async fn get_queue(&self, id: QueueId) -> DbResult<Option<Queue>> {
        sqlx::query("SELECT * FROM queues WHERE id = ?")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?
            .map(queue_from_row)
            .transpose()
    }

    /// Lists queues ordered by id.
    ///
    /// # Errors
    ///
    /// Returns an error when row decoding fails.
    pub async fn list_queues(&self) -> DbResult<Vec<Queue>> {
        let rows = sqlx::query("SELECT * FROM queues ORDER BY id")
            .fetch_all(&self.pool)
            .await?;

        rows.into_iter().map(queue_from_row).collect()
    }

    /// Deletes one non-default queue. Queue items are removed by cascade.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to execute the delete.
    pub async fn delete_queue(&self, id: QueueId) -> DbResult<bool> {
        let rows = sqlx::query("DELETE FROM queues WHERE id = ? AND id != ?")
            .bind(id.get())
            .bind(QueueId::MAIN.get())
            .execute(&self.pool)
            .await?
            .rows_affected();

        Ok(rows > 0)
    }

    /// Adds or updates one queue item.
    ///
    /// # Errors
    ///
    /// Returns an error when the row violates constraints.
    pub async fn add_item(&self, item: QueueItem) -> DbResult<()> {
        upsert_queue_item(&self.pool, item).await
    }

    /// Replaces queue order in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when any item violates constraints.
    pub async fn set_items(&self, queue_id: QueueId, items: &[QueueItem]) -> DbResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM queue_items WHERE queue_id = ?")
            .bind(queue_id.get())
            .execute(&mut *tx)
            .await?;

        for item in items {
            ensure_parent(
                item.queue_id.get(),
                queue_id.get(),
                "queue item",
                item.download_id.get(),
                "queue",
            )?;
            upsert_queue_item(&mut *tx, *item).await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Lists queue items ordered by position.
    ///
    /// # Errors
    ///
    /// Returns an error when row decoding fails.
    pub async fn list_items(&self, queue_id: QueueId) -> DbResult<Vec<QueueItem>> {
        let rows = sqlx::query(
            "SELECT queue_id, download_id, position FROM queue_items WHERE queue_id = ? ORDER BY position, download_id",
        )
        .bind(queue_id.get())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(queue_item_from_row).collect()
    }

    /// Lists queues that contain one download.
    ///
    /// # Errors
    ///
    /// Returns an error when row decoding fails.
    pub async fn queue_ids_for_download(&self, download_id: DownloadId) -> DbResult<Vec<QueueId>> {
        let ids = sqlx::query_scalar(
            "SELECT queue_id FROM queue_items WHERE download_id = ? ORDER BY queue_id",
        )
        .bind(download_id.get())
        .fetch_all(&self.pool)
        .await?;

        Ok(ids.into_iter().map(QueueId::new).collect())
    }

    /// Removes one item from one queue.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to execute the delete.
    pub async fn remove_item(&self, queue_id: QueueId, download_id: DownloadId) -> DbResult<bool> {
        let rows = sqlx::query("DELETE FROM queue_items WHERE queue_id = ? AND download_id = ?")
            .bind(queue_id.get())
            .bind(download_id.get())
            .execute(&self.pool)
            .await?
            .rows_affected();

        Ok(rows > 0)
    }
}

/// Repository for key/value JSON settings.
#[derive(Debug, Clone)]
pub struct SettingsRepository {
    pool: SqlitePool,
}

impl SettingsRepository {
    /// Creates a settings repository backed by the provided pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Stores a raw JSON value.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to upsert the row.
    pub async fn set_raw(&self, key: &str, value_json: &str, updated_at: i64) -> DbResult<()> {
        sqlx::query(
            r#"
            INSERT INTO settings (key, value_json, updated_at)
            VALUES (?, ?, ?)
            ON CONFLICT(key) DO UPDATE SET
                value_json = excluded.value_json,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(key)
        .bind(value_json)
        .bind(updated_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Serializes and stores a JSON setting.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization or SQLite execution fails.
    pub async fn set_json<T>(&self, key: &str, value: &T, updated_at: i64) -> DbResult<()>
    where
        T: Serialize,
    {
        let value_json = serde_json::to_string(value)?;
        self.set_raw(key, &value_json, updated_at).await
    }

    /// Fetches a raw JSON value by key.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite row decoding fails.
    pub async fn get_raw(&self, key: &str) -> DbResult<Option<String>> {
        sqlx::query("SELECT value_json FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?
            .map(|row| row.try_get("value_json"))
            .transpose()
            .map_err(Into::into)
    }

    /// Fetches and deserializes a JSON setting by key.
    ///
    /// # Errors
    ///
    /// Returns an error when row decoding or deserialization fails.
    pub async fn get_json<T>(&self, key: &str) -> DbResult<Option<T>>
    where
        T: DeserializeOwned,
    {
        self.get_raw(key)
            .await?
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(Into::into)
    }

    /// Removes one setting by key.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to execute the delete.
    pub async fn remove(&self, key: &str) -> DbResult<bool> {
        let rows = sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?
            .rows_affected();

        Ok(rows > 0)
    }
}

fn bind_download_fields<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>,
    item: &'q DownloadItem,
) -> DbResult<sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>> {
    let headers_json = if item.headers.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&item.headers)?)
    };
    let folder = path_to_db("folder", &item.folder)?;
    let (error_kind, error_message) = item.failure.as_ref().map_or((None, None), |failure| {
        (Some(failure.kind.as_str()), Some(failure.message.as_str()))
    });

    Ok(query
        .bind(item.kind.as_str())
        .bind(item.url.as_str())
        .bind(item.download_page.as_deref())
        .bind(headers_json)
        .bind(item.file_name.as_str())
        .bind(folder)
        .bind(item.status.as_str())
        .bind(option_bytes_to_i64("total_bytes", item.total_bytes)?)
        .bind(bytes_to_i64("downloaded_bytes", item.downloaded_bytes)?)
        .bind(item.etag.as_deref())
        .bind(item.last_modified.as_deref())
        .bind(
            item.preferred_connections
                .map(|value| i64::from(value.get())),
        )
        .bind(option_bps_to_i64("speed_limit_bps", item.speed_limit)?)
        .bind(error_kind)
        .bind(error_message)
        .bind(item.created_at)
        .bind(item.started_at)
        .bind(item.completed_at)
        .bind(item.updated_at))
}

async fn insert_download<'e, E>(executor: E, item: &DownloadItem) -> DbResult<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    bind_download_fields(
        sqlx::query(
            r#"
        INSERT INTO downloads (
            kind, url, download_page, headers_json, file_name, folder, status, total_bytes,
            downloaded_bytes, etag, last_modified, preferred_connections, speed_limit_bps,
            error_kind, error_message, created_at, started_at, completed_at, updated_at, id
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        ),
        item,
    )?
    .bind(item.id.get())
    .execute(executor)
    .await?;

    Ok(())
}

async fn insert_part<'e, E>(executor: E, part: &DownloadPart) -> DbResult<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    bind_part_fields(sqlx::query(
        r#"
        INSERT INTO download_parts (
            id, download_id, part_index, start_byte, end_byte, current_byte, status, retry_count, updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    ), part)?
    .execute(executor)
    .await?;

    Ok(())
}

async fn upsert_part<'e, E>(executor: E, part: &DownloadPart) -> DbResult<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    bind_part_fields(sqlx::query(
        r#"
        INSERT INTO download_parts (
            id, download_id, part_index, start_byte, end_byte, current_byte, status, retry_count, updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(download_id, part_index) DO UPDATE SET
            id = excluded.id,
            start_byte = excluded.start_byte,
            end_byte = excluded.end_byte,
            current_byte = excluded.current_byte,
            status = excluded.status,
            retry_count = excluded.retry_count,
            updated_at = excluded.updated_at
        "#,
    ), part)?
    .execute(executor)
    .await?;

    Ok(())
}

fn bind_part_fields<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>,
    part: &'q DownloadPart,
) -> DbResult<sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>> {
    Ok(query
        .bind(part.id.get())
        .bind(part.download_id.get())
        .bind(i64::from(part.index))
        .bind(bytes_to_i64("start_byte", part.start_byte)?)
        .bind(option_bytes_to_i64("end_byte", part.end_byte)?)
        .bind(bytes_to_i64("current_byte", part.current_byte)?)
        .bind(part.status.as_str())
        .bind(i64::from(part.retry_count))
        .bind(part.updated_at))
}

fn download_from_row(row: SqliteRow) -> DbResult<DownloadItem> {
    let kind: String = row.try_get("kind")?;
    let status: String = row.try_get("status")?;
    let headers_json: Option<String> = row.try_get("headers_json")?;
    let headers = headers_json
        .map(|value| serde_json::from_str(&value))
        .transpose()?
        .unwrap_or_default();
    let error_kind: Option<String> = row.try_get("error_kind")?;
    let error_message: Option<String> = row.try_get("error_message")?;
    let failure = error_kind
        .map(|kind| {
            Ok::<_, DbError>(DownloadFailure {
                kind: FailureKind::from_str(&kind)?,
                message: error_message.unwrap_or_default(),
            })
        })
        .transpose()?;
    let folder: String = row.try_get("folder")?;

    Ok(DownloadItem {
        id: DownloadId::new(row.try_get("id")?),
        kind: DownloadKind::from_str(&kind)?,
        url: row.try_get("url")?,
        download_page: row.try_get("download_page")?,
        headers,
        file_name: row.try_get("file_name")?,
        folder: PathBuf::from(folder),
        status: DownloadStatus::from_str(&status)?,
        total_bytes: option_i64_to_bytes("total_bytes", row.try_get("total_bytes")?)?,
        downloaded_bytes: i64_to_bytes("downloaded_bytes", row.try_get("downloaded_bytes")?)?,
        etag: row.try_get("etag")?,
        last_modified: row.try_get("last_modified")?,
        preferred_connections: option_i64_to_nonzero_u16(
            "preferred_connections",
            row.try_get("preferred_connections")?,
        )?,
        speed_limit: option_i64_to_bps("speed_limit_bps", row.try_get("speed_limit_bps")?)?,
        failure,
        created_at: row.try_get("created_at")?,
        started_at: row.try_get("started_at")?,
        completed_at: row.try_get("completed_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn part_from_row(row: SqliteRow) -> DbResult<DownloadPart> {
    let status: String = row.try_get("status")?;

    Ok(DownloadPart {
        id: PartId::new(row.try_get("id")?),
        download_id: DownloadId::new(row.try_get("download_id")?),
        index: i64_to_u32("part_index", row.try_get("part_index")?)?,
        start_byte: i64_to_bytes("start_byte", row.try_get("start_byte")?)?,
        end_byte: option_i64_to_bytes("end_byte", row.try_get("end_byte")?)?,
        current_byte: i64_to_bytes("current_byte", row.try_get("current_byte")?)?,
        status: PartStatus::from_str(&status)?,
        retry_count: i64_to_u32("retry_count", row.try_get("retry_count")?)?,
        updated_at: row.try_get("updated_at")?,
    })
}

async fn insert_queue<'e, E>(executor: E, queue: &Queue) -> DbResult<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        r#"
        INSERT INTO queues (id, name, max_concurrent, stop_on_empty, schedule_json, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(queue.id.get())
    .bind(queue.name.as_str())
    .bind(i64::from(queue.max_concurrent.get()))
    .bind(i64::from(queue.stop_on_empty))
    .bind(queue.schedule_json.as_deref())
    .bind(queue.created_at)
    .bind(queue.updated_at)
    .execute(executor)
    .await?;

    Ok(())
}

async fn upsert_queue<'e, E>(executor: E, queue: &Queue) -> DbResult<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        r#"
        INSERT INTO queues (id, name, max_concurrent, stop_on_empty, schedule_json, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            max_concurrent = excluded.max_concurrent,
            stop_on_empty = excluded.stop_on_empty,
            schedule_json = excluded.schedule_json,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(queue.id.get())
    .bind(queue.name.as_str())
    .bind(i64::from(queue.max_concurrent.get()))
    .bind(i64::from(queue.stop_on_empty))
    .bind(queue.schedule_json.as_deref())
    .bind(queue.created_at)
    .bind(queue.updated_at)
    .execute(executor)
    .await?;

    Ok(())
}

fn queue_from_row(row: SqliteRow) -> DbResult<Queue> {
    Ok(Queue {
        id: QueueId::new(row.try_get("id")?),
        name: row.try_get("name")?,
        max_concurrent: i64_to_nonzero_u16("max_concurrent", row.try_get("max_concurrent")?)?,
        stop_on_empty: i64_to_bool("stop_on_empty", row.try_get("stop_on_empty")?)?,
        schedule_json: row.try_get("schedule_json")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

async fn upsert_queue_item<'e, E>(executor: E, item: QueueItem) -> DbResult<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        r#"
        INSERT INTO queue_items (queue_id, download_id, position)
        VALUES (?, ?, ?)
        ON CONFLICT(queue_id, download_id) DO UPDATE SET position = excluded.position
        "#,
    )
    .bind(item.queue_id.get())
    .bind(item.download_id.get())
    .bind(i64::from(item.position))
    .execute(executor)
    .await?;

    Ok(())
}

fn queue_item_from_row(row: SqliteRow) -> DbResult<QueueItem> {
    Ok(QueueItem {
        queue_id: QueueId::new(row.try_get("queue_id")?),
        download_id: DownloadId::new(row.try_get("download_id")?),
        position: i64_to_u32("position", row.try_get("position")?)?,
    })
}

fn ensure_affected(rows: u64, entity: &'static str, id: i64) -> DbResult<()> {
    if rows == 0 {
        return Err(DbError::NotFound { entity, id });
    }
    Ok(())
}

fn ensure_parent(
    actual_parent_id: i64,
    expected_parent_id: i64,
    child: &'static str,
    child_id: i64,
    parent: &'static str,
) -> DbResult<()> {
    if actual_parent_id != expected_parent_id {
        return Err(DbError::ParentMismatch {
            child,
            child_id,
            parent,
            parent_id: expected_parent_id,
        });
    }
    Ok(())
}

fn path_to_db<'a>(field: &'static str, path: &'a Path) -> DbResult<&'a str> {
    path.to_str().ok_or(DbError::NonUtf8Path { field })
}

fn bytes_to_i64(field: &'static str, value: Bytes) -> DbResult<i64> {
    i64::try_from(value.get()).map_err(|_| DbError::IntegerOutOfRange {
        field,
        value: i128::from(value.get()),
    })
}

fn option_bytes_to_i64(field: &'static str, value: Option<Bytes>) -> DbResult<Option<i64>> {
    value.map(|value| bytes_to_i64(field, value)).transpose()
}

fn option_bps_to_i64(field: &'static str, value: Option<BytesPerSecond>) -> DbResult<Option<i64>> {
    value
        .map(|value| {
            i64::try_from(value.get()).map_err(|_| DbError::IntegerOutOfRange {
                field,
                value: i128::from(value.get()),
            })
        })
        .transpose()
}

fn i64_to_u64(field: &'static str, value: i64) -> DbResult<u64> {
    u64::try_from(value).map_err(|_| DbError::IntegerOutOfRange {
        field,
        value: i128::from(value),
    })
}

fn i64_to_u32(field: &'static str, value: i64) -> DbResult<u32> {
    u32::try_from(value).map_err(|_| DbError::IntegerOutOfRange {
        field,
        value: i128::from(value),
    })
}

fn i64_to_bytes(field: &'static str, value: i64) -> DbResult<Bytes> {
    i64_to_u64(field, value).map(Bytes::new)
}

fn option_i64_to_bytes(field: &'static str, value: Option<i64>) -> DbResult<Option<Bytes>> {
    value.map(|value| i64_to_bytes(field, value)).transpose()
}

fn option_i64_to_bps(field: &'static str, value: Option<i64>) -> DbResult<Option<BytesPerSecond>> {
    value
        .map(|value| i64_to_u64(field, value).map(BytesPerSecond::new))
        .transpose()
}

fn i64_to_nonzero_u16(field: &'static str, value: i64) -> DbResult<NonZeroU16> {
    let value = u16::try_from(value).map_err(|_| DbError::IntegerOutOfRange {
        field,
        value: i128::from(value),
    })?;
    NonZeroU16::new(value).ok_or(DbError::IntegerOutOfRange { field, value: 0 })
}

fn option_i64_to_nonzero_u16(
    field: &'static str,
    value: Option<i64>,
) -> DbResult<Option<NonZeroU16>> {
    value
        .map(|value| i64_to_nonzero_u16(field, value))
        .transpose()
}

fn i64_to_bool(field: &'static str, value: i64) -> DbResult<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DbError::IntegerOutOfRange {
            field,
            value: i128::from(value),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;

    use super::*;
    use crate::db;

    struct TestDb {
        _dir: TempDir,
        pool: SqlitePool,
    }

    async fn test_db() -> DbResult<TestDb> {
        let dir = tempfile::tempdir().map_err(sqlx::Error::Io)?;
        let db_path = dir.path().join("raijin-test.sqlite");
        let database_url = format!("sqlite://{}", db_path.display());
        let pool = db::bootstrap(&database_url).await?;
        Ok(TestDb { _dir: dir, pool })
    }

    fn non_zero_u16(value: u16) -> NonZeroU16 {
        NonZeroU16::new(value).expect("test value must be non-zero")
    }

    fn sample_download(id: i64) -> DownloadItem {
        let mut headers = BTreeMap::new();
        headers.insert("authorization".to_owned(), "Bearer token".to_owned());

        DownloadItem {
            id: DownloadId::new(id),
            kind: DownloadKind::Http,
            url: "https://example.com/file.bin".to_owned(),
            download_page: Some("https://example.com".to_owned()),
            headers,
            file_name: "file.bin".to_owned(),
            folder: PathBuf::from("C:/Downloads"),
            status: DownloadStatus::Added,
            total_bytes: Some(Bytes::new(1024)),
            downloaded_bytes: Bytes::new(0),
            etag: Some("etag-1".to_owned()),
            last_modified: Some("Sat, 08 Aug 2026 00:00:00 GMT".to_owned()),
            preferred_connections: Some(non_zero_u16(4)),
            speed_limit: Some(BytesPerSecond::new(1024 * 1024)),
            failure: None,
            created_at: 1,
            started_at: None,
            completed_at: None,
            updated_at: 1,
        }
    }

    fn sample_part(id: i64, download_id: DownloadId, index: u32) -> DownloadPart {
        DownloadPart {
            id: PartId::new(id),
            download_id,
            index,
            start_byte: Bytes::new(u64::from(index) * 512),
            end_byte: Some(Bytes::new(u64::from(index + 1) * 512 - 1)),
            current_byte: Bytes::new(u64::from(index) * 512),
            status: PartStatus::Idle,
            retry_count: 0,
            updated_at: 1,
        }
    }

    #[tokio::test]
    async fn bootstrap_should_run_migrations_and_enable_wal() -> DbResult<()> {
        let db = test_db().await?;

        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&db.pool)
            .await?;

        assert_eq!(journal_mode, "wal");
        Ok(())
    }

    #[tokio::test]
    async fn downloads_should_insert_update_list_and_delete() -> DbResult<()> {
        let db = test_db().await?;
        let repository = DownloadRepository::new(db.pool.clone());
        let mut item = sample_download(1);

        repository.add(&item).await?;
        item.status = DownloadStatus::Paused;
        item.downloaded_bytes = Bytes::new(128);
        item.failure = Some(DownloadFailure {
            kind: FailureKind::Network,
            message: "connection closed".to_owned(),
        });
        item.updated_at = 2;
        repository.update(&item).await?;

        let stored = repository.get(item.id).await?;
        assert_eq!(stored.as_ref(), Some(&item));
        assert_eq!(repository.list().await?, vec![item.clone()]);
        assert!(repository.remove(item.id).await?);
        assert_eq!(repository.get(item.id).await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn downloads_and_parts_should_insert_transactionally() -> DbResult<()> {
        let db = test_db().await?;
        let downloads = DownloadRepository::new(db.pool.clone());
        let parts = PartRepository::new(db.pool.clone());
        let item = sample_download(1);
        let part_rows = vec![sample_part(1, item.id, 0), sample_part(2, item.id, 1)];

        downloads.add_with_parts(&item, &part_rows).await?;

        assert_eq!(parts.list_for_download(item.id).await?, part_rows);
        Ok(())
    }

    #[tokio::test]
    async fn parts_should_replace_and_remove_by_download() -> DbResult<()> {
        let db = test_db().await?;
        let downloads = DownloadRepository::new(db.pool.clone());
        let parts = PartRepository::new(db.pool.clone());
        let item = sample_download(1);
        downloads.add(&item).await?;
        let initial = vec![sample_part(1, item.id, 0), sample_part(2, item.id, 1)];
        parts.set_for_download(item.id, &initial).await?;

        let mut updated = sample_part(3, item.id, 0);
        updated.current_byte = Bytes::new(256);
        updated.status = PartStatus::Receiving;
        parts.set(&updated).await?;

        assert_eq!(
            parts.list_for_download(item.id).await?,
            vec![updated, initial[1].clone()]
        );
        assert_eq!(parts.remove_for_download(item.id).await?, 2);
        Ok(())
    }

    #[tokio::test]
    async fn queue_should_create_default_queue_and_persist_order() -> DbResult<()> {
        let db = test_db().await?;
        let downloads = DownloadRepository::new(db.pool.clone());
        downloads.add(&sample_download(1)).await?;
        downloads.add(&sample_download(2)).await?;
        let queues = QueueRepository::new(db.pool.clone());

        queues.ensure_default_queue(10).await?;
        queues.ensure_default_queue(20).await?;
        let default_queue = queues.get_queue(QueueId::MAIN).await?;
        assert_eq!(
            default_queue.as_ref().map(|queue| queue.name.as_str()),
            Some("Main")
        );

        queues
            .set_items(
                QueueId::MAIN,
                &[
                    QueueItem {
                        queue_id: QueueId::MAIN,
                        download_id: DownloadId::new(2),
                        position: 0,
                    },
                    QueueItem {
                        queue_id: QueueId::MAIN,
                        download_id: DownloadId::new(1),
                        position: 1,
                    },
                ],
            )
            .await?;

        let items = queues.list_items(QueueId::MAIN).await?;
        assert_eq!(items[0].download_id, DownloadId::new(2));
        assert_eq!(items[1].download_id, DownloadId::new(1));
        Ok(())
    }

    #[tokio::test]
    async fn settings_should_store_get_and_remove_json_values() -> DbResult<()> {
        let db = test_db().await?;
        let settings = SettingsRepository::new(db.pool.clone());
        let value = vec!["one".to_owned(), "two".to_owned()];

        settings.set_json("test.list", &value, 1).await?;
        let stored: Option<Vec<String>> = settings.get_json("test.list").await?;

        assert_eq!(stored, Some(value));
        assert!(settings.remove("test.list").await?);
        assert_eq!(settings.get_raw("test.list").await?, None);
        Ok(())
    }
}
