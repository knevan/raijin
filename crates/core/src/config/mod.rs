//! Application configuration types and loading helpers.

use std::num::NonZeroU16;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::download::DownloadConfig;

/// Persisted desktop UI settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopSettings {
    /// Default folder used by manual HTTP downloads.
    pub default_download_folder: PathBuf,
    /// Root folder used to resolve category folders.
    pub category_root: PathBuf,
    /// Whether category-based folder selection is enabled in new download dialogs.
    pub use_category_by_default: bool,
    /// Preferred worker count for ranged downloads.
    pub thread_count: NonZeroU16,
    /// Maximum downloads that the main queue may run concurrently.
    pub max_concurrent_downloads: u16,
    /// Maximum retry attempts for download jobs.
    pub max_download_retries: u32,
    /// Whether dynamic part creation is enabled in the UI model.
    pub dynamic_part_creation: bool,
    /// Optional global speed limit in bytes per second.
    pub global_speed_limit_bps: Option<u64>,
    /// Whether browser integration should be enabled once integration server is wired.
    pub browser_integration_enabled: bool,
}

impl DesktopSettings {
    /// Creates settings from platform paths and engine defaults.
    #[must_use]
    pub fn from_defaults(default_download_folder: PathBuf, category_root: PathBuf) -> Self {
        let download_config = DownloadConfig::default();
        Self {
            default_download_folder,
            category_root,
            use_category_by_default: true,
            thread_count: download_config.default_connections,
            max_concurrent_downloads: 3,
            max_download_retries: download_config.max_retries.get(),
            dynamic_part_creation: true,
            global_speed_limit_bps: download_config.global_speed_limit.map(|limit| limit.get()),
            browser_integration_enabled: false,
        }
    }
}
