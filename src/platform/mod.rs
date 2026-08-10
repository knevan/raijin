use std::path::PathBuf;

use thiserror::Error;

/// Result type for platform helpers.
pub type PlatformResult<T> = Result<T, PlatformError>;

/// Platform helper errors.
#[derive(Debug, Error)]
pub enum PlatformError {
    /// Current process has no usable working or home data directory.
    #[error("no usable application data directory found")]
    DataDirectoryUnavailable,
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Path cannot be represented as UTF-8 for SQLite URL use.
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
}

/// Returns Raijin's application data directory.
///
/// # Errors
///
/// Returns an error when no suitable base directory exists or creation fails.
pub fn app_data_dir() -> PlatformResult<PathBuf> {
    let base = std::env::var_os("RAIJIN_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .or_else(|| std::env::current_dir().ok())
        .ok_or(PlatformError::DataDirectoryUnavailable)?;
    let dir = base.join("raijin");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Returns default download folder.
///
/// # Errors
///
/// Returns an error when no suitable folder exists or creation fails.
pub fn default_download_dir() -> PlatformResult<PathBuf> {
    let dir = std::env::var_os("RAIJIN_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Downloads"))
        })
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads")))
        .or_else(|| std::env::current_dir().ok())
        .ok_or(PlatformError::DataDirectoryUnavailable)?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Returns SQLite URL for the desktop database.
///
/// # Errors
///
/// Returns an error when the app data directory cannot be created or path is not UTF-8.
pub fn desktop_database_url() -> PlatformResult<String> {
    if let Ok(url) = std::env::var("RAIJIN_DATABASE_URL") {
        return Ok(url);
    }
    sqlite_url(app_data_dir()?.join("raijin.sqlite"))
}

fn sqlite_url(path: PathBuf) -> PlatformResult<String> {
    let value = path
        .to_str()
        .ok_or_else(|| PlatformError::NonUtf8Path(path.clone()))?;
    Ok(format!("sqlite://{value}"))
}
