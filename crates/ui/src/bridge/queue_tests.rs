use std::num::NonZeroU16;

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
async fn add_to_queue_should_wait_without_starting_the_queue() {
    let folder = tempfile::tempdir().expect("destination");
    let (manager, queues) = managers(1).await;
    let id = queues.create("Later").await.expect("create");
    for _ in 0..2 {
        EngineCommand::AddToQueue {
            request: NewDownload::http(
                "http://127.0.0.1:1/file",
                "file.bin",
                folder.path().to_path_buf(),
            ),
            queue_id: id,
            start_queue: false,
        }
        .dispatch_to(&manager, &queues)
        .await
        .expect("add");
    }
    let items = manager.list().await.expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].status, DownloadStatus::Queued);
    assert_eq!(items[1].status, DownloadStatus::Queued);
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
async fn queued_add_should_respect_aggregate_queue_limit() {
    let folder = tempfile::tempdir().expect("destination");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let url = format!("http://{}/file", listener.local_addr().expect("address"));
    let (manager, queues) = managers(1).await;
    for file in ["first.bin", "second.bin"] {
        EngineCommand::AddToQueue {
            request: NewDownload::http(&url, file, folder.path().to_path_buf()),
            queue_id: raijin_core::download::QueueId::MAIN,
            start_queue: false,
        }
        .dispatch_to(&manager, &queues)
        .await
        .expect("add");
    }
    assert_eq!(manager.list_active().await.expect("active").len(), 1);
    queues.shutdown().await.expect("shutdown queues");
    manager.shutdown().await.expect("shutdown engine");
}

#[tokio::test]
async fn direct_add_should_run_in_parallel_without_queue_membership() {
    let folder = tempfile::tempdir().expect("destination");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let url = format!("http://{}/file", listener.local_addr().expect("address"));
    let (manager, queues) = managers(1).await;
    queues
        .stop(raijin_core::download::QueueId::MAIN)
        .await
        .expect("stop main");
    for file in ["direct-first.bin", "direct-second.bin"] {
        EngineCommand::AddDirect {
            request: NewDownload::http(&url, file, folder.path().to_path_buf()),
        }
        .dispatch_to(&manager, &queues)
        .await
        .expect("direct add");
    }
    assert_eq!(manager.list_active().await.expect("active").len(), 2);
    assert!(
        queues
            .list()
            .await
            .expect("queues")
            .iter()
            .all(|snapshot| snapshot.items.is_empty())
    );
    EngineCommand::PauseAll
        .dispatch_to(&manager, &queues)
        .await
        .expect("pause all");
    EngineCommand::ResumeAll
        .dispatch_to(&manager, &queues)
        .await
        .expect("resume all");
    assert_eq!(manager.list_active().await.expect("resumed").len(), 2);
    queues.shutdown().await.expect("shutdown queues");
    manager.shutdown().await.expect("shutdown engine");
}

#[tokio::test]
async fn queued_add_should_activate_the_queue_only_when_requested() {
    let folder = tempfile::tempdir().expect("destination");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let url = format!("http://{}/file", listener.local_addr().expect("address"));
    let (manager, queues) = managers(1).await;
    let id = queues.create("Later").await.expect("queue");
    EngineCommand::AddToQueue {
        request: NewDownload::http(&url, "queued.bin", folder.path().to_path_buf()),
        queue_id: id,
        start_queue: true,
    }
    .dispatch_to(&manager, &queues)
    .await
    .expect("queued add");
    assert!(
        queues
            .list()
            .await
            .expect("queues")
            .iter()
            .find(|snapshot| snapshot.queue.id == id)
            .expect("queue")
            .active
    );
    assert_eq!(manager.list_active().await.expect("active").len(), 1);
    queues.shutdown().await.expect("shutdown queues");
    manager.shutdown().await.expect("shutdown engine");
}
