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
    /// Minimize button hides to tray instead of taskbar-minimizing.
    ///
    /// Reserved for a future OS-minimize hook: GPUI exposes no minimize
    /// interception, so the OS button always minimizes to the taskbar
    /// (platform default) regardless of this flag. Defaults to `false`
    /// to match actual behavior.
    #[serde(default)]
    pub minimize_to_tray: bool,
    /// Close button hides to tray instead of quitting.
    ///
    /// Explicit Quit (tray menu, shortcut) always exits regardless.
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    /// Launch Raijin automatically at login.
    #[serde(default)]
    pub launch_at_startup: bool,
}

const fn default_true() -> bool {
    true
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
            minimize_to_tray: false,
            close_to_tray: true,
            launch_at_startup: false,
        }
    }

    /// Resolves the configured minimize/close behavior for tray wiring.
    #[must_use]
    pub const fn tray_behavior(&self) -> crate::platform::tray::TrayBehavior {
        crate::platform::tray::TrayBehavior::new(self.minimize_to_tray, self.close_to_tray)
    }
}

/// Settings-table key holding the serialized [`DesktopSettings`].
pub const DESKTOP_SETTINGS_KEY: &str = "desktop.settings";

/// Loads persisted [`DesktopSettings`], falling back to `fallback`.
///
/// Unknown fields are ignored and missing tray flags default (see M6
/// upgrade test), so older rows never brick startup. Corrupt rows log a
/// warning and also fall back instead of failing boot.
pub async fn load_desktop_settings(
    repository: &crate::db::SettingsRepository,
    fallback: DesktopSettings,
) -> DesktopSettings {
    match repository.get_raw(DESKTOP_SETTINGS_KEY).await {
        Ok(Some(raw)) => match serde_json::from_str(&raw) {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(?error, "desktop settings corrupt, using defaults");
                fallback
            }
        },
        Ok(None) => fallback,
        Err(error) => {
            tracing::warn!(?error, "desktop settings unreadable, using defaults");
            fallback
        }
    }
}

/// Persists [`DesktopSettings`] transactionally via upsert.
///
/// # Errors
///
/// Returns an error when serialization or SQLite execution fails.
pub async fn save_desktop_settings(
    repository: &crate::db::SettingsRepository,
    settings: &DesktopSettings,
) -> Result<(), crate::db::DbError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(i64::MAX))
        .unwrap_or(0);
    repository
        .set_json(DESKTOP_SETTINGS_KEY, settings, now)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_should_close_to_tray_without_autostart() {
        let settings = DesktopSettings::from_defaults(
            PathBuf::from("C:/Downloads"),
            PathBuf::from("C:/Downloads/raijin"),
        );

        // X hides to tray; minimize stays a taskbar minimize (no OS hook).
        assert!(!settings.minimize_to_tray);
        assert!(settings.close_to_tray);
        assert!(!settings.launch_at_startup);
    }

    #[test]
    fn legacy_settings_without_tray_fields_should_load_with_defaults() {
        // Persisted settings from before M6 lack the new keys; strict
        // readers would brick upgrades, so new fields default.
        let legacy = serde_json::json!({
            "default_download_folder": "C:/Downloads",
            "category_root": "C:/Downloads/raijin",
            "use_category_by_default": true,
            "thread_count": 8,
            "max_concurrent_downloads": 3,
            "max_download_retries": 5,
            "dynamic_part_creation": true,
            "global_speed_limit_bps": null,
            "browser_integration_enabled": false
        });

        let settings: DesktopSettings =
            serde_json::from_value(legacy).expect("legacy settings must load");

        assert!(!settings.minimize_to_tray);
        assert!(settings.close_to_tray);
        assert!(!settings.launch_at_startup);
    }

    #[test]
    fn settings_should_round_trip_tray_flags() {
        let mut settings = DesktopSettings::from_defaults(
            PathBuf::from("C:/Downloads"),
            PathBuf::from("C:/Downloads/raijin"),
        );
        settings.minimize_to_tray = true;
        settings.launch_at_startup = true;

        let json = serde_json::to_string(&settings).expect("settings must serialize");
        let restored: DesktopSettings =
            serde_json::from_str(&json).expect("settings must deserialize");

        assert_eq!(restored, settings);
    }

    #[tokio::test]
    async fn settings_should_persist_and_fall_back_on_corrupt_rows() {
        let dir = tempfile::tempdir().expect("temp dir must exist");
        let db_path = dir.path().join("raijin-settings-test.sqlite");
        let pool = crate::db::bootstrap(&format!("sqlite://{}", db_path.display()))
            .await
            .expect("bootstrap must succeed");
        let repository = crate::db::SettingsRepository::new(pool);
        let fallback = DesktopSettings::from_defaults(
            PathBuf::from("C:/Downloads"),
            PathBuf::from("C:/Downloads/raijin"),
        );

        // Missing row falls back without error.
        assert_eq!(
            load_desktop_settings(&repository, fallback.clone()).await,
            fallback
        );

        let mut changed = fallback.clone();
        changed.minimize_to_tray = true;
        changed.launch_at_startup = true;
        save_desktop_settings(&repository, &changed)
            .await
            .expect("save must succeed");
        assert_eq!(
            load_desktop_settings(&repository, fallback.clone()).await,
            changed
        );

        // Corrupt JSON never breaks boot; defaults win with a warning.
        repository
            .set_raw(DESKTOP_SETTINGS_KEY, "{not json", 1)
            .await
            .expect("raw write must succeed");
        assert_eq!(
            load_desktop_settings(&repository, fallback.clone()).await,
            fallback
        );
    }
}
