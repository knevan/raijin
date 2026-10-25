use std::path::{Path, PathBuf};

use thiserror::Error;

pub mod autostart;
pub mod deeplink;
#[cfg(windows)]
pub mod instance;
pub mod tray;

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
    /// File manager target could not be accessed.
    #[error("cannot access {}: {source}", path.display())]
    PathAccess {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
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

/// Desktop command used to open a path with the OS shell.
///
/// Kept as data (instead of spawning directly) so callers and tests can
/// inspect the exact program and arguments per platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCommand {
    /// Program to execute.
    pub program: String,
    /// Arguments passed to the program.
    pub args: Vec<String>,
}

impl OpenCommand {
    /// Spawns the command detached without waiting for completion.
    ///
    /// # Errors
    ///
    /// Returns an error when the process cannot be spawned.
    pub fn spawn(&self) -> PlatformResult<()> {
        std::process::Command::new(&self.program)
            .args(&self.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(PlatformError::Io)?;
        Ok(())
    }
}

/// Builds the OS command that opens a file with its default application.
#[must_use]
pub fn open_file_command(path: &Path) -> OpenCommand {
    let path = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        OpenCommand {
            program: "cmd".to_owned(),
            args: vec!["/C".to_owned(), "start".to_owned(), String::new(), path],
        }
    }
    #[cfg(target_os = "macos")]
    {
        OpenCommand {
            program: "open".to_owned(),
            args: vec![path],
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        OpenCommand {
            program: "xdg-open".to_owned(),
            args: vec![path],
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        OpenCommand {
            program: String::new(),
            args: vec![path],
        }
    }
}

/// Builds the OS command that reveals a file, or opens its folder if missing.
///
/// Existing directories are opened directly. Linux opens the containing folder
/// without selecting the file.
///
/// # Errors
///
/// Returns an error when the target or its containing folder cannot be accessed.
pub fn reveal_in_folder_command(path: &Path) -> PlatformResult<OpenCommand> {
    if path.as_os_str().is_empty() {
        return Err(PlatformError::PathAccess {
            path: path.to_owned(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "target path is empty"),
        });
    }
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let folder = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            return open_folder_command(folder);
        }
        Err(source) => {
            return Err(PlatformError::PathAccess {
                path: path.to_owned(),
                source,
            });
        }
    };
    if metadata.is_dir() {
        return open_folder_command(path);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        open_folder_command(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        let path = shell_path(path)?.to_string_lossy().into_owned();
        #[cfg(windows)]
        let command = OpenCommand {
            program: "explorer".to_owned(),
            args: vec!["/select,".to_owned(), path],
        };
        #[cfg(target_os = "macos")]
        let command = OpenCommand {
            program: "open".to_owned(),
            args: vec!["-R".to_owned(), path],
        };
        #[cfg(not(any(windows, unix)))]
        let command = OpenCommand {
            program: String::new(),
            args: vec![path],
        };
        Ok(command)
    }
}

fn shell_path(path: &Path) -> PlatformResult<PathBuf> {
    std::fs::canonicalize(path)
        .map(normalize_canonical)
        .map_err(|source| PlatformError::PathAccess {
            path: path.to_owned(),
            source,
        })
}

/// Builds the OS command that opens a directory without selecting it.
///
/// # Errors
///
/// Returns an error when the target is missing, inaccessible, or not a directory.
pub fn open_folder_command(path: &Path) -> PlatformResult<OpenCommand> {
    let target = shell_path(path)?;
    let metadata = std::fs::metadata(&target).map_err(|source| PlatformError::PathAccess {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(PlatformError::PathAccess {
            path: path.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "target is not a directory",
            ),
        });
    }
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    #[cfg(not(any(windows, unix)))]
    let program = "";
    Ok(OpenCommand {
        program: program.to_owned(),
        args: vec![target.to_string_lossy().into_owned()],
    })
}

/// Opens a directory in the system file manager (detached).
///
/// # Errors
///
/// Returns an error when the directory is inaccessible or the OS helper cannot be spawned.
pub fn open_folder(path: &Path) -> PlatformResult<()> {
    open_folder_command(path)?.spawn()
}

/// Opens a file with its default application (detached).
///
/// # Errors
///
/// Returns an error when the path has no file name context or the OS helper
/// cannot be spawned.
pub fn open_file(path: &Path) -> PlatformResult<()> {
    open_file_command(path).spawn()
}

/// Reveals a file in the system file manager (detached).
///
/// # Errors
///
/// Returns an error when the target folder is inaccessible or the OS helper cannot be spawned.
pub fn reveal_in_folder(path: &Path) -> PlatformResult<()> {
    reveal_in_folder_command(path)?.spawn()
}

/// Disk capacity snapshot for the filesystem holding a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    /// Total filesystem size in bytes.
    pub total_bytes: u64,
    /// Free space available to the user in bytes.
    pub available_bytes: u64,
}

impl DiskUsage {
    /// Bytes currently used on the filesystem.
    #[must_use]
    pub fn used_bytes(self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }

    /// Fraction of the filesystem in use, clamped to `0.0..=1.0`.
    ///
    /// Degenerate zero-size filesystems report `0.0` instead of dividing.
    #[must_use]
    pub fn used_fraction(self) -> f32 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.used_bytes() as f32 / self.total_bytes as f32).clamp(0.0, 1.0)
    }
}

/// Detects disk capacity for the filesystem holding `path`.
///
/// The longest matching mount point wins, so nested mounts resolve to the
/// inner filesystem. Returns `None` when the path cannot be canonicalized
/// or no mounted disk contains it.
#[must_use]
pub fn disk_usage_for(path: &Path) -> Option<DiskUsage> {
    let canonical = normalize_canonical(std::fs::canonicalize(path).ok()?);
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let disk = disks
        .iter()
        .filter(|disk| canonical.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())?;
    Some(DiskUsage {
        total_bytes: disk.total_space(),
        available_bytes: disk.available_space(),
    })
}

/// Strips Windows verbatim prefixes (`\\?\`, `\\?\UNC\`) so canonicalized
/// paths compare equal against mount points like `C:\`.
fn normalize_canonical(path: PathBuf) -> PathBuf {
    const VERBATIM: &str = r"\\?\";
    const VERBATIM_UNC: &str = r"\\?\UNC\";
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(VERBATIM_UNC) {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(VERBATIM) {
        return PathBuf::from(rest.to_owned());
    }
    path
}
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

    #[test]
    fn open_commands_should_reference_target_path() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("file.zip");
        std::fs::write(&path, b"download").expect("download file");

        let open = open_file_command(&path);
        let reveal = reveal_in_folder_command(&path).expect("reveal command");

        assert!(!open.program.is_empty());
        assert!(!reveal.program.is_empty());
        assert!(
            open.args.iter().any(|arg| arg.contains("file.zip")),
            "open command must reference the file: {open:?}"
        );
        assert!(
            reveal.args.iter().any(|arg| arg.contains("file.zip")
                || arg
                    == shell_path(dir.path())
                        .expect("folder path")
                        .to_string_lossy()
                        .as_ref()),
            "reveal command must reference the file or its folder: {reveal:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn reveal_command_should_separate_select_switch_from_spaced_unicode_path() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("Antigravity IDE 日本語.exe");
        std::fs::write(&path, b"download").expect("download file");
        let expected = normalize_canonical(std::fs::canonicalize(&path).expect("canonical path"));

        let command = reveal_in_folder_command(&path).expect("reveal command");

        assert_eq!(command.program, "explorer");
        assert_eq!(
            command.args,
            vec![
                "/select,".to_owned(),
                expected.to_string_lossy().into_owned()
            ]
        );
    }

    #[test]
    fn reveal_command_should_open_parent_when_file_is_missing() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("missing download.exe");

        let command = reveal_in_folder_command(&path).expect("fallback command");

        assert_eq!(
            command,
            open_folder_command(dir.path()).expect("folder command")
        );
    }

    #[test]
    fn reveal_command_should_open_existing_directory_without_selecting_it() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let expected =
            normalize_canonical(std::fs::canonicalize(dir.path()).expect("canonical path"));

        let command = reveal_in_folder_command(dir.path()).expect("folder command");

        assert_eq!(command.args, vec![expected.to_string_lossy().into_owned()]);
    }

    #[test]
    fn reveal_command_should_fail_when_destination_folder_is_missing() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let folder = dir.path().join("missing folder");

        let error =
            reveal_in_folder_command(&folder.join("download.exe")).expect_err("missing folder");

        assert!(matches!(&error, PlatformError::PathAccess { path, source }
            if path == &folder && source.kind() == std::io::ErrorKind::NotFound));
        assert!(error.to_string().contains("missing folder"));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn open_folder_command_should_not_fall_back_to_parent_for_missing_directory() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("missing folder");

        assert!(open_folder_command(&path).is_err());
    }

    #[test]
    fn open_folder_command_should_reject_file_targets() {
        let file = tempfile::NamedTempFile::new().expect("temporary file");

        let error = open_folder_command(file.path()).expect_err("not a directory");

        assert!(matches!(error, PlatformError::PathAccess { source, .. }
            if source.kind() == std::io::ErrorKind::NotADirectory));
    }

    #[test]
    fn reveal_command_should_reject_empty_path() {
        assert!(reveal_in_folder_command(Path::new("")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn shell_paths_should_strip_verbatim_prefixes() {
        assert_eq!(
            normalize_canonical(PathBuf::from(r"\\?\C:\Downloads\file.exe")),
            PathBuf::from(r"C:\Downloads\file.exe")
        );
        assert_eq!(
            normalize_canonical(PathBuf::from(r"\\?\UNC\server\share\file.exe")),
            PathBuf::from(r"\\server\share\file.exe")
        );
    }

    #[test]
    fn disk_usage_should_report_capacity_for_existing_dir() {
        let dir = std::env::temp_dir();
        let usage = disk_usage_for(&dir).expect("temp dir must sit on a mounted disk");

        assert!(usage.total_bytes > 0, "total must be positive: {usage:?}");
        assert!(
            usage.available_bytes <= usage.total_bytes,
            "available cannot exceed total: {usage:?}"
        );
        assert!((0.0..=1.0).contains(&usage.used_fraction()));
        assert_eq!(
            usage.used_bytes(),
            usage.total_bytes - usage.available_bytes
        );
    }

    #[test]
    fn disk_usage_should_reject_missing_paths() {
        assert_eq!(
            disk_usage_for(&PathBuf::from("raijin-definitely-missing-dir-xyz")),
            None
        );
    }
}
