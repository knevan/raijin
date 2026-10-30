use std::num::NonZeroU16;
use std::path::PathBuf;

use raijin_core::db::{DownloadRepository, PartRepository, QueueRepository};
use raijin_core::download::{DownloadManagerHandle, DownloadStatus, NewDownload};
use raijin_core::queue::{QueueManagerHandle, QueueManagerOptions};

use super::EngineCommand;

pub(crate) async fn managers(limit: u16) -> (DownloadManagerHandle, QueueManagerHandle) {
    let pool = raijin_core::db::bootstrap("sqlite::memory:")
        .await
        .expect("database");
    let downloads = DownloadRepository::new(pool.clone());
    let (manager, _task) = DownloadManagerHandle::spawn(
        downloads.clone(),
        PartRepository::new(pool.clone()),
        Default::default(),
    );
    let (queues, _task) = QueueManagerHandle::spawn(
        downloads,
        QueueRepository::new(pool),
        manager.clone(),
        manager.subscribe(),
        QueueManagerOptions {
            global_limit: NonZeroU16::new(limit).expect("limit"),
            ..Default::default()
        },
    );
    queues.list().await.expect("boot");
    (manager, queues)
}

#[tokio::test]
async fn add_should_wait_for_stopped_queue_and_keep_add_paused_idle() {
    let (manager, queues) = managers(1).await;
    let id = queues.create("Later").await.expect("create");
    for autostart in [true, false] {
        EngineCommand::AddToQueue {
            request: NewDownload::http(
                "http://127.0.0.1:1/file",
                "file.bin",
                PathBuf::from("downloads"),
            ),
            queue_id: id,
            autostart,
        }
        .dispatch_to(&manager, &queues)
        .await
        .expect("add");
    }
    let items = manager.list().await.expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].status, DownloadStatus::Queued);
    assert_eq!(items[1].status, DownloadStatus::Paused);
    assert!(manager.list_active().await.expect("active").is_empty());
    assert_eq!(
        queues
            .list()
            .await
            .expect("queues")
            .iter()
            .find(|s| s.queue.id == id)
            .expect("queue")
            .items
            .len(),
        2
    );
    queues.shutdown().await.expect("shutdown queues");
    manager.shutdown().await.expect("shutdown engine");
}

#[tokio::test]
async fn add_should_never_bypass_global_admission_limit() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let url = format!("http://{}/file", listener.local_addr().expect("address"));
    let (manager, queues) = managers(1).await;
    for file in ["first.bin", "second.bin"] {
        EngineCommand::AddToQueue {
            request: NewDownload::http(&url, file, PathBuf::from("downloads")),
            queue_id: raijin_core::download::QueueId::MAIN,
            autostart: true,
        }
        .dispatch_to(&manager, &queues)
        .await
        .expect("add");
    }
    assert_eq!(manager.list_active().await.expect("active").len(), 1);
    queues.shutdown().await.expect("shutdown queues");
    manager.shutdown().await.expect("shutdown engine");
}
