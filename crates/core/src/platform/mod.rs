use std::path::{Path, PathBuf};

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
    let dir = std::env::var_os("RAIJIN_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| platform_app_data_base_dir().map(raijin_app_data_dir))
        .ok_or(PlatformError::DataDirectoryUnavailable)?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(windows)]
fn platform_app_data_base_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("AppData/Roaming"))
        })
        .or_else(|| std::env::current_dir().ok())
}

#[cfg(target_os = "macos")]
fn platform_app_data_base_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"))
        .or_else(|| std::env::current_dir().ok())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_app_data_base_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .or_else(|| std::env::current_dir().ok())
}

#[cfg(not(any(windows, unix)))]
fn platform_app_data_base_dir() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

/// Returns default download folder.
///
/// # Errors
///
/// Returns an error when no suitable folder exists or creation fails.
pub fn default_download_dir() -> PlatformResult<PathBuf> {
    let dir = match std::env::var_os("RAIJIN_DOWNLOAD_DIR") {
        Some(path) => PathBuf::from(path),
        None => default_download_dir_from_category_root(&download_category_root_dir()?),
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Returns the root directory for user-visible download categories.
///
/// # Errors
///
/// Returns an error when no suitable downloads directory exists or creation fails.
pub fn download_category_root_dir() -> PlatformResult<PathBuf> {
    let dir = std::env::var_os("RAIJIN_DOWNLOAD_ROOT")
        .map(PathBuf::from)
        .or_else(|| platform_downloads_base_dir().map(raijin_download_root_dir))
        .ok_or(PlatformError::DataDirectoryUnavailable)?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(windows)]
fn platform_downloads_base_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(|home| PathBuf::from(home).join("Downloads"))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads")))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|dir| dir.join("Downloads"))
        })
}

#[cfg(unix)]
fn platform_downloads_base_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads")))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|dir| dir.join("Downloads"))
        })
}

#[cfg(not(any(windows, unix)))]
fn platform_downloads_base_dir() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .map(|dir| dir.join("Downloads"))
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

fn raijin_app_data_dir(base: PathBuf) -> PathBuf {
    base.join("raijin")
}

fn raijin_download_root_dir(downloads_dir: PathBuf) -> PathBuf {
    downloads_dir.join("raijin")
}

fn default_download_dir_from_category_root(category_root: &Path) -> PathBuf {
    category_root.join("Downloads")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_data_dir_should_append_raijin_to_platform_base() {
        assert_eq!(
            raijin_app_data_dir(PathBuf::from("base")),
            PathBuf::from("base").join("raijin")
        );
    }

    #[test]
    fn download_root_should_append_raijin_to_user_downloads() {
        assert_eq!(
            raijin_download_root_dir(PathBuf::from("Downloads")),
            PathBuf::from("Downloads").join("raijin")
        );
    }

    #[test]
    fn default_download_dir_should_live_under_download_category_root() {
        assert_eq!(
            default_download_dir_from_category_root(&PathBuf::from("Downloads").join("raijin")),
            PathBuf::from("Downloads").join("raijin").join("Downloads")
        );
    }
}
