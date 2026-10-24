//! Process logging and crash diagnostics for the GUI binary.
//!
//! The GUI binary is a Windows GUI-subsystem executable: the OS allocates no
//! console, so stdout/stderr writes are discarded silently (the OS handle is
//! null and the write still reports success). Both routine logs and fatal
//! startup/crash output therefore need a file sink to stay observable.
//!
//! Scope follows ABDM: the routine log file is written in debug builds only;
//! crashes are always recorded because a release build has no other channel.

use std::fmt::Display;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

use crate::platform;

/// File name of the routine log written in debug builds.
const APP_LOG_NAME: &str = "AppLog.log";
/// File name receiving panic payloads and fatal startup errors.
const CRASH_LOG_NAME: &str = "crash.log";
/// Subdirectory created under the platform app data directory.
const LOG_DIR_NAME: &str = "log";

/// Env var that turns on the routine log file outside debug builds.
pub const LOG_FILE_ENV: &str = "RAIJIN_LOG_FILE";

/// Returns `<app data>/log`, creating the directory when missing.
///
/// # Errors
///
/// Returns the platform error when the app data directory is unavailable or
/// the log directory cannot be created.
pub fn log_dir() -> platform::PlatformResult<PathBuf> {
    let dir = platform::app_data_dir()?.join(LOG_DIR_NAME);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Compile-time switch for the routine log file, plus the env override.
///
/// The file sink is off in release builds: it costs writes on every event and
/// no production consumer reads it. [`LOG_FILE_ENV`] can force it on when a
/// release build has to be diagnosed on a user machine.
#[must_use]
pub fn app_log_file_enabled() -> bool {
    cfg!(debug_assertions) || std::env::var_os(LOG_FILE_ENV).is_some()
}

/// Writer handle shared by the tracing layer and the crash paths.
///
/// One `Arc` is cloned into the subscriber; the panic hook and fatal-exit
/// helper open the same path themselves, so both survive a failed
/// subscriber install.
#[derive(Clone, Debug)]
pub struct LogFile {
    path: PathBuf,
    sink: std::sync::Arc<Mutex<File>>,
}

impl LogFile {
    /// Opens (or creates) `path` in append mode.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the file cannot be opened for appending.
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            sink: std::sync::Arc::new(Mutex::new(file)),
        })
    }

    /// Path this handle appends to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one already-formatted line.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the write fails.
    pub fn write_line(&self, line: &str) -> std::io::Result<()> {
        let mut guard = self
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.write_all(line.as_bytes())?;
        guard.write_all(b"\n")?;
        guard.flush()
    }
}

impl<'a> MakeWriter<'a> for LogFile {
    type Writer = SharedFileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedFileWriter(std::sync::Arc::clone(&self.sink))
    }
}

/// Per-event writer that locks the shared file for the duration of one write.
///
/// `tracing_subscriber` requires the writer type itself to implement
/// `io::Write`; the lock is taken per write and never held across an await, so
/// concurrent engine threads serialize only on the file append.
pub struct SharedFileWriter(std::sync::Arc<Mutex<File>>);

impl Write for SharedFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut guard = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.flush()
    }
}

/// True when stdout is attached to a terminal.
///
/// Debug runs from a terminal keep their console stream; GUI launches (and
/// every release launch) do not, so they skip the console layer entirely.
#[must_use]
pub fn stdout_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

/// Installs the GUI binary's subscriber: optional file sink in debug builds,
/// optional console layer when stdout is a real terminal.
///
/// A failed file open is reported on stderr and drops only the file layer;
/// tracing setup must never abort startup.
pub fn init_app_tracing() {
    use tracing_subscriber::prelude::*;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = if app_log_file_enabled() {
        match log_dir()
            .map_err(|error| error.to_string())
            .and_then(|dir| {
                LogFile::open(dir.join(APP_LOG_NAME)).map_err(|error| error.to_string())
            }) {
            Ok(file) => Some(
                tracing_subscriber::fmt::layer()
                    .with_writer(file)
                    .with_ansi(false),
            ),
            Err(error) => {
                eprintln!("log file unavailable, logging to console only: {error}");
                None
            }
        }
    } else {
        None
    };
    let console_layer = stdout_is_terminal().then(|| {
        tracing_subscriber::fmt::layer()
            .with_ansi(true)
            .with_writer(std::io::stdout)
    });
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(console_layer)
        .try_init();
}

/// Records `message` plus the current backtrace in the crash log.
///
/// Appends rather than truncates so repeated failures from one session stay
/// visible. Returns the write error so callers can surface it on stderr when
/// one exists.
///
/// # Errors
///
/// Returns the platform or I/O error when the crash log cannot be written.
pub fn append_crash_log(message: &str) -> Result<(), String> {
    append_crash_log_at(&crash_log_path()?, message)
}

/// Appends one crash entry to an explicit path.
///
/// Split from [`append_crash_log`] so tests exercise the real writer without
/// redirecting the process app-data directory.
///
/// # Errors
///
/// Returns a message when the file cannot be opened or written.
fn append_crash_log_at(path: &Path, message: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("crash log {} unavailable: {error}", path.display()))?;
    writeln!(
        file,
        "[{}] {message}\nbacktrace:\n{}",
        timestamp(),
        std::backtrace::Backtrace::force_capture()
    )
    .map_err(|error| format!("crash log {} write failed: {error}", path.display()))?;
    Ok(())
}

/// Absolute path of the crash log file.
///
/// # Errors
///
/// Returns a message when the log directory is unavailable.
pub fn crash_log_path() -> Result<PathBuf, String> {
    log_dir()
        .map(|dir| dir.join(CRASH_LOG_NAME))
        .map_err(|error| format!("no log directory: {error}"))
}

/// Installs a panic hook that records the payload and location before the
/// default hook prints.
///
/// Required because `profile.deploy` uses `panic = "abort"`: the process dies
/// with no message and no console, so the file is the only evidence.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_panic(info, |message| {
            if let Err(error) = append_crash_log(message) {
                eprintln!("{error}");
            }
        });
        default_hook(info);
    }));
}

/// Formats a panic into a crash-log message and hands it to `record`.
///
/// Split from [`install_panic_hook`] so the formatting contract is testable
/// without swapping the process-wide hook.
fn record_panic(info: &std::panic::PanicHookInfo<'_>, record: impl FnOnce(&str)) {
    let location = info
        .location()
        .map(|location| location.to_string())
        .unwrap_or_else(|| "<unknown location>".to_owned());
    record(&format!("panic at {location}: {}", panic_payload(info)));
}

/// Extracts a printable panic message from the payload.
fn panic_payload(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_owned();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "<non-string panic payload>".to_owned()
}

/// Records a fatal startup error and returns the message for the caller.
///
/// Writing the crash log is best effort: the original error must reach the
/// caller even when the file cannot be written.
#[must_use]
pub fn record_fatal<E: Display>(context: &str, error: E) -> String {
    let message = format!("{context}: {error}");
    if let Err(log_error) = append_crash_log(&format!("fatal: {message}")) {
        eprintln!("{log_error}");
    }
    message
}

/// Wall-clock timestamp for crash log entries.
///
/// Crash logging must not depend on the tracing subscriber (it can be the
/// thing that failed), so this reads the system clock directly.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    format!("{now}ms since epoch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_file_should_append_lines_and_survive_reopen() {
        let dir = tempfile::tempdir().expect("temp dir must exist");
        let path = dir.path().join(LOG_DIR_NAME).join(APP_LOG_NAME);
        let file = LogFile::open(path.clone()).expect("open must succeed");
        assert_eq!(file.path(), path, "path must be preserved");
        file.write_line("first").expect("write must succeed");
        drop(file);

        let reopened = LogFile::open(path.clone()).expect("reopen must succeed");
        reopened.write_line("second").expect("write must succeed");
        drop(reopened);

        let content = std::fs::read_to_string(&path).expect("log must exist");
        assert_eq!(content, "first\nsecond\n");
    }

    #[test]
    fn crash_entry_should_append_payload_and_backtrace() {
        let dir = tempfile::tempdir().expect("temp dir must exist");
        let path = dir.path().join(CRASH_LOG_NAME);
        append_crash_log_at(&path, "boom").expect("crash log must be writable");
        append_crash_log_at(&path, "boom again").expect("crash log must append");

        let content = std::fs::read_to_string(&path).expect("crash log must exist");
        assert!(content.contains("boom"), "payload missing: {content}");
        assert!(content.contains("boom again"), "append missing: {content}");
        assert!(content.contains("backtrace:"), "backtrace missing");
    }

    #[test]
    fn crash_entry_should_report_unwritable_path_with_context() {
        let dir = tempfile::tempdir().expect("temp dir must exist");
        // A directory at the target path makes the open fail deterministically.
        let path = dir.path().join("crash-dir");
        std::fs::create_dir(&path).expect("dir must exist");
        let error = append_crash_log_at(&path, "boom").expect_err("open must fail");
        assert!(error.contains("crash log"), "context missing: {error}");
        assert!(error.contains("crash-dir"), "path missing: {error}");
    }

    #[test]
    fn panic_hook_should_record_payload_and_location() {
        // Capture through the real hook plumbing: install a quiet hook that
        // forwards to the recorder, then panic inside `catch_unwind`.
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = std::sync::Arc::clone(&captured);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            record_panic(info, |message| {
                *sink.lock().expect("capture lock") = message.to_owned();
            });
        }));
        let _ = std::panic::catch_unwind(|| panic!("boom from test"));
        std::panic::set_hook(previous);

        let message = captured.lock().expect("capture lock").clone();
        assert!(
            message.starts_with("panic at "),
            "location missing: {message}"
        );
        assert!(
            message.contains("boom from test"),
            "payload missing: {message}"
        );
    }

    #[test]
    fn app_log_file_should_follow_debug_builds() {
        // The env override only matters in release builds; asserting the
        // debug default keeps the contract documented and tested.
        if cfg!(debug_assertions) {
            assert!(app_log_file_enabled());
        }
    }
}
