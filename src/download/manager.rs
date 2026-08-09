use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::db::{DbError, DownloadRepository, PartRepository};
use crate::download::{
    Bytes, BytesPerSecond, DownloadFailure, DownloadId, DownloadItem, DownloadKind,
    DownloadProgress, DownloadStatus, HttpDownloadJob, ReqwestHttpClient,
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
}

/// New download request accepted by the manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewDownload {
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
            url: url.into(),
            download_page: None,
            headers: BTreeMap::new(),
            file_name: file_name.into(),
            folder,
            preferred_connections: None,
            speed_limit: None,
        }
    }
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
    /// Pause a download skeleton.
    Pause {
        id: DownloadId,
        reply: oneshot::Sender<DownloadManagerResult<DownloadItem>>,
    },
    /// Resume a download skeleton.
    Resume {
        id: DownloadId,
        reply: oneshot::Sender<DownloadManagerResult<DownloadItem>>,
    },
    /// Remove a download.
    Remove {
        id: DownloadId,
        reply: oneshot::Sender<DownloadManagerResult<bool>>,
    },
    /// Stop the manager actor.
    Shutdown {
        reply: oneshot::Sender<DownloadManagerResult<()>>,
    },
    #[doc(hidden)]
    JobFinished { id: DownloadId },
}

/// Events published by the download manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadEvent {
    /// Manager boot loaded unfinished downloads from persistence.
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

    /// Resumes a download skeleton.
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
    cancellation: CancellationToken,
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
        }
    }

    async fn run(mut self) -> DownloadManagerResult<()> {
        self.boot().await?;

        while let Some(command) = self.commands.recv().await {
            if self.handle_command(command).await? {
                break;
            }
        }

        self.cancel_active_jobs();
        self.publish(DownloadEvent::Shutdown);
        Ok(())
    }

    async fn boot(&mut self) -> DownloadManagerResult<()> {
        let unfinished = self.repository.list_unfinished().await?;
        for item in &unfinished {
            if should_start_on_boot(item.status) {
                self.start_job(item.clone());
            }
        }
        self.publish(DownloadEvent::BootLoaded {
            downloads: unfinished,
        });
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
            DownloadCommand::Remove { id, reply } => {
                let result = self.remove_download(id).await;
                send_reply(reply, result);
                Ok(false)
            }
            DownloadCommand::Shutdown { reply } => {
                send_reply(reply, Ok(()));
                Ok(true)
            }
            DownloadCommand::JobFinished { id } => {
                self.active_jobs.remove(&id);
                Ok(false)
            }
        }
    }

    async fn add_download(&mut self, request: NewDownload) -> DownloadManagerResult<DownloadItem> {
        let now_ms = now_ms()?;
        let item = DownloadItem {
            id: self.repository.next_id().await?,
            kind: DownloadKind::Http,
            url: request.url,
            download_page: request.download_page,
            headers: request.headers,
            file_name: request.file_name,
            folder: request.folder,
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
        item.status = DownloadStatus::Paused;
        item.updated_at = now_ms()?;
        self.repository.update(&item).await?;

        if let Some(job) = self.active_jobs.get_mut(&id) {
            job.cancellation.cancel();
        }

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
        self.start_job(item.clone());
        Ok(item)
    }

    async fn remove_download(&mut self, id: DownloadId) -> DownloadManagerResult<bool> {
        if let Some(job) = self.active_jobs.remove(&id) {
            job.cancellation.cancel();
        }

        let removed = self.repository.remove(id).await?;
        if removed {
            self.publish(DownloadEvent::DownloadRemoved { id });
        }
        Ok(removed)
    }

    async fn item_for_update(&self, id: DownloadId) -> DownloadManagerResult<DownloadItem> {
        self.repository
            .get(id)
            .await?
            .ok_or(DownloadManagerError::NotFound(id))
    }

    fn cancel_active_jobs(&mut self) {
        for job in self.active_jobs.values() {
            job.cancellation.cancel();
            job.task.abort();
        }
    }

    fn start_job(&mut self, item: DownloadItem) {
        if let Some(job) = self.active_jobs.remove(&item.id) {
            job.cancellation.cancel();
            job.task.abort();
        }

        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_http_job_task(JobTaskContext {
            download_repo: self.repository.clone(),
            part_repo: self.part_repository.clone(),
            events: self.events.clone(),
            commands: self.command_sender.clone(),
            item: item.clone(),
            cancellation: cancellation.clone(),
        }));
        self.active_jobs
            .insert(item.id, ActiveJob { cancellation, task });
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
    download_repo: DownloadRepository,
    part_repo: PartRepository,
    events: broadcast::Sender<DownloadEvent>,
    commands: mpsc::Sender<DownloadCommand>,
    item: DownloadItem,
    cancellation: CancellationToken,
}

async fn run_http_job_task(context: JobTaskContext) {
    let id = context.item.id;
    let result = run_http_job_task_inner(&context).await;
    match result {
        Ok(item) => publish_terminal_item(&context.events, item),
        Err(error) => {
            let failure = error.to_failure();
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
    if context
        .commands
        .send(DownloadCommand::JobFinished { id })
        .await
        .is_err()
    {
        tracing::trace!(
            download_id = id.get(),
            "download manager stopped before job cleanup"
        );
    }
}

async fn run_http_job_task_inner(
    context: &JobTaskContext,
) -> Result<DownloadItem, crate::download::HttpDownloadJobError> {
    let client = ReqwestHttpClient::new()?;
    let job = HttpDownloadJob::new(
        context.download_repo.clone(),
        context.part_repo.clone(),
        client,
        context.item.clone(),
    );
    let job_future = job.run(context.cancellation.clone());
    tokio::pin!(job_future);
    let mut ticker = tokio::time::interval(JOB_PROGRESS_TICK);
    loop {
        tokio::select! {
            result = &mut job_future => return result,
            _ = ticker.tick() => publish_progress_snapshot(context).await,
        }
    }
}

async fn publish_progress_snapshot(context: &JobTaskContext) {
    match context.download_repo.get(context.item.id).await {
        Ok(Some(item)) => {
            publish_event(
                &context.events,
                DownloadEvent::DownloadProgress {
                    id: item.id,
                    progress: DownloadProgress {
                        downloaded_bytes: item.downloaded_bytes,
                        total_bytes: item.total_bytes,
                        speed: BytesPerSecond::ZERO,
                        eta_seconds: None,
                        active_part_count: u16::from(item.status == DownloadStatus::Downloading),
                    },
                },
            );
            publish_event(&context.events, DownloadEvent::DownloadChanged { item });
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(
            ?error,
            download_id = context.item.id.get(),
            "failed to publish download progress snapshot"
        ),
    }
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

fn should_start_on_boot(status: DownloadStatus) -> bool {
    matches!(
        status,
        DownloadStatus::Queued
            | DownloadStatus::Downloading
            | DownloadStatus::Retrying
            | DownloadStatus::PreparingFile
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
        let request = NewDownload::http(
            "https://example.com/file.bin",
            "file.bin",
            PathBuf::from("C:/Downloads"),
        );

        let added = handle.add(request).await?;
        let downloads = handle.list().await?;

        assert_eq!(downloads, vec![added]);
        stop_manager(&handle, task).await
    }

    #[tokio::test]
    async fn boot_should_recreate_unfinished_download_metadata() -> DownloadManagerResult<()> {
        let db = test_db().await?;
        let unfinished = sample_item(DownloadId::new(1), DownloadStatus::Paused);
        let completed = sample_item(DownloadId::new(2), DownloadStatus::Completed);
        db.repository.add(&unfinished).await?;
        db.repository.add(&completed).await?;
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
                downloads: vec![unfinished]
            }
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
        let removed = handle.remove(added.id).await?;

        assert_eq!(paused.status, DownloadStatus::Paused);
        assert_eq!(resumed.status, DownloadStatus::Queued);
        assert!(removed);
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
