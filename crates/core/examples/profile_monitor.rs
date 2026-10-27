use std::collections::BTreeMap;
use std::error::Error;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use raijin_core::download::{
    Bytes, BytesPerSecond, DownloadEvent, DownloadId, DownloadItem, DownloadKind, DownloadProgress,
    DownloadStatus,
};
use raijin_core::monitor::DownloadMonitorHandle;
use tokio::sync::broadcast;

const HISTORY_COUNT: i64 = 1_000;
const ACTIVE_COUNT: i64 = 16;
const UPDATE_COUNT: u64 = 500;

#[tokio::main(worker_threads = 4)]
#[hotpath::main(format = "json", output_path = "hotpath-monitor.json")]
async fn main() -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(30), run_workload()).await?
}

#[hotpath::measure]
async fn run_workload() -> Result<(), Box<dyn Error>> {
    let downloads = (1..=HISTORY_COUNT + ACTIVE_COUNT)
        .map(sample_item)
        .collect();
    let (events, receiver) = broadcast::channel(32);
    let (monitor, task) = DownloadMonitorHandle::spawn_with_downloads(receiver, None, downloads);
    let mut updates = monitor.subscribe();
    let initial = monitor.state();
    assert_eq!(initial.completed.len(), usize::try_from(HISTORY_COUNT)?);
    assert_eq!(initial.active.len(), usize::try_from(ACTIVE_COUNT)?);
    read_snapshots(&monitor);

    for downloaded in 1..=UPDATE_COUNT {
        events.send(DownloadEvent::DownloadProgress {
            id: DownloadId::new(HISTORY_COUNT + 1),
            progress: DownloadProgress {
                downloaded_bytes: Bytes::new(downloaded),
                total_bytes: Some(Bytes::new(1_000)),
                speed: BytesPerSecond::ZERO,
                eta_seconds: None,
                active_part_count: 4,
            },
        })?;
        updates.changed().await?;
        let snapshot = monitor.state();
        assert_eq!(
            snapshot.active[&DownloadId::new(HISTORY_COUNT + 1)].downloaded_bytes,
            Bytes::new(downloaded),
        );
    }

    assert_eq!(
        initial.active[&DownloadId::new(HISTORY_COUNT + 1)].downloaded_bytes,
        Bytes::ZERO,
    );
    read_snapshots(&monitor);
    events.send(DownloadEvent::Shutdown)?;
    task.await??;
    Ok(())
}

#[hotpath::measure]
fn read_snapshots(monitor: &DownloadMonitorHandle) {
    let snapshot = monitor.state();
    for _ in 0..10_000 {
        let current = monitor.state();
        assert!(Arc::ptr_eq(&snapshot, &current));
        black_box(current);
    }
}

fn sample_item(id: i64) -> DownloadItem {
    let completed = id <= HISTORY_COUNT;
    DownloadItem {
        id: DownloadId::new(id),
        kind: DownloadKind::Http,
        url: format!("https://example.invalid/file-{id}.bin"),
        download_page: None,
        headers: BTreeMap::new(),
        file_name: format!("file-{id}.bin"),
        folder: PathBuf::from("downloads"),
        category: None,
        status: if completed {
            DownloadStatus::Completed
        } else {
            DownloadStatus::Downloading
        },
        total_bytes: Some(Bytes::new(1_000)),
        downloaded_bytes: if completed {
            Bytes::new(1_000)
        } else {
            Bytes::ZERO
        },
        etag: None,
        last_modified: None,
        preferred_connections: None,
        speed_limit: None,
        failure: None,
        created_at: id,
        started_at: None,
        completed_at: completed.then_some(id),
        updated_at: id,
    }
}
