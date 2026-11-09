use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::db::{DbError, DownloadRepository, PartRepository};
use crate::download::job::{LiveDownloadProgress, unique_destination_path};
use crate::download::{
    Bytes, BytesPerSecond, DownloadFailure, DownloadId, DownloadItem, DownloadKind, DownloadPart,
    DownloadProgress, DownloadStatus, HttpDownloadJob, PartStatus, ReqwestHttpClient, SpeedLimiter,
};

const JOB_PROGRESS_TICK: Duration = Duration::from_millis(250);

/// Default bounded command queue size.
pub const DEFAULT_COMMAND_BUFFER: usize = 64;

/// Default event broadcast channel size.
pub const DEFAULT_EVENT_BUFFER: usize = 256;

/// Result type returned by download manager APIs.
pub type DownloadManagerResult<T> = Result<T, DownloadManagerError>;

/// Errors produced by the download manager actor or handle.
#[derive(Debug, Error)]
pub enum DownloadManagerError {
    /// Database operation failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Command channel is closed because the manager has stopped.
    #[error("download manager is stopped")]
    Stopped,
    /// Command response channel closed before a reply was sent.
    #[error("download manager response was dropped")]
    ResponseDropped,
    /// Requested download does not exist.
    #[error("download `{0}` not found")]
    NotFound(DownloadId),
    /// System clock is earlier than Unix epoch.
    #[error("system clock is earlier than Unix epoch")]
    ClockBeforeEpoch,
    /// System clock value does not fit in database timestamp range.
    #[error("system clock timestamp is out of range")]
    ClockOutOfRange,
    /// A canceled worker has not stopped safely enough to mutate its artifacts.
    #[error("download `{0}` is still stopping; try again shortly")]
    StillStopping(DownloadId),
}

/// New download request accepted by the manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewDownload {
    /// Download implementation selected for this source.
    #[serde(default = "http_download_kind")]
    pub kind: DownloadKind,
    /// Primary source URL.
    pub url: String,
    /// Optional page URL where the download was discovered.
    pub download_page: Option<String>,
    /// Request headers captured for authenticated or browser-originated downloads.
    pub headers: BTreeMap<String, String>,
    /// Final file name without parent folder.
    pub file_name: String,
    /// Destination folder.
    pub folder: PathBuf,
    /// User-selected category; absent means General.
    #[serde(default)]
    pub category: Option<String>,
    /// Preferred worker count for future ranged downloads.
    pub preferred_connections: Option<std::num::NonZeroU16>,
    /// Optional per-download throughput limit.
    pub speed_limit: Option<BytesPerSecond>,
}

impl NewDownload {
    /// Creates a minimal HTTP download request.
    #[must_use]
    pub fn http(url: impl Into<String>, file_name: impl Into<String>, folder: PathBuf) -> Self {
        Self {
            kind: DownloadKind::Http,
            url: url.into(),
            download_page: None,
            headers: BTreeMap::new(),
            file_name: file_name.into(),
            folder,
            category: None,
            preferred_connections: None,
            speed_limit: None,
        }
    }

    /// Creates an HLS VOD request with an MPEG-TS output name.
    #[must_use]
    pub fn hls(url: impl Into<String>, file_name: impl Into<String>, folder: PathBuf) -> Self {
        let mut request = Self::http(url, hls_file_name(&file_name.into()), folder);
        request.kind = DownloadKind::Hls;
        request
    }
}

fn http_download_kind() -> DownloadKind {
    DownloadKind::Http
}

/// Returns whether an HTTP source has an HLS playlist path extension.
#[must_use]
pub fn is_hls_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.path().to_ascii_lowercase().ends_with(".m3u8")
    })
}

/// Normalizes an untrusted media file name to an MPEG-TS basename.
#[must_use]
pub fn hls_file_name(value: &str) -> String {
    let name = crate::integration::sanitize_file_name(value);
    let stem = Path::new(&name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty() && !stem.starts_with('.'))
        .unwrap_or("video");
    format!("{stem}.ts")
}

/// Commands owned by the download manager actor.
#[derive(Debug)]
pub enum DownloadCommand {
    /// Add a new download.
    AddDownload {
        request: NewDownload,
        reply: oneshot::Sender<DownloadManagerResult<DownloadItem>>,
    },
    /// List persisted downloads.
    List {
        reply: oneshot::Sender<DownloadManagerResult<Vec<DownloadItem>>>,
    },
    /// List downloads that currently have a live worker.
    ///
    /// Queue admission uses this instead of persisted status: a status can be
    /// stale after a crash, while a live job cannot.
    ListActive {
        reply: oneshot::Sender<DownloadManagerResult<Vec<DownloadId>>>,
    },
    /// Pause a download skeleton.
    Pause {
        id: DownloadId,
        reply: oneshot::Sender<DownloadManagerResult<DownloadItem>>,
    },
    /// Mark a download ready for execution without starting it.
    ///
    /// Queue admission is the only caller of [`DownloadCommand::Start`], so
    /// this never spawns a worker on its own.
    Resume {
        id: DownloadId,
        reply: oneshot::Sender<DownloadManagerResult<DownloadItem>>,
    },
    /// Start the worker for a download that is already `Queued`.
    ///
    /// Issued by queue admission. Starting a download that is not `Queued`
    /// still spawns the job: the queue owns the decision, the engine owns the
    /// execution.
    Start {
        id: DownloadId,
        reply: oneshot::Sender<DownloadManagerResult<DownloadItem>>,
    },
    /// Remove a download.
    Remove {
        id: DownloadId,
        reply: oneshot::Sender<DownloadManagerResult<bool>>,
    },
    /// Reset a download after a validation error: clears persisted parts and
    /// partial bytes so the next run re-probes the remote from scratch.
    Reset {
        id: DownloadId,
        reply: oneshot::Sender<DownloadManagerResult<DownloadItem>>,
    },
    /// Pause every active download (tray "Pause all").
    PauseAll {
        reply: oneshot::Sender<DownloadManagerResult<Vec<DownloadItem>>>,
    },
    /// Resume every paused download (tray "Resume all").
    ResumeAll {
        reply: oneshot::Sender<DownloadManagerResult<Vec<DownloadItem>>>,
    },
    /// Replace the shared global throughput limit (`None` disables shaping).
    SetGlobalLimit {
        limit: Option<BytesPerSecond>,
        reply: oneshot::Sender<DownloadManagerResult<()>>,
    },
    /// Stop the manager actor.
    Shutdown {
        reply: oneshot::Sender<DownloadManagerResult<()>>,
    },
    #[doc(hidden)]
    JobFinished { id: DownloadId, identity: Arc<()> },
}

/// Events published by the download manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadEvent {
    /// Manager boot loaded persisted downloads (all except removed) for UI seeding.
    BootLoaded { downloads: Vec<DownloadItem> },
    /// Download metadata was added.
    DownloadAdded { item: DownloadItem },
    /// Download metadata changed.
    DownloadChanged { item: DownloadItem },
    /// Download skeleton moved to paused state.
    DownloadPaused { item: DownloadItem },
    /// Download skeleton moved to queued state for future worker startup.
    DownloadResumed { item: DownloadItem },
    /// Download was removed.
    DownloadRemoved { id: DownloadId },
    /// Download failed before worker execution.
    DownloadFailed {
        id: DownloadId,
        failure: DownloadFailure,
    },
    /// Placeholder progress event for future jobs.
    DownloadProgress {
        id: DownloadId,
        progress: DownloadProgress,
    },
    /// Manager actor stopped.
    Shutdown,
}

/// Runtime options for the manager actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadManagerOptions {
    /// Bounded command channel capacity.
    pub command_buffer: usize,
    /// Broadcast event channel capacity.
    pub event_buffer: usize,
}

impl Default for DownloadManagerOptions {
    fn default() -> Self {
        Self {
            command_buffer: DEFAULT_COMMAND_BUFFER,
            event_buffer: DEFAULT_EVENT_BUFFER,
        }
    }
}

/// Public cloneable handle used to send commands to the manager actor.
#[derive(Debug, Clone)]
pub struct DownloadManagerHandle {
    commands: mpsc::Sender<DownloadCommand>,
    events: broadcast::Sender<DownloadEvent>,
}

impl DownloadManagerHandle {
    /// Spawns a download manager actor and returns its handle plus task handle.
    #[must_use]
    pub fn spawn(
        repository: DownloadRepository,
        part_repository: PartRepository,
        options: DownloadManagerOptions,
    ) -> (Self, JoinHandle<DownloadManagerResult<()>>) {
        let (handle, task, _) = Self::spawn_with_events(repository, part_repository, options);
        (handle, task)
    }

    /// Spawns a manager and returns an event receiver subscribed before boot.
    #[must_use]
    pub fn spawn_with_events(
        repository: DownloadRepository,
        part_repository: PartRepository,
        options: DownloadManagerOptions,
    ) -> (
        Self,
        JoinHandle<DownloadManagerResult<()>>,
        broadcast::Receiver<DownloadEvent>,
    ) {
        let (command_tx, command_rx) = mpsc::channel(options.command_buffer);
        let (event_tx, event_rx) = broadcast::channel(options.event_buffer);
        let handle = Self {
            commands: command_tx,
            events: event_tx.clone(),
        };
        let manager = DownloadManager::new(
            repository,
            part_repository,
            command_rx,
            event_tx,
            handle.commands.clone(),
        );
        let task = tokio::spawn(manager.run());

        (handle, task, event_rx)
    }

    /// Subscribes to manager events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<DownloadEvent> {
        self.events.subscribe()
    }

    /// Adds a new download.
    ///
    /// # Errors
    ///
    /// Returns an error when the manager stops or persistence fails.
    pub async fn add(&self, request: NewDownload) -> DownloadManagerResult<DownloadItem> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::AddDownload { request, reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }

    /// Lists downloads.
    ///
    /// # Errors
    ///
    /// Returns an error when the manager stops or persistence fails.
    pub async fn list(&self) -> DownloadManagerResult<Vec<DownloadItem>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::List { reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }

    /// Lists downloads that currently have a live worker.
    ///
    /// # Errors
    ///
    /// Returns an error when the manager stops.
    pub async fn list_active(&self) -> DownloadManagerResult<Vec<DownloadId>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::ListActive { reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }

    /// Pauses a download skeleton.
    ///
    /// # Errors
    ///
    /// Returns an error when the download does not exist or persistence fails.
    pub async fn pause(&self, id: DownloadId) -> DownloadManagerResult<DownloadItem> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::Pause { id, reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }

    /// Marks a download `Queued` without starting a worker.
    ///
    /// Queue admission turns a `Queued` download into a running job through
    /// [`DownloadManagerHandle::start`], so a resume request alone must never
    /// bypass concurrency limits.
    ///
    /// # Errors
    ///
    /// Returns an error when the download does not exist or persistence fails.
    pub async fn resume(&self, id: DownloadId) -> DownloadManagerResult<DownloadItem> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::Resume { id, reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }

    /// Starts the worker for a `Queued` download.
    ///
    /// # Errors
    ///
    /// Returns an error when the download does not exist or persistence fails.
    pub async fn start(&self, id: DownloadId) -> DownloadManagerResult<DownloadItem> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::Start { id, reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }

    /// Removes a download.
    ///
    /// # Errors
    ///
    /// Returns an error when the manager stops or persistence fails.
    pub async fn remove(&self, id: DownloadId) -> DownloadManagerResult<bool> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::Remove { id, reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }

    /// Resets a download after a validation error.
    ///
    /// Clears persisted parts, partial bytes, and stale validators, then
    /// re-queues the download so the next run re-probes from scratch.
    ///
    /// # Errors
    ///
    /// Returns an error when the download does not exist or persistence fails.
    pub async fn reset(&self, id: DownloadId) -> DownloadManagerResult<DownloadItem> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::Reset { id, reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }

    /// Pauses every active download.
    ///
    /// Used by the tray "Pause all" action as a single actor command instead
    /// of N round-trips from the UI. Downloads already paused, completed, or
    /// failed are left untouched.
    ///
    /// # Errors
    ///
    /// Returns an error when the manager stops or persistence fails.
    pub async fn pause_all(&self) -> DownloadManagerResult<Vec<DownloadItem>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::PauseAll { reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }

    /// Resumes every paused download.
    ///
    /// Failed downloads keep their error until an explicit reset; completed
    /// and active downloads are left untouched.
    ///
    /// # Errors
    ///
    /// Returns an error when the manager stops or persistence fails.
    pub async fn resume_all(&self) -> DownloadManagerResult<Vec<DownloadItem>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::ResumeAll { reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }

    /// Replaces the shared global throughput limit layered over per-download limits.
    ///
    /// `None` disables global shaping and restores the zero-cost path.
    ///
    /// # Errors
    ///
    /// Returns an error when the manager has already stopped.
    pub async fn set_global_speed_limit(
        &self,
        limit: Option<BytesPerSecond>,
    ) -> DownloadManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::SetGlobalLimit { limit, reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }

    /// Requests graceful actor shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error when the manager has already stopped.
    pub async fn shutdown(&self) -> DownloadManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(DownloadCommand::Shutdown { reply })
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| DownloadManagerError::ResponseDropped)?
    }
}

#[derive(Debug)]
struct ActiveJob {
    kind: DownloadKind,
    identity: Arc<()>,
    completion_cancellation: CancellationToken,
    cancellation: CancellationToken,
    live_progress: Arc<LiveDownloadProgress>,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct DownloadManager {
    repository: DownloadRepository,
    part_repository: PartRepository,
    commands: mpsc::Receiver<DownloadCommand>,
    events: broadcast::Sender<DownloadEvent>,
    command_sender: mpsc::Sender<DownloadCommand>,
    active_jobs: HashMap<DownloadId, ActiveJob>,
    /// Shared global limiter cloned into spawned jobs; `None` disables shaping.
    global_limiter: Option<Arc<SpeedLimiter>>,
}

impl DownloadManager {
    fn new(
        repository: DownloadRepository,
        part_repository: PartRepository,
        commands: mpsc::Receiver<DownloadCommand>,
        events: broadcast::Sender<DownloadEvent>,
        command_sender: mpsc::Sender<DownloadCommand>,
    ) -> Self {
        Self {
            repository,
            part_repository,
            commands,
            events,
            command_sender,
            active_jobs: HashMap::new(),
            global_limiter: None,
        }
    }

    async fn run(mut self) -> DownloadManagerResult<()> {
        self.boot().await?;

        while let Some(command) = self.commands.recv().await {
            self.active_jobs.retain(|_, job| !job.task.is_finished());
            if self.handle_command(command).await? {
                break;
            }
        }

        let active_ids: Vec<_> = self.active_jobs.keys().copied().collect();
        for id in active_ids {
            self.stop_active_job(id).await?;
        }
        self.publish(DownloadEvent::Shutdown);
        Ok(())
    }

    async fn boot(&mut self) -> DownloadManagerResult<()> {
        let stored = self.repository.list().await?;
        let mut visible: Vec<DownloadItem> = stored
            .into_iter()
            .filter(|item| item.status != DownloadStatus::Removed)
            .collect();
        // A row persisted as running cannot be running after a restart: no job
        // survived, so normalize it back to `Queued` and let queue admission
        // pick it up. Starting here would ignore concurrency limits.
        for item in &mut visible {
            if should_requeue_on_boot(item.status) {
                item.status = DownloadStatus::Queued;
                self.repository.update(item).await?;
            }
        }
        self.publish(DownloadEvent::BootLoaded { downloads: visible });
        Ok(())
    }

    async fn handle_command(&mut self, command: DownloadCommand) -> DownloadManagerResult<bool> {
        match command {
            DownloadCommand::AddDownload { request, reply } => {
                let result = self.add_download(request).await;
                send_reply(reply, result);
                Ok(false)
            }
            DownloadCommand::List { reply } => {
                send_reply(reply, self.repository.list().await.map_err(Into::into));
                Ok(false)
            }
            DownloadCommand::ListActive { reply } => {
                let mut ids: Vec<DownloadId> = self.active_jobs.keys().copied().collect();
                ids.sort_unstable();
                send_reply(reply, Ok(ids));
                Ok(false)
            }
            DownloadCommand::Pause { id, reply } => {
                let result = self.pause_download(id).await;
                send_reply(reply, result);
                Ok(false)
            }
            DownloadCommand::Resume { id, reply } => {
                let result = self.resume_download(id).await;
                send_reply(reply, result);
                Ok(false)
            }
            DownloadCommand::Start { id, reply } => {
                let result = self.start_download(id).await;
                send_reply(reply, result);
                Ok(false)
            }
            DownloadCommand::Remove { id, reply } => {
                let result = self.remove_download(id).await;
                send_reply(reply, result);
                Ok(false)
            }
            DownloadCommand::Reset { id, reply } => {
                let result = self.reset_download(id).await;
                send_reply(reply, result);
                Ok(false)
            }
            DownloadCommand::PauseAll { reply } => {
                let result = self.pause_all_downloads().await;
                send_reply(reply, result);
                Ok(false)
            }
            DownloadCommand::ResumeAll { reply } => {
                let result = self.resume_all_downloads().await;
                send_reply(reply, result);
                Ok(false)
            }
            DownloadCommand::SetGlobalLimit { limit, reply } => {
                self.global_limiter = limit.and_then(SpeedLimiter::new);
                send_reply(reply, Ok(()));
                Ok(false)
            }
            DownloadCommand::Shutdown { reply } => {
                send_reply(reply, Ok(()));
                Ok(true)
            }
            DownloadCommand::JobFinished { id, identity } => {
                if self
                    .active_jobs
                    .get(&id)
                    .is_some_and(|job| Arc::ptr_eq(&job.identity, &identity))
                {
                    self.active_jobs.remove(&id);
                }
                Ok(false)
            }
        }
    }

    async fn add_download(&mut self, request: NewDownload) -> DownloadManagerResult<DownloadItem> {
        let now_ms = now_ms()?;
        let folder = request.folder;
        let requested_name = if request.kind == DownloadKind::Hls {
            hls_file_name(&request.file_name)
        } else {
            request.file_name.clone()
        };
        let file_name = unique_file_name(&folder, &requested_name).await?;
        let item = DownloadItem {
            id: self.repository.next_id().await?,
            kind: request.kind,
            url: request.url,
            download_page: request.download_page,
            headers: request.headers,
            file_name,
            folder,
            category: request.category,
            status: DownloadStatus::Added,
            total_bytes: None,
            downloaded_bytes: Bytes::ZERO,
            etag: None,
            last_modified: None,
            preferred_connections: request.preferred_connections,
            speed_limit: request.speed_limit,
            failure: None,
            created_at: now_ms,
            started_at: None,
            completed_at: None,
            updated_at: now_ms,
        };

        self.repository.add(&item).await?;
        self.publish(DownloadEvent::DownloadAdded { item: item.clone() });
        Ok(item)
    }

    async fn pause_download(&mut self, id: DownloadId) -> DownloadManagerResult<DownloadItem> {
        let mut item = self.item_for_update(id).await?;
        if item.kind == DownloadKind::Hls {
            self.stop_active_job(id).await?;
            item = self.item_for_update(id).await?;
            if item.status == DownloadStatus::Completed {
                return Ok(item);
            }
        }
        item.status = DownloadStatus::Paused;
        item.updated_at = now_ms()?;

        if let Some(job) = self.active_jobs.get_mut(&id) {
            let (downloaded_bytes, _) = job.live_progress.snapshot();
            item.downloaded_bytes = item.downloaded_bytes.max(downloaded_bytes);
            job.live_progress.clear_active_parts();
            job.cancellation.cancel();
        }
        self.repository.update(&item).await?;

        self.publish(DownloadEvent::DownloadPaused { item: item.clone() });
        Ok(item)
    }

    async fn resume_download(&mut self, id: DownloadId) -> DownloadManagerResult<DownloadItem> {
        let mut item = self.item_for_update(id).await?;
        item.status = DownloadStatus::Queued;
        item.updated_at = now_ms()?;
        item.failure = None;
        self.repository.update(&item).await?;

        self.publish(DownloadEvent::DownloadResumed { item: item.clone() });
        Ok(item)
    }

    async fn start_download(&mut self, id: DownloadId) -> DownloadManagerResult<DownloadItem> {
        self.stop_active_job(id).await?;
        let mut item = self.item_for_update(id).await?;
        if item.status != DownloadStatus::Queued {
            item.status = DownloadStatus::Queued;
            item.updated_at = now_ms()?;
            self.repository.update(&item).await?;
            self.publish(DownloadEvent::DownloadResumed { item: item.clone() });
        }
        self.start_job(item.clone());
        Ok(item)
    }

    async fn remove_download(&mut self, id: DownloadId) -> DownloadManagerResult<bool> {
        let Some(item) = self.repository.get(id).await? else {
            return Ok(false);
        };
        self.stop_active_job(id).await?;
        let item = self.repository.get(id).await?.unwrap_or(item);
        delete_download_files(&item).await?;

        let removed = self.repository.remove(id).await?;
        if removed {
            self.publish(DownloadEvent::DownloadRemoved { id });
        }
        Ok(removed)
    }

    async fn reset_download(&mut self, id: DownloadId) -> DownloadManagerResult<DownloadItem> {
        self.stop_active_job(id).await?;
        let mut item = self.item_for_update(id).await?;
        self.part_repository.remove_for_download(id).await?;
        if item.kind == DownloadKind::Hls {
            self.repository.hls_repository().clear(id).await?;
            remove_segment_directory(&item).await?;
        }
        remove_file_if_exists(&incomplete_path_for_item(&item)).await?;

        // Drop stale validators so the next run skips resume validation
        // (`probe` only validates when partial bytes exist) and re-probes fresh.
        // The row is only re-queued here; admission decides when it runs.
        item.status = DownloadStatus::Queued;
        item.total_bytes = None;
        item.downloaded_bytes = Bytes::ZERO;
        item.etag = None;
        item.last_modified = None;
        item.failure = None;
        item.started_at = None;
        item.completed_at = None;
        item.updated_at = now_ms()?;
        self.repository.update(&item).await?;

        self.publish(DownloadEvent::DownloadChanged { item: item.clone() });
        Ok(item)
    }

    async fn pause_all_downloads(&mut self) -> DownloadManagerResult<Vec<DownloadItem>> {
        let now = now_ms()?;
        let mut paused = Vec::new();
        for mut item in self.repository.list().await? {
            if !is_active_status(item.status) {
                continue;
            }
            item.status = DownloadStatus::Paused;
            item.updated_at = now;
            if let Some(job) = self.active_jobs.get(&item.id) {
                let (downloaded_bytes, _) = job.live_progress.snapshot();
                item.downloaded_bytes = item.downloaded_bytes.max(downloaded_bytes);
                job.live_progress.clear_active_parts();
                job.cancellation.cancel();
            }
            self.repository.update(&item).await?;
            self.publish(DownloadEvent::DownloadPaused { item: item.clone() });
            paused.push(item);
        }
        Ok(paused)
    }

    async fn resume_all_downloads(&mut self) -> DownloadManagerResult<Vec<DownloadItem>> {
        let now = now_ms()?;
        let mut resumed = Vec::new();
        for mut item in self.repository.list().await? {
            if item.status != DownloadStatus::Paused {
                continue;
            }
            item.status = DownloadStatus::Queued;
            item.updated_at = now;
            item.failure = None;
            self.repository.update(&item).await?;
            self.publish(DownloadEvent::DownloadResumed { item: item.clone() });
            resumed.push(item);
        }
        // No start: queue admission owns when a `Queued` row runs.
        Ok(resumed)
    }

    async fn stop_active_job(&mut self, id: DownloadId) -> DownloadManagerResult<()> {
        if let Some(mut job) = self.active_jobs.remove(&id) {
            job.completion_cancellation.cancel();
            job.cancellation.cancel();
            if job.kind != DownloadKind::Hls {
                job.task.abort();
                let _ = job.task.await;
                return Ok(());
            }
            match tokio::time::timeout(Duration::from_secs(15), &mut job.task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.is_cancelled() => {}
                Ok(Err(error)) => {
                    tracing::warn!(%id, ?error, "download job failed during cancellation")
                }
                Err(_) => {
                    self.active_jobs.insert(id, job);
                    return Err(DownloadManagerError::StillStopping(id));
                }
            }
        }
        Ok(())
    }

    async fn item_for_update(&self, id: DownloadId) -> DownloadManagerResult<DownloadItem> {
        self.repository
            .get(id)
            .await?
            .ok_or(DownloadManagerError::NotFound(id))
    }

    fn start_job(&mut self, item: DownloadItem) {
        let identity = Arc::new(());
        let completion_cancellation = CancellationToken::new();
        let cancellation = CancellationToken::new();
        let live_progress = Arc::new(LiveDownloadProgress::new(item.downloaded_bytes));
        let task = tokio::spawn(run_job_task(JobTaskContext {
            identity: Arc::clone(&identity),
            completion_cancellation: completion_cancellation.clone(),
            download_repo: self.repository.clone(),
            part_repo: self.part_repository.clone(),
            events: self.events.clone(),
            commands: self.command_sender.clone(),
            item: item.clone(),
            live_progress: Arc::clone(&live_progress),
            cancellation: cancellation.clone(),
            global_limiter: self.global_limiter.clone(),
        }));
        self.active_jobs.insert(
            item.id,
            ActiveJob {
                kind: item.kind,
                identity,
                completion_cancellation,
                cancellation,
                live_progress,
                task,
            },
        );
    }

    fn publish(&self, event: DownloadEvent) {
        match self.events.send(event) {
            Ok(_) => {}
            Err(error) => tracing::trace!(?error, "download event had no subscribers"),
        }
    }
}

fn send_reply<T>(
    reply: oneshot::Sender<DownloadManagerResult<T>>,
    result: DownloadManagerResult<T>,
) {
    if reply.send(result).is_err() {
        tracing::trace!("download command reply receiver dropped");
    }
}

struct JobTaskContext {
    identity: Arc<()>,
    completion_cancellation: CancellationToken,
    download_repo: DownloadRepository,
    part_repo: PartRepository,
    events: broadcast::Sender<DownloadEvent>,
    commands: mpsc::Sender<DownloadCommand>,
    item: DownloadItem,
    live_progress: Arc<LiveDownloadProgress>,
    cancellation: CancellationToken,
    global_limiter: Option<Arc<SpeedLimiter>>,
}

async fn run_job_task(context: JobTaskContext) {
    let id = context.item.id;
    let result = run_job_task_inner(&context).await;
    match result {
        Ok(item) => publish_terminal_item(&context.events, item),
        Err(failure) => {
            match context.download_repo.get(id).await {
                Ok(Some(item)) => publish_terminal_item(&context.events, item),
                Ok(None) => {}
                Err(db_error) => tracing::warn!(
                    ?db_error,
                    download_id = id.get(),
                    "failed to load failed download state"
                ),
            }
            publish_event(
                &context.events,
                DownloadEvent::DownloadFailed { id, failure },
            );
        }
    }
    tokio::select! {
        biased;
        _ = context.completion_cancellation.cancelled() => {}
        result = context.commands.send(DownloadCommand::JobFinished {
            id,
            identity: Arc::clone(&context.identity),
        }) => {
            if result.is_err() {
                tracing::trace!(download_id = id.get(), "download manager stopped before job cleanup");
            }
        }
    }
}

async fn run_job_task_inner(context: &JobTaskContext) -> Result<DownloadItem, DownloadFailure> {
    let job_future = async {
        match context.item.kind {
            DownloadKind::Http => HttpDownloadJob::new(
                context.download_repo.clone(),
                context.part_repo.clone(),
                ReqwestHttpClient::new().map_err(|error| {
                    crate::download::HttpDownloadJobError::from(error).to_failure()
                })?,
                context.item.clone(),
            )
            .with_live_progress(Arc::clone(&context.live_progress))
            .with_global_limiter(context.global_limiter.clone())
            .run(context.cancellation.clone())
            .await
            .map_err(|error| error.to_failure()),
            DownloadKind::Hls => crate::download::hls::HlsDownloadJob::new(
                context.download_repo.clone(),
                context.item.clone(),
            )
            .with_live_progress(Arc::clone(&context.live_progress))
            .with_global_limiter(context.global_limiter.clone())
            .run(context.cancellation.clone())
            .await
            .map_err(|error| error.to_failure()),
            DownloadKind::Aria2 => {
                let failure = DownloadFailure {
                    kind: crate::download::FailureKind::Validation,
                    message: "Aria2 downloads are not supported".to_owned(),
                };
                let mut item = context.item.clone();
                item.status = DownloadStatus::Error;
                item.failure = Some(failure.clone());
                context
                    .download_repo
                    .update(&item)
                    .await
                    .map_err(|error| DownloadFailure {
                        kind: crate::download::FailureKind::Disk,
                        message: error.to_string(),
                    })?;
                Err(failure)
            }
        }
    };
    tokio::pin!(job_future);
    let mut ticker = tokio::time::interval(JOB_PROGRESS_TICK);
    let mut last_progress = None;
    let mut last_status = None;
    loop {
        tokio::select! {
            result = &mut job_future => return result,
            _ = ticker.tick() => publish_progress_snapshot(context, &mut last_progress, &mut last_status).await,
        }
    }
}

async fn publish_progress_snapshot(
    context: &JobTaskContext,
    last_progress: &mut Option<DownloadProgress>,
    last_status: &mut Option<DownloadStatus>,
) {
    match context.download_repo.get(context.item.id).await {
        Ok(Some(item)) => {
            if context.item.kind == DownloadKind::Hls && *last_status != Some(item.status) {
                *last_status = Some(item.status);
                publish_event(
                    &context.events,
                    DownloadEvent::DownloadChanged { item: item.clone() },
                );
            }
            let progress = progress_from_item(context, &item).await;
            if progress.active_part_count > 0 || last_progress.as_ref() != Some(&progress) {
                *last_progress = Some(progress);
                publish_event(
                    &context.events,
                    DownloadEvent::DownloadProgress {
                        id: item.id,
                        progress,
                    },
                );
            }
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(
            ?error,
            download_id = context.item.id.get(),
            "failed to publish download progress snapshot"
        ),
    }
}

async fn progress_from_item(context: &JobTaskContext, item: &DownloadItem) -> DownloadProgress {
    let (live_downloaded_bytes, live_active_part_count) = context.live_progress.snapshot();
    if context.item.kind == DownloadKind::Hls {
        return DownloadProgress {
            downloaded_bytes: live_downloaded_bytes,
            total_bytes: item.total_bytes,
            speed: BytesPerSecond::ZERO,
            eta_seconds: None,
            active_part_count: live_active_part_count,
        };
    }
    if live_active_part_count > 0 {
        return DownloadProgress {
            downloaded_bytes: live_downloaded_bytes,
            total_bytes: item.total_bytes,
            speed: BytesPerSecond::ZERO,
            eta_seconds: None,
            active_part_count: live_active_part_count,
        };
    }

    let parts = match context.part_repo.list_for_download(item.id).await {
        Ok(parts) => parts,
        Err(error) => {
            tracing::warn!(
                ?error,
                download_id = item.id.get(),
                "failed to load download parts for progress snapshot"
            );
            Vec::new()
        }
    };

    let persisted_downloaded_bytes = if parts.is_empty() {
        item.downloaded_bytes
    } else {
        Bytes::new(downloaded_from_parts(&parts))
    };
    let downloaded_bytes = live_downloaded_bytes.max(persisted_downloaded_bytes);
    let active_part_count = live_active_part_count.max(active_part_count(&parts, item.status));

    DownloadProgress {
        downloaded_bytes,
        total_bytes: item.total_bytes,
        speed: BytesPerSecond::ZERO,
        eta_seconds: None,
        active_part_count,
    }
}

fn downloaded_from_parts(parts: &[DownloadPart]) -> u64 {
    parts
        .iter()
        .map(|part| {
            part.current_byte
                .get()
                .saturating_sub(part.start_byte.get())
        })
        .sum()
}

fn active_part_count(parts: &[DownloadPart], status: DownloadStatus) -> u16 {
    let count = parts
        .iter()
        .filter(|part| matches!(part.status, PartStatus::Connecting | PartStatus::Receiving))
        .count();
    u16::try_from(count).unwrap_or(u16::MAX).max(u16::from(
        parts.is_empty() && status == DownloadStatus::Downloading,
    ))
}

fn publish_terminal_item(events: &broadcast::Sender<DownloadEvent>, item: DownloadItem) {
    match item.status {
        DownloadStatus::Completed | DownloadStatus::Error => {
            publish_event(events, DownloadEvent::DownloadChanged { item });
        }
        DownloadStatus::Paused => publish_event(events, DownloadEvent::DownloadPaused { item }),
        _ => publish_event(events, DownloadEvent::DownloadChanged { item }),
    }
}

fn publish_event(events: &broadcast::Sender<DownloadEvent>, event: DownloadEvent) {
    match events.send(event) {
        Ok(_) => {}
        Err(error) => tracing::trace!(?error, "download event had no subscribers"),
    }
}

async fn remove_segment_directory(item: &DownloadItem) -> std::io::Result<()> {
    match fs::remove_dir_all(crate::download::hls::segment_directory(item)).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn delete_download_files(item: &DownloadItem) -> std::io::Result<()> {
    if item.kind == DownloadKind::Hls {
        remove_segment_directory(item).await?;
    }
    remove_file_if_exists(&incomplete_path_for_item(item)).await?;
    remove_file_if_exists(&final_path_for_item(item)).await
}

async fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn final_path_for_item(item: &DownloadItem) -> PathBuf {
    item.folder.join(&item.file_name)
}

fn incomplete_path_for_item(item: &DownloadItem) -> PathBuf {
    item.folder.join(format!(
        "{}{}",
        item.file_name,
        crate::download::DownloadConfig::default().incomplete_extension
    ))
}

async fn unique_file_name(folder: &Path, file_name: &str) -> DownloadManagerResult<String> {
    let requested = folder.join(file_name);
    let unique = unique_destination_path(
        &requested,
        &crate::download::DownloadConfig::default().incomplete_extension,
    )
    .await
    .map_err(|error| {
        DownloadManagerError::Db(DbError::Sqlx(sqlx::Error::Io(error_to_io(error))))
    })?;
    unique
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .ok_or(DownloadManagerError::ClockOutOfRange)
}

fn error_to_io(error: crate::download::HttpDownloadJobError) -> std::io::Error {
    match error {
        crate::download::HttpDownloadJobError::Io(error) => error,
        other => std::io::Error::other(other.to_string()),
    }
}

/// True for statuses that "Pause all" must suspend.
fn is_active_status(status: DownloadStatus) -> bool {
    matches!(
        status,
        DownloadStatus::Added
            | DownloadStatus::Queued
            | DownloadStatus::Downloading
            | DownloadStatus::Retrying
            | DownloadStatus::PreparingFile
    )
}

/// True for statuses a killed process could not have left behind.
fn should_requeue_on_boot(status: DownloadStatus) -> bool {
    matches!(
        status,
        DownloadStatus::Downloading | DownloadStatus::Retrying | DownloadStatus::PreparingFile
    )
}

fn now_ms() -> DownloadManagerResult<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| DownloadManagerError::ClockBeforeEpoch)?;
    i64::try_from(duration.as_millis()).map_err(|_| DownloadManagerError::ClockOutOfRange)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::db;
    use crate::download::PartId;

    #[test]
    fn hls_names_should_use_safe_ts_basenames() {
        for (input, expected) in [
            ("playlist.m3u8", "playlist.ts"),
            ("Movie.mp4", "Movie.ts"),
            ("video.ts", "video.ts"),
            ("../../movie.m3u8", "movie.ts"),
            ("", "video.ts"),
        ] {
            assert_eq!(hls_file_name(input), expected);
        }
    }

    #[test]
    fn hls_detection_should_use_url_path_not_query() {
        assert!(is_hls_url("https://example.com/VIDEO.M3U8?token=secret"));
        assert!(!is_hls_url("https://example.com/file.bin?name=video.m3u8"));
        assert!(!is_hls_url("file:///video.m3u8"));
    }

    #[test]
    fn legacy_requests_should_default_to_http() {
        let request = NewDownload::http(
            "https://example.com/file",
            "file.bin",
            PathBuf::from("downloads"),
        );
        let mut value = serde_json::to_value(&request).expect("request serialization");
        value
            .as_object_mut()
            .expect("request object")
            .remove("kind");
        let decoded: NewDownload = serde_json::from_value(value).expect("legacy request");
        assert_eq!(decoded.kind, DownloadKind::Http);
    }

    struct TestDb {
        _dir: TempDir,
        repository: DownloadRepository,
        part_repository: PartRepository,
    }

    async fn test_db() -> DownloadManagerResult<TestDb> {
        let dir = tempfile::tempdir()
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;
        let db_path = dir.path().join("raijin-manager-test.sqlite");
        let database_url = format!("sqlite://{}", db_path.display());
        let pool = db::bootstrap(&database_url).await?;
        Ok(TestDb {
            _dir: dir,
            repository: DownloadRepository::new(pool.clone()),
            part_repository: PartRepository::new(pool),
        })
    }

    async fn stop_manager(
        handle: &DownloadManagerHandle,
        task: JoinHandle<DownloadManagerResult<()>>,
    ) -> DownloadManagerResult<()> {
        handle.shutdown().await?;
        task.await.map_err(|_| DownloadManagerError::Stopped)??;
        Ok(())
    }

    #[tokio::test]
    async fn stale_completion_should_not_remove_replacement_job() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let (commands, receiver) = mpsc::channel(4);
        let (events, _) = broadcast::channel(4);
        let mut manager = DownloadManager::new(
            db.repository,
            db.part_repository,
            receiver,
            events,
            commands,
        );
        let id = DownloadId::new(1);
        let identity = Arc::new(());
        manager.active_jobs.insert(
            id,
            ActiveJob {
                kind: DownloadKind::Hls,
                identity: Arc::clone(&identity),
                completion_cancellation: CancellationToken::new(),
                cancellation: CancellationToken::new(),
                live_progress: Arc::new(LiveDownloadProgress::default()),
                task: tokio::spawn(async {}),
            },
        );
        manager
            .handle_command(DownloadCommand::JobFinished {
                id,
                identity: Arc::new(()),
            })
            .await?;
        assert!(manager.active_jobs.contains_key(&id));
        manager
            .handle_command(DownloadCommand::JobFinished { id, identity })
            .await?;
        assert!(manager.active_jobs.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn hls_reset_should_clear_plan_and_segment_files() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let (handle, task) = DownloadManagerHandle::spawn(
            db.repository.clone(),
            db.part_repository,
            DownloadManagerOptions::default(),
        );
        let added = handle
            .add(NewDownload::hls(
                "https://example.com/index.m3u8",
                "index.m3u8",
                folder.path().to_path_buf(),
            ))
            .await?;
        assert_eq!(added.kind, DownloadKind::Hls);
        assert_eq!(added.file_name, "index.ts");
        let plan = crate::db::HlsPlanRecord {
            playlist_url: added.url.clone(),
            segments: vec![crate::db::HlsSegmentRecord {
                index: 0,
                sequence: 10,
                url: "https://example.com/0.ts".to_owned(),
                duration: 5.0,
                length: None,
                sha256: None,
                etag: None,
                last_modified: None,
            }],
        };
        db.repository
            .hls_repository()
            .save_plan(added.id, &plan)
            .await?;
        let directory = crate::download::hls::segment_directory(&added);
        fs::create_dir_all(&directory).await?;
        fs::write(directory.join("0.tmp"), b"incomplete").await?;
        let reset = handle.reset(added.id).await?;
        assert_eq!(reset.status, DownloadStatus::Queued);
        assert!(
            db.repository
                .hls_repository()
                .load(added.id)
                .await?
                .is_none()
        );
        assert!(!directory.exists());
        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn actor_should_add_and_list_downloads_without_network() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let (handle, task) = DownloadManagerHandle::spawn(
            db.repository,
            db.part_repository,
            DownloadManagerOptions {
                command_buffer: 8,
                event_buffer: 8,
            },
        );
        let mut request = NewDownload::http(
            "https://example.com/file.bin",
            "file.bin",
            PathBuf::from("C:/Downloads"),
        );
        request.category = Some("Compressed".to_owned());

        let added = handle.add(request).await?;
        assert_eq!(added.category.as_deref(), Some("Compressed"));
        let downloads = handle.list().await?;

        assert_eq!(downloads, vec![added]);
        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn boot_should_seed_visible_downloads_including_completed() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let unfinished = sample_item(DownloadId::new(1), DownloadStatus::Paused);
        let completed = sample_item(DownloadId::new(2), DownloadStatus::Completed);
        let removed = sample_item(DownloadId::new(3), DownloadStatus::Removed);
        db.repository.add(&unfinished).await?;
        db.repository.add(&completed).await?;
        db.repository.add(&removed).await?;
        let (handle, task, mut events) = DownloadManagerHandle::spawn_with_events(
            db.repository,
            db.part_repository,
            DownloadManagerOptions {
                command_buffer: 8,
                event_buffer: 8,
            },
        );

        let event = events
            .recv()
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;

        assert_eq!(
            event,
            DownloadEvent::BootLoaded {
                downloads: vec![unfinished, completed]
            }
        );
        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn boot_should_requeue_rows_that_claimed_to_be_running() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let running = sample_item(DownloadId::new(1), DownloadStatus::Downloading);
        let retrying = sample_item(DownloadId::new(2), DownloadStatus::Retrying);
        let queued = sample_item(DownloadId::new(3), DownloadStatus::Queued);
        db.repository.add(&running).await?;
        db.repository.add(&retrying).await?;
        db.repository.add(&queued).await?;
        let (handle, task, mut events) = DownloadManagerHandle::spawn_with_events(
            db.repository.clone(),
            db.part_repository,
            DownloadManagerOptions {
                command_buffer: 8,
                event_buffer: 8,
            },
        );

        let event = events
            .recv()
            .await
            .map_err(|_| DownloadManagerError::Stopped)?;
        let DownloadEvent::BootLoaded { downloads } = event else {
            return Err(DownloadManagerError::Stopped);
        };

        assert!(
            downloads
                .iter()
                .all(|item| item.status == DownloadStatus::Queued),
            "boot must normalize crash-survivor statuses: {downloads:?}"
        );
        assert_eq!(
            db.repository.get(running.id).await?.map(|item| item.status),
            Some(DownloadStatus::Queued),
            "normalization must be persisted, not only published"
        );
        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn command_channel_should_apply_backpressure() -> DownloadManagerResult<()> {
        let (sender, mut receiver) = mpsc::channel::<DownloadCommand>(1);
        let (reply_a, _rx_a) = oneshot::channel();
        let (reply_b, _rx_b) = oneshot::channel();

        sender
            .try_send(DownloadCommand::List { reply: reply_a })
            .map_err(|_| DownloadManagerError::Stopped)?;
        let second_send = sender.try_send(DownloadCommand::List { reply: reply_b });

        assert!(matches!(
            second_send,
            Err(mpsc::error::TrySendError::Full(_))
        ));
        let _ = receiver.recv().await;
        Ok(())
    }

    #[tokio::test]
    async fn actor_should_pause_resume_and_remove_download() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let (handle, task) = DownloadManagerHandle::spawn(
            db.repository,
            db.part_repository,
            DownloadManagerOptions {
                command_buffer: 8,
                event_buffer: 8,
            },
        );
        let added = handle
            .add(NewDownload::http(
                "https://example.com/file.bin",
                "file.bin",
                PathBuf::from("C:/Downloads"),
            ))
            .await?;

        let paused = handle.pause(added.id).await?;
        let resumed = handle.resume(added.id).await?;
        let started = handle.start(added.id).await?;
        let removed = handle.remove(added.id).await?;

        assert_eq!(paused.status, DownloadStatus::Paused);
        assert_eq!(resumed.status, DownloadStatus::Queued);
        assert_eq!(started.status, DownloadStatus::Queued);
        assert!(removed);
        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn resume_should_not_spawn_a_job_until_start_is_requested() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let folder = tempfile::tempdir()
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;
        let (handle, task) = DownloadManagerHandle::spawn(
            db.repository.clone(),
            db.part_repository,
            DownloadManagerOptions {
                command_buffer: 8,
                event_buffer: 16,
            },
        );
        let added = handle
            .add(NewDownload::http(
                "https://127.0.0.1:1/never.bin",
                "never.bin",
                folder.path().to_path_buf(),
            ))
            .await?;

        handle.resume(added.id).await?;
        // Give a wrongly spawned job time to fail against the dead URL.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let listed = handle.list().await?;

        assert_eq!(
            listed.first().map(|item| item.status),
            Some(DownloadStatus::Queued),
            "resume alone must leave the row queued for admission"
        );
        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn actor_should_pause_all_active_and_resume_all_paused() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let (handle, task) = DownloadManagerHandle::spawn(
            db.repository,
            db.part_repository,
            DownloadManagerOptions {
                command_buffer: 8,
                event_buffer: 32,
            },
        );
        let first = handle
            .add(NewDownload::http(
                "https://example.com/a.bin",
                "a.bin",
                PathBuf::from("C:/Downloads"),
            ))
            .await?;
        let second = handle
            .add(NewDownload::http(
                "https://example.com/b.bin",
                "b.bin",
                PathBuf::from("C:/Downloads"),
            ))
            .await?;
        handle.resume(first.id).await?;
        handle.resume(second.id).await?;

        let paused = handle.pause_all().await?;
        assert_eq!(paused.len(), 2);
        assert!(
            paused
                .iter()
                .all(|item| item.status == DownloadStatus::Paused)
        );

        // Second call is a no-op: already-paused downloads stay untouched.
        assert!(handle.pause_all().await?.is_empty());

        let resumed = handle.resume_all().await?;
        assert_eq!(resumed.len(), 2);
        assert!(
            resumed
                .iter()
                .all(|item| item.status == DownloadStatus::Queued)
        );
        assert!(handle.resume_all().await?.is_empty());

        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn remove_should_delete_final_and_incomplete_files() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let folder = tempfile::tempdir()
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;
        let (handle, task) = DownloadManagerHandle::spawn(
            db.repository,
            db.part_repository,
            DownloadManagerOptions {
                command_buffer: 8,
                event_buffer: 8,
            },
        );
        let added = handle
            .add(NewDownload::http(
                "https://example.com/file.bin",
                "file.bin",
                folder.path().to_path_buf(),
            ))
            .await?;
        let final_path = final_path_for_item(&added);
        let incomplete_path = incomplete_path_for_item(&added);
        tokio::fs::write(&final_path, b"complete")
            .await
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;
        tokio::fs::write(&incomplete_path, b"partial")
            .await
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;

        let removed = handle.remove(added.id).await?;

        assert!(removed);
        assert!(!final_path.exists());
        assert!(!incomplete_path.exists());
        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn add_should_choose_unique_file_name_when_destination_exists()
    -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let folder = tempfile::tempdir()
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;
        tokio::fs::write(folder.path().join("file.bin"), b"existing")
            .await
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;
        let (handle, task) = DownloadManagerHandle::spawn(
            db.repository,
            db.part_repository,
            DownloadManagerOptions {
                command_buffer: 8,
                event_buffer: 8,
            },
        );

        let added = handle
            .add(NewDownload::http(
                "https://example.com/file.bin",
                "file.bin",
                folder.path().to_path_buf(),
            ))
            .await?;

        assert_eq!(added.file_name, "file(1).bin");
        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn reset_should_clear_parts_and_failure_then_requeue() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let folder = tempfile::tempdir()
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;
        let (handle, task) = DownloadManagerHandle::spawn(
            db.repository.clone(),
            db.part_repository.clone(),
            DownloadManagerOptions {
                command_buffer: 8,
                event_buffer: 8,
            },
        );
        let added = handle
            .add(NewDownload::http(
                "https://example.com/file.bin",
                "file.bin",
                folder.path().to_path_buf(),
            ))
            .await?;
        // Simulate a paused validation failure with stale parts and partial bytes.
        let mut failed = added.clone();
        failed.status = DownloadStatus::Paused;
        failed.total_bytes = Some(Bytes::new(1024));
        failed.downloaded_bytes = Bytes::new(512);
        failed.etag = Some("\"stale-etag\"".to_owned());
        failed.failure = Some(DownloadFailure {
            kind: crate::download::FailureKind::Validation,
            message: "remote ETag changed".to_owned(),
        });
        db.repository.update(&failed).await?;
        db.part_repository
            .set(&DownloadPart {
                id: PartId::new(added.id.get()),
                download_id: added.id,
                index: 0,
                start_byte: Bytes::ZERO,
                end_byte: Some(Bytes::new(1023)),
                current_byte: Bytes::new(512),
                status: PartStatus::Receiving,
                retry_count: 0,
                updated_at: 1,
            })
            .await?;
        let incomplete_path = incomplete_path_for_item(&added);
        tokio::fs::write(&incomplete_path, vec![b'p'; 512])
            .await
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;

        let reset = handle.reset(added.id).await?;

        assert_eq!(reset.status, DownloadStatus::Queued);
        assert_eq!(reset.downloaded_bytes, Bytes::ZERO);
        assert_eq!(reset.total_bytes, None);
        assert_eq!(reset.etag, None);
        assert_eq!(reset.failure, None);
        assert!(
            db.part_repository
                .list_for_download(added.id)
                .await?
                .is_empty()
        );
        assert!(!incomplete_path.exists());
        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn reset_should_reject_unknown_download() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let (handle, task) = DownloadManagerHandle::spawn(
            db.repository,
            db.part_repository,
            DownloadManagerOptions {
                command_buffer: 8,
                event_buffer: 8,
            },
        );

        let error = handle
            .reset(DownloadId::new(999))
            .await
            .expect_err("reset must fail for unknown download");

        assert!(matches!(error, DownloadManagerError::NotFound(_)));
        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn set_global_limit_should_accept_limit_and_disable() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let (handle, task) = DownloadManagerHandle::spawn(
            db.repository,
            db.part_repository,
            DownloadManagerOptions {
                command_buffer: 8,
                event_buffer: 8,
            },
        );

        handle
            .set_global_speed_limit(Some(BytesPerSecond::new(1024)))
            .await?;
        // Zero restores the disabled zero-cost path (`SpeedLimiter::new` returns `None`).
        handle
            .set_global_speed_limit(Some(BytesPerSecond::ZERO))
            .await?;
        handle.set_global_speed_limit(None).await?;

        stop_manager(&handle, task).await
    }

    fn sample_item(id: DownloadId, status: DownloadStatus) -> DownloadItem {
        DownloadItem {
            id,
            kind: DownloadKind::Http,
            url: "https://example.com/file.bin".to_owned(),
            download_page: None,
            headers: BTreeMap::new(),
            file_name: "file.bin".to_owned(),
            folder: PathBuf::from("C:/Downloads"),
            category: None,
            status,
            total_bytes: None,
            downloaded_bytes: Bytes::ZERO,
            etag: None,
            last_modified: None,
            preferred_connections: None,
            speed_limit: None,
            failure: None,
            created_at: 1,
            started_at: None,
            completed_at: None,
            updated_at: 1,
        }
    }
}
