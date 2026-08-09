use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::db::{DbError, QueueRepository};
use crate::download::{
    DownloadEvent, DownloadId, DownloadManagerError, DownloadManagerHandle, DownloadStatus, QueueId,
};
use crate::queue::{Queue, QueueItem};

/// Default bounded queue command channel size.
pub const DEFAULT_QUEUE_COMMAND_BUFFER: usize = 64;

/// Result type returned by queue manager APIs.
pub type QueueManagerResult<T> = Result<T, QueueManagerError>;

/// Errors produced by queue manager APIs.
#[derive(Debug, Error)]
pub enum QueueManagerError {
    /// Database operation failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// Download manager command failed.
    #[error(transparent)]
    Download(#[from] DownloadManagerError),
    /// Queue manager command channel is closed.
    #[error("queue manager is stopped")]
    Stopped,
    /// Command response channel closed before a reply was sent.
    #[error("queue manager response was dropped")]
    ResponseDropped,
    /// Requested queue does not exist.
    #[error("queue `{0}` not found")]
    QueueNotFound(QueueId),
    /// Requested download is not in the queue.
    #[error("download `{download_id}` not found in queue `{queue_id}`")]
    QueueItemNotFound {
        /// Queue id.
        queue_id: QueueId,
        /// Download id.
        download_id: DownloadId,
    },
    /// Reorder request omitted or duplicated queue items.
    #[error("queue reorder must contain every queued download exactly once")]
    InvalidReorder,
    /// System clock is earlier than Unix epoch.
    #[error("system clock is earlier than Unix epoch")]
    ClockBeforeEpoch,
    /// System clock value does not fit in database timestamp range.
    #[error("system clock timestamp is out of range")]
    ClockOutOfRange,
}

/// Commands handled by the queue manager actor.
#[derive(Debug)]
pub enum QueueCommand {
    /// Add one download to a queue at the tail.
    Enqueue {
        /// Target queue id.
        queue_id: QueueId,
        /// Download id to enqueue.
        download_id: DownloadId,
        /// Reply channel.
        reply: oneshot::Sender<QueueManagerResult<Vec<QueueItem>>>,
    },
    /// Replace queue item order.
    Reorder {
        /// Target queue id.
        queue_id: QueueId,
        /// Ordered download ids.
        download_ids: Vec<DownloadId>,
        /// Reply channel.
        reply: oneshot::Sender<QueueManagerResult<Vec<QueueItem>>>,
    },
    /// Update queue settings.
    SetQueue {
        /// Queue settings to persist.
        queue: Queue,
        /// Reply channel.
        reply: oneshot::Sender<QueueManagerResult<Queue>>,
    },
    /// Start one queue.
    Start {
        /// Queue id.
        queue_id: QueueId,
        /// Reply channel.
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
    /// Stop one queue and pause active queue items.
    Stop {
        /// Queue id.
        queue_id: QueueId,
        /// Reply channel.
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
    /// List queue items.
    ListItems {
        /// Queue id.
        queue_id: QueueId,
        /// Reply channel.
        reply: oneshot::Sender<QueueManagerResult<Vec<QueueItem>>>,
    },
    /// Stop the queue manager actor.
    Shutdown {
        /// Reply channel.
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
}

/// Queue events emitted by the queue manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueEvent {
    /// Default main queue was ensured during boot.
    Booted { queue_id: QueueId },
    /// Queue items changed.
    ItemsChanged {
        queue_id: QueueId,
        items: Vec<QueueItem>,
    },
    /// Queue started.
    Started { queue_id: QueueId },
    /// Queue stopped.
    Stopped { queue_id: QueueId },
    /// Download was started by queue scheduling.
    DownloadStarted {
        queue_id: QueueId,
        download_id: DownloadId,
    },
    /// Download was removed from queue after completion/removal.
    DownloadFinished {
        queue_id: QueueId,
        download_id: DownloadId,
    },
    /// Queue manager stopped.
    Shutdown,
}

/// Queue manager runtime options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueManagerOptions {
    /// Bounded command channel capacity.
    pub command_buffer: usize,
}

impl Default for QueueManagerOptions {
    fn default() -> Self {
        Self {
            command_buffer: DEFAULT_QUEUE_COMMAND_BUFFER,
        }
    }
}

/// Public cloneable queue manager handle.
#[derive(Debug, Clone)]
pub struct QueueManagerHandle {
    commands: mpsc::Sender<QueueCommand>,
    events: broadcast::Sender<QueueEvent>,
}

impl QueueManagerHandle {
    /// Spawns a queue manager.
    #[must_use]
    pub fn spawn(
        repository: QueueRepository,
        downloads: DownloadManagerHandle,
        download_events: broadcast::Receiver<DownloadEvent>,
        options: QueueManagerOptions,
    ) -> (Self, JoinHandle<QueueManagerResult<()>>) {
        let (handle, task, _) =
            Self::spawn_with_events(repository, downloads, download_events, options);
        (handle, task)
    }

    /// Spawns a queue manager and returns a pre-boot event receiver.
    #[must_use]
    pub fn spawn_with_events(
        repository: QueueRepository,
        downloads: DownloadManagerHandle,
        download_events: broadcast::Receiver<DownloadEvent>,
        options: QueueManagerOptions,
    ) -> (
        Self,
        JoinHandle<QueueManagerResult<()>>,
        broadcast::Receiver<QueueEvent>,
    ) {
        let (command_tx, command_rx) = mpsc::channel(options.command_buffer);
        let (event_tx, event_rx) = broadcast::channel(options.command_buffer.max(8));
        let handle = Self {
            commands: command_tx,
            events: event_tx.clone(),
        };
        let manager =
            QueueManager::new(repository, downloads, download_events, command_rx, event_tx);
        let task = tokio::spawn(manager.run());
        (handle, task, event_rx)
    }

    /// Subscribes to queue events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<QueueEvent> {
        self.events.subscribe()
    }

    /// Adds one download to a queue.
    ///
    /// # Errors
    ///
    /// Returns an error when persistence fails or manager stops.
    pub async fn enqueue(
        &self,
        queue_id: QueueId,
        download_id: DownloadId,
    ) -> QueueManagerResult<Vec<QueueItem>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(QueueCommand::Enqueue {
                queue_id,
                download_id,
                reply,
            })
            .await
            .map_err(|_| QueueManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Replaces persisted queue order.
    ///
    /// # Errors
    ///
    /// Returns an error if order is invalid or persistence fails.
    pub async fn reorder(
        &self,
        queue_id: QueueId,
        download_ids: Vec<DownloadId>,
    ) -> QueueManagerResult<Vec<QueueItem>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(QueueCommand::Reorder {
                queue_id,
                download_ids,
                reply,
            })
            .await
            .map_err(|_| QueueManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Persists queue settings.
    ///
    /// # Errors
    ///
    /// Returns an error when persistence fails.
    pub async fn set_queue(&self, queue: Queue) -> QueueManagerResult<Queue> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(QueueCommand::SetQueue { queue, reply })
            .await
            .map_err(|_| QueueManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Starts one queue.
    ///
    /// # Errors
    ///
    /// Returns an error when scheduling fails.
    pub async fn start(&self, queue_id: QueueId) -> QueueManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(QueueCommand::Start { queue_id, reply })
            .await
            .map_err(|_| QueueManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Stops one queue and pauses its active items.
    ///
    /// # Errors
    ///
    /// Returns an error when pause commands fail.
    pub async fn stop(&self, queue_id: QueueId) -> QueueManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(QueueCommand::Stop { queue_id, reply })
            .await
            .map_err(|_| QueueManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Lists queue items.
    ///
    /// # Errors
    ///
    /// Returns an error when persistence read fails.
    pub async fn list_items(&self, queue_id: QueueId) -> QueueManagerResult<Vec<QueueItem>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(QueueCommand::ListItems { queue_id, reply })
            .await
            .map_err(|_| QueueManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Requests graceful shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error when manager has already stopped.
    pub async fn shutdown(&self) -> QueueManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(QueueCommand::Shutdown { reply })
            .await
            .map_err(|_| QueueManagerError::Stopped)?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }
}

#[derive(Debug)]
struct QueueRuntime {
    running: bool,
    active: HashSet<DownloadId>,
}

impl QueueRuntime {
    fn new() -> Self {
        Self {
            running: false,
            active: HashSet::new(),
        }
    }
}

struct QueueManager {
    repository: QueueRepository,
    downloads: DownloadManagerHandle,
    download_events: broadcast::Receiver<DownloadEvent>,
    commands: mpsc::Receiver<QueueCommand>,
    events: broadcast::Sender<QueueEvent>,
    main: QueueRuntime,
}

impl QueueManager {
    fn new(
        repository: QueueRepository,
        downloads: DownloadManagerHandle,
        download_events: broadcast::Receiver<DownloadEvent>,
        commands: mpsc::Receiver<QueueCommand>,
        events: broadcast::Sender<QueueEvent>,
    ) -> Self {
        Self {
            repository,
            downloads,
            download_events,
            commands,
            events,
            main: QueueRuntime::new(),
        }
    }

    async fn run(mut self) -> QueueManagerResult<()> {
        self.boot().await?;
        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    let Some(command) = command else { break; };
                    if self.handle_command(command).await? {
                        break;
                    }
                }
                event = self.download_events.recv() => {
                    match event {
                        Ok(event) => self.handle_download_event(event).await?,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => tracing::warn!(skipped, "queue manager lagged download events"),
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        self.publish(QueueEvent::Shutdown);
        Ok(())
    }

    async fn boot(&mut self) -> QueueManagerResult<()> {
        self.repository.ensure_default_queue(now_ms()?).await?;
        self.publish(QueueEvent::Booted {
            queue_id: QueueId::MAIN,
        });
        Ok(())
    }

    async fn handle_command(&mut self, command: QueueCommand) -> QueueManagerResult<bool> {
        match command {
            QueueCommand::Enqueue {
                queue_id,
                download_id,
                reply,
            } => {
                let result = self.enqueue(queue_id, download_id).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Reorder {
                queue_id,
                download_ids,
                reply,
            } => {
                let result = self.reorder(queue_id, download_ids).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::SetQueue { queue, reply } => {
                let result = self.set_queue(queue).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Start { queue_id, reply } => {
                let result = self.start_queue(queue_id).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Stop { queue_id, reply } => {
                let result = self.stop_queue(queue_id).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::ListItems { queue_id, reply } => {
                send_reply(
                    reply,
                    self.repository
                        .list_items(queue_id)
                        .await
                        .map_err(Into::into),
                );
                Ok(false)
            }
            QueueCommand::Shutdown { reply } => {
                send_reply(reply, Ok(()));
                Ok(true)
            }
        }
    }

    async fn enqueue(
        &mut self,
        queue_id: QueueId,
        download_id: DownloadId,
    ) -> QueueManagerResult<Vec<QueueItem>> {
        self.queue(queue_id).await?;
        let mut items = self.repository.list_items(queue_id).await?;
        if items.iter().any(|item| item.download_id == download_id) {
            return Ok(items);
        }
        let position = u32::try_from(items.len()).map_err(|_| QueueManagerError::InvalidReorder)?;
        items.push(QueueItem {
            queue_id,
            download_id,
            position,
        });
        self.repository.set_items(queue_id, &items).await?;
        self.publish(QueueEvent::ItemsChanged {
            queue_id,
            items: items.clone(),
        });
        if self.runtime(queue_id)?.running {
            self.schedule(queue_id).await?;
        }
        Ok(items)
    }

    async fn reorder(
        &mut self,
        queue_id: QueueId,
        download_ids: Vec<DownloadId>,
    ) -> QueueManagerResult<Vec<QueueItem>> {
        let existing = self.repository.list_items(queue_id).await?;
        validate_reorder(&existing, &download_ids)?;
        let items = download_ids
            .into_iter()
            .enumerate()
            .map(|(position, download_id)| {
                u32::try_from(position)
                    .map(|position| QueueItem {
                        queue_id,
                        download_id,
                        position,
                    })
                    .map_err(|_| QueueManagerError::InvalidReorder)
            })
            .collect::<QueueManagerResult<Vec<_>>>()?;
        self.repository.set_items(queue_id, &items).await?;
        self.publish(QueueEvent::ItemsChanged {
            queue_id,
            items: items.clone(),
        });
        Ok(items)
    }

    async fn set_queue(&mut self, mut queue: Queue) -> QueueManagerResult<Queue> {
        queue.updated_at = now_ms()?;
        self.repository.set_queue(&queue).await?;
        if self.runtime(queue.id)?.running {
            self.schedule(queue.id).await?;
        }
        Ok(queue)
    }

    async fn start_queue(&mut self, queue_id: QueueId) -> QueueManagerResult<()> {
        self.queue(queue_id).await?;
        self.runtime_mut(queue_id)?.running = true;
        self.publish(QueueEvent::Started { queue_id });
        self.schedule(queue_id).await
    }

    async fn stop_queue(&mut self, queue_id: QueueId) -> QueueManagerResult<()> {
        self.queue(queue_id).await?;
        self.runtime_mut(queue_id)?.running = false;
        let active = self
            .runtime(queue_id)?
            .active
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for download_id in active {
            let _item = self.downloads.pause(download_id).await?;
        }
        self.runtime_mut(queue_id)?.active.clear();
        self.publish(QueueEvent::Stopped { queue_id });
        Ok(())
    }

    async fn handle_download_event(&mut self, event: DownloadEvent) -> QueueManagerResult<()> {
        let Some((download_id, terminal)) = terminal_download(&event) else {
            return Ok(());
        };
        if !terminal {
            return Ok(());
        }
        let queue_id = QueueId::MAIN;
        self.runtime_mut(queue_id)?.active.remove(&download_id);
        if self.repository.remove_item(queue_id, download_id).await? {
            self.compact_positions(queue_id).await?;
            self.publish(QueueEvent::DownloadFinished {
                queue_id,
                download_id,
            });
        }
        if self.runtime(queue_id)?.running {
            self.schedule(queue_id).await?;
        }
        Ok(())
    }

    async fn schedule(&mut self, queue_id: QueueId) -> QueueManagerResult<()> {
        let queue = self.queue(queue_id).await?;
        let max_active = usize::from(queue.max_concurrent.get());
        loop {
            if !self.runtime(queue_id)?.running
                || self.runtime(queue_id)?.active.len() >= max_active
            {
                return Ok(());
            }
            let Some(next) = self.next_inactive_item(queue_id).await? else {
                if queue.stop_on_empty {
                    self.runtime_mut(queue_id)?.running = false;
                    self.publish(QueueEvent::Stopped { queue_id });
                }
                return Ok(());
            };
            let _item = self.downloads.resume(next.download_id).await?;
            self.runtime_mut(queue_id)?.active.insert(next.download_id);
            self.publish(QueueEvent::DownloadStarted {
                queue_id,
                download_id: next.download_id,
            });
        }
    }

    async fn next_inactive_item(&self, queue_id: QueueId) -> QueueManagerResult<Option<QueueItem>> {
        let active = &self.runtime(queue_id)?.active;
        Ok(self
            .repository
            .list_items(queue_id)
            .await?
            .into_iter()
            .find(|item| !active.contains(&item.download_id)))
    }

    async fn compact_positions(&self, queue_id: QueueId) -> QueueManagerResult<()> {
        let items = self.repository.list_items(queue_id).await?;
        let compacted = items
            .into_iter()
            .enumerate()
            .map(|(position, item)| {
                u32::try_from(position)
                    .map(|position| QueueItem { position, ..item })
                    .map_err(|_| QueueManagerError::InvalidReorder)
            })
            .collect::<QueueManagerResult<Vec<_>>>()?;
        self.repository.set_items(queue_id, &compacted).await?;
        self.publish(QueueEvent::ItemsChanged {
            queue_id,
            items: compacted,
        });
        Ok(())
    }

    async fn queue(&self, queue_id: QueueId) -> QueueManagerResult<Queue> {
        self.repository
            .get_queue(queue_id)
            .await?
            .ok_or(QueueManagerError::QueueNotFound(queue_id))
    }

    fn runtime(&self, queue_id: QueueId) -> QueueManagerResult<&QueueRuntime> {
        if queue_id == QueueId::MAIN {
            Ok(&self.main)
        } else {
            Err(QueueManagerError::QueueNotFound(queue_id))
        }
    }

    fn runtime_mut(&mut self, queue_id: QueueId) -> QueueManagerResult<&mut QueueRuntime> {
        if queue_id == QueueId::MAIN {
            Ok(&mut self.main)
        } else {
            Err(QueueManagerError::QueueNotFound(queue_id))
        }
    }

    fn publish(&self, event: QueueEvent) {
        match self.events.send(event) {
            Ok(_) => {}
            Err(error) => tracing::trace!(?error, "queue event had no subscribers"),
        }
    }
}

fn terminal_download(event: &DownloadEvent) -> Option<(DownloadId, bool)> {
    match event {
        DownloadEvent::DownloadChanged { item }
        | DownloadEvent::DownloadPaused { item }
        | DownloadEvent::DownloadResumed { item } => Some((
            item.id,
            matches!(
                item.status,
                DownloadStatus::Completed | DownloadStatus::Removed
            ),
        )),
        DownloadEvent::DownloadRemoved { id } => Some((*id, true)),
        _ => None,
    }
}

fn validate_reorder(existing: &[QueueItem], download_ids: &[DownloadId]) -> QueueManagerResult<()> {
    if existing.len() != download_ids.len() {
        return Err(QueueManagerError::InvalidReorder);
    }
    let expected = existing
        .iter()
        .map(|item| item.download_id)
        .collect::<HashSet<_>>();
    let actual = download_ids.iter().copied().collect::<HashSet<_>>();
    if expected == actual && actual.len() == download_ids.len() {
        Ok(())
    } else {
        Err(QueueManagerError::InvalidReorder)
    }
}

fn send_reply<T>(reply: oneshot::Sender<QueueManagerResult<T>>, result: QueueManagerResult<T>) {
    if reply.send(result).is_err() {
        tracing::trace!("queue command reply receiver dropped");
    }
}

fn now_ms() -> QueueManagerResult<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| QueueManagerError::ClockBeforeEpoch)?;
    i64::try_from(duration.as_millis()).map_err(|_| QueueManagerError::ClockOutOfRange)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::num::NonZeroU16;
    use std::path::PathBuf;
    use std::sync::Arc;

    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::broadcast;
    use tokio::time::{Duration, timeout};

    use super::*;
    use crate::db::{self, DownloadRepository, PartRepository};
    use crate::download::{Bytes, DownloadItem, DownloadManagerOptions, NewDownload};
    use crate::monitor::DownloadMonitorHandle;

    struct TestHarness {
        _dir: TempDir,
        queue_repo: QueueRepository,
        download_handle: DownloadManagerHandle,
        download_task: JoinHandle<Result<(), DownloadManagerError>>,
        queue_handle: QueueManagerHandle,
        queue_task: JoinHandle<QueueManagerResult<()>>,
        download_events: broadcast::Sender<DownloadEvent>,
    }

    impl TestHarness {
        async fn spawn() -> QueueManagerResult<Self> {
            let dir = tempfile::tempdir()
                .map_err(sqlx::Error::Io)
                .map_err(DbError::from)?;
            let db_path = dir.path().join("raijin-queue-test.sqlite");
            let database_url = format!("sqlite://{}", db_path.display());
            let pool = db::bootstrap(&database_url).await?;
            let download_repo = DownloadRepository::new(pool.clone());
            let part_repo = PartRepository::new(pool.clone());
            let queue_repo = QueueRepository::new(pool);
            let (download_handle, download_task) = DownloadManagerHandle::spawn(
                download_repo.clone(),
                part_repo,
                DownloadManagerOptions {
                    command_buffer: 16,
                    event_buffer: 16,
                },
            );
            let (download_events, receiver) = broadcast::channel(16);
            let (queue_handle, queue_task, mut queue_events) =
                QueueManagerHandle::spawn_with_events(
                    queue_repo.clone(),
                    download_handle.clone(),
                    receiver,
                    QueueManagerOptions { command_buffer: 16 },
                );
            match queue_events.recv().await {
                Ok(QueueEvent::Booted { .. }) => {}
                Ok(_) | Err(_) => return Err(QueueManagerError::Stopped),
            }
            Ok(Self {
                _dir: dir,
                queue_repo,
                download_handle,
                download_task,
                queue_handle,
                queue_task,
                download_events,
            })
        }

        async fn shutdown(self) -> QueueManagerResult<()> {
            self.queue_handle.shutdown().await?;
            self.queue_task
                .await
                .map_err(|_| QueueManagerError::Stopped)??;
            self.download_handle.shutdown().await?;
            self.download_task
                .await
                .map_err(|_| QueueManagerError::Stopped)??;
            Ok(())
        }
    }

    fn non_zero_u16(value: u16) -> NonZeroU16 {
        match NonZeroU16::new(value) {
            Some(value) => value,
            None => panic!("test value must be non-zero"),
        }
    }

    fn queue(max_concurrent: u16) -> Queue {
        Queue {
            id: QueueId::MAIN,
            name: "Main".to_owned(),
            max_concurrent: non_zero_u16(max_concurrent),
            stop_on_empty: false,
            schedule_json: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    async fn add_downloads(
        harness: &TestHarness,
        count: usize,
    ) -> QueueManagerResult<Vec<DownloadItem>> {
        let mut items = Vec::with_capacity(count);
        for index in 0..count {
            let item = harness
                .download_handle
                .add(crate::download::NewDownload::http(
                    format!("https://example.com/file-{index}.bin"),
                    format!("file-{index}.bin"),
                    PathBuf::from("C:/Downloads"),
                ))
                .await?;
            harness.queue_handle.enqueue(QueueId::MAIN, item.id).await?;
            items.push(item);
        }
        Ok(items)
    }

    fn completed_event(mut item: DownloadItem) -> DownloadEvent {
        item.status = DownloadStatus::Completed;
        item.downloaded_bytes = item.total_bytes.unwrap_or(Bytes::ZERO);
        DownloadEvent::DownloadChanged { item }
    }

    #[derive(Debug)]
    struct TestServer {
        addr: SocketAddr,
        task: JoinHandle<()>,
    }

    impl TestServer {
        async fn spawn(body: Vec<u8>, slow_body: bool) -> std::io::Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let state = Arc::new((body, slow_body));
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let _ = handle_connection(stream, state).await;
                    });
                }
            });
            Ok(Self { addr, task })
        }

        fn url(&self, name: &str) -> String {
            format!("http://{}/{}", self.addr, name)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn handle_connection(
        mut stream: TcpStream,
        state: Arc<(Vec<u8>, bool)>,
    ) -> std::io::Result<()> {
        let mut request = vec![0_u8; 4096];
        let read = stream.read(&mut request).await?;
        let request = String::from_utf8_lossy(&request[..read]);
        let body = &state.0;
        let range = requested_range(&request);
        let (status, reason, response_body, content_range) = if let Some((start, end)) = range {
            let start = usize::try_from(start)
                .unwrap_or(0)
                .min(body.len().saturating_sub(1));
            let end = end
                .and_then(|end| usize::try_from(end).ok())
                .unwrap_or(body.len().saturating_sub(1))
                .min(body.len().saturating_sub(1));
            (
                206,
                "Partial Content",
                &body[start..=end],
                Some(format!("bytes {start}-{end}/{}", body.len())),
            )
        } else {
            (200, "OK", body.as_slice(), None)
        };

        let mut headers = format!(
            "HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: {}\r\nETag: \"queue-e2e\"\r\nContent-Type: application/octet-stream\r\n",
            response_body.len()
        );
        if let Some(content_range) = content_range {
            headers.push_str("Content-Range: ");
            headers.push_str(&content_range);
            headers.push_str("\r\n");
        }
        headers.push_str("\r\n");
        stream.write_all(headers.as_bytes()).await?;
        if state.1 && range.is_none() {
            for chunk in response_body.chunks(1024) {
                stream.write_all(chunk).await?;
                tokio::time::sleep(Duration::from_millis(4)).await;
            }
        } else {
            stream.write_all(response_body).await?;
        }
        stream.shutdown().await
    }

    fn requested_range(request: &str) -> Option<(u64, Option<u64>)> {
        request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.eq_ignore_ascii_case("range") {
                return None;
            }
            let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
            Some((
                start.parse().ok()?,
                if end.is_empty() {
                    None
                } else {
                    Some(end.parse().ok()?)
                },
            ))
        })
    }

    #[tokio::test]
    async fn queue_should_start_at_most_max_concurrent_downloads() -> QueueManagerResult<()> {
        let harness = TestHarness::spawn().await?;
        harness.queue_handle.set_queue(queue(2)).await?;
        let items = add_downloads(&harness, 3).await?;

        harness.queue_handle.start(QueueId::MAIN).await?;
        let downloads = harness.download_handle.list().await?;
        let queued = downloads
            .iter()
            .filter(|item| item.status == DownloadStatus::Queued)
            .count();

        assert_eq!(queued, 2);
        assert_eq!(items.len(), 3);
        harness.shutdown().await
    }

    #[tokio::test]
    async fn queue_should_start_next_item_after_completion() -> QueueManagerResult<()> {
        let harness = TestHarness::spawn().await?;
        harness.queue_handle.set_queue(queue(1)).await?;
        let items = add_downloads(&harness, 2).await?;
        let mut events = harness.queue_handle.subscribe();
        harness.queue_handle.start(QueueId::MAIN).await?;

        let _sent = harness
            .download_events
            .send(completed_event(items[0].clone()));
        let mut saw_second_start = false;
        for _ in 0..8 {
            match events.recv().await {
                Ok(QueueEvent::DownloadStarted { download_id, .. })
                    if download_id == items[1].id =>
                {
                    saw_second_start = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let queued = harness
            .download_handle
            .list()
            .await?
            .into_iter()
            .find(|item| item.id == items[1].id)
            .ok_or(QueueManagerError::Stopped)?;

        assert!(saw_second_start);
        assert_eq!(queued.status, DownloadStatus::Queued);
        harness.shutdown().await
    }

    #[tokio::test]
    async fn queue_stop_should_pause_active_items() -> QueueManagerResult<()> {
        let harness = TestHarness::spawn().await?;
        harness.queue_handle.set_queue(queue(2)).await?;
        let items = add_downloads(&harness, 3).await?;
        harness.queue_handle.start(QueueId::MAIN).await?;

        harness.queue_handle.stop(QueueId::MAIN).await?;
        let downloads = harness.download_handle.list().await?;
        let paused = downloads
            .iter()
            .filter(|item| {
                (item.id == items[0].id || item.id == items[1].id)
                    && item.status == DownloadStatus::Paused
            })
            .count();

        assert_eq!(paused, 2);
        harness.shutdown().await
    }

    #[tokio::test]
    async fn reorder_should_persist_queue_order() -> QueueManagerResult<()> {
        let harness = TestHarness::spawn().await?;
        let items = add_downloads(&harness, 3).await?;
        let expected = vec![items[2].id, items[0].id, items[1].id];

        harness
            .queue_handle
            .reorder(QueueId::MAIN, expected.clone())
            .await?;
        let persisted = harness.queue_repo.list_items(QueueId::MAIN).await?;
        let actual = persisted
            .into_iter()
            .map(|item| item.download_id)
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
        harness.shutdown().await
    }

    #[tokio::test]
    async fn completion_should_remove_item_from_queue() -> QueueManagerResult<()> {
        let harness = TestHarness::spawn().await?;
        harness.queue_handle.set_queue(queue(1)).await?;
        let items = add_downloads(&harness, 2).await?;
        let mut events = harness.queue_handle.subscribe();
        harness.queue_handle.start(QueueId::MAIN).await?;

        let _sent = harness
            .download_events
            .send(completed_event(items[0].clone()));
        for _ in 0..8 {
            if matches!(events.recv().await, Ok(QueueEvent::DownloadFinished { download_id, .. }) if download_id == items[0].id)
            {
                break;
            }
        }
        let persisted = harness.queue_repo.list_items(QueueId::MAIN).await?;

        assert!(!persisted.iter().any(|item| item.download_id == items[0].id));
        harness.shutdown().await
    }

    #[tokio::test]
    async fn queue_monitor_and_manager_should_run_download_end_to_end() -> QueueManagerResult<()> {
        let server = TestServer::spawn(vec![b'e'; 128 * 1024], true)
            .await
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;
        let harness = TestHarness::spawn().await?;
        harness.queue_handle.set_queue(queue(1)).await?;
        let folder = tempfile::tempdir()
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;
        let mut download_events = harness.download_handle.subscribe();
        let mut forwarded_download_events = harness.download_handle.subscribe();
        let forwarded_sender = harness.download_events.clone();
        let forwarder = tokio::spawn(async move {
            while let Ok(event) = forwarded_download_events.recv().await {
                let _ = forwarded_sender.send(event);
            }
        });
        let (monitor, monitor_task) =
            DownloadMonitorHandle::spawn(harness.download_handle.subscribe(), None);
        let mut queue_events = harness.queue_handle.subscribe();
        let mut first_request = NewDownload::http(
            server.url("first.bin"),
            "first.bin",
            folder.path().to_path_buf(),
        );
        first_request.preferred_connections = Some(non_zero_u16(1));
        let mut second_request = NewDownload::http(
            server.url("second.bin"),
            "second.bin",
            folder.path().to_path_buf(),
        );
        second_request.preferred_connections = Some(non_zero_u16(1));
        let first = harness.download_handle.add(first_request).await?;
        let second = harness.download_handle.add(second_request).await?;
        harness
            .queue_handle
            .enqueue(QueueId::MAIN, first.id)
            .await?;
        harness
            .queue_handle
            .enqueue(QueueId::MAIN, second.id)
            .await?;

        harness.queue_handle.start(QueueId::MAIN).await?;

        let saw_progress = timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    download_events.recv().await,
                    Ok(DownloadEvent::DownloadProgress { id, progress })
                        if id == first.id && progress.downloaded_bytes > Bytes::ZERO
                ) {
                    break true;
                }
            }
        })
        .await
        .map_err(|_| QueueManagerError::Stopped)?;
        let saw_second_start = timeout(Duration::from_secs(10), async {
            loop {
                if matches!(
                    queue_events.recv().await,
                    Ok(QueueEvent::DownloadStarted { download_id, .. }) if download_id == second.id
                ) {
                    break true;
                }
            }
        })
        .await
        .map_err(|_| QueueManagerError::Stopped)?;
        let state = monitor.state();
        let completed_or_active =
            state.completed.contains_key(&first.id) || state.active.contains_key(&first.id);

        harness.queue_handle.stop(QueueId::MAIN).await?;
        let _ = harness.download_handle.pause(second.id).await;

        assert!(saw_progress);
        assert!(saw_second_start);
        assert!(completed_or_active);
        harness.shutdown().await?;
        forwarder.abort();
        monitor_task
            .await
            .map_err(|_| QueueManagerError::Stopped)?
            .map_err(|_| QueueManagerError::Stopped)
    }
}
