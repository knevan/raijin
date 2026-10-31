//! Queue actor: membership, ordering, and concurrency admission.
//!
//! The queue never runs a download itself. It owns *when* a `Queued` row may
//! run and drives the engine through [`DownloadManagerHandle::start`] /
//! `pause`, which keeps queue policy out of the download workers.
//!
//! Invariants:
//!
//! - `active` is the single source of truth for running slots. A download has
//!   exactly one owning queue, so moving it relabels the owner instead of
//!   restarting the job or leaking a slot.
//! - A download is recorded in `active` *before* the engine is asked to start
//!   it. The reverse order would let the echoed status event admit it twice.
//! - `blocked` holds downloads whose last run failed. Without it a failing
//!   download is re-admitted immediately (fail -> free slot -> admit -> fail).
//! - Membership rows are deleted when a download completes, matching ABDM's
//!   work-list semantics: "queue is empty" stays a property of the table
//!   instead of a status predicate every consumer must reimplement.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU16;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Local, NaiveDateTime};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::db::{DbError, DownloadRepository, QueueRepository};
use crate::download::{
    DownloadEvent, DownloadId, DownloadItem, DownloadManagerError, DownloadManagerHandle,
    DownloadStatus, QueueId,
};
use crate::queue::model::{Queue, QueueItem};
use crate::queue::schedule::{QueueSchedule, QueueScheduleError, local_now};

/// Default bounded command queue size.
pub const DEFAULT_QUEUE_COMMAND_BUFFER: usize = 64;

/// Default queue event buffer size.
pub const DEFAULT_QUEUE_EVENT_BUFFER: usize = 64;

/// How often enabled schedules are polled.
const SCHEDULE_TICK: std::time::Duration = std::time::Duration::from_secs(10);

/// Result type returned by queue APIs.
pub type QueueManagerResult<T> = Result<T, QueueManagerError>;

/// Errors produced by the queue actor or handle.
#[derive(Debug, Error)]
pub enum QueueManagerError {
    /// Database operation failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// Engine command failed.
    #[error(transparent)]
    Engine(#[from] DownloadManagerError),
    /// The queue actor is no longer running.
    #[error("queue manager is stopped")]
    Stopped,
    /// Command response channel closed before a reply was sent.
    #[error("queue manager response was dropped")]
    ResponseDropped,
    /// Requested queue does not exist.
    #[error("queue `{0}` not found")]
    QueueNotFound(QueueId),
    /// Requested download does not exist.
    #[error("download `{0}` not found")]
    DownloadNotFound(DownloadId),
    /// The reserved main queue cannot be deleted.
    #[error("the main queue cannot be deleted")]
    MainQueueNotDeletable,
    /// A download record exists, but its admission setup did not finish.
    #[error("download `{id}` was added but queue setup failed: {source}")]
    AdmissionAfterAdd {
        id: DownloadId,
        #[source]
        source: Box<QueueManagerError>,
    },
    /// Queue names must contain visible text.
    #[error("enter a queue name")]
    InvalidName,
    /// Membership moves cannot transfer live workers or finished downloads.
    #[error("pause download `{0}` before moving it; finished downloads cannot be queued")]
    NotMovable(DownloadId),
    /// Schedule rejected by the typed boundary.
    #[error(transparent)]
    Schedule(#[from] QueueScheduleError),
    /// System clock is earlier than Unix epoch.
    #[error("system clock is earlier than Unix epoch")]
    ClockBeforeEpoch,
    /// System clock value does not fit in database timestamp range.
    #[error("system clock timestamp is out of range")]
    ClockOutOfRange,
}

/// Runtime options for the queue actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueManagerOptions {
    /// Bounded command channel capacity.
    pub command_buffer: usize,
    /// Broadcast event channel capacity.
    pub event_buffer: usize,
    /// Hard ceiling for concurrently running downloads across all queues.
    pub global_limit: NonZeroU16,
}

impl Default for QueueManagerOptions {
    fn default() -> Self {
        Self {
            command_buffer: DEFAULT_QUEUE_COMMAND_BUFFER,
            event_buffer: DEFAULT_QUEUE_EVENT_BUFFER,
            global_limit: NonZeroU16::new(3).unwrap_or(NonZeroU16::MIN),
        }
    }
}

/// Commands owned by the queue actor.
#[derive(Debug)]
pub enum QueueCommand {
    /// List queues with their ordered member ids.
    List {
        reply: oneshot::Sender<QueueManagerResult<Vec<QueueSnapshot>>>,
    },
    /// Create a queue and return its id.
    Create {
        name: String,
        reply: oneshot::Sender<QueueManagerResult<QueueId>>,
    },
    /// Delete a non-main queue.
    Delete {
        id: QueueId,
        reply: oneshot::Sender<QueueManagerResult<bool>>,
    },
    /// Append downloads to a queue, moving them out of any other queue.
    Enqueue {
        queue_id: QueueId,
        downloads: Vec<DownloadId>,
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
    /// Move membership without resuming downloads.
    Move {
        queue_id: QueueId,
        downloads: Vec<DownloadId>,
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
    /// Drop one download from whichever queue holds it.
    RemoveItem {
        download_id: DownloadId,
        reply: oneshot::Sender<QueueManagerResult<bool>>,
    },
    /// Replace the full order of one queue.
    Reorder {
        queue_id: QueueId,
        order: Vec<DownloadId>,
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
    /// Update per-queue concurrency, stop-on-empty, or schedule.
    SetQueue {
        queue: Queue,
        schedule: Option<QueueSchedule>,
        reply: oneshot::Sender<QueueManagerResult<Queue>>,
    },
    /// Update user-facing properties without rewriting schedules.
    Configure {
        id: QueueId,
        name: String,
        limit: NonZeroU16,
        stop_on_empty: bool,
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
    /// Activate a queue so admission may fill its slots.
    Start {
        id: QueueId,
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
    /// Deactivate a queue and pause its running downloads.
    Stop {
        id: QueueId,
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
    /// Replace the global concurrency ceiling (Main mirrors it).
    SetGlobalLimit {
        limit: NonZeroU16,
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
    /// Rebuild runtime state from SQLite; run on boot and after event lag.
    Reconcile {
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
    /// Stop the queue actor.
    Shutdown {
        reply: oneshot::Sender<QueueManagerResult<()>>,
    },
}

/// Events published by the queue actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueEvent {
    /// Queue metadata or membership changed; consumers should re-list.
    Changed { queue_id: Option<QueueId> },
    /// A queue switched between active and inactive.
    ActivityChanged { queue_id: QueueId, active: bool },
    /// A queue ran out of work while it was active.
    BecameEmpty { queue_id: QueueId },
    /// A schedule start edge fired.
    StartTimeReached { queue_id: QueueId, was_active: bool },
    /// A schedule stop edge fired.
    EndTimeReached { queue_id: QueueId, was_active: bool },
}

/// Queue row plus its ordered member ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueSnapshot {
    /// Persisted queue metadata.
    pub queue: Queue,
    /// Member download ids ordered by position.
    pub items: Vec<DownloadId>,
    /// Decoded schedule, when the row has a valid one.
    pub schedule: Option<QueueSchedule>,
    /// Whether the queue currently accepts admission.
    pub active: bool,
}

/// Cloneable handle used to send commands to the queue actor.
#[derive(Debug, Clone)]
pub struct QueueManagerHandle {
    commands: mpsc::Sender<QueueCommand>,
    events: broadcast::Sender<QueueEvent>,
    snapshots: watch::Receiver<Vec<QueueSnapshot>>,
}

impl QueueManagerHandle {
    /// Spawns the queue actor and returns its handle plus task handle.
    #[must_use]
    pub fn spawn(
        downloads: DownloadRepository,
        queues: QueueRepository,
        engine: DownloadManagerHandle,
        engine_events: broadcast::Receiver<DownloadEvent>,
        options: QueueManagerOptions,
    ) -> (Self, JoinHandle<QueueManagerResult<()>>) {
        let (command_tx, command_rx) = mpsc::channel(options.command_buffer);
        let (event_tx, _event_rx) = broadcast::channel(options.event_buffer);
        let (snapshot_tx, snapshot_rx) = watch::channel(Vec::new());
        let handle = Self {
            commands: command_tx,
            events: event_tx.clone(),
            snapshots: snapshot_rx,
        };
        let manager = QueueManager {
            downloads,
            queues,
            engine,
            commands: command_rx,
            engine_events,
            events: event_tx,
            snapshots: snapshot_tx,
            global_limit: options.global_limit,
            queues_state: Vec::new(),
            statuses: HashMap::new(),
            active_queues: HashSet::new(),
            active: HashMap::new(),
            blocked: HashSet::new(),
            suspended: HashSet::new(),
            empty_notified: HashSet::new(),
            next_start: HashMap::new(),
            next_stop: HashMap::new(),
        };
        let task = tokio::spawn(manager.run());
        (handle, task)
    }

    /// Subscribes to queue events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<QueueEvent> {
        self.events.subscribe()
    }

    /// Returns a receiver for the latest queue snapshot.
    ///
    /// Views render from this watch channel and send commands back, so a UI
    /// thread never reads the queue tables itself.
    #[must_use]
    pub fn snapshots(&self) -> watch::Receiver<Vec<QueueSnapshot>> {
        self.snapshots.clone()
    }

    /// Returns the latest queue snapshot.
    #[must_use]
    pub fn state(&self) -> Vec<QueueSnapshot> {
        self.snapshots.borrow().clone()
    }

    /// Lists queues with ordered members.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor stopped or persistence fails.
    pub async fn list(&self) -> QueueManagerResult<Vec<QueueSnapshot>> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::List { reply }).await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Creates a queue.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor stopped or persistence fails.
    pub async fn create(&self, name: impl Into<String>) -> QueueManagerResult<QueueId> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::Create {
            name: name.into(),
            reply,
        })
        .await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Deletes a non-main queue.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor stopped or persistence fails.
    pub async fn delete(&self, id: QueueId) -> QueueManagerResult<bool> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::Delete { id, reply }).await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Appends downloads to a queue, moving them out of any other queue.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor stopped or persistence fails.
    pub async fn enqueue(
        &self,
        queue_id: QueueId,
        downloads: impl IntoIterator<Item = DownloadId>,
    ) -> QueueManagerResult<()> {
        let downloads = downloads.into_iter().collect::<Vec<_>>();
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::Enqueue {
            queue_id,
            downloads,
            reply,
        })
        .await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Moves memberships without resuming downloads or transferring live workers.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor stopped or persistence fails.
    pub async fn move_downloads(
        &self,
        queue_id: QueueId,
        downloads: Vec<DownloadId>,
    ) -> QueueManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::Move {
            queue_id,
            downloads,
            reply,
        })
        .await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Removes one download's membership.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor or database is unavailable.
    pub async fn remove_item(&self, download_id: DownloadId) -> QueueManagerResult<bool> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::RemoveItem { download_id, reply })
            .await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Replaces one queue's order.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor stopped or persistence fails.
    pub async fn reorder(
        &self,
        queue_id: QueueId,
        order: Vec<DownloadId>,
    ) -> QueueManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::Reorder {
            queue_id,
            order,
            reply,
        })
        .await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Updates queue metadata and its optional schedule.
    ///
    /// # Errors
    ///
    /// Returns an error when the schedule is invalid, the queue is unknown, or
    /// persistence fails.
    pub async fn set_queue(
        &self,
        queue: Queue,
        schedule: Option<QueueSchedule>,
    ) -> QueueManagerResult<Queue> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::SetQueue {
            queue,
            schedule,
            reply,
        })
        .await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Updates basic properties while preserving the stored schedule.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor stopped or persistence fails.
    pub async fn configure(
        &self,
        id: QueueId,
        name: String,
        limit: NonZeroU16,
        stop_on_empty: bool,
    ) -> QueueManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::Configure {
            id,
            name,
            limit,
            stop_on_empty,
            reply,
        })
        .await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Starts admission without resuming manually paused items.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor or database is unavailable.
    pub async fn start(&self, id: QueueId) -> QueueManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::Start { id, reply }).await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Deactivates a queue and pauses its running downloads.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor stopped or persistence fails.
    pub async fn stop(&self, id: QueueId) -> QueueManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::Stop { id, reply }).await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Replaces the global concurrency ceiling.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor stopped or persistence fails.
    pub async fn set_global_limit(&self, limit: NonZeroU16) -> QueueManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::SetGlobalLimit { limit, reply })
            .await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Rebuilds runtime state from SQLite.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor stopped or persistence fails.
    pub async fn reconcile(&self) -> QueueManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::Reconcile { reply }).await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    /// Requests graceful actor shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor already stopped.
    pub async fn shutdown(&self) -> QueueManagerResult<()> {
        let (reply, receiver) = oneshot::channel();
        self.send(QueueCommand::Shutdown { reply }).await?;
        receiver
            .await
            .map_err(|_| QueueManagerError::ResponseDropped)?
    }

    async fn send(&self, command: QueueCommand) -> QueueManagerResult<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| QueueManagerError::Stopped)
    }
}

/// Queue row plus its ordered members and decoded schedule.
#[derive(Debug, Clone)]
struct QueueState {
    queue: Queue,
    items: Vec<DownloadId>,
    schedule: Option<QueueSchedule>,
}

#[derive(Debug)]
struct QueueManager {
    downloads: DownloadRepository,
    queues: QueueRepository,
    engine: DownloadManagerHandle,
    commands: mpsc::Receiver<QueueCommand>,
    engine_events: broadcast::Receiver<DownloadEvent>,
    events: broadcast::Sender<QueueEvent>,
    snapshots: watch::Sender<Vec<QueueSnapshot>>,
    global_limit: NonZeroU16,
    queues_state: Vec<QueueState>,
    /// Last known durable status per download id.
    statuses: HashMap<DownloadId, DownloadStatus>,
    active_queues: HashSet<QueueId>,
    /// Download -> owning queue for every running slot.
    active: HashMap<DownloadId, QueueId>,
    /// Downloads whose last run failed; re-admitted only after a retry signal.
    blocked: HashSet<DownloadId>,
    suspended: HashSet<DownloadId>,
    /// Queues already reported empty, so `BecameEmpty` stays an edge event.
    empty_notified: HashSet<QueueId>,
    next_start: HashMap<QueueId, NaiveDateTime>,
    next_stop: HashMap<QueueId, NaiveDateTime>,
}

impl QueueManager {
    async fn run(mut self) -> QueueManagerResult<()> {
        self.boot().await?;

        let mut ticker = tokio::time::interval(SCHEDULE_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    match command {
                        Some(command) => {
                            if self.handle_command(command).await? {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                event = self.engine_events.recv() => {
                    match event {
                        Ok(event) => {
                            if matches!(event, DownloadEvent::Shutdown) {
                                break;
                            }
                            self.on_download_event(event).await?;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            // Dropping events would leave slots or blockers stale
                            // forever, so rebuild from SQLite instead.
                            tracing::warn!(skipped, "queue lagged download events, reconciling");
                            self.reconcile().await?;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = ticker.tick() => {
                    self.on_schedule_tick().await?;
                }
            }
        }

        Ok(())
    }

    async fn boot(&mut self) -> QueueManagerResult<()> {
        let now = now_ms()?;
        self.queues
            .ensure_default_queue(now, self.global_limit)
            .await?;
        self.reconcile().await?;
        // Runs once per process: rows created before queue admission existed
        // have no membership and would otherwise never start again. Doing it
        // here (not in `reconcile`) keeps an explicit remove-from-queue from
        // being undone by the next lag-triggered rebuild.
        self.adopt_orphan_downloads().await?;
        // The main queue is the application default: after a restart, unfinished
        // work must resume exactly like reopening the app expects it to.
        if self.queue_state(QueueId::MAIN).is_some() {
            self.set_active(QueueId::MAIN, true).await?;
        }
        self.publish(QueueEvent::Changed { queue_id: None });
        Ok(())
    }

    async fn handle_command(&mut self, command: QueueCommand) -> QueueManagerResult<bool> {
        match command {
            QueueCommand::List { reply } => {
                let result = Ok(self.snapshot());
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Create { name, reply } => {
                let result = self.create_queue(name).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Delete { id, reply } => {
                let result = self.delete_queue(id).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Enqueue {
                queue_id,
                downloads,
                reply,
            } => {
                let result = self.enqueue(queue_id, &downloads).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Move {
                queue_id,
                downloads,
                reply,
            } => {
                let result = self.move_downloads(queue_id, &downloads).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::RemoveItem { download_id, reply } => {
                let result = self.remove_item(download_id).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Reorder {
                queue_id,
                order,
                reply,
            } => {
                let result = self.reorder(queue_id, order).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::SetQueue {
                queue,
                schedule,
                reply,
            } => {
                let result = self.set_queue(queue, schedule).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Configure {
                id,
                name,
                limit,
                stop_on_empty,
                reply,
            } => {
                let result = self.configure(id, name, limit, stop_on_empty).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Start { id, reply } => {
                let result = self.set_active(id, true).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Stop { id, reply } => {
                let result = self.set_active(id, false).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::SetGlobalLimit { limit, reply } => {
                let result = self.set_global_limit(limit).await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Reconcile { reply } => {
                let result = self.reconcile().await;
                send_reply(reply, result);
                Ok(false)
            }
            QueueCommand::Shutdown { reply } => {
                send_reply(reply, Ok(()));
                Ok(true)
            }
        }
    }

    fn queue_state(&self, id: QueueId) -> Option<&QueueState> {
        self.queues_state.iter().find(|state| state.queue.id == id)
    }

    fn queue_state_mut(&mut self, id: QueueId) -> Option<&mut QueueState> {
        self.queues_state
            .iter_mut()
            .find(|state| state.queue.id == id)
    }

    fn snapshot(&self) -> Vec<QueueSnapshot> {
        self.queues_state
            .iter()
            .map(|state| QueueSnapshot {
                queue: state.queue.clone(),
                items: state.items.clone(),
                schedule: state.schedule.clone(),
                active: self.active_queues.contains(&state.queue.id),
            })
            .collect()
    }

    async fn create_queue(&mut self, name: String) -> QueueManagerResult<QueueId> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(QueueManagerError::InvalidName);
        }
        let now = now_ms()?;
        let id = self.queues.next_queue_id().await?;
        let queue = Queue {
            id,
            name,
            max_concurrent: NonZeroU16::new(2).unwrap_or(NonZeroU16::MIN),
            stop_on_empty: false,
            schedule_json: None,
            created_at: now,
            updated_at: now,
        };
        self.queues.create_queue(&queue).await?;
        self.queues_state.push(QueueState {
            queue,
            items: Vec::new(),
            schedule: None,
        });
        self.publish(QueueEvent::Changed { queue_id: Some(id) });
        Ok(id)
    }

    async fn delete_queue(&mut self, id: QueueId) -> QueueManagerResult<bool> {
        if id == QueueId::MAIN {
            return Err(QueueManagerError::MainQueueNotDeletable);
        }
        if self.queue_state(id).is_none() {
            return Err(QueueManagerError::QueueNotFound(id));
        }
        // Deactivate first: deleting the row cascades membership, and a running
        // job must not keep a slot for a queue that no longer exists.
        self.set_active(id, false).await?;
        let members = self
            .queue_state(id)
            .ok_or(QueueManagerError::QueueNotFound(id))?
            .items
            .clone();
        let mut transferred = Vec::new();
        for download_id in members {
            self.suspended.remove(&download_id);
            let Some(item) = self.downloads.get(download_id).await? else {
                continue;
            };
            if matches!(
                item.status,
                DownloadStatus::Completed | DownloadStatus::Removed
            ) {
                continue;
            }
            let item = self.engine.pause(download_id).await?;
            self.statuses.insert(download_id, item.status);
            transferred.push(download_id);
        }
        let removed = self.queues.delete_queue_into_main(id).await?;
        if let Some(main) = self.queue_state_mut(QueueId::MAIN) {
            for download_id in transferred {
                if !main.items.contains(&download_id) {
                    main.items.push(download_id);
                }
            }
        }
        self.queues_state.retain(|state| state.queue.id != id);
        self.active.retain(|_, owner| *owner != id);
        self.next_start.remove(&id);
        self.next_stop.remove(&id);
        self.publish(QueueEvent::Changed { queue_id: Some(id) });
        self.admit_all().await?;
        Ok(removed)
    }

    async fn enqueue(
        &mut self,
        queue_id: QueueId,
        downloads: &[DownloadId],
    ) -> QueueManagerResult<()> {
        if self.queue_state(queue_id).is_none() {
            return Err(QueueManagerError::QueueNotFound(queue_id));
        }
        if downloads.is_empty() {
            return Ok(());
        }
        for download_id in downloads {
            let item = self
                .downloads
                .get(*download_id)
                .await?
                .ok_or(QueueManagerError::DownloadNotFound(*download_id))?;
            self.attach(*download_id, queue_id).await?;
            // An explicit enqueue is the user retrying, so clear the block.
            self.blocked.remove(download_id);
            self.statuses.insert(*download_id, item.status);
        }
        self.persist_items(queue_id).await?;
        for download_id in downloads {
            self.set_queued(*download_id).await?;
        }
        // New work means the next drain must report emptiness again.
        self.empty_notified.remove(&queue_id);
        self.publish(QueueEvent::Changed {
            queue_id: Some(queue_id),
        });
        self.admit_all().await?;
        Ok(())
    }

    async fn move_downloads(
        &mut self,
        queue_id: QueueId,
        ids: &[DownloadId],
    ) -> QueueManagerResult<()> {
        if self.queue_state(queue_id).is_none() {
            return Err(QueueManagerError::QueueNotFound(queue_id));
        }
        let running = self.engine.list_active().await?;
        for id in ids {
            let item = self
                .downloads
                .get(*id)
                .await?
                .ok_or(QueueManagerError::DownloadNotFound(*id))?;
            if running.contains(id)
                || self.active.contains_key(id)
                || matches!(
                    item.status,
                    DownloadStatus::Completed
                        | DownloadStatus::Removed
                        | DownloadStatus::Downloading
                        | DownloadStatus::PreparingFile
                        | DownloadStatus::Retrying
                )
            {
                return Err(QueueManagerError::NotMovable(*id));
            }
        }
        self.queues.move_items(queue_id, ids).await?;
        for state in &mut self.queues_state {
            if state.queue.id != queue_id {
                state.items.retain(|id| !ids.contains(id));
            } else {
                for id in ids {
                    if !state.items.contains(id) {
                        state.items.push(*id);
                    }
                }
            }
        }
        for id in ids {
            self.suspended.remove(id);
        }
        self.empty_notified.remove(&queue_id);
        self.publish(QueueEvent::Changed { queue_id: None });
        self.admit_all().await
    }

    /// Moves `download_id` into `queue_id`, dropping any previous membership.
    async fn attach(
        &mut self,
        download_id: DownloadId,
        queue_id: QueueId,
    ) -> QueueManagerResult<()> {
        if self.queue_state(queue_id).is_none() {
            return Err(QueueManagerError::QueueNotFound(queue_id));
        }
        let previous: Vec<QueueId> = self
            .queues_state
            .iter_mut()
            .filter_map(|state| {
                if state.queue.id == queue_id {
                    return None;
                }
                let found = state.items.contains(&download_id);
                if found {
                    state.items.retain(|id| *id != download_id);
                    return Some(state.queue.id);
                }
                None
            })
            .collect();
        for id in previous {
            self.queues.remove_item(id, download_id).await?;
        }
        if let Some(state) = self.queue_state_mut(queue_id)
            && !state.items.contains(&download_id)
        {
            state.items.push(download_id);
        }
        Ok(())
    }

    /// Moves an `Added` row to `Queued` so admission may pick it up.
    async fn set_queued(&mut self, download_id: DownloadId) -> QueueManagerResult<()> {
        if !matches!(self.statuses.get(&download_id), Some(DownloadStatus::Added)) {
            return Ok(());
        }
        let item = self.engine.resume(download_id).await?;
        self.statuses.insert(download_id, item.status);
        Ok(())
    }

    async fn remove_item(&mut self, download_id: DownloadId) -> QueueManagerResult<bool> {
        self.suspended.remove(&download_id);
        let Some(queue_id) = self
            .queues_state
            .iter()
            .find_map(|state| state.items.contains(&download_id).then_some(state.queue.id))
        else {
            return Ok(false);
        };
        if let Some(state) = self.queue_state_mut(queue_id) {
            state.items.retain(|id| *id != download_id);
        }
        self.queues.remove_item(queue_id, download_id).await?;
        self.publish(QueueEvent::Changed {
            queue_id: Some(queue_id),
        });
        self.admit_all().await?;
        Ok(true)
    }

    async fn reorder(
        &mut self,
        queue_id: QueueId,
        order: Vec<DownloadId>,
    ) -> QueueManagerResult<()> {
        let Some(state) = self.queue_state_mut(queue_id) else {
            return Err(QueueManagerError::QueueNotFound(queue_id));
        };
        // Keep unknown ids out so a stale UI order cannot invent membership.
        let mut remaining = state.items.clone();
        let mut unique: Vec<DownloadId> = Vec::with_capacity(remaining.len());
        for id in order {
            if remaining.contains(&id) && !unique.contains(&id) {
                unique.push(id);
                remaining.retain(|item| *item != id);
            }
        }
        // Anything the caller omitted keeps its relative order at the tail.
        unique.extend(remaining);
        state.items = unique;
        self.persist_items(queue_id).await?;
        self.publish(QueueEvent::Changed {
            queue_id: Some(queue_id),
        });
        Ok(())
    }

    async fn set_queue(
        &mut self,
        queue: Queue,
        schedule: Option<QueueSchedule>,
    ) -> QueueManagerResult<Queue> {
        let id = queue.id;
        if self.queue_state(id).is_none() {
            return Err(QueueManagerError::QueueNotFound(id));
        }
        let schedule = schedule.map(QueueSchedule::canonicalized);
        let mut queue = queue;
        queue.name = queue.name.trim().to_owned();
        if queue.name.is_empty() {
            return Err(QueueManagerError::InvalidName);
        }
        queue.schedule_json = match &schedule {
            Some(schedule) => Some(schedule.to_json()?),
            None => None,
        };
        queue.updated_at = now_ms()?;
        self.queues.set_queue(&queue).await?;
        if let Some(state) = self.queue_state_mut(id) {
            state.queue = queue.clone();
            state.schedule = schedule;
        }
        self.recompute_edges(id)?;
        self.publish(QueueEvent::Changed { queue_id: Some(id) });
        self.admit_all().await?;
        Ok(queue)
    }

    async fn configure(
        &mut self,
        id: QueueId,
        name: String,
        limit: NonZeroU16,
        stop_on_empty: bool,
    ) -> QueueManagerResult<()> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(QueueManagerError::InvalidName);
        }
        let mut queue = self
            .queue_state(id)
            .ok_or(QueueManagerError::QueueNotFound(id))?
            .queue
            .clone();
        queue.name = name;
        queue.max_concurrent = limit;
        queue.stop_on_empty = stop_on_empty;
        queue.updated_at = now_ms()?;
        self.queues.set_queue(&queue).await?;
        if let Some(state) = self.queue_state_mut(id) {
            state.queue = queue;
        }
        self.publish(QueueEvent::Changed { queue_id: Some(id) });
        self.admit_all().await
    }

    async fn set_global_limit(&mut self, limit: NonZeroU16) -> QueueManagerResult<()> {
        self.global_limit = limit;
        let main_id = QueueId::MAIN;
        let needs_sync = self
            .queue_state(main_id)
            .is_some_and(|state| state.queue.max_concurrent != limit);
        if needs_sync {
            let queue = {
                let state = self
                    .queue_state_mut(main_id)
                    .ok_or(QueueManagerError::QueueNotFound(main_id))?;
                state.queue.max_concurrent = limit;
                state.queue.updated_at = now_ms()?;
                state.queue.clone()
            };
            self.queues.set_queue(&queue).await?;
        }
        self.admit_all().await
    }

    async fn set_active(&mut self, id: QueueId, active: bool) -> QueueManagerResult<()> {
        if self.queue_state(id).is_none() {
            return Err(QueueManagerError::QueueNotFound(id));
        }
        let was_active = self.active_queues.contains(&id);
        if active {
            self.active_queues.insert(id);
            let resume: Vec<_> = self
                .queue_state(id)
                .into_iter()
                .flat_map(|state| state.items.iter().copied())
                .filter(|download_id| self.suspended.contains(download_id))
                .collect();
            for download_id in resume {
                if matches!(
                    self.statuses.get(&download_id),
                    Some(DownloadStatus::Paused)
                ) {
                    let item = self.engine.resume(download_id).await?;
                    self.statuses.insert(download_id, item.status);
                }
                self.suspended.remove(&download_id);
            }
            // A queue start is an explicit retry signal: unblock its members.
            let members: HashSet<DownloadId> = self
                .queue_state(id)
                .map(|state| state.items.iter().copied().collect())
                .unwrap_or_default();
            self.blocked
                .retain(|download_id| !members.contains(download_id));
        } else {
            self.active_queues.remove(&id);
            self.pause_queue(id).await?;
        }
        if was_active != active {
            self.publish(QueueEvent::ActivityChanged {
                queue_id: id,
                active,
            });
        }
        self.admit_all().await?;
        Ok(())
    }

    async fn pause_queue(&mut self, id: QueueId) -> QueueManagerResult<()> {
        let running: Vec<DownloadId> = self
            .active
            .iter()
            .filter(|(_, owner)| **owner == id)
            .map(|(download_id, _)| *download_id)
            .collect();
        for download_id in running {
            let item = self.engine.pause(download_id).await?;
            self.statuses.insert(download_id, item.status);
            self.suspended.insert(download_id);
            self.active.remove(&download_id);
        }
        Ok(())
    }

    /// Rebuilds statuses, membership, `active`, and `blocked` from SQLite.
    async fn reconcile(&mut self) -> QueueManagerResult<()> {
        let stored = self.downloads.list().await?;
        self.statuses = stored.iter().map(|item| (item.id, item.status)).collect();
        // Live workers, not persisted status: a crashed process leaves rows
        // claiming `Downloading` with no job behind them, and adopting those as
        // active would permanently occupy concurrency slots.
        let running: HashSet<DownloadId> = self
            .engine
            .list_active()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

        self.queues_state.clear();
        for queue in self.queues.list_queues().await? {
            let schedule = QueueSchedule::from_json(queue.schedule_json.as_deref());
            let items = self
                .queues
                .list_items(queue.id)
                .await?
                .into_iter()
                .map(|item| item.download_id)
                .collect::<Vec<_>>();
            self.queues_state.push(QueueState {
                queue,
                items,
                schedule,
            });
        }
        let known: HashSet<QueueId> = self.queues_state.iter().map(|s| s.queue.id).collect();
        self.active_queues.retain(|id| known.contains(id));

        // Completed downloads leave the work list (ABDM semantics). Errors stay
        // as blockers so a restart cannot re-admit a download that just failed.
        let mut stale: Vec<(QueueId, DownloadId)> = Vec::new();
        let mut next_active: HashMap<DownloadId, QueueId> = HashMap::new();
        let mut blocked = HashSet::new();
        for state in &mut self.queues_state {
            let queue_id = state.queue.id;
            state
                .items
                .retain(|download_id| match self.statuses.get(download_id) {
                    Some(DownloadStatus::Completed) | None => {
                        stale.push((queue_id, *download_id));
                        false
                    }
                    Some(_) => true,
                });
            for download_id in &state.items {
                if running.contains(download_id) {
                    // Duplicate membership (the schema allows it) must not own
                    // two slots: the lowest queue id wins.
                    next_active.entry(*download_id).or_insert(queue_id);
                } else if matches!(self.statuses.get(download_id), Some(DownloadStatus::Error)) {
                    blocked.insert(*download_id);
                }
            }
        }
        for (queue_id, download_id) in stale {
            self.queues.remove_item(queue_id, download_id).await?;
        }
        for state in &self.queues_state {
            self.persist_items(state.queue.id).await?;
        }

        self.active = next_active;
        self.blocked = blocked;
        self.empty_notified.clear();
        let ids: Vec<QueueId> = self.queues_state.iter().map(|s| s.queue.id).collect();
        for id in ids {
            self.recompute_edges(id)?;
        }
        self.publish(QueueEvent::Changed { queue_id: None });
        self.admit_all().await?;
        Ok(())
    }

    /// Attaches unfinished downloads that belong to no queue to Main.
    async fn adopt_orphan_downloads(&mut self) -> QueueManagerResult<()> {
        let stored = self.downloads.list().await?;
        // Paused rows keep their status; membership only decides who owns them.
        let mut orphans: Vec<DownloadId> = stored
            .iter()
            .filter(|item| {
                !matches!(
                    item.status,
                    DownloadStatus::Completed | DownloadStatus::Removed
                )
            })
            .map(|item| item.id)
            .collect();
        if orphans.is_empty() {
            self.publish(QueueEvent::Changed { queue_id: None });
            return Ok(());
        }
        for state in &self.queues_state {
            orphans.retain(|id| !state.items.contains(id));
        }
        if orphans.is_empty() {
            self.publish(QueueEvent::Changed { queue_id: None });
            return Ok(());
        }
        let main = QueueId::MAIN;
        let mut position = self.queue_state(main).map_or(0, |state| {
            u32::try_from(state.items.len()).unwrap_or(u32::MAX)
        });
        for download_id in &orphans {
            if let Some(state) = self.queue_state_mut(main) {
                state.items.push(*download_id);
            }
            self.queues
                .add_item(QueueItem {
                    queue_id: main,
                    download_id: *download_id,
                    position,
                })
                .await?;
            position = position.saturating_add(1);
        }
        tracing::info!(
            adopted = orphans.len(),
            "attached pre-queue downloads to the main queue"
        );
        self.publish(QueueEvent::Changed { queue_id: None });
        Ok(())
    }

    async fn on_download_event(&mut self, event: DownloadEvent) -> QueueManagerResult<()> {
        match event {
            DownloadEvent::BootLoaded { .. } => self.reconcile().await?,
            DownloadEvent::DownloadAdded { item }
            | DownloadEvent::DownloadChanged { item }
            | DownloadEvent::DownloadPaused { item }
            | DownloadEvent::DownloadResumed { item } => self.on_status(item).await?,
            DownloadEvent::DownloadRemoved { id } => {
                let owned = self.active.remove(&id).is_some();
                self.blocked.remove(&id);
                self.statuses.remove(&id);
                let member = self
                    .queues_state
                    .iter()
                    .any(|state| state.items.contains(&id));
                if member {
                    self.remove_item(id).await?;
                } else if owned {
                    self.admit_all().await?;
                }
            }
            DownloadEvent::DownloadFailed { id, .. } => {
                self.blocked.insert(id);
                self.active.remove(&id);
                self.admit_all().await?;
            }
            DownloadEvent::DownloadProgress { .. } | DownloadEvent::Shutdown => {}
        }
        Ok(())
    }

    async fn on_status(&mut self, item: DownloadItem) -> QueueManagerResult<()> {
        let id = item.id;
        let previous_owner = self.active.remove(&id);
        self.statuses.insert(id, item.status);
        match item.status {
            DownloadStatus::Completed => {
                self.suspended.remove(&id);
                self.blocked.remove(&id);
                let member = self
                    .queues_state
                    .iter()
                    .any(|state| state.items.contains(&id));
                if member {
                    self.remove_item(id).await?;
                } else if previous_owner.is_some() {
                    self.admit_all().await?;
                }
            }
            DownloadStatus::Paused | DownloadStatus::Error | DownloadStatus::Removed => {
                if matches!(item.status, DownloadStatus::Error) {
                    self.blocked.insert(id);
                }
                // Stopping or failing frees the slot; filling it is admission's
                // decision, so every exit path funnels through one call.
                if previous_owner.is_some() {
                    self.admit_all().await?;
                }
            }
            DownloadStatus::Queued | DownloadStatus::Added => {
                self.suspended.remove(&id);
                // Leaving `Error` clears the block so the user's retry runs.
                self.blocked.remove(&id);
                // Keep the slot if we already owned it; a resume issued outside
                // admission must not free a slot the queue still holds.
                if let Some(owner) = previous_owner {
                    self.active.insert(id, owner);
                }
                self.admit_all().await?;
            }
            DownloadStatus::Downloading
            | DownloadStatus::Retrying
            | DownloadStatus::PreparingFile => {
                // Adopt a download started outside admission so the slot count
                // stays balanced with reality.
                let owner = previous_owner.unwrap_or_else(|| self.owner_of(id));
                self.active.insert(id, owner);
            }
        }
        Ok(())
    }

    /// Returns the owning queue for `download_id`, falling back to Main.
    fn owner_of(&self, download_id: DownloadId) -> QueueId {
        self.queues_state
            .iter()
            .find(|state| state.items.contains(&download_id))
            .map(|state| state.queue.id)
            .unwrap_or(QueueId::MAIN)
    }

    /// Fills every active queue up to its budget and the global ceiling.
    async fn admit_all(&mut self) -> QueueManagerResult<()> {
        for index in 0..self.queues_state.len() {
            let queue_id = self.queues_state[index].queue.id;
            if !self.active_queues.contains(&queue_id) {
                continue;
            }
            loop {
                if self.active.len() >= usize::from(self.global_limit.get()) {
                    break;
                }
                let budget = usize::from(self.queues_state[index].queue.max_concurrent.get());
                let running = self
                    .active
                    .values()
                    .filter(|owner| **owner == queue_id)
                    .count();
                if running >= budget {
                    break;
                }
                let Some(next) = self.next_admissible(queue_id) else {
                    break;
                };
                self.start_now(next, queue_id).await;
            }
        }
        self.check_empty();
        Ok(())
    }

    /// Picks the first `Queued`, unblocked member that is not already running.
    fn next_admissible(&self, queue_id: QueueId) -> Option<DownloadId> {
        let state = self.queue_state(queue_id)?;
        state.items.iter().copied().find(|download_id| {
            !self.active.contains_key(download_id)
                && !self.blocked.contains(download_id)
                && matches!(self.statuses.get(download_id), Some(DownloadStatus::Queued))
        })
    }

    /// Records the slot before asking the engine to start, so an echoed status
    /// event cannot admit the same download twice.
    async fn start_now(&mut self, download_id: DownloadId, queue_id: QueueId) {
        self.active.insert(download_id, queue_id);
        match self.engine.start(download_id).await {
            Ok(item) => {
                self.statuses.insert(download_id, item.status);
            }
            Err(error) => {
                // The slot must not stay reserved for a download that never
                // started; block it so admission does not spin on the same row.
                tracing::warn!(%download_id, %error, "queue admission start failed");
                self.active.remove(&download_id);
                self.blocked.insert(download_id);
            }
        }
    }

    fn check_empty(&mut self) {
        let emptied: Vec<QueueId> = self
            .queues_state
            .iter()
            .filter(|state| {
                self.active_queues.contains(&state.queue.id)
                    && state.items.is_empty()
                    && !self.active.values().any(|owner| *owner == state.queue.id)
            })
            .map(|state| state.queue.id)
            .collect();
        for queue_id in emptied {
            let stop = self
                .queue_state(queue_id)
                .is_some_and(|state| state.queue.stop_on_empty);
            // Edge, not level: completion actions (notification, power actions)
            // hang off this event, so repeat drains must stay silent.
            if self.empty_notified.insert(queue_id) {
                self.publish(QueueEvent::BecameEmpty { queue_id });
            }
            if stop {
                self.active_queues.remove(&queue_id);
                self.publish(QueueEvent::ActivityChanged {
                    queue_id,
                    active: false,
                });
            }
        }
    }

    async fn persist_items(&self, queue_id: QueueId) -> QueueManagerResult<()> {
        let Some(state) = self.queue_state(queue_id) else {
            return Ok(());
        };
        let items = state
            .items
            .iter()
            .enumerate()
            .map(|(position, download_id)| QueueItem {
                queue_id,
                download_id: *download_id,
                position: u32::try_from(position).unwrap_or(u32::MAX),
            })
            .collect::<Vec<_>>();
        self.queues.set_items(queue_id, &items).await?;
        Ok(())
    }

    async fn on_schedule_tick(&mut self) -> QueueManagerResult<()> {
        let now = local_now(Local::now());
        let mut edges: Vec<(NaiveDateTime, QueueId, ScheduleEdge)> = Vec::new();
        for state in &self.queues_state {
            let queue_id = state.queue.id;
            if let Some(edge) = self.next_start.get(&queue_id).filter(|edge| **edge <= now) {
                edges.push((*edge, queue_id, ScheduleEdge::Start));
            }
            if let Some(edge) = self.next_stop.get(&queue_id).filter(|edge| **edge <= now) {
                edges.push((*edge, queue_id, ScheduleEdge::Stop));
            }
        }
        if edges.is_empty() {
            return Ok(());
        }
        // Both edges can pass during one suspend or a long tick gap. Applying
        // the most recent one last makes the newest intent win, so waking up
        // after a stop edge never leaves the queue running.
        edges.sort_by_key(|(at, queue_id, edge)| (*at, *queue_id, edge.order()));
        for (_, queue_id, edge) in edges {
            let was_active = self.active_queues.contains(&queue_id);
            match edge {
                ScheduleEdge::Start => {
                    self.publish(QueueEvent::StartTimeReached {
                        queue_id,
                        was_active,
                    });
                    if let Err(error) = self.set_active(queue_id, true).await {
                        tracing::warn!(%queue_id, %error, "scheduled queue start failed");
                    }
                }
                ScheduleEdge::Stop => {
                    self.publish(QueueEvent::EndTimeReached {
                        queue_id,
                        was_active,
                    });
                    if let Err(error) = self.set_active(queue_id, false).await {
                        tracing::warn!(%queue_id, %error, "scheduled queue stop failed");
                    }
                }
            }
            self.recompute_edges(queue_id)?;
        }
        Ok(())
    }

    fn recompute_edges(&mut self, queue_id: QueueId) -> QueueManagerResult<()> {
        let schedule = self
            .queue_state(queue_id)
            .and_then(|state| state.schedule.clone());
        let Some(schedule) = schedule else {
            self.next_start.remove(&queue_id);
            self.next_stop.remove(&queue_id);
            return Ok(());
        };
        let now = local_now(Local::now());
        match schedule.next_start_after(now)? {
            Some(edge) => {
                self.next_start.insert(queue_id, edge);
            }
            None => {
                self.next_start.remove(&queue_id);
            }
        }
        match schedule.next_stop_after(now)? {
            Some(edge) => {
                self.next_stop.insert(queue_id, edge);
            }
            None => {
                self.next_stop.remove(&queue_id);
            }
        }
        Ok(())
    }

    /// Publishes one edge event and the current snapshot.
    ///
    /// Every state change already emits an event, so refreshing the snapshot
    /// here keeps both channels consistent without extra call sites.
    fn publish(&self, event: QueueEvent) {
        if let Err(error) = self.events.send(event) {
            tracing::trace!(?error, "queue event had no subscribers");
        }
        // No receivers is normal when no view is mounted.
        if self.snapshots.send(self.snapshot()).is_err() {
            tracing::trace!("queue snapshot had no subscribers");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ScheduleEdge {
    Start,
    Stop,
}

impl ScheduleEdge {
    const fn order(self) -> u8 {
        match self {
            Self::Start => 0,
            Self::Stop => 1,
        }
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
    use std::collections::BTreeMap;
    use std::num::NonZeroU16;
    use std::path::PathBuf;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::sync::broadcast;

    // Explicit imports (see other modules): a `use super::*` glob would shadow
    // the built-in `#[test]` attribute with GPUI's `test` macro.
    use super::{
        DEFAULT_QUEUE_EVENT_BUFFER, QueueManagerError, QueueManagerHandle, QueueManagerOptions,
        QueueManagerResult,
    };
    use crate::db::{self, DbError, DownloadRepository, PartRepository, QueueRepository};
    use crate::download::{
        Bytes, DownloadEvent, DownloadId, DownloadItem, DownloadKind, DownloadManagerHandle,
        DownloadManagerOptions, DownloadStatus, QueueId,
    };
    use crate::queue::model::Queue;
    use crate::queue::schedule::QueueSchedule;

    struct Harness {
        _dir: TempDir,
        downloads: DownloadRepository,
        queues: QueueRepository,
        engine: DownloadManagerHandle,
        engine_events: broadcast::Sender<DownloadEvent>,
    }

    async fn harness() -> QueueManagerResult<Harness> {
        let dir = tempfile::tempdir()
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;
        let db_path = dir.path().join("raijin-queue-test.sqlite");
        let pool = db::bootstrap(&format!("sqlite://{}", db_path.display())).await?;
        let downloads = DownloadRepository::new(pool.clone());
        let queues = QueueRepository::new(pool.clone());
        let (engine, _task, _rx) = DownloadManagerHandle::spawn_with_events(
            downloads.clone(),
            PartRepository::new(pool),
            DownloadManagerOptions {
                command_buffer: 16,
                event_buffer: 64,
            },
        );
        let (engine_events, _) = broadcast::channel(64);
        Ok(Harness {
            _dir: dir,
            downloads,
            queues,
            engine,
            engine_events,
        })
    }

    async fn spawn_queue(
        harness: &Harness,
        global_limit: u16,
    ) -> QueueManagerResult<(
        QueueManagerHandle,
        tokio::task::JoinHandle<QueueManagerResult<()>>,
    )> {
        Ok(QueueManagerHandle::spawn(
            harness.downloads.clone(),
            harness.queues.clone(),
            harness.engine.clone(),
            harness.engine_events.subscribe(),
            QueueManagerOptions {
                command_buffer: 16,
                event_buffer: DEFAULT_QUEUE_EVENT_BUFFER,
                global_limit: NonZeroU16::new(global_limit).unwrap_or(NonZeroU16::MIN),
            },
        ))
    }

    /// Waits until the engine reports a live worker for `download_id`.
    async fn wait_for_active(harness: &Harness, download_id: DownloadId) -> QueueManagerResult<()> {
        for _ in 0..120 {
            if harness.engine.list_active().await?.contains(&download_id) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Err(QueueManagerError::DownloadNotFound(download_id))
    }

    /// Inserts a row pointing at a dead URL so tests never touch the network.
    async fn seed_download(harness: &Harness, id: i64, status: DownloadStatus) {
        seed_download_with_url(
            harness,
            id,
            status,
            format!("https://127.0.0.1:1/file-{id}.bin"),
        )
        .await;
    }

    /// Inserts a row with an explicit URL.
    async fn seed_download_with_url(
        harness: &Harness,
        id: i64,
        status: DownloadStatus,
        url: String,
    ) {
        let item = DownloadItem {
            id: DownloadId::new(id),
            kind: DownloadKind::Http,
            url,
            download_page: None,
            headers: BTreeMap::new(),
            file_name: format!("file-{id}.bin"),
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
        };
        let _ = harness.downloads.add(&item).await;
    }

    fn queue(id: i64, budget: u16) -> Queue {
        Queue {
            id: QueueId::new(id),
            name: format!("Q{id}"),
            max_concurrent: NonZeroU16::new(budget).unwrap_or(NonZeroU16::MIN),
            stop_on_empty: false,
            schedule_json: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    async fn stop(
        handle: &QueueManagerHandle,
        task: tokio::task::JoinHandle<QueueManagerResult<()>>,
    ) -> QueueManagerResult<()> {
        handle.shutdown().await?;
        task.await.map_err(|_| QueueManagerError::Stopped)??;
        Ok(())
    }

    async fn find(
        handle: &QueueManagerHandle,
        id: QueueId,
    ) -> QueueManagerResult<super::QueueSnapshot> {
        handle
            .list()
            .await?
            .into_iter()
            .find(|snapshot| snapshot.queue.id == id)
            .ok_or(QueueManagerError::QueueNotFound(id))
    }

    #[tokio::test]
    async fn boot_should_create_main_queue_and_activate_it() -> QueueManagerResult<()> {
        let harness = harness().await?;
        let (handle, task) = spawn_queue(&harness, 3).await?;

        let main = find(&handle, QueueId::MAIN).await?;

        assert_eq!(main.queue.name, "Main");
        assert_eq!(
            main.queue.max_concurrent.get(),
            3,
            "Main mirrors the global ceiling on creation"
        );
        assert!(main.active, "the main queue resumes on boot");
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn enqueue_should_persist_order_and_move_between_queues() -> QueueManagerResult<()> {
        let harness = harness().await?;
        seed_download(&harness, 1, DownloadStatus::Paused).await;
        seed_download(&harness, 2, DownloadStatus::Paused).await;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        let other = handle.create("Night").await?;

        handle
            .enqueue(QueueId::MAIN, [DownloadId::new(1), DownloadId::new(2)])
            .await?;
        handle.enqueue(other, [DownloadId::new(2)]).await?;

        assert_eq!(
            find(&handle, QueueId::MAIN).await?.items,
            vec![DownloadId::new(1)]
        );
        assert_eq!(find(&handle, other).await?.items, vec![DownloadId::new(2)]);
        let stored = harness.queues.list_items(other).await?;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].position, 0);
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn enqueue_should_reject_unknown_queue_and_download() -> QueueManagerResult<()> {
        let harness = harness().await?;
        let (handle, task) = spawn_queue(&harness, 2).await?;

        assert!(matches!(
            handle.enqueue(QueueId::new(42), [DownloadId::new(1)]).await,
            Err(QueueManagerError::QueueNotFound(_))
        ));
        assert!(matches!(
            handle.enqueue(QueueId::MAIN, [DownloadId::new(7)]).await,
            Err(QueueManagerError::DownloadNotFound(_))
        ));
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn reorder_should_apply_the_given_order_and_keep_omitted_items_at_the_tail()
    -> QueueManagerResult<()> {
        let harness = harness().await?;
        for id in 1..=3 {
            seed_download(&harness, id, DownloadStatus::Paused).await;
        }
        let (handle, task) = spawn_queue(&harness, 2).await?;
        handle
            .enqueue(
                QueueId::MAIN,
                [DownloadId::new(1), DownloadId::new(2), DownloadId::new(3)],
            )
            .await?;

        handle
            .reorder(
                QueueId::MAIN,
                vec![
                    DownloadId::new(3),
                    // Unknown ids are ignored, never invented.
                    DownloadId::new(99),
                    DownloadId::new(1),
                ],
            )
            .await?;

        assert_eq!(
            find(&handle, QueueId::MAIN).await?.items,
            vec![DownloadId::new(3), DownloadId::new(1), DownloadId::new(2)]
        );
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn completed_download_should_leave_the_work_list() -> QueueManagerResult<()> {
        let harness = harness().await?;
        seed_download(&harness, 1, DownloadStatus::Paused).await;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        handle.enqueue(QueueId::MAIN, [DownloadId::new(1)]).await?;

        let mut completed = harness
            .downloads
            .get(DownloadId::new(1))
            .await?
            .ok_or(QueueManagerError::DownloadNotFound(DownloadId::new(1)))?;
        completed.status = DownloadStatus::Completed;
        harness.downloads.update(&completed).await?;
        // The actor learns about completion from the engine event stream.
        let _ = harness
            .engine_events
            .send(DownloadEvent::DownloadChanged { item: completed });
        wait_until_empty(&handle, QueueId::MAIN).await?;

        let stored = harness.queues.list_items(QueueId::MAIN).await?;
        assert!(stored.is_empty(), "completion deletes the membership row");
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn boot_should_block_already_failed_members_instead_of_restarting_them()
    -> QueueManagerResult<()> {
        let harness = harness().await?;
        seed_download(&harness, 1, DownloadStatus::Error).await;
        let fresh = harness
            .queues
            .ensure_default_queue(1, NonZeroU16::new(2).unwrap_or(NonZeroU16::MIN))
            .await?;
        harness
            .queues
            .add_item(crate::queue::QueueItem {
                queue_id: fresh,
                download_id: DownloadId::new(1),
                position: 0,
            })
            .await?;

        let (handle, task) = spawn_queue(&harness, 2).await?;
        tokio::time::sleep(Duration::from_millis(250)).await;

        // A failed row that survived a restart must not be re-admitted: a spin
        // would rewrite it (bumping `updated_at`) on every pass.
        let stored = harness
            .downloads
            .get(DownloadId::new(1))
            .await?
            .ok_or(QueueManagerError::DownloadNotFound(DownloadId::new(1)))?;
        assert_eq!(stored.status, DownloadStatus::Error);
        assert_eq!(
            stored.updated_at, 1,
            "a blocked download must stay untouched"
        );
        assert_eq!(
            find(&handle, QueueId::MAIN).await?.items,
            vec![DownloadId::new(1)]
        );
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn queue_stop_should_pause_running_members_and_release_slots() -> QueueManagerResult<()> {
        let harness = harness().await?;
        seed_download(&harness, 1, DownloadStatus::Queued).await;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        handle.enqueue(QueueId::MAIN, [DownloadId::new(1)]).await?;
        // Let admission start the job (it fails against the dead port, which is
        // fine: the point is that Stop leaves nothing running).
        tokio::time::sleep(Duration::from_millis(120)).await;

        handle.stop(QueueId::MAIN).await?;

        assert!(!find(&handle, QueueId::MAIN).await?.active);
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn deleting_a_queue_should_remove_members_and_reject_main() -> QueueManagerResult<()> {
        let harness = harness().await?;
        seed_download(&harness, 1, DownloadStatus::Paused).await;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        let extra = handle.create("Temp").await?;
        handle.enqueue(extra, [DownloadId::new(1)]).await?;

        assert!(handle.delete(extra).await?);
        assert!(matches!(
            handle.delete(QueueId::MAIN).await,
            Err(QueueManagerError::MainQueueNotDeletable)
        ));

        let queues = handle.list().await?;
        assert!(
            queues.iter().all(|snapshot| snapshot.queue.id != extra),
            "deleted queue must disappear from the snapshot"
        );
        let stored = harness.queues.list_items(extra).await?;
        assert!(stored.is_empty(), "membership cascades on delete");
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn move_should_preserve_status_order_and_validate_whole_batch() -> QueueManagerResult<()>
    {
        let harness = harness().await?;
        seed_download(&harness, 1, DownloadStatus::Paused).await;
        seed_download(&harness, 2, DownloadStatus::Added).await;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        let destination = handle.create("  Later  ").await?;
        assert_eq!(find(&handle, destination).await?.queue.name, "Later");
        assert!(matches!(
            handle.create(" ").await,
            Err(QueueManagerError::InvalidName)
        ));
        handle
            .move_downloads(destination, vec![DownloadId::new(1), DownloadId::new(2)])
            .await?;
        assert_eq!(
            find(&handle, destination).await?.items,
            vec![DownloadId::new(1), DownloadId::new(2)]
        );
        assert_eq!(
            harness
                .downloads
                .get(DownloadId::new(1))
                .await?
                .expect("row")
                .status,
            DownloadStatus::Paused
        );
        assert_eq!(
            harness
                .downloads
                .get(DownloadId::new(2))
                .await?
                .expect("row")
                .status,
            DownloadStatus::Added
        );
        let result = handle
            .move_downloads(
                QueueId::MAIN,
                vec![DownloadId::new(1), DownloadId::new(999)],
            )
            .await;
        assert!(matches!(
            result,
            Err(QueueManagerError::DownloadNotFound(_))
        ));
        assert_eq!(
            harness
                .queues
                .queue_ids_for_download(DownloadId::new(1))
                .await?,
            vec![destination]
        );
        assert!(
            harness
                .queues
                .move_items(QueueId::MAIN, &[DownloadId::new(1), DownloadId::new(999)])
                .await
                .is_err()
        );
        assert_eq!(
            harness
                .queues
                .queue_ids_for_download(DownloadId::new(1))
                .await?,
            vec![destination]
        );
        handle
            .move_downloads(destination, vec![DownloadId::new(1)])
            .await?;
        assert_eq!(
            find(&handle, destination).await?.items,
            vec![DownloadId::new(1), DownloadId::new(2)]
        );
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn configure_should_preserve_opaque_schedule_and_persist_properties()
    -> QueueManagerResult<()> {
        let harness = harness().await?;
        let (handle, task) = spawn_queue(&harness, 3).await?;
        let id = handle.create("Later").await?;
        let mut stored = harness.queues.get_queue(id).await?.expect("queue");
        stored.schedule_json = Some("invalid legacy schedule".to_owned());
        harness.queues.set_queue(&stored).await?;
        handle.reconcile().await?;
        let limit = NonZeroU16::new(1).expect("nonzero");
        handle
            .configure(id, " Renamed ".to_owned(), limit, true)
            .await?;
        let saved = harness.queues.get_queue(id).await?.expect("queue");
        assert_eq!(saved.name, "Renamed");
        assert_eq!(saved.max_concurrent, limit);
        assert!(saved.stop_on_empty);
        assert_eq!(saved.schedule_json, stored.schedule_json);
        assert!(matches!(
            handle.configure(id, " ".to_owned(), limit, false).await,
            Err(QueueManagerError::InvalidName)
        ));
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn delete_should_transfer_paused_members_to_main_and_keep_downloads()
    -> QueueManagerResult<()> {
        let harness = harness().await?;
        seed_download(&harness, 1, DownloadStatus::Paused).await;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        handle.stop(QueueId::MAIN).await?;
        let id = handle.create("Later").await?;
        handle.move_downloads(id, vec![DownloadId::new(1)]).await?;
        harness.engine.resume(DownloadId::new(1)).await?;
        handle.delete(id).await?;
        assert_eq!(
            harness
                .queues
                .queue_ids_for_download(DownloadId::new(1))
                .await?,
            vec![QueueId::MAIN]
        );
        assert_eq!(
            harness
                .downloads
                .get(DownloadId::new(1))
                .await?
                .expect("download kept")
                .status,
            DownloadStatus::Paused
        );
        assert!(harness.engine.list_active().await?.is_empty());
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn live_stop_start_should_resume_only_queue_suspended_downloads() -> QueueManagerResult<()>
    {
        let harness = harness().await?;
        let addr = black_hole_server()
            .await
            .map_err(|error| QueueManagerError::Engine(error.into()))?;
        let url = format!("http://{addr}/file.bin");
        seed_download_with_url(&harness, 1, DownloadStatus::Paused, url.clone()).await;
        seed_download_with_url(&harness, 2, DownloadStatus::Paused, url).await;
        let (handle, task) = QueueManagerHandle::spawn(
            harness.downloads.clone(),
            harness.queues.clone(),
            harness.engine.clone(),
            harness.engine.subscribe(),
            QueueManagerOptions::default(),
        );
        let id = handle.create("Later").await?;
        handle
            .move_downloads(id, vec![DownloadId::new(1), DownloadId::new(2)])
            .await?;
        harness.engine.resume(DownloadId::new(1)).await?;
        assert!(!find(&handle, id).await?.active);
        assert!(
            harness.engine.list_active().await?.is_empty(),
            "resume must not start a stopped queue"
        );
        handle.start(id).await?;
        wait_for_active(&harness, DownloadId::new(1)).await?;
        assert!(matches!(
            handle
                .move_downloads(QueueId::MAIN, vec![DownloadId::new(1)])
                .await,
            Err(QueueManagerError::NotMovable(_))
        ));
        handle.stop(id).await?;
        handle.start(id).await?;
        wait_for_active(&harness, DownloadId::new(1)).await?;
        assert_eq!(
            harness
                .downloads
                .get(DownloadId::new(2))
                .await?
                .expect("paused row")
                .status,
            DownloadStatus::Paused
        );
        handle.stop(id).await?;
        stop(&handle, task).await?;
        Ok(())
    }

    #[tokio::test]
    async fn global_limit_should_span_queues_and_lower_without_pausing_workers()
    -> QueueManagerResult<()> {
        let harness = harness().await?;
        let addr = black_hole_server()
            .await
            .map_err(|error| QueueManagerError::Engine(error.into()))?;
        for id in 1..=3 {
            seed_download_with_url(
                &harness,
                id,
                DownloadStatus::Paused,
                format!("http://{addr}/{id}"),
            )
            .await;
        }
        let (handle, task) = QueueManagerHandle::spawn(
            harness.downloads.clone(),
            harness.queues.clone(),
            harness.engine.clone(),
            harness.engine.subscribe(),
            QueueManagerOptions {
                global_limit: NonZeroU16::new(2).expect("limit"),
                ..Default::default()
            },
        );
        let custom = handle.create("Later").await?;
        handle
            .configure(
                custom,
                "Later".to_owned(),
                NonZeroU16::new(1).expect("limit"),
                false,
            )
            .await?;
        handle
            .move_downloads(custom, vec![DownloadId::new(1), DownloadId::new(2)])
            .await?;
        handle.start(custom).await?;
        for id in 1..=3 {
            harness.engine.resume(DownloadId::new(id)).await?;
        }
        wait_for_active(&harness, DownloadId::new(1)).await?;
        wait_for_active(&harness, DownloadId::new(3)).await?;
        assert_eq!(harness.engine.list_active().await?.len(), 2);
        assert!(
            !harness
                .engine
                .list_active()
                .await?
                .contains(&DownloadId::new(2))
        );
        handle
            .set_global_limit(NonZeroU16::new(1).expect("limit"))
            .await?;
        assert_eq!(
            harness.engine.list_active().await?.len(),
            2,
            "lowering the limit must not stop workers"
        );
        harness.engine.pause(DownloadId::new(1)).await?;
        assert!(
            !harness
                .engine
                .list_active()
                .await?
                .contains(&DownloadId::new(2))
        );
        harness.engine.pause(DownloadId::new(3)).await?;
        wait_for_active(&harness, DownloadId::new(2)).await?;
        handle.stop(custom).await?;
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn restart_should_keep_membership_order_and_leave_custom_queues_stopped()
    -> QueueManagerResult<()> {
        let harness = harness().await?;
        seed_download(&harness, 1, DownloadStatus::Paused).await;
        seed_download(&harness, 2, DownloadStatus::Paused).await;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        let id = handle.create("Later").await?;
        handle
            .move_downloads(id, vec![DownloadId::new(2), DownloadId::new(1)])
            .await?;
        handle.start(id).await?;
        stop(&handle, task).await?;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        let snapshot = find(&handle, id).await?;
        assert_eq!(snapshot.items, vec![DownloadId::new(2), DownloadId::new(1)]);
        assert!(!snapshot.active);
        assert!(find(&handle, QueueId::MAIN).await?.active);
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn schedule_write_should_be_strict_and_clearable() -> QueueManagerResult<()> {
        let harness = harness().await?;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        let mut main = find(&handle, QueueId::MAIN).await?.queue;

        let mut schedule = QueueSchedule::default();
        // A fresh install must never start anything on its own.
        assert!(!schedule.is_active());
        schedule.start_enabled = true;
        main = handle.set_queue(main.clone(), Some(schedule)).await?;
        assert!(main.schedule_json.is_some());

        let empty = QueueSchedule {
            days: Vec::new(),
            ..QueueSchedule::default()
        };
        assert!(matches!(
            handle.set_queue(main.clone(), Some(empty)).await,
            Err(QueueManagerError::Schedule(_))
        ));

        main = handle.set_queue(main, None).await?;
        assert_eq!(main.schedule_json, None);
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn global_limit_should_resize_the_main_budget() -> QueueManagerResult<()> {
        let harness = harness().await?;
        let (handle, task) = spawn_queue(&harness, 3).await?;

        handle
            .set_global_limit(NonZeroU16::new(5).unwrap_or(NonZeroU16::MIN))
            .await?;

        assert_eq!(
            find(&handle, QueueId::MAIN)
                .await?
                .queue
                .max_concurrent
                .get(),
            5
        );
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn reconcile_should_rebuild_active_and_blocked_from_sqlite() -> QueueManagerResult<()> {
        let harness = harness().await?;
        seed_download(&harness, 1, DownloadStatus::Queued).await;
        seed_download(&harness, 2, DownloadStatus::Error).await;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        let extra = handle.create("Night").await?;
        handle
            .enqueue(extra, [DownloadId::new(1), DownloadId::new(2)])
            .await?;

        // Reconcile reads durable state only; membership must survive.
        handle.reconcile().await?;

        assert_eq!(
            find(&handle, extra).await?.items,
            vec![DownloadId::new(1), DownloadId::new(2)]
        );
        stop(&handle, task).await
    }

    /// Listener that accepts connections and never answers, so a started job
    /// stays pending instead of failing fast.
    async fn black_hole_server() -> std::io::Result<std::net::SocketAddr> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let held = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let holder = std::sync::Arc::clone(&held);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                holder.lock().await.push(stream);
            }
        });
        Ok(addr)
    }

    #[tokio::test]
    async fn admission_should_respect_the_per_queue_budget_and_start_the_next_on_completion()
    -> QueueManagerResult<()> {
        let harness = harness().await?;
        let addr = black_hole_server()
            .await
            .map_err(|error| QueueManagerError::Engine(error.into()))?;
        let base = format!("http://{addr}");
        for id in 1..=3 {
            seed_download_with_url(
                &harness,
                id,
                DownloadStatus::Queued,
                format!("{base}/file-{id}.bin"),
            )
            .await;
        }
        // One slot per queue, so admission must serialize.
        let (handle, task) = spawn_queue(&harness, 1).await?;
        handle
            .enqueue(
                QueueId::MAIN,
                [DownloadId::new(1), DownloadId::new(2), DownloadId::new(3)],
            )
            .await?;
        tokio::time::sleep(Duration::from_millis(400)).await;

        let running = harness.engine.list_active().await?;
        assert_eq!(
            running,
            vec![DownloadId::new(1)],
            "a budget of one must admit exactly the head of the queue"
        );

        // Completion frees the slot; the next member must take it.
        let mut done = harness
            .downloads
            .get(DownloadId::new(1))
            .await?
            .ok_or(QueueManagerError::DownloadNotFound(DownloadId::new(1)))?;
        done.status = DownloadStatus::Completed;
        harness.downloads.update(&done).await?;
        let _ = harness
            .engine_events
            .send(DownloadEvent::DownloadChanged { item: done });
        // The probe blocks on the silent socket, so a live worker is the only
        // observable proof that admission ran. Job 1 may linger locally because
        // the test fakes its completion in SQLite.
        wait_for_active(&harness, DownloadId::new(2)).await?;

        // The finished member left the work list and the tail is still waiting,
        // so the freed slot went to exactly one download.
        assert_eq!(
            find(&handle, QueueId::MAIN).await?.items,
            vec![DownloadId::new(2), DownloadId::new(3)]
        );
        let third = harness
            .downloads
            .get(DownloadId::new(3))
            .await?
            .ok_or(QueueManagerError::DownloadNotFound(DownloadId::new(3)))?;
        assert_eq!(
            third.status,
            DownloadStatus::Queued,
            "the budget of one must keep the third member queued"
        );
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn boot_should_adopt_downloads_created_before_queue_admission_existed()
    -> QueueManagerResult<()> {
        let harness = harness().await?;
        // No membership row: this is the pre-M3 state of an existing install.
        seed_download(&harness, 1, DownloadStatus::Paused).await;
        seed_download(&harness, 2, DownloadStatus::Completed).await;

        let (handle, task) = spawn_queue(&harness, 2).await?;

        // Paused work is adopted so a later resume has an owner; finished work
        // stays out of the queue because it is not work.
        assert_eq!(
            find(&handle, QueueId::MAIN).await?.items,
            vec![DownloadId::new(1)]
        );
        let stored = harness.queues.list_items(QueueId::MAIN).await?;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].download_id, DownloadId::new(1));
        assert_eq!(stored[0].position, 0);
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn adopted_download_should_resume_once_the_queue_reaches_it() -> QueueManagerResult<()> {
        let harness = harness().await?;
        let addr = black_hole_server()
            .await
            .map_err(|error| QueueManagerError::Engine(error.into()))?;
        seed_download_with_url(
            &harness,
            1,
            DownloadStatus::Queued,
            format!("http://{addr}/orphan.bin"),
        )
        .await;
        let (handle, task) = spawn_queue(&harness, 2).await?;

        // Adoption must be enough for admission to pick the row up, otherwise
        // pre-M3 downloads would silently stop working.
        wait_for_active(&harness, DownloadId::new(1)).await?;
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn stop_on_empty_should_deactivate_only_when_the_flag_is_set() -> QueueManagerResult<()> {
        let harness = harness().await?;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        let extra = handle.create("Batch").await?;

        // Default is off: an empty active queue keeps accepting work.
        handle.start(extra).await?;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            find(&handle, extra).await?.active,
            "stop_on_empty defaults to false so new work still runs"
        );

        let mut queue_row = find(&handle, extra).await?.queue;
        queue_row.stop_on_empty = true;
        let schedule = QueueSchedule::default();
        handle.set_queue(queue_row, Some(schedule)).await?;
        handle.stop(extra).await?;
        handle.start(extra).await?;
        tokio::time::sleep(Duration::from_millis(120)).await;

        assert!(
            !find(&handle, extra).await?.active,
            "an empty queue with stop_on_empty must deactivate itself"
        );
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn became_empty_should_fire_once_per_drain() -> QueueManagerResult<()> {
        let harness = harness().await?;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        let extra = handle.create("Batch").await?;
        let mut events = handle.subscribe();

        // Activating an idle queue drains it immediately; further admission
        // passes must stay silent so completion actions cannot fire twice.
        handle.start(extra).await?;
        handle.start(extra).await?;
        handle.stop(extra).await?;
        handle.start(extra).await?;
        let mut fires = 0;
        for _ in 0..10 {
            while let Ok(event) = events.try_recv() {
                if matches!(event, super::QueueEvent::BecameEmpty { queue_id } if queue_id == extra)
                {
                    fires += 1;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(fires, 1, "BecameEmpty is an edge, not a level");

        // New work resets the edge, so the next drain reports again.
        seed_download(&harness, 1, DownloadStatus::Error).await;
        handle.enqueue(extra, [DownloadId::new(1)]).await?;
        let mut after_work = 0;
        for _ in 0..10 {
            while let Ok(event) = events.try_recv() {
                if matches!(event, super::QueueEvent::BecameEmpty { queue_id } if queue_id == extra)
                {
                    after_work += 1;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(after_work, 0, "a blocked member keeps the queue non-empty");
        stop(&handle, task).await
    }

    #[tokio::test]
    async fn snapshot_watch_should_publish_after_every_change() -> QueueManagerResult<()> {
        let harness = harness().await?;
        seed_download(&harness, 1, DownloadStatus::Paused).await;
        let (handle, task) = spawn_queue(&harness, 2).await?;
        let mut snapshots = handle.snapshots();

        handle.enqueue(QueueId::MAIN, [DownloadId::new(1)]).await?;
        snapshots
            .changed()
            .await
            .map_err(|_| QueueManagerError::Stopped)?;

        let current = snapshots.borrow().clone();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].items, vec![DownloadId::new(1)]);
        // The handle exposes the same view without awaiting.
        assert_eq!(handle.state(), current);
        stop(&handle, task).await
    }

    #[test]
    fn queue_builder_should_keep_its_budget() {
        assert_eq!(queue(4, 7).max_concurrent.get(), 7);
    }

    /// Polls until `queue_id` has no members.
    async fn wait_until_empty(
        handle: &QueueManagerHandle,
        queue_id: QueueId,
    ) -> QueueManagerResult<()> {
        for _ in 0..40 {
            if find(handle, queue_id).await?.items.is_empty() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Err(QueueManagerError::QueueNotFound(queue_id))
    }
}
