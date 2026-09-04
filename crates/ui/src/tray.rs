use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use raijin_core::download::DownloadStatus;
use raijin_core::platform::tray::{
    CloseDecision, MinimizeDecision, TrayBehavior, WindowVisibility, tray_tooltip,
};

/// Shared tray session handle (cloneable across views and callbacks).
///
/// All locks guard a single enum/flag copy and are never held across
/// `.await` or GPUI updates.
#[derive(Debug, Clone)]
pub struct TraySession {
    behavior: std::sync::Arc<Mutex<TrayBehavior>>,
    visibility: std::sync::Arc<Mutex<WindowVisibility>>,
    quitting: std::sync::Arc<AtomicBool>,
    /// Set when the shell destroys its own window for a tray hide
    /// (Windows/Linux have no `hide()`); the `on_window_closed` observer
    /// consumes it and stands down instead of reopening or quitting.
    programmatic_close: std::sync::Arc<AtomicBool>,
    /// Set once the native tray icon is live. Closes then stay closed
    /// (the icon owns restore); without an icon the observer must reopen
    /// or quit so the app never runs windowless with no way back.
    tray_icon_live: std::sync::Arc<AtomicBool>,
    /// Last activity counts feeding the tray menu enabled-flags.
    activity: std::sync::Arc<Mutex<(usize, usize)>>,
    /// Last tooltip text for the 1s tray sync loop (views write, loop reads).
    last_tooltip: std::sync::Arc<Mutex<String>>,
}

impl TraySession {
    /// Creates a session from the persisted settings behavior.
    #[must_use]
    pub fn new(behavior: TrayBehavior) -> Self {
        Self {
            behavior: std::sync::Arc::new(Mutex::new(behavior)),
            visibility: std::sync::Arc::new(Mutex::new(WindowVisibility::Visible)),
            quitting: std::sync::Arc::new(AtomicBool::new(false)),
            programmatic_close: std::sync::Arc::new(AtomicBool::new(false)),
            tray_icon_live: std::sync::Arc::new(AtomicBool::new(false)),
            activity: std::sync::Arc::new(Mutex::new((0, 0))),
            last_tooltip: std::sync::Arc::new(Mutex::new(tray_tooltip("Raijin", 0, 0))),
        }
    }

    /// Current minimize/close behavior (copied under a short lock).
    #[must_use]
    pub fn behavior(&self) -> TrayBehavior {
        *self.behavior.lock().expect("tray behavior lock poisoned")
    }

    /// Replaces the behavior (settings page writes through here; the shell
    /// also degrades to plain behavior when the native icon is unavailable).
    pub fn set_behavior(&self, behavior: TrayBehavior) {
        *self.behavior.lock().expect("tray behavior lock poisoned") = behavior;
    }

    /// Current window visibility (copied under a short lock).
    #[must_use]
    pub fn visibility(&self) -> WindowVisibility {
        *self
            .visibility
            .lock()
            .expect("tray visibility lock poisoned")
    }

    /// True while hidden to tray (low-resource mode: shed polling).
    #[must_use]
    pub fn is_hidden(&self) -> bool {
        self.visibility().is_hidden()
    }

    /// True when monitor polling + repaints should run.
    #[must_use]
    pub fn should_poll(&self) -> bool {
        self.visibility().should_poll()
    }

    /// Resolves a minimize request without recording a transition.
    ///
    /// No GPUI hook intercepts the OS minimize button yet, so this documents
    /// the configured intent for a future hook; the OS button currently
    /// minimizes to the taskbar (platform default) on all OSes.
    #[allow(dead_code)]
    #[must_use]
    pub fn minimize_requested(&self) -> MinimizeDecision {
        self.behavior().on_minimize()
    }

    /// Resolves a close request without recording a transition.
    ///
    /// Callers execute the decision, then record it via
    /// [`TraySession::note_hidden_to_tray`] (suspended window) or
    /// [`TraySession::begin_programmatic_close`] (destroyed window).
    ///
    /// `quit_requested` covers explicit Quit (tray menu, shortcut): it
    /// always destroys the window.
    #[must_use]
    pub fn close_requested(&self, quit_requested: bool) -> CloseDecision {
        self.behavior()
            .on_close(quit_requested || self.is_quitting())
    }

    /// Records a suspend-to-tray (macOS `hide()`; window stays alive).
    pub fn note_hidden_to_tray(&self) {
        self.visibility
            .lock()
            .expect("tray visibility lock poisoned")
            .hide_to_tray();
    }

    /// Arms the closed-observer stand-down, then the caller destroys its own
    /// window (`remove_window`) for a tray hide on platforms without
    /// `hide()` (Windows/Linux). Show rebuilds from monitor + DB.
    pub fn begin_programmatic_close(&self) {
        self.programmatic_close.store(true, Ordering::SeqCst);
    }

    /// Disarms a stand-down that never led to a close (the deferred removal
    /// found no window or lost a nested-update race and will retry).
    pub fn cancel_programmatic_close(&self) {
        self.programmatic_close.store(false, Ordering::SeqCst);
    }

    /// Marks the window shown again.
    ///
    /// Returns true when preserved view state (selection, scroll, drafts)
    /// can be reused (hide path) vs a full rebuild from monitor + DB
    /// (closed path, see `plan.md` M6 window rule).
    pub fn mark_shown(&self) -> bool {
        self.visibility
            .lock()
            .expect("tray visibility lock poisoned")
            .show()
    }

    /// Marks the window destroyed via `remove_window`.
    pub fn mark_closed(&self) {
        self.visibility
            .lock()
            .expect("tray visibility lock poisoned")
            .close();
    }

    /// Called from the `on_window_closed` observer (OS chrome close or the
    /// shell's own programmatic hide).
    ///
    /// A programmatic hide (shell destroyed its own window for a tray hide)
    /// stands down: the session lands on Closed and the icon owns restore.
    /// Otherwise, with a live icon, tray-configured closes stay closed
    /// (Show reopens on demand) and plain closes quit. Without an icon the
    /// observer reopens or quits so the app never runs windowless blind.
    pub fn os_window_closed(&self) -> OsCloseOutcome {
        if self.programmatic_close.swap(false, Ordering::SeqCst) {
            self.mark_closed();
            return OsCloseOutcome::Nothing;
        }
        let was_hidden = self.is_hidden();
        self.mark_closed();
        if self.is_quitting() {
            return OsCloseOutcome::Nothing;
        }
        // The live icon owns restore, so tray-configured closes stay closed
        // (Show reopens on demand) and plain closes quit. Without an icon
        // the observer reopens or quits so the app never runs blind.
        if self.tray_icon_live() {
            if was_hidden || self.behavior().close_to_tray {
                OsCloseOutcome::Nothing
            } else {
                OsCloseOutcome::Quit
            }
        } else if was_hidden || self.behavior().close_to_tray {
            OsCloseOutcome::Reopen
        } else {
            OsCloseOutcome::Quit
        }
    }

    /// Marks the native tray icon live (called once after a successful
    /// build). Closes then stay closed; the icon owns the restore path.
    pub fn set_tray_icon_live(&self) {
        self.tray_icon_live.store(true, Ordering::SeqCst);
    }

    /// True after [`TraySession::set_tray_icon_live`].
    #[must_use]
    pub fn tray_icon_live(&self) -> bool {
        self.tray_icon_live.load(Ordering::SeqCst)
    }

    /// Marks an explicit quit so pending close observers stand down.
    pub fn set_quitting(&self) {
        self.quitting.store(true, Ordering::SeqCst);
    }

    /// True after [`TraySession::set_quitting`].
    #[must_use]
    pub fn is_quitting(&self) -> bool {
        self.quitting.load(Ordering::SeqCst)
    }

    /// Records engine activity and returns the new tooltip text.
    ///
    /// Called on every monitor tick (visible) and every hidden tick, so the
    /// tray menu enabled-flags and tooltip track downloads in both modes.
    pub fn update_activity(
        &self,
        active_count: usize,
        paused_count: usize,
        total_speed_bps: u64,
    ) -> String {
        *self.activity.lock().expect("tray activity lock poisoned") = (active_count, paused_count);
        let tooltip = tray_tooltip("Raijin", active_count, total_speed_bps);
        *self
            .last_tooltip
            .lock()
            .expect("tray tooltip lock poisoned") = tooltip.clone();
        tooltip
    }

    /// Last tooltip text written by [`TraySession::update_activity`].
    #[must_use]
    pub fn last_tooltip(&self) -> String {
        self.last_tooltip
            .lock()
            .expect("tray tooltip lock poisoned")
            .clone()
    }

    /// True when at least one download is transferring (menu: Pause enabled).
    #[must_use]
    pub fn has_active(&self) -> bool {
        self.activity.lock().expect("tray activity lock poisoned").0 > 0
    }

    /// True when at least one download is paused (menu: Resume enabled).
    #[must_use]
    pub fn has_paused(&self) -> bool {
        self.activity.lock().expect("tray activity lock poisoned").1 > 0
    }
}

/// Outcome of an OS-chrome window close (see [`TraySession::os_window_closed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsCloseOutcome {
    /// Re-open the window from monitor state + DB (close-to-tray UX).
    Reopen,
    /// Quit the app (plain close behavior).
    Quit,
    /// Do nothing (quit in progress, or the native icon owns restore).
    Nothing,
}

/// Summarizes monitor state for menu/tooltip updates without touching views.
///
/// Returns `(active_count, paused_count, total_speed_bps)`. Pure over the
/// projected snapshot, so the hidden tray tick can call it without entity
/// access.
#[must_use]
pub fn summarize_monitor(snapshot: &raijin_core::monitor::MonitorState) -> (usize, usize, u64) {
    let mut active = 0usize;
    let mut speed = 0u64;
    for view in snapshot.active.values() {
        if view.status == DownloadStatus::Downloading {
            active += 1;
        }
        speed = speed.saturating_add(view.speed.get());
    }
    let paused = snapshot
        .completed
        .values()
        .filter(|view| view.status == DownloadStatus::Paused)
        .count();
    (active, paused, speed)
}

/// Reports whether `--minimized` was passed (start hidden to tray).
#[must_use]
pub fn parse_start_minimized(args: impl IntoIterator<Item = String>) -> bool {
    args.into_iter().any(|arg| arg == "--minimized")
}

/// Extracts the `--add` URL for the standalone Add Download window.
///
/// Accepted forms: `--add <url>`, `--add=<url>`, and bare `--add` (empty
/// initial URL; the window then falls back to a valid clipboard URL).
/// Returns `None` when no `--add` flag is present.
#[must_use]
pub fn parse_add_url(args: impl IntoIterator<Item = String>) -> Option<String> {
    let args: Vec<String> = args.into_iter().collect();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--add" {
            // Attached value wins; a missing value means an empty form.
            if let Some(next) = iter.peek()
                && !next.starts_with('-')
            {
                return Some((*next).clone());
            }
            return Some(String::new());
        }
        if let Some(value) = arg.strip_prefix("--add=") {
            return Some(value.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    // Explicit imports: a `use super::*` glob would shadow the built-in
    // `#[test]` attribute with GPUI's `test` macro (see format.rs).
    use raijin_core::download::{Bytes, BytesPerSecond, DownloadId, DownloadStatus};
    use raijin_core::monitor::{DownloadView, MonitorState};
    use raijin_core::platform::tray::{
        CloseDecision, MinimizeDecision, TrayBehavior, WindowVisibility,
    };

    use super::{
        OsCloseOutcome, TraySession, parse_add_url, parse_start_minimized, summarize_monitor,
    };

    fn session() -> TraySession {
        TraySession::new(TrayBehavior::new(true, true))
    }

    #[test]
    fn minimize_should_resolve_per_behavior_without_recording() {
        let tray = session();
        assert_eq!(tray.minimize_requested(), MinimizeDecision::HideToTray);
        // Pure decision: no transition until the caller records it.
        assert_eq!(tray.visibility(), WindowVisibility::Visible);

        let plain = TraySession::new(TrayBehavior::new(false, false));
        assert_eq!(plain.minimize_requested(), MinimizeDecision::MinimizeWindow);
    }

    #[test]
    fn hide_paths_should_record_suspend_or_programmatic_close() {
        let tray = session();
        tray.note_hidden_to_tray();
        assert!(tray.is_hidden());
        assert!(!tray.should_poll());
        assert!(tray.mark_shown(), "hide path must preserve view state");

        // Programmatic destroy (Windows/Linux): the observer stands down and
        // the session lands on Closed, so Show rebuilds from monitor + DB.
        tray.begin_programmatic_close();
        assert_eq!(tray.os_window_closed(), OsCloseOutcome::Nothing);
        assert_eq!(tray.visibility(), WindowVisibility::Closed);
        assert!(
            !tray.mark_shown(),
            "closed path must rebuild from monitor + DB"
        );
    }

    #[test]
    fn cancelled_programmatic_close_should_not_stand_down_observer() {
        let tray = session();
        tray.begin_programmatic_close();
        tray.cancel_programmatic_close();
        // A disarmed hide behaves like a normal OS close again.
        assert_eq!(tray.os_window_closed(), OsCloseOutcome::Reopen);
    }

    #[test]
    fn close_should_resolve_per_behavior_without_recording() {
        let tray = session();
        assert_eq!(tray.close_requested(false), CloseDecision::HideToTray);
        assert_eq!(tray.visibility(), WindowVisibility::Visible);

        let plain = TraySession::new(TrayBehavior::new(false, false));
        assert_eq!(plain.close_requested(false), CloseDecision::RemoveWindow);

        // Explicit quit always destroys, even with close-to-tray on.
        assert_eq!(tray.close_requested(true), CloseDecision::RemoveWindow);
    }

    #[test]
    fn os_close_should_reopen_or_quit_per_behavior_until_quit() {
        // No icon yet: close-to-tray reopens (nothing to restore from).
        let tray = session();
        assert!(!tray.tray_icon_live());
        assert_eq!(tray.os_window_closed(), OsCloseOutcome::Reopen);
        // The window is destroyed; the caller reopens it from monitor + DB.
        assert_eq!(tray.visibility(), WindowVisibility::Closed);
        assert!(
            !tray.mark_shown(),
            "closed path must rebuild from monitor + DB"
        );
        assert_eq!(tray.visibility(), WindowVisibility::Visible);

        // Plain close behavior: OS X quits the app.
        let plain = TraySession::new(TrayBehavior::new(false, false));
        assert_eq!(plain.os_window_closed(), OsCloseOutcome::Quit);

        // Explicit quit never reopens.
        tray.set_quitting();
        assert!(tray.is_quitting());
        assert_eq!(tray.os_window_closed(), OsCloseOutcome::Nothing);
    }

    #[test]
    fn os_close_should_stay_closed_once_icon_is_live() {
        // Live icon + close-to-tray: X hides, Show reopens on demand.
        let tray = session();
        tray.set_tray_icon_live();
        assert!(tray.tray_icon_live());
        assert_eq!(tray.os_window_closed(), OsCloseOutcome::Nothing);
        assert_eq!(tray.visibility(), WindowVisibility::Closed);
        assert!(
            !tray.mark_shown(),
            "closed path must rebuild from monitor + DB"
        );

        // Live icon + plain close: X quits the app.
        let plain = TraySession::new(TrayBehavior::new(false, false));
        plain.set_tray_icon_live();
        assert_eq!(plain.os_window_closed(), OsCloseOutcome::Quit);
    }

    #[test]
    fn activity_should_feed_tooltip_and_menu_flags() {
        let tray = session();
        assert!(!tray.has_active());
        assert!(!tray.has_paused());
        assert_eq!(tray.last_tooltip(), "Raijin — Idle");

        assert_eq!(tray.update_activity(0, 0, 0), "Raijin — Idle");
        let tooltip = tray.update_activity(2, 1, 2048);
        assert!(tooltip.contains('2'), "tooltip was {tooltip}");
        assert_eq!(tray.last_tooltip(), tooltip);
        assert!(tray.has_active());
        assert!(tray.has_paused());
    }

    #[test]
    fn session_should_apply_settings_behavior() {
        let tray = session();
        tray.set_behavior(TrayBehavior::new(false, false));
        assert_eq!(tray.behavior(), TrayBehavior::new(false, false));
        assert_eq!(tray.minimize_requested(), MinimizeDecision::MinimizeWindow);
        assert_eq!(tray.os_window_closed(), OsCloseOutcome::Quit);
    }

    #[test]
    fn start_minimized_flag_should_require_exact_match() {
        assert!(parse_start_minimized([
            "raijin".to_owned(),
            "--minimized".to_owned()
        ]));
        assert!(!parse_start_minimized(["raijin".to_owned()]));
        assert!(!parse_start_minimized([
            "raijin".to_owned(),
            "--minimized=true".to_owned()
        ]));
    }

    #[test]
    fn add_url_should_support_space_equals_and_bare_forms() {
        assert_eq!(
            parse_add_url([
                "raijin".to_owned(),
                "--add".to_owned(),
                "https://example.com/a.zip".to_owned()
            ]),
            Some("https://example.com/a.zip".to_owned())
        );
        assert_eq!(
            parse_add_url([
                "raijin".to_owned(),
                "--add=https://example.com/b.zip".to_owned()
            ]),
            Some("https://example.com/b.zip".to_owned())
        );
        // Bare `--add` opens an empty form (clipboard fallback applies).
        assert_eq!(
            parse_add_url(["raijin".to_owned(), "--add".to_owned()]),
            Some(String::new())
        );
        // A following flag is not consumed as the URL.
        assert_eq!(
            parse_add_url([
                "raijin".to_owned(),
                "--add".to_owned(),
                "--minimized".to_owned()
            ]),
            Some(String::new())
        );
        assert_eq!(parse_add_url(["raijin".to_owned()]), None);
    }

    #[test]
    fn monitor_summary_should_count_active_and_sum_speed() {
        use std::collections::BTreeMap;

        let now = 1_700_000_000_000;
        let mut active = BTreeMap::new();
        active.insert(
            DownloadId::new(1),
            DownloadView {
                id: DownloadId::new(1),
                url: "https://example.com/a.zip".to_owned(),
                file_name: "a.zip".to_owned(),
                folder: std::path::PathBuf::from("C:/Downloads"),
                status: DownloadStatus::Downloading,
                downloaded_bytes: Bytes::new(10),
                total_bytes: Some(Bytes::new(100)),
                speed: BytesPerSecond::new(1024),
                eta_seconds: None,
                active_part_count: 1,
                failure: None,
                created_at: now,
                updated_at: now,
            },
        );
        active.insert(
            DownloadId::new(2),
            DownloadView {
                id: DownloadId::new(2),
                url: "https://example.com/b.zip".to_owned(),
                file_name: "b.zip".to_owned(),
                folder: std::path::PathBuf::from("C:/Downloads"),
                status: DownloadStatus::Downloading,
                downloaded_bytes: Bytes::new(20),
                total_bytes: Some(Bytes::new(200)),
                speed: BytesPerSecond::new(2048),
                eta_seconds: None,
                active_part_count: 2,
                failure: None,
                created_at: now,
                updated_at: now,
            },
        );
        let snapshot = MonitorState {
            active,
            completed: std::collections::BTreeMap::from([(
                DownloadId::new(3),
                DownloadView {
                    id: DownloadId::new(3),
                    url: "https://example.com/c.zip".to_owned(),
                    file_name: "c.zip".to_owned(),
                    folder: std::path::PathBuf::from("C:/Downloads"),
                    status: DownloadStatus::Paused,
                    downloaded_bytes: Bytes::new(5),
                    total_bytes: Some(Bytes::new(50)),
                    speed: BytesPerSecond::ZERO,
                    eta_seconds: None,
                    active_part_count: 0,
                    failure: None,
                    created_at: now,
                    updated_at: now,
                },
            )]),
        };

        assert_eq!(summarize_monitor(&snapshot), (2, 1, 3072));
        assert_eq!(summarize_monitor(&MonitorState::default()), (0, 0, 0));
    }
}
