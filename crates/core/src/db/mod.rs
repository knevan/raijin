use std::str::FromStr;

use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{ConnectOptions, SqlitePool};

pub mod repositories;

pub use repositories::{
    DbError, DbResult, DownloadRepository, PartRepository, QueueRepository, SettingsRepository,
};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Creates a SQLite pool with Raijin's required connection settings.
///
/// # Errors
///
/// Returns an error when the database URL is invalid or SQLite cannot open the database.
pub async fn connect(database_url: &str) -> DbResult<SqlitePool> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .disable_statement_logging();

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;

    Ok(pool)
}

/// Runs all pending migrations against an existing SQLite pool.
///
/// # Errors
///
/// Returns an error when any migration fails.
pub async fn run_migrations(pool: &SqlitePool) -> DbResult<()> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

/// Opens a SQLite database, enables required pragmas, and runs migrations.
///
/// # Errors
///
/// Returns an error when connecting or migrating fails.
pub async fn bootstrap(database_url: &str) -> DbResult<SqlitePool> {
    let pool = connect(database_url).await?;
    run_migrations(&pool).await?;
    tracing::debug!("sqlite database bootstrapped");
    Ok(pool)
}
