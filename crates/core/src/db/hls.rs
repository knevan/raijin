use sqlx::{Row, SqlitePool};

use crate::db::repositories::{DbError, DbResult, i64_to_u32, i64_to_u64};
use crate::download::DownloadId;

/// One persisted HLS media segment.
///
/// `length` and `sha256` are `Some` only after the segment finished downloading;
/// every other field combination describes a pending segment.
#[derive(Debug, Clone, PartialEq)]
pub struct HlsSegmentRecord {
    /// Zero-based position of the segment inside the plan.
    pub index: u32,
    /// Media sequence number announced by the playlist.
    pub sequence: u64,
    /// Segment URL.
    pub url: String,
    /// Segment duration in seconds.
    pub duration: f64,
    /// Completed segment size in bytes.
    pub length: Option<u64>,
    /// Completed segment checksum.
    pub sha256: Option<String>,
    /// Validator captured while downloading the segment.
    pub etag: Option<String>,
    /// Validator captured while downloading the segment.
    pub last_modified: Option<String>,
}

/// Persisted HLS plan for one download.
#[derive(Debug, Clone, PartialEq)]
pub struct HlsPlanRecord {
    /// Playlist the plan was resolved from.
    pub playlist_url: String,
    /// Segments ordered by index.
    pub segments: Vec<HlsSegmentRecord>,
}

/// Repository for persisted HLS plans and segments.
#[derive(Debug, Clone)]
pub struct HlsRepository {
    pool: SqlitePool,
}

/// Upper bound mirrored by the `hls_segments.duration` CHECK constraint.
const MAX_DURATION: f64 = 1.0e12;

/// Length of a SHA-256 digest in lowercase or uppercase hexadecimal.
const SHA256_HEX_LEN: usize = 64;

impl HlsRepository {
    /// Creates an HLS repository backed by the provided pool.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Loads the stored plan and its segments ordered by index.
    ///
    /// Returns `None` when the download has no HLS plan.
    ///
    /// # Errors
    ///
    /// Returns an error when row decoding fails or the job row is missing while
    /// segment rows exist.
    pub async fn load(&self, id: DownloadId) -> DbResult<Option<HlsPlanRecord>> {
        let mut tx = self.pool.begin().await?;
        let playlist_url: Option<String> =
            sqlx::query_scalar("SELECT playlist_url FROM hls_jobs WHERE download_id = ?")
                .bind(id.get())
                .fetch_optional(&mut *tx)
                .await?;
        let Some(playlist_url) = playlist_url else {
            return Ok(None);
        };

        let rows = sqlx::query(
            r#"
            SELECT segment_index, sequence, url, duration, length, sha256, etag, last_modified
            FROM hls_segments
            WHERE download_id = ?
            ORDER BY segment_index
            "#,
        )
        .bind(id.get())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let segments = rows
            .into_iter()
            .map(segment_from_row)
            .collect::<DbResult<Vec<_>>>()?;

        Ok(Some(HlsPlanRecord {
            playlist_url,
            segments,
        }))
    }

    /// Replaces the stored plan and every segment row in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when the download does not exist or any row violates a
    /// constraint. The previous plan stays untouched on failure.
    pub async fn save_plan(&self, id: DownloadId, plan: &HlsPlanRecord) -> DbResult<()> {
        validate_plan(plan)?;

        let mut tx = self.pool.begin().await?;
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM downloads WHERE id = ?")
            .bind(id.get())
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(DbError::NotFound {
                entity: "download",
                id: id.get(),
            });
        }

        sqlx::query(
            r#"
            INSERT INTO hls_jobs (download_id, playlist_url)
            VALUES (?, ?)
            ON CONFLICT(download_id) DO UPDATE SET playlist_url = excluded.playlist_url
            "#,
        )
        .bind(id.get())
        .bind(plan.playlist_url.as_str())
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM hls_segments WHERE download_id = ?")
            .bind(id.get())
            .execute(&mut *tx)
            .await?;

        for segment in &plan.segments {
            sqlx::query(
                r#"
                INSERT INTO hls_segments (
                    download_id, segment_index, sequence, url, duration, length, sha256, etag, last_modified
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(id.get())
            .bind(i64::from(segment.index))
            .bind(sequence_to_i64(segment.sequence)?)
            .bind(segment.url.as_str())
            .bind(segment.duration)
            .bind(length_to_i64(segment.length)?)
            .bind(segment.sha256.as_deref())
            .bind(segment.etag.as_deref())
            .bind(segment.last_modified.as_deref())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Records a finished segment and its validators in one transaction.
    ///
    /// The aggregate `downloads.downloaded_bytes` column is advanced by the
    /// difference to any previously recorded length (so a repeated completion is a
    /// no-op) and `downloads.updated_at` is refreshed. No other download column is
    /// touched: status and total size stay the engine's decision.
    ///
    /// # Errors
    ///
    /// Returns an error when the segment does not exist, the checksum is blank, or
    /// the byte count is out of range.
    pub async fn complete_segment(
        &self,
        id: DownloadId,
        index: u32,
        length: u64,
        sha256: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> DbResult<()> {
        if sha256.trim().is_empty() {
            return Err(DbError::EmptyValue { field: "sha256" });
        }
        validate_checksum(index, sha256)?;
        let new_length = u64_to_i64("length", length)?;
        if new_length <= 0 {
            return Err(DbError::InvalidCompletedLength { index });
        }

        let mut tx = self.pool.begin().await?;
        let previous: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT length FROM hls_segments WHERE download_id = ? AND segment_index = ?",
        )
        .bind(id.get())
        .bind(i64::from(index))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(previous) = previous else {
            return Err(DbError::NotFound {
                entity: "hls segment",
                id: i64::from(index),
            });
        };
        let downloaded = read_downloaded_bytes(&mut tx, id).await?;
        let next_downloaded = progress_after(downloaded, new_length - previous.unwrap_or(0))?;

        sqlx::query(
            r#"
            UPDATE hls_segments
            SET length = ?, sha256 = ?, etag = ?, last_modified = ?
            WHERE download_id = ? AND segment_index = ?
            "#,
        )
        .bind(new_length)
        .bind(sha256)
        .bind(etag)
        .bind(last_modified)
        .bind(id.get())
        .bind(i64::from(index))
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            UPDATE downloads
            SET downloaded_bytes = ?,
                updated_at = CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)
            WHERE id = ?
            "#,
        )
        .bind(next_downloaded)
        .bind(id.get())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Marks one segment as pending again, dropping its completion metadata.
    ///
    /// `downloads.downloaded_bytes` is reduced by the previously recorded length
    /// inside the same transaction so aggregate progress stays consistent.
    ///
    /// # Errors
    ///
    /// Returns an error when the segment does not exist or SQLite cannot commit.
    pub async fn invalidate_segment(&self, id: DownloadId, index: u32) -> DbResult<()> {
        let mut tx = self.pool.begin().await?;
        let previous: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT length FROM hls_segments WHERE download_id = ? AND segment_index = ?",
        )
        .bind(id.get())
        .bind(i64::from(index))
        .fetch_optional(&mut *tx)
        .await?;

        let Some(previous) = previous else {
            return Err(DbError::NotFound {
                entity: "hls segment",
                id: i64::from(index),
            });
        };

        sqlx::query(
            r#"
            UPDATE hls_segments
            SET length = NULL, sha256 = NULL, etag = NULL, last_modified = NULL
            WHERE download_id = ? AND segment_index = ?
            "#,
        )
        .bind(id.get())
        .bind(i64::from(index))
        .execute(&mut *tx)
        .await?;

        if let Some(previous) = previous {
            let downloaded = read_downloaded_bytes(&mut tx, id).await?;
            let next_downloaded = progress_after(downloaded, -previous)?;
            sqlx::query("UPDATE downloads SET downloaded_bytes = ? WHERE id = ?")
                .bind(next_downloaded)
                .bind(id.get())
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Removes the plan row. Segment rows follow through the cascade.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite fails to execute the delete.
    pub async fn clear(&self, id: DownloadId) -> DbResult<()> {
        sqlx::query("DELETE FROM hls_jobs WHERE download_id = ?")
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn validate_plan(plan: &HlsPlanRecord) -> DbResult<()> {
    if plan.playlist_url.trim().is_empty() {
        return Err(DbError::EmptyValue {
            field: "playlist_url",
        });
    }
    for (expected, segment) in plan.segments.iter().enumerate() {
        if u64::from(segment.index) != expected as u64 {
            return Err(DbError::HlsSegmentsOutOfOrder {
                expected: u32::try_from(expected).unwrap_or(u32::MAX),
                found: segment.index,
            });
        }
        if segment.url.trim().is_empty() {
            return Err(DbError::EmptyValue { field: "url" });
        }
        if !segment.duration.is_finite()
            || segment.duration <= 0.0
            || segment.duration >= MAX_DURATION
        {
            return Err(DbError::InvalidDuration {
                value: segment.duration,
            });
        }
        if let Some(sha256) = &segment.sha256
            && sha256.trim().is_empty()
        {
            return Err(DbError::EmptyValue { field: "sha256" });
        }
        if segment.length.is_some() != segment.sha256.is_some() {
            return Err(DbError::HlsCompletionMetadataMismatch {
                index: segment.index,
            });
        }
        if segment.length == Some(0) {
            return Err(DbError::InvalidCompletedLength {
                index: segment.index,
            });
        }
        if let Some(sha256) = segment.sha256.as_deref() {
            validate_checksum(segment.index, sha256)?;
        }
    }
    Ok(())
}

fn validate_checksum(index: u32, value: &str) -> DbResult<()> {
    if value.len() != SHA256_HEX_LEN || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DbError::InvalidChecksum { index });
    }
    Ok(())
}

fn progress_after(current: i64, delta: i64) -> DbResult<i64> {
    let sum = i128::from(current) + i128::from(delta);
    i64::try_from(sum.max(0)).map_err(|_| DbError::IntegerOutOfRange {
        field: "downloaded_bytes",
        value: sum,
    })
}

/// Reads the aggregate as text and parses it strictly, so a value SQLite widened to
/// REAL cannot be silently coerced into an integer.
async fn read_downloaded_bytes(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: DownloadId,
) -> DbResult<i64> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT CAST(downloaded_bytes AS TEXT) FROM downloads WHERE id = ?")
            .bind(id.get())
            .fetch_optional(&mut **tx)
            .await?;
    let raw = raw.ok_or(DbError::NotFound {
        entity: "download",
        id: id.get(),
    })?;
    raw.parse::<i64>()
        .map_err(|_| DbError::NonIntegerAggregate {
            field: "downloaded_bytes",
            value: raw,
        })
}

fn sequence_to_i64(value: u64) -> DbResult<i64> {
    i64::try_from(value).map_err(|_| DbError::IntegerOutOfRange {
        field: "sequence",
        value: i128::from(value),
    })
}

fn length_to_i64(value: Option<u64>) -> DbResult<Option<i64>> {
    value.map(|value| u64_to_i64("length", value)).transpose()
}

fn u64_to_i64(field: &'static str, value: u64) -> DbResult<i64> {
    i64::try_from(value).map_err(|_| DbError::IntegerOutOfRange {
        field,
        value: i128::from(value),
    })
}

fn segment_from_row(row: sqlx::sqlite::SqliteRow) -> DbResult<HlsSegmentRecord> {
    let length: Option<i64> = row.try_get("length")?;
    let sha256: Option<String> = row.try_get("sha256")?;

    Ok(HlsSegmentRecord {
        index: i64_to_u32("segment_index", row.try_get("segment_index")?)?,
        sequence: i64_to_u64("sequence", row.try_get("sequence")?)?,
        url: row.try_get("url")?,
        duration: row.try_get("duration")?,
        length: length
            .map(|value| i64_to_u64("length", value))
            .transpose()?,
        sha256,
        etag: row.try_get("etag")?,
        last_modified: row.try_get("last_modified")?,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::db;
    use crate::db::repositories::DownloadRepository;

    const SAMPLE_SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    struct TestDb {
        _dir: TempDir,
        pool: SqlitePool,
    }

    async fn test_db() -> DbResult<TestDb> {
        let dir = tempfile::tempdir().map_err(sqlx::Error::Io)?;
        let db_path = dir.path().join("raijin-hls-test.sqlite");
        let database_url = format!("sqlite://{}", db_path.display());
        let pool = db::bootstrap(&database_url).await?;
        Ok(TestDb { _dir: dir, pool })
    }

    async fn insert_download(pool: &SqlitePool, id: i64) -> DbResult<()> {
        sqlx::query(
            r#"
            INSERT INTO downloads (id, kind, url, file_name, folder, status, downloaded_bytes, created_at, updated_at)
            VALUES (?, 'http', 'https://example.com/stream', 'stream.ts', 'C:/Downloads', 'added', 0, 1, 1)
            "#,
        )
        .bind(id)
        .execute(pool)
        .await?;
        Ok(())
    }

    fn pending_segment(index: u32) -> HlsSegmentRecord {
        HlsSegmentRecord {
            index,
            sequence: u64::from(index) + 10,
            url: format!("https://example.com/segment-{index}.ts"),
            duration: 6.0,
            length: None,
            sha256: None,
            etag: None,
            last_modified: None,
        }
    }

    fn sample_plan() -> HlsPlanRecord {
        HlsPlanRecord {
            playlist_url: "https://example.com/master.m3u8".to_owned(),
            segments: vec![pending_segment(0), pending_segment(1)],
        }
    }

    async fn downloaded_bytes(pool: &SqlitePool, id: i64) -> DbResult<i64> {
        sqlx::query_scalar("SELECT downloaded_bytes FROM downloads WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(Into::into)
    }

    async fn segment_count(pool: &SqlitePool, id: i64) -> DbResult<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM hls_segments WHERE download_id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(Into::into)
    }

    #[tokio::test]
    async fn save_and_load_should_roundtrip_plan() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        assert_eq!(repository.load(DownloadId::new(1)).await?, None);

        let mut plan = sample_plan();
        plan.segments[1].etag = Some("etag-2".to_owned());
        repository.save_plan(DownloadId::new(1), &plan).await?;

        assert_eq!(repository.load(DownloadId::new(1)).await?, Some(plan));
        Ok(())
    }

    #[tokio::test]
    async fn save_plan_should_replace_previous_plan_atomically() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;

        let replacement = HlsPlanRecord {
            playlist_url: "https://example.com/v2.m3u8".to_owned(),
            segments: vec![pending_segment(0)],
        };
        repository
            .save_plan(DownloadId::new(1), &replacement)
            .await?;

        assert_eq!(
            repository.load(DownloadId::new(1)).await?,
            Some(replacement)
        );
        assert_eq!(segment_count(&db.pool, 1).await?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn save_plan_should_rollback_on_invalid_segment() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;

        let mut broken = sample_plan();
        broken.playlist_url = "https://example.com/broken.m3u8".to_owned();
        broken.segments[1].duration = f64::NAN;
        assert!(
            repository
                .save_plan(DownloadId::new(1), &broken)
                .await
                .is_err()
        );

        assert_eq!(
            repository.load(DownloadId::new(1)).await?,
            Some(sample_plan()),
            "failed save must keep the previous plan"
        );
        Ok(())
    }

    #[tokio::test]
    async fn save_plan_should_require_contiguous_ordered_indices() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());

        let mut gap = sample_plan();
        gap.segments[1].index = 2;
        assert!(matches!(
            repository.save_plan(DownloadId::new(1), &gap).await,
            Err(DbError::HlsSegmentsOutOfOrder { .. })
        ));

        let mut unsorted = sample_plan();
        unsorted.segments.swap(0, 1);
        assert!(matches!(
            repository.save_plan(DownloadId::new(1), &unsorted).await,
            Err(DbError::HlsSegmentsOutOfOrder { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn save_plan_should_reject_unpaired_completion_metadata() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());

        let mut length_only = sample_plan();
        length_only.segments[0].length = Some(10);
        assert!(matches!(
            repository.save_plan(DownloadId::new(1), &length_only).await,
            Err(DbError::HlsCompletionMetadataMismatch { index: 0 })
        ));

        let mut sha_only = sample_plan();
        sha_only.segments[1].sha256 = Some(SAMPLE_SHA.to_owned());
        assert!(matches!(
            repository.save_plan(DownloadId::new(1), &sha_only).await,
            Err(DbError::HlsCompletionMetadataMismatch { index: 1 })
        ));

        let mut paired = sample_plan();
        paired.segments[0].length = Some(10);
        paired.segments[0].sha256 = Some(SAMPLE_SHA.to_owned());
        repository.save_plan(DownloadId::new(1), &paired).await?;
        assert_eq!(repository.load(DownloadId::new(1)).await?, Some(paired));
        Ok(())
    }

    #[tokio::test]
    async fn save_plan_should_reject_out_of_range_durations() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());

        for duration in [0.0, -1.0, f64::INFINITY, MAX_DURATION, 1.0e300] {
            let mut plan = sample_plan();
            plan.segments[0].duration = duration;
            assert!(
                matches!(
                    repository.save_plan(DownloadId::new(1), &plan).await,
                    Err(DbError::InvalidDuration { .. })
                ),
                "duration {duration} must be rejected"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn complete_segment_should_reject_progress_overflow_and_roll_back() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;
        sqlx::query("UPDATE downloads SET downloaded_bytes = ? WHERE id = 1")
            .bind(i64::MAX - 1)
            .execute(&db.pool)
            .await?;

        assert!(matches!(
            repository
                .complete_segment(DownloadId::new(1), 0, 10, SAMPLE_SHA, None, None)
                .await,
            Err(DbError::IntegerOutOfRange { .. })
        ));

        let plan = repository
            .load(DownloadId::new(1))
            .await?
            .expect("plan must exist");
        assert_eq!(
            plan.segments[0].length, None,
            "segment update must roll back"
        );
        assert_eq!(downloaded_bytes(&db.pool, 1).await?, i64::MAX - 1);
        Ok(())
    }

    #[tokio::test]
    async fn complete_segment_should_be_idempotent_for_same_length() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;

        repository
            .complete_segment(DownloadId::new(1), 0, 512, SAMPLE_SHA, None, None)
            .await?;
        repository
            .complete_segment(DownloadId::new(1), 0, 512, SAMPLE_SHA, None, None)
            .await?;

        assert_eq!(downloaded_bytes(&db.pool, 1).await?, 512);
        Ok(())
    }

    #[tokio::test]
    async fn save_plan_should_reject_non_sha256_checksums_and_empty_lengths() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());

        let mut short = sample_plan();
        short.segments[0].length = Some(10);
        short.segments[0].sha256 = Some("abc".to_owned());
        assert!(matches!(
            repository.save_plan(DownloadId::new(1), &short).await,
            Err(DbError::InvalidChecksum { index: 0 })
        ));

        let mut non_hex = sample_plan();
        non_hex.segments[0].length = Some(10);
        non_hex.segments[0].sha256 = Some("z".repeat(SHA256_HEX_LEN));
        assert!(matches!(
            repository.save_plan(DownloadId::new(1), &non_hex).await,
            Err(DbError::InvalidChecksum { index: 0 })
        ));

        let mut empty_length = sample_plan();
        empty_length.segments[0].length = Some(0);
        empty_length.segments[0].sha256 = Some(SAMPLE_SHA.to_owned());
        assert!(matches!(
            repository
                .save_plan(DownloadId::new(1), &empty_length)
                .await,
            Err(DbError::InvalidCompletedLength { index: 0 })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn complete_segment_should_reject_non_sha256_checksum() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;

        for checksum in ["abc", "g".repeat(SHA256_HEX_LEN).as_str()] {
            assert!(matches!(
                repository
                    .complete_segment(DownloadId::new(1), 0, 16, checksum, None, None)
                    .await,
                Err(DbError::InvalidChecksum { index: 0 })
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn complete_segment_should_reject_zero_length() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;

        assert!(matches!(
            repository
                .complete_segment(DownloadId::new(1), 0, 0, SAMPLE_SHA, None, None)
                .await,
            Err(DbError::InvalidCompletedLength { index: 0 })
        ));
        assert_eq!(downloaded_bytes(&db.pool, 1).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn complete_segment_should_reject_aggregate_widened_to_real() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;
        sqlx::query("UPDATE downloads SET downloaded_bytes = 9.5e18 WHERE id = 1")
            .execute(&db.pool)
            .await?;
        let stored_type: String =
            sqlx::query_scalar("SELECT typeof(downloaded_bytes) FROM downloads WHERE id = 1")
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(
            stored_type, "real",
            "precondition: SQLite widened the value"
        );

        assert!(matches!(
            repository
                .complete_segment(DownloadId::new(1), 0, 16, SAMPLE_SHA, None, None)
                .await,
            Err(DbError::NonIntegerAggregate { .. })
        ));

        let plan = repository
            .load(DownloadId::new(1))
            .await?
            .expect("plan must exist");
        assert_eq!(
            plan.segments[0].length, None,
            "segment update must roll back"
        );
        Ok(())
    }

    #[tokio::test]
    async fn save_plan_should_reject_missing_download() -> DbResult<()> {
        let db = test_db().await?;
        let repository = HlsRepository::new(db.pool.clone());
        assert!(matches!(
            repository
                .save_plan(DownloadId::new(9), &sample_plan())
                .await,
            Err(DbError::NotFound { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn complete_segment_should_record_metadata_and_progress() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;

        repository
            .complete_segment(
                DownloadId::new(1),
                1,
                2048,
                SAMPLE_SHA,
                Some("etag-1"),
                Some("Sat, 08 Aug 2026 00:00:00 GMT"),
            )
            .await?;

        let plan = repository
            .load(DownloadId::new(1))
            .await?
            .expect("plan must exist");
        assert_eq!(plan.segments[0].length, None);
        assert_eq!(plan.segments[1].length, Some(2048));
        assert_eq!(plan.segments[1].sha256.as_deref(), Some(SAMPLE_SHA));
        assert_eq!(plan.segments[1].etag.as_deref(), Some("etag-1"));
        assert_eq!(downloaded_bytes(&db.pool, 1).await?, 2048);
        Ok(())
    }

    #[tokio::test]
    async fn complete_segment_should_not_touch_lifecycle_columns() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;
        sqlx::query(
            r#"
            UPDATE downloads
            SET status = 'paused', total_bytes = 4096, started_at = 77,
                completed_at = 88, error_kind = 'network', error_message = 'stalled'
            WHERE id = 1
            "#,
        )
        .execute(&db.pool)
        .await?;

        repository
            .complete_segment(DownloadId::new(1), 0, 1024, SAMPLE_SHA, Some("e"), None)
            .await?;

        let row = sqlx::query(
            r#"
            SELECT status, total_bytes, started_at, completed_at, error_kind, error_message
            FROM downloads WHERE id = 1
            "#,
        )
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(row.try_get::<String, _>("status")?, "paused");
        assert_eq!(row.try_get::<Option<i64>, _>("total_bytes")?, Some(4096));
        assert_eq!(row.try_get::<Option<i64>, _>("started_at")?, Some(77));
        assert_eq!(row.try_get::<Option<i64>, _>("completed_at")?, Some(88));
        assert_eq!(
            row.try_get::<Option<String>, _>("error_kind")?.as_deref(),
            Some("network")
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("error_message")?
                .as_deref(),
            Some("stalled")
        );
        assert_eq!(downloaded_bytes(&db.pool, 1).await?, 1024);
        Ok(())
    }

    #[tokio::test]
    async fn complete_segment_should_reject_unknown_segment_and_empty_checksum() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;

        assert!(matches!(
            repository
                .complete_segment(DownloadId::new(1), 7, 1, SAMPLE_SHA, None, None)
                .await,
            Err(DbError::NotFound { .. })
        ));
        assert!(matches!(
            repository
                .complete_segment(DownloadId::new(1), 1, 1, "  ", None, None)
                .await,
            Err(DbError::EmptyValue { .. })
        ));
        assert_eq!(downloaded_bytes(&db.pool, 1).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn invalidate_segment_should_clear_metadata_and_progress() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;
        repository
            .complete_segment(DownloadId::new(1), 0, 1024, SAMPLE_SHA, None, None)
            .await?;

        repository.invalidate_segment(DownloadId::new(1), 0).await?;

        let plan = repository
            .load(DownloadId::new(1))
            .await?
            .expect("plan must exist");
        assert_eq!(plan.segments[0], pending_segment(0));
        assert_eq!(downloaded_bytes(&db.pool, 1).await?, 0);
        assert!(matches!(
            repository.invalidate_segment(DownloadId::new(1), 5).await,
            Err(DbError::NotFound { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn clear_and_download_removal_should_cascade_hls_rows() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        insert_download(&db.pool, 2).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;
        repository
            .save_plan(DownloadId::new(2), &sample_plan())
            .await?;

        repository.clear(DownloadId::new(1)).await?;
        assert_eq!(repository.load(DownloadId::new(1)).await?, None);
        assert_eq!(segment_count(&db.pool, 1).await?, 0);
        assert!(repository.load(DownloadId::new(2)).await?.is_some());

        let downloads = DownloadRepository::new(db.pool.clone());
        assert!(downloads.remove(DownloadId::new(2)).await?);
        assert_eq!(repository.load(DownloadId::new(2)).await?, None);
        assert_eq!(segment_count(&db.pool, 2).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn schema_should_reject_unpaired_completion_metadata() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;

        let length_only = sqlx::query(
            "UPDATE hls_segments SET length = 10 WHERE download_id = 1 AND segment_index = 0",
        )
        .execute(&db.pool)
        .await;
        assert!(length_only.is_err());

        let sha_only = sqlx::query(
            "UPDATE hls_segments SET sha256 = 'abc' WHERE download_id = 1 AND segment_index = 0",
        )
        .execute(&db.pool)
        .await;
        assert!(sha_only.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn schema_should_reject_negative_or_zero_values() -> DbResult<()> {
        let db = test_db().await?;
        insert_download(&db.pool, 1).await?;
        let repository = HlsRepository::new(db.pool.clone());
        repository
            .save_plan(DownloadId::new(1), &sample_plan())
            .await?;

        for sql in [
            "UPDATE hls_segments SET duration = 0 WHERE download_id = 1",
            "UPDATE hls_segments SET duration = -1 WHERE download_id = 1",
            "UPDATE hls_segments SET sequence = -1 WHERE download_id = 1",
            "UPDATE hls_segments SET url = '' WHERE download_id = 1",
        ] {
            assert!(sqlx::query(sql).execute(&db.pool).await.is_err(), "{sql}");
        }
        Ok(())
    }
}
