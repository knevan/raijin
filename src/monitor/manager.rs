use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::download::{DownloadEvent, DownloadItem};
use crate::monitor::{MonitorState, Projection};

/// Result type returned by monitor APIs.
pub type DownloadMonitorResult<T> = Result<T, DownloadMonitorError>;

/// Monitor actor errors.
#[derive(Debug, Error)]
pub enum DownloadMonitorError {
    /// Download event stream closed.
    #[error("download event stream closed")]
    EventStreamClosed,
    /// Monitor state subscribers were dropped.
    #[error("monitor state subscribers were dropped")]
    StateSubscribersDropped,
    /// System clock is earlier than Unix epoch.
    #[error("system clock is earlier than Unix epoch")]
    ClockBeforeEpoch,
    /// System clock value does not fit in timestamp range.
    #[error("system clock timestamp is out of range")]
    ClockOutOfRange,
}

/// Cloneable handle for observing projected monitor state.
#[derive(Debug, Clone)]
pub struct DownloadMonitorHandle {
    state: watch::Receiver<MonitorState>,
}

impl DownloadMonitorHandle {
    /// Spawns a monitor actor.
    #[must_use]
    pub fn spawn(
        events: broadcast::Receiver<DownloadEvent>,
        speed_window_ms: Option<u64>,
    ) -> (Self, JoinHandle<DownloadMonitorResult<()>>) {
        Self::spawn_with_downloads(events, speed_window_ms, Vec::new())
    }

    /// Spawns a monitor actor seeded with existing downloads.
    #[must_use]
    pub fn spawn_with_downloads(
        events: broadcast::Receiver<DownloadEvent>,
        speed_window_ms: Option<u64>,
        downloads: Vec<DownloadItem>,
    ) -> (Self, JoinHandle<DownloadMonitorResult<()>>) {
        let mut projection = Projection::new(speed_window_ms);
        if let Ok(now) = now_ms() {
            for item in &downloads {
                projection.apply_item(item, now);
            }
        }
        let (state_tx, state_rx) = watch::channel(MonitorState::default());
        let _sent = state_tx.send(projection.state().clone());
        let monitor = DownloadMonitor {
            events,
            state: state_tx,
            projection,
        };
        let task = tokio::spawn(monitor.run());
        (Self { state: state_rx }, task)
    }

    /// Returns a receiver for monitor state snapshots.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<MonitorState> {
        self.state.clone()
    }

    /// Returns latest monitor state snapshot.
    #[must_use]
    pub fn state(&self) -> MonitorState {
        self.state.borrow().clone()
    }
}

struct DownloadMonitor {
    events: broadcast::Receiver<DownloadEvent>,
    state: watch::Sender<MonitorState>,
    projection: Projection,
}

impl DownloadMonitor {
    async fn run(mut self) -> DownloadMonitorResult<()> {
        loop {
            let event = match self.events.recv().await {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "monitor lagged download events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if matches!(event, DownloadEvent::Shutdown) {
                break;
            }
            self.apply_event(event)?;
            self.publish()?;
        }
        Ok(())
    }

    fn apply_event(&mut self, event: DownloadEvent) -> DownloadMonitorResult<()> {
        let now = now_ms()?;
        match event {
            DownloadEvent::BootLoaded { downloads } => {
                for item in downloads {
                    self.projection.apply_item(&item, now);
                }
            }
            DownloadEvent::DownloadAdded { item }
            | DownloadEvent::DownloadChanged { item }
            | DownloadEvent::DownloadPaused { item }
            | DownloadEvent::DownloadResumed { item } => self.projection.apply_item(&item, now),
            DownloadEvent::DownloadRemoved { id } => self.projection.remove(id),
            DownloadEvent::DownloadFailed { id, failure } => {
                let updated_at =
                    i64::try_from(now).map_err(|_| DownloadMonitorError::ClockOutOfRange)?;
                self.projection.apply_failure(id, failure, updated_at);
            }
            DownloadEvent::DownloadProgress { id, progress } => {
                self.projection.apply_progress(id, progress, now);
            }
            DownloadEvent::Shutdown => {}
        }
        Ok(())
    }

    fn publish(&self) -> DownloadMonitorResult<()> {
        self.state
            .send(self.projection.state().clone())
            .map_err(|_| DownloadMonitorError::StateSubscribersDropped)
    }
}

fn now_ms() -> DownloadMonitorResult<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| DownloadMonitorError::ClockBeforeEpoch)?;
    u64::try_from(duration.as_millis()).map_err(|_| DownloadMonitorError::ClockOutOfRange)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use tokio::sync::broadcast;

    use super::*;
    use crate::download::{
        Bytes, BytesPerSecond, DownloadItem, DownloadKind, DownloadProgress, DownloadStatus,
    };

    #[tokio::test]
    async fn monitor_should_project_events_into_state() -> DownloadMonitorResult<()> {
        let (events, receiver) = broadcast::channel(16);
        let (handle, task) = DownloadMonitorHandle::spawn(receiver, None);
        let mut state_rx = handle.subscribe();
        let item = item(1, DownloadStatus::Downloading, Bytes::ZERO);

        let _sent = events.send(DownloadEvent::DownloadAdded { item: item.clone() });
        state_rx
            .changed()
            .await
            .map_err(|_| DownloadMonitorError::EventStreamClosed)?;
        let _sent = events.send(DownloadEvent::DownloadProgress {
            id: item.id,
            progress: DownloadProgress {
                downloaded_bytes: Bytes::new(100),
                total_bytes: Some(Bytes::new(1_000)),
                speed: BytesPerSecond::ZERO,
                eta_seconds: None,
                active_part_count: 1,
            },
        });
        state_rx
            .changed()
            .await
            .map_err(|_| DownloadMonitorError::EventStreamClosed)?;
        let snapshot = state_rx.borrow().clone();
        let _sent = events.send(DownloadEvent::Shutdown);
        task.await
            .map_err(|_| DownloadMonitorError::EventStreamClosed)??;

        assert_eq!(
            snapshot
                .active
                .get(&item.id)
                .map(|view| view.downloaded_bytes),
            Some(Bytes::new(100))
        );
        Ok(())
    }

    fn item(id: i64, status: DownloadStatus, downloaded_bytes: Bytes) -> DownloadItem {
        DownloadItem {
            id: crate::download::DownloadId::new(id),
            kind: DownloadKind::Http,
            url: format!("https://example.com/file-{id}.bin"),
            download_page: None,
            headers: BTreeMap::new(),
            file_name: format!("file-{id}.bin"),
            folder: PathBuf::from("C:/Downloads"),
            status,
            total_bytes: Some(Bytes::new(1_000)),
            downloaded_bytes,
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
