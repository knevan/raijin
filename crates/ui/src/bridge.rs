use std::path::PathBuf;

use raijin_core::download::{DownloadId, DownloadManagerHandle, NewDownload, QueueId};
use raijin_core::monitor::DownloadMonitorHandle;
use raijin_core::queue::QueueManagerHandle;

/// Cloneable engine endpoints owned by GPUI views.
#[derive(Debug, Clone)]
pub struct EngineHandles {
    /// Actor handle for pause/resume/remove/reset/add commands.
    pub manager: DownloadManagerHandle,
    /// Queue actor handle for membership and admission commands.
    pub queues: QueueManagerHandle,
    /// Projected monitor state subscribed by the polling task.
    pub monitor: DownloadMonitorHandle,
    /// Companion Tokio runtime for spawning engine commands.
    pub runtime: tokio::runtime::Handle,
    /// Default destination folder for new downloads.
    pub default_folder: PathBuf,
    /// Root folder that holds one subfolder per file category.
    pub category_root: PathBuf,
    /// Whether new-download forms check "Use category" by default.
    pub use_category_default: bool,
}

/// Engine command dispatched from UI event handlers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCommand {
    /// Pause one download.
    Pause(DownloadId),
    /// Resume one download (re-queues it for worker startup).
    Resume(DownloadId),
    /// Remove one download and delete its files.
    Remove(DownloadId),
    /// Reset one download after a validation error so it re-probes fresh.
    Reset(DownloadId),
    /// Pause every active download (tray "Pause all", one actor command).
    PauseAll,
    /// Resume every paused download (tray "Resume all", one actor command).
    ResumeAll,
    /// Add a download and immediately resume it.
    Add(NewDownload),
    /// Add a download without resuming it (stays `Added` until resumed).
    AddPaused(NewDownload),
    /// Move downloads into a queue (drag onto a queue).
    MoveToQueue {
        /// Destination queue.
        queue_id: QueueId,
        /// Downloads to move, in selection order.
        downloads: Vec<DownloadId>,
    },
}

impl EngineHandles {
    /// Dispatches a command without blocking the GPUI thread.
    ///
    /// Failures are logged; visible state (including error kinds) arrives
    /// through the monitor projection on the next poll tick.
    pub fn dispatch(&self, command: EngineCommand) {
        let manager = self.manager.clone();
        let queues = self.queues.clone();
        self.runtime.spawn(async move {
            // Each arm keeps its own error type: queue and engine failures are
            // different domain errors and must not be flattened into one.
            match command {
                EngineCommand::MoveToQueue {
                    queue_id,
                    downloads,
                } => {
                    if let Err(error) = queues.enqueue(queue_id, downloads).await {
                        tracing::warn!(?error, "queue move failed");
                    }
                }
                other => {
                    if let Err(error) = other.dispatch_to(&manager, &queues).await {
                        tracing::warn!(?error, "engine command failed");
                    }
                }
            }
        });
    }
}

impl EngineCommand {
    /// Runs an engine-owned command against the download manager.
    ///
    /// # Errors
    ///
    /// Returns the manager error when the command could not be applied.
    async fn dispatch_to(
        self,
        manager: &DownloadManagerHandle,
        queues: &QueueManagerHandle,
    ) -> raijin_core::download::DownloadManagerResult<()> {
        match self {
            Self::Pause(id) => manager.pause(id).await.map(|_| ()),
            Self::Resume(id) => manager.resume(id).await.map(|_| ()),
            Self::Remove(id) => manager.remove(id).await.map(|_| ()),
            Self::Reset(id) => manager.reset(id).await.map(|_| ()),
            Self::PauseAll => manager.pause_all().await.map(|_| ()),
            Self::ResumeAll => manager.resume_all().await.map(|_| ()),
            // Adds must go through the queue: admission is the only path that
            // starts a job, so an add without membership would never run.
            Self::Add(request) => Self::add_queued(manager, queues, request, true).await,
            Self::AddPaused(request) => Self::add_queued(manager, queues, request, false).await,
            Self::MoveToQueue { .. } => Ok(()),
        }
    }

    /// Adds a download, attaches it to Main, and optionally starts it.
    ///
    /// # Errors
    ///
    /// Returns the manager error when the add fails, or `Stopped` when the
    /// queue actor is gone.
    async fn add_queued(
        manager: &DownloadManagerHandle,
        queues: &QueueManagerHandle,
        request: NewDownload,
        start: bool,
    ) -> raijin_core::download::DownloadManagerResult<()> {
        let added = manager.add(request).await?;
        queues
            .enqueue(QueueId::MAIN, [added.id])
            .await
            .map_err(|_| raijin_core::download::DownloadManagerError::Stopped)?;
        if start {
            manager.start(added.id).await.map(|_| ())
        } else {
            Ok(())
        }
    }
}

/// Validation failure for the add-download form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewDownloadError {
    /// URL is empty or uses an unsupported scheme.
    InvalidUrl,
}

impl std::fmt::Display for NewDownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl => f.write_str("Enter a valid http(s) URL"),
        }
    }
}

/// Builds a [`NewDownload`] request from raw form input.
///
/// An empty file name falls back to the last URL path segment, then to
/// `download.bin`. Only `http` and `https` URLs are accepted; the engine
/// speaks plain HTTP and anything else would fail obscurely later.
pub fn build_new_download(
    url: &str,
    file_name: &str,
    folder: PathBuf,
) -> Result<NewDownload, NewDownloadError> {
    let url = url.trim();
    if !is_http_url(url) {
        return Err(NewDownloadError::InvalidUrl);
    }
    let file_name = file_name.trim();
    let file_name = if file_name.is_empty() {
        suggest_filename(url)
    } else {
        file_name.to_owned()
    };
    Ok(NewDownload::http(url, file_name, folder))
}

fn is_http_url(url: &str) -> bool {
    is_http_url_str(url)
}

/// Returns true when `url` uses an `http(s)` scheme with a non-empty rest.
///
/// Shared by the main-page dialog path and the standalone Add Download
/// window so both validate identically.
#[must_use]
pub fn is_http_url_str(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")) && url.len() > "https://".len()
}

/// Suggests a file name from the last URL path segment.
///
/// Query strings and fragments are stripped, then percent-escapes are decoded
/// like ABDM `FilenameDecoder` (keeps `+` literal, tolerant on bad escapes).
/// Bare hosts, trailing slashes, and unusable segments fall back to
/// `download.bin`.
#[must_use]
pub fn suggest_filename(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let path = after_scheme.split(['?', '#']).next().unwrap_or_default();
    let segment = path.rsplit('/').next().unwrap_or_default().trim();
    if !path.contains('/') || segment.is_empty() || segment.contains(['\\', ':']) {
        return "download.bin".to_owned();
    }
    // Single decoder owned by core (ABDM `FilenameDecoder` parity).
    let decoded = raijin_core::download::decode_percent_filename(segment);
    let decoded = decoded.trim();
    if decoded.is_empty() {
        return "download.bin".to_owned();
    }
    // Cap the suggestion so absurdly long segments cannot create hostile names.
    decoded.chars().take(128).collect()
}

#[cfg(test)]
mod tests {
    // Explicit imports (see format.rs): a `use super::*` glob would shadow
    // the built-in `#[test]` attribute with GPUI's `test` macro.
    use std::path::PathBuf;

    use super::{NewDownloadError, build_new_download, suggest_filename};

    #[test]
    fn build_should_accept_http_urls_and_derive_filename() {
        let request = build_new_download(
            "https://example.com/files/app.zip",
            "",
            PathBuf::from("C:/Downloads"),
        )
        .expect("valid URL must build");

        assert_eq!(request.url, "https://example.com/files/app.zip");
        assert_eq!(request.file_name, "app.zip");
    }

    #[test]
    fn build_should_keep_explicit_filename_and_trim_input() {
        let request = build_new_download(
            "  https://example.com/app.zip  ",
            "  custom-name.zip ",
            PathBuf::from("C:/Downloads"),
        )
        .expect("valid URL must build");

        assert_eq!(request.url, "https://example.com/app.zip");
        assert_eq!(request.file_name, "custom-name.zip");
    }

    #[test]
    fn build_should_reject_non_http_urls() {
        for url in ["", "ftp://example.com/file.zip", "notaurl", "https://"] {
            assert_eq!(
                build_new_download(url, "file.zip", PathBuf::from("C:/Downloads")),
                Err(NewDownloadError::InvalidUrl),
                "must reject {url:?}"
            );
        }
    }

    #[test]
    fn suggest_should_strip_query_and_fallback() {
        assert_eq!(
            suggest_filename("https://example.com/a/b.zip?v=2#frag"),
            "b.zip"
        );
        assert_eq!(suggest_filename("https://example.com/"), "download.bin");
        assert_eq!(suggest_filename("https://example.com"), "download.bin");
    }

    #[test]
    fn suggest_should_decode_percent_escapes_like_abdm() {
        assert_eq!(
            suggest_filename("https://example.com/windows-x64/Antigravity%20IDE.exe"),
            "Antigravity IDE.exe"
        );
        // `+` stays literal (not a space), matching ABDM `FilenameDecoder`.
        assert_eq!(suggest_filename("https://example.com/a%2Bb.zip"), "a+b.zip");
        // Stray `%` stays literal so no name is lost.
        assert_eq!(suggest_filename("https://example.com/100%.exe"), "100%.exe");
    }
}
