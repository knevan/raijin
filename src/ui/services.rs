use std::path::PathBuf;

use thiserror::Error;

use crate::db::{self, DbError, DownloadRepository, PartRepository, QueueRepository};
use crate::download::{DownloadManagerHandle, DownloadManagerOptions};
use crate::monitor::DownloadMonitorHandle;
use crate::platform::{self, PlatformError};
use crate::queue::{QueueManagerHandle, QueueManagerOptions};

#[derive(Debug, Error)]
pub(crate) enum AppServicesError {
    #[error(transparent)]
    Platform(#[from] PlatformError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Download(#[from] crate::download::DownloadManagerError),
}

#[derive(Debug, Clone)]
pub(crate) struct AppServices {
    pub(crate) downloads: DownloadManagerHandle,
    pub(crate) queues: QueueManagerHandle,
    pub(crate) monitor: DownloadMonitorHandle,
    pub(crate) default_folder: PathBuf,
    pub(crate) category_root: PathBuf,
}

impl AppServices {
    pub(crate) async fn start() -> Result<Self, AppServicesError> {
        let database_url = platform::desktop_database_url()?;
        let pool = db::bootstrap(&database_url).await?;
        let download_repo = DownloadRepository::new(pool.clone());
        let part_repo = PartRepository::new(pool.clone());
        let queue_repo = QueueRepository::new(pool);
        let existing_downloads = download_repo.list().await?;
        let category_root = platform::download_category_root_dir()?;
        let default_folder = platform::default_download_dir()?;

        let (downloads, _download_task) = DownloadManagerHandle::spawn(
            download_repo,
            part_repo,
            DownloadManagerOptions::default(),
        );
        let (monitor, _monitor_task) = DownloadMonitorHandle::spawn_with_downloads(
            downloads.subscribe(),
            Some(5_000),
            existing_downloads,
        );
        let (queues, _queue_task) = QueueManagerHandle::spawn(
            queue_repo,
            downloads.clone(),
            downloads.subscribe(),
            QueueManagerOptions::default(),
        );

        Ok(Self {
            downloads,
            queues,
            monitor,
            default_folder,
            category_root,
        })
    }
}
