use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::download::{
    Bytes, BytesPerSecond, DownloadFailure, DownloadId, DownloadItem, DownloadProgress,
    DownloadStatus,
};
use crate::monitor::SpeedMeter;

/// UI-safe view of one download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadView {
    /// Stable download id.
    pub id: DownloadId,
    /// Source URL.
    pub url: String,
    /// File name shown by UI.
    pub file_name: String,
    /// Destination folder.
    pub folder: PathBuf,
    /// Durable status.
    pub status: DownloadStatus,
    /// Downloaded bytes.
    pub downloaded_bytes: Bytes,
    /// Total bytes, if known.
    pub total_bytes: Option<Bytes>,
    /// Current speed.
    pub speed: BytesPerSecond,
    /// Estimated seconds remaining.
    pub eta_seconds: Option<u64>,
    /// Active part workers.
    pub active_part_count: u16,
    /// Last failure shown to UI.
    pub failure: Option<DownloadFailure>,
    /// Creation timestamp.
    pub created_at: i64,
    /// Last update timestamp.
    pub updated_at: i64,
}

impl DownloadView {
    fn from_item(item: &DownloadItem) -> Self {
        Self {
            id: item.id,
            url: item.url.clone(),
            file_name: item.file_name.clone(),
            folder: item.folder.clone(),
            status: item.status,
            downloaded_bytes: item.downloaded_bytes,
            total_bytes: item.total_bytes,
            speed: BytesPerSecond::ZERO,
            eta_seconds: None,
            active_part_count: 0,
            failure: item.failure.clone(),
            created_at: item.created_at,
            updated_at: item.updated_at,
        }
    }

    fn apply_progress(&mut self, progress: DownloadProgress) {
        if progress.active_part_count > 0 {
            self.status = DownloadStatus::Downloading;
        }
        self.downloaded_bytes = progress.downloaded_bytes;
        self.total_bytes = progress.total_bytes.or(self.total_bytes);
        self.speed = progress.speed;
        self.eta_seconds = progress.eta_seconds;
        self.active_part_count = progress.active_part_count;
    }

    fn from_failure(id: DownloadId, failure: DownloadFailure, updated_at: i64) -> Self {
        Self {
            id,
            url: String::new(),
            file_name: String::new(),
            folder: PathBuf::new(),
            status: DownloadStatus::Error,
            downloaded_bytes: Bytes::ZERO,
            total_bytes: None,
            speed: BytesPerSecond::ZERO,
            eta_seconds: None,
            active_part_count: 0,
            failure: Some(failure),
            created_at: updated_at,
            updated_at,
        }
    }
}

/// Full UI-safe monitor state.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MonitorState {
    /// Active downloads keyed by id.
    pub active: BTreeMap<DownloadId, DownloadView>,
    /// Completed or failed downloads keyed by id.
    pub completed: BTreeMap<DownloadId, DownloadView>,
}

/// Stateful download event projection.
#[derive(Debug, Clone)]
pub struct Projection {
    state: MonitorState,
    meters: BTreeMap<DownloadId, SpeedMeter>,
    speed_window_ms: Option<u64>,
}

impl Projection {
    /// Creates an empty projection.
    #[must_use]
    pub fn new(speed_window_ms: Option<u64>) -> Self {
        Self {
            state: MonitorState::default(),
            meters: BTreeMap::new(),
            speed_window_ms,
        }
    }

    /// Returns current UI-safe state.
    #[must_use]
    pub fn state(&self) -> &MonitorState {
        &self.state
    }

    /// Applies a full item snapshot.
    pub fn apply_item(&mut self, item: &DownloadItem, now_ms: u64) {
        let mut view = DownloadView::from_item(item);
        let speed = self
            .meters
            .entry(item.id)
            .or_insert_with(|| SpeedMeter::with_window(self.speed_window_ms))
            .record(now_ms, item.downloaded_bytes);
        view.speed = speed;
        view.eta_seconds = eta(item.downloaded_bytes, item.total_bytes, speed);
        self.place_view(view);
    }

    /// Applies a progress snapshot by id.
    pub fn apply_progress(&mut self, id: DownloadId, mut progress: DownloadProgress, now_ms: u64) {
        let speed = self
            .meters
            .entry(id)
            .or_insert_with(|| SpeedMeter::with_window(self.speed_window_ms))
            .record(now_ms, progress.downloaded_bytes);
        progress.speed = speed;
        progress.eta_seconds = eta(progress.downloaded_bytes, progress.total_bytes, speed);
        if let Some(view) = self.state.active.get_mut(&id) {
            view.apply_progress(progress);
            return;
        }
        if let Some(view) = self.state.completed.get_mut(&id) {
            view.apply_progress(progress);
        }
    }

    /// Moves one download to completed/error projection with failure detail when present.
    pub fn apply_failure(&mut self, id: DownloadId, failure: DownloadFailure, updated_at: i64) {
        if let Some(mut view) = self.state.active.remove(&id) {
            view.status = DownloadStatus::Error;
            view.failure = Some(failure);
            view.updated_at = updated_at;
            self.state.completed.insert(id, view);
            return;
        }
        if let Some(mut view) = self.state.completed.remove(&id) {
            view.status = DownloadStatus::Error;
            view.failure = Some(failure);
            view.updated_at = updated_at;
            self.state.completed.insert(id, view);
            return;
        }
        self.state
            .completed
            .insert(id, DownloadView::from_failure(id, failure, updated_at));
    }

    /// Removes one download from all projections.
    pub fn remove(&mut self, id: DownloadId) {
        self.state.active.remove(&id);
        self.state.completed.remove(&id);
        self.meters.remove(&id);
    }

    /// Clears speed history for one download lifecycle.
    pub fn reset_meter(&mut self, id: DownloadId) {
        self.meters.remove(&id);
    }

    fn place_view(&mut self, view: DownloadView) {
        self.state.active.remove(&view.id);
        self.state.completed.remove(&view.id);
        match view.status {
            DownloadStatus::Completed | DownloadStatus::Error => {
                self.state.completed.insert(view.id, view);
            }
            DownloadStatus::Removed => {
                self.meters.remove(&view.id);
            }
            _ => {
                self.state.active.insert(view.id, view);
            }
        }
    }
}

fn eta(downloaded: Bytes, total: Option<Bytes>, speed: BytesPerSecond) -> Option<u64> {
    let total = total?.get();
    let speed = speed.get();
    if speed == 0 || downloaded.get() >= total {
        return None;
    }
    Some((total - downloaded.get()).div_ceil(speed))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::download::{DownloadKind, FailureKind};

    #[test]
    fn projection_should_split_active_and_completed_from_items() {
        let mut projection = Projection::new(None);
        let active = item(1, DownloadStatus::Downloading, Bytes::new(100));
        let completed = item(2, DownloadStatus::Completed, Bytes::new(200));

        projection.apply_item(&active, 1_000);
        projection.apply_item(&completed, 1_000);

        assert!(projection.state().active.contains_key(&active.id));
        assert!(projection.state().completed.contains_key(&completed.id));
    }

    #[test]
    fn projection_should_compute_speed_and_eta_from_progress() {
        let mut projection = Projection::new(None);
        let mut active = item(1, DownloadStatus::Downloading, Bytes::ZERO);
        active.total_bytes = Some(Bytes::new(2_000));
        projection.apply_item(&active, 1_000);

        projection.apply_progress(
            active.id,
            DownloadProgress {
                downloaded_bytes: Bytes::new(1_000),
                total_bytes: Some(Bytes::new(2_000)),
                speed: BytesPerSecond::ZERO,
                eta_seconds: None,
                active_part_count: 1,
            },
            2_000,
        );

        let view = projection.state().active.get(&active.id);
        assert_eq!(
            view.map(|view| view.speed),
            Some(BytesPerSecond::new(1_000))
        );
        assert_eq!(view.and_then(|view| view.eta_seconds), Some(1));
    }

    #[test]
    fn projection_should_remove_downloads() {
        let mut projection = Projection::new(None);
        let active = item(1, DownloadStatus::Downloading, Bytes::new(100));
        projection.apply_item(&active, 1_000);

        projection.remove(active.id);

        assert!(!projection.state().active.contains_key(&active.id));
    }

    #[test]
    fn projection_should_reset_speed_history_between_lifecycles() {
        let mut projection = Projection::new(None);
        let active = item(1, DownloadStatus::Downloading, Bytes::ZERO);
        projection.apply_item(&active, 1_000);
        projection.apply_progress(
            active.id,
            DownloadProgress {
                downloaded_bytes: Bytes::new(1_000),
                total_bytes: Some(Bytes::new(3_000)),
                speed: BytesPerSecond::ZERO,
                eta_seconds: None,
                active_part_count: 1,
            },
            2_000,
        );
        projection.reset_meter(active.id);

        projection.apply_progress(
            active.id,
            DownloadProgress {
                downloaded_bytes: Bytes::new(1_000),
                total_bytes: Some(Bytes::new(3_000)),
                speed: BytesPerSecond::ZERO,
                eta_seconds: None,
                active_part_count: 1,
            },
            3_000,
        );
        let after_reset = projection
            .state()
            .active
            .get(&active.id)
            .map(|view| view.speed);

        projection.apply_progress(
            active.id,
            DownloadProgress {
                downloaded_bytes: Bytes::new(2_000),
                total_bytes: Some(Bytes::new(3_000)),
                speed: BytesPerSecond::ZERO,
                eta_seconds: None,
                active_part_count: 1,
            },
            4_000,
        );
        let after_new_delta = projection
            .state()
            .active
            .get(&active.id)
            .map(|view| view.speed);

        assert_eq!(after_reset, Some(BytesPerSecond::ZERO));
        assert_eq!(after_new_delta, Some(BytesPerSecond::new(1_000)));
    }

    #[test]
    fn projection_should_record_failure_for_unknown_download() {
        let mut projection = Projection::new(None);
        let id = DownloadId::new(42);

        projection.apply_failure(
            id,
            DownloadFailure {
                kind: FailureKind::Network,
                message: "network error".to_owned(),
            },
            1_234,
        );

        let view = projection.state().completed.get(&id);
        assert_eq!(view.map(|view| view.status), Some(DownloadStatus::Error));
        assert_eq!(view.map(|view| view.updated_at), Some(1_234));
    }

    fn item(id: i64, status: DownloadStatus, downloaded_bytes: Bytes) -> DownloadItem {
        DownloadItem {
            id: DownloadId::new(id),
            kind: DownloadKind::Http,
            url: format!("https://example.com/file-{id}.bin"),
            download_page: None,
            headers: BTreeMap::new(),
            file_name: format!("file-{id}.bin"),
            folder: PathBuf::from("C:/Downloads"),
            status,
            total_bytes: Some(Bytes::new(200)),
            downloaded_bytes,
            etag: None,
            last_modified: None,
            preferred_connections: None,
            speed_limit: None,
            failure: (status == DownloadStatus::Error).then_some(DownloadFailure {
                kind: FailureKind::Network,
                message: "network error".to_owned(),
            }),
            created_at: 1,
            started_at: None,
            completed_at: None,
            updated_at: 1,
        }
    }
}
