#[cfg(test)]
pub(crate) mod queue_tests;

use std::path::PathBuf;

use gpui_kit::{App, Window};
use raijin_core::download::{DownloadId, DownloadManagerHandle, NewDownload, QueueId};
use raijin_core::monitor::DownloadMonitorHandle;
use raijin_core::queue::QueueManagerHandle;

#[cfg(windows)]
pub fn spawn_launch_dispatcher(
    cx: &mut gpui_kit::App,
    mut receiver: tokio::sync::mpsc::Receiver<raijin_core::platform::instance::DesktopCommand>,
    handles: EngineHandles,
    tray: crate::tray::TraySession,
    settings: crate::settings::SettingsHandles,
) {
    use raijin_core::platform::instance::DesktopCommand;

    cx.spawn(async move |cx| {
        while let Some(command) = receiver.recv().await {
            cx.update(|cx| match command {
                DesktopCommand::BrowserIntegration => {
                    crate::settings::open_browser_integration(cx, &settings)
                }
                DesktopCommand::Activate => {
                    if cx.windows().is_empty() {
                        crate::open_main_window(cx, &handles, &tray, &settings);
                    } else {
                        for window in cx.windows() {
                            if let Err(error) =
                                window.update(cx, |_, window, _| window.activate_window())
                            {
                                tracing::warn!(%error, "could not activate desktop window");
                            }
                        }
                    }
                }
                DesktopCommand::Add(url) => {
                    crate::add_download::open_add_download_window(cx, &handles, &url)
                }
                DesktopCommand::Background => {}
            });
        }
    })
    .detach();
}

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
    /// Whether new-download forms check "Use Category" by default.
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
    AddToQueue {
        request: NewDownload,
        queue_id: QueueId,
        autostart: bool,
    },
    Queue(QueueAction),
    /// Move downloads into a queue (drag onto a queue).
    MoveToQueue {
        /// Destination queue.
        queue_id: QueueId,
        /// Downloads to move, in selection order.
        downloads: Vec<DownloadId>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueAction {
    Create(String),
    Save {
        id: QueueId,
        name: String,
        limit: std::num::NonZeroU16,
        stop_on_empty: bool,
    },
    Delete(QueueId),
    Start(QueueId),
    Stop(QueueId),
    Reorder {
        id: QueueId,
        order: Vec<DownloadId>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum CommandFailure {
    #[error(transparent)]
    Queue(#[from] raijin_core::queue::QueueManagerError),
    #[error("command interrupted: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl CommandFailure {
    pub fn created_download(&self) -> Option<DownloadId> {
        match self {
            Self::Queue(raijin_core::queue::QueueManagerError::AdmissionAfterAdd {
                id, ..
            }) => Some(*id),
            _ => None,
        }
    }
}

impl EngineHandles {
    /// Dispatches a command for windowless callers; failures are logged.
    pub fn dispatch(&self, command: EngineCommand) {
        self.execute(command);
    }

    fn execute(
        &self,
        command: EngineCommand,
    ) -> tokio::task::JoinHandle<raijin_core::queue::QueueManagerResult<()>> {
        let manager = self.manager.clone();
        let queues = self.queues.clone();
        self.runtime.spawn(async move {
            let result = command.dispatch_to(&manager, &queues).await;
            if let Err(error) = &result {
                tracing::warn!(?error, "download command failed");
            }
            result
        })
    }

    pub fn dispatch_in(&self, command: EngineCommand, window: &mut Window, cx: &mut App) {
        use gpui_kit::component::WindowExt as _;
        self.run_in(command, window, cx, |result, window, cx| {
            if let Err(error) = result {
                window.push_notification(error.to_string(), cx);
            }
        });
    }

    pub fn run_in(
        &self,
        command: EngineCommand,
        window: &mut Window,
        cx: &mut App,
        complete: impl FnOnce(Result<(), CommandFailure>, &mut Window, &mut App) + 'static,
    ) {
        let task = self.execute(command);
        let handle = window.window_handle();
        cx.spawn(async move |cx| {
            let result = match task.await {
                Ok(result) => result.map_err(CommandFailure::from),
                Err(error) => Err(CommandFailure::from(error)),
            };
            let _ = handle.update(cx, |_, window, cx| complete(result, window, cx));
        })
        .detach();
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
    ) -> raijin_core::queue::QueueManagerResult<()> {
        if let Self::Queue(action) = self {
            return match action {
                QueueAction::Create(name) => queues.create(name).await.map(|_| ()),
                QueueAction::Delete(id) => queues.delete(id).await.map(|_| ()),
                QueueAction::Start(id) => queues.start(id).await,
                QueueAction::Stop(id) => queues.stop(id).await,
                QueueAction::Reorder { id, order } => queues.reorder(id, order).await,
                QueueAction::Save {
                    id,
                    name,
                    limit,
                    stop_on_empty,
                } => queues.configure(id, name, limit, stop_on_empty).await,
            };
        }
        let result = match self {
            Self::Pause(id) => manager.pause(id).await.map(|_| ()),
            Self::Resume(id) => manager.resume(id).await.map(|_| ()),
            Self::Remove(id) => manager.remove(id).await.map(|_| ()),
            Self::Reset(id) => manager.reset(id).await.map(|_| ()),
            Self::PauseAll => manager.pause_all().await.map(|_| ()),
            Self::ResumeAll => manager.resume_all().await.map(|_| ()),
            // Adds must go through the queue: admission is the only path that
            // starts a job, so an add without membership would never run.
            Self::AddToQueue {
                request,
                queue_id,
                autostart,
            } => return Self::add_queued(manager, queues, request, queue_id, autostart).await,
            Self::MoveToQueue {
                queue_id,
                downloads,
            } => return queues.move_downloads(queue_id, downloads).await,
            Self::Queue(_) => unreachable!(),
        };
        result.map_err(Into::into)
    }

    /// Adds a download and hands execution to queue admission.
    ///
    /// # Errors
    ///
    /// Returns the manager error when the add fails, or `Stopped` when the
    /// queue actor is gone.
    async fn add_queued(
        manager: &DownloadManagerHandle,
        queues: &QueueManagerHandle,
        request: NewDownload,
        queue_id: QueueId,
        start: bool,
    ) -> raijin_core::queue::QueueManagerResult<()> {
        if !queues
            .list()
            .await?
            .iter()
            .any(|snapshot| snapshot.queue.id == queue_id)
        {
            return Err(raijin_core::queue::QueueManagerError::QueueNotFound(
                queue_id,
            ));
        }
        let added = manager.add(request).await?;
        let admission = async {
            if !start {
                manager.pause(added.id).await?;
            }
            queues.enqueue(queue_id, [added.id]).await
        }
        .await;
        admission.map_err(
            |source| raijin_core::queue::QueueManagerError::AdmissionAfterAdd {
                id: added.id,
                source: Box::new(source),
            },
        )
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
