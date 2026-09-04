use thiserror::Error;

/// Result type for tray backend operations.
pub type TrayResult<T> = Result<T, TrayError>;

/// Tray backend errors.
#[derive(Debug, Error)]
pub enum TrayError {
    /// Native tray item could not be created or updated.
    #[error("tray backend failed: {0}")]
    Backend(String),
    /// Platform does not support a native tray icon.
    #[error("system tray is not supported on this platform")]
    Unsupported,
}

/// Backend-neutral tray event produced by native callbacks.
///
/// Direction: native tray -> app. The UI layer forwards these to engine
/// commands or window actions; backends never touch engine state directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Primary action (left click) or "Show" menu item.
    ShowRequested,
    /// Pause every active download.
    PauseAllRequested,
    /// Resume every paused download.
    ResumeAllRequested,
    /// Reveal the download folder in the file manager.
    OpenFolderRequested,
    /// Open the settings page.
    OpenSettingsRequested,
    /// Quit the application.
    QuitRequested,
}

/// Stable identifier for a tray menu action.
///
/// Labels live in [`TrayMenuItem`] so backends can render them; the id is
/// what maps a native click back to a [`TrayEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayActionId {
    /// Show (or hide) the main window.
    Show,
    /// Pause every active download.
    PauseAll,
    /// Resume every paused download.
    ResumeAll,
    /// Reveal the download folder.
    OpenFolder,
    /// Open settings.
    OpenSettings,
    /// Quit the application.
    Quit,
}

impl TrayActionId {
    /// Maps the action to the event the app must handle.
    #[must_use]
    pub const fn event(self) -> TrayEvent {
        match self {
            Self::Show => TrayEvent::ShowRequested,
            Self::PauseAll => TrayEvent::PauseAllRequested,
            Self::ResumeAll => TrayEvent::ResumeAllRequested,
            Self::OpenFolder => TrayEvent::OpenFolderRequested,
            Self::OpenSettings => TrayEvent::OpenSettingsRequested,
            Self::Quit => TrayEvent::QuitRequested,
        }
    }
}

/// One tray menu entry, mirroring ABDM `Tray.kt` (`Item` / `SubMenu` /
/// `Divider`) so the future native menu is a mechanical translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayMenuItem {
    /// Clickable action with an enabled flag.
    Action {
        /// Stable action id.
        id: TrayActionId,
        /// Display label.
        label: String,
        /// Disabled items render greyed out and ignore clicks.
        enabled: bool,
    },
    /// Non-clickable visual divider.
    Separator,
    /// Nested submenu with its own items.
    Submenu {
        /// Display label.
        label: String,
        /// Disabled submenus render greyed out with all children inert.
        enabled: bool,
        /// Nested entries (separators and further submenus allowed).
        items: Vec<TrayMenuItem>,
    },
}

impl TrayMenuItem {
    /// Builds an enabled action item.
    #[must_use]
    pub fn action(id: TrayActionId, label: impl Into<String>) -> Self {
        Self::Action {
            id,
            label: label.into(),
            enabled: true,
        }
    }

    /// Builds an action item with an explicit enabled flag.
    #[must_use]
    pub fn action_enabled(id: TrayActionId, label: impl Into<String>, enabled: bool) -> Self {
        Self::Action {
            id,
            label: label.into(),
            enabled,
        }
    }

    /// Counts clickable actions recursively, including nested submenu items.
    #[must_use]
    pub fn action_count(&self) -> usize {
        match self {
            Self::Action { .. } => 1,
            Self::Separator => 0,
            Self::Submenu { items, .. } => items.iter().map(Self::action_count).sum(),
        }
    }
}

/// Ordered tray menu contents (top to bottom).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayMenu {
    items: Vec<TrayMenuItem>,
}

impl TrayMenu {
    /// Creates an empty menu.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one entry.
    pub fn push(&mut self, item: TrayMenuItem) {
        self.items.push(item);
    }

    /// Returns menu entries in display order.
    #[must_use]
    pub fn items(&self) -> &[TrayMenuItem] {
        &self.items
    }

    /// Counts clickable actions recursively.
    #[must_use]
    pub fn action_count(&self) -> usize {
        self.items.iter().map(TrayMenuItem::action_count).sum()
    }

    /// Builds the default Raijin menu from `plan.md` M6.
    ///
    /// Layout: Show/Hide, separator, Pause all / Resume all (enabled flags
    /// follow engine state so the menu never offers a no-op), separator,
    /// Open folder, Settings, separator, Quit.
    #[must_use]
    pub fn default_menu(has_active: bool, has_paused: bool) -> Self {
        let mut menu = Self::new();
        menu.push(TrayMenuItem::action(TrayActionId::Show, "Show Raijin"));
        menu.push(TrayMenuItem::Separator);
        menu.push(TrayMenuItem::action_enabled(
            TrayActionId::PauseAll,
            "Pause all",
            has_active,
        ));
        menu.push(TrayMenuItem::action_enabled(
            TrayActionId::ResumeAll,
            "Resume all",
            has_paused,
        ));
        menu.push(TrayMenuItem::Separator);
        menu.push(TrayMenuItem::action(
            TrayActionId::OpenFolder,
            "Open downloads folder",
        ));
        menu.push(TrayMenuItem::action(TrayActionId::OpenSettings, "Settings"));
        menu.push(TrayMenuItem::Separator);
        menu.push(TrayMenuItem::action(TrayActionId::Quit, "Quit"));
        menu
    }
}

/// Icon rendering policy. macOS tray icons must be monochrome vectors;
/// other platforms use the full-color icon (ABDM `Tray.kt` rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIconKind {
    /// Full-color icon (Windows, Linux).
    Color,
    /// Monochrome template icon (macOS).
    Monochrome,
}

/// Returns the icon policy for the compiling platform.
#[must_use]
pub const fn tray_icon_kind() -> TrayIconKind {
    if cfg!(target_os = "macos") {
        TrayIconKind::Monochrome
    } else {
        TrayIconKind::Color
    }
}

/// Builds the tray tooltip: `"<app> — <state>"`.
///
/// Shows active count + throughput while downloading, otherwise idle.
/// Throughput formatting stays plain (`B/s` units) because this layer is
/// UI-free; the GPUI shell may replace it with its rich formatter.
#[must_use]
pub fn tray_tooltip(app_name: &str, active_count: usize, total_speed_bps: u64) -> String {
    if active_count == 0 {
        return format!("{app_name} — Idle");
    }
    let speed = format_plain_speed(total_speed_bps);
    let downloads = if active_count == 1 {
        "1 download"
    } else {
        &format!("{active_count} downloads")
    };
    // `downloads` borrows either a static or a temporary; clone into owned
    // text so both arms share one return type without lifetime juggling.
    format!("{app_name} — {downloads} · {speed}")
}

fn format_plain_speed(bytes_per_second: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let value = bytes_per_second as f64;
    if value >= GIB {
        format!("{:.1} GiB/s", value / GIB)
    } else if value >= MIB {
        format!("{:.1} MiB/s", value / MIB)
    } else if value >= KIB {
        format!("{:.1} KiB/s", value / KIB)
    } else {
        format!("{bytes_per_second} B/s")
    }
}

/// Where the main window currently lives.
///
/// `hide` (suspend) keeps the root view + GPU resources alive, so Show is
/// cheap (re-show + re-subscribe). `remove_window` destroys them, so Show
/// must rebuild from monitor state + DB. See `plan.md` M6 window rule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WindowVisibility {
    /// Window open and shown (normal state).
    #[default]
    Visible,
    /// Window hidden to tray; root view state (selection, scroll, drafts)
    /// is preserved and Show restores it without rebuild.
    HiddenToTray,
    /// Window destroyed via `remove_window`; Show must re-create it from
    /// monitor state + DB and must not reuse cached entities.
    Closed,
}

impl WindowVisibility {
    /// True while the window is hidden to tray (low-resource tray mode).
    #[must_use]
    pub const fn is_hidden(self) -> bool {
        matches!(self, Self::HiddenToTray)
    }

    /// True when monitor polling + repaints should run (window visible).
    ///
    /// Tray mode sheds `DataTable`/`VirtualList` subscriptions, progress
    /// repaints, and animations; only the tray event handler + essential
    /// timers (tooltip refresh) stay alive.
    #[must_use]
    pub const fn should_poll(self) -> bool {
        matches!(self, Self::Visible)
    }

    /// True when Show restores preserved view state (no rebuild needed).
    #[must_use]
    pub const fn preserves_state_on_show(self) -> bool {
        matches!(self, Self::HiddenToTray)
    }

    /// True when Show must rebuild from monitor state + DB.
    #[must_use]
    pub const fn needs_rebuild_on_show(self) -> bool {
        matches!(self, Self::Closed)
    }

    /// Transitions to hidden-to-tray (cheap Show path).
    pub fn hide_to_tray(&mut self) {
        *self = Self::HiddenToTray;
    }

    /// Transitions back to visible. Returns true when preserved view state
    /// can be reused (hide path) vs a full rebuild (closed path).
    pub fn show(&mut self) -> bool {
        let preserves = self.preserves_state_on_show();
        *self = Self::Visible;
        preserves
    }

    /// Transitions to destroyed (expensive Show path: rebuild from monitor).
    pub fn close(&mut self) {
        *self = Self::Closed;
    }
}

/// User-configurable minimize/close behavior (persisted in settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrayBehavior {
    /// Minimize button hides to tray instead of taskbar-minimizing.
    pub minimize_to_tray: bool,
    /// Close button hides to tray instead of quitting.
    pub close_to_tray: bool,
}

impl TrayBehavior {
    /// Creates a behavior pair; defaults match `DesktopSettings`.
    #[must_use]
    pub const fn new(minimize_to_tray: bool, close_to_tray: bool) -> Self {
        Self {
            minimize_to_tray,
            close_to_tray,
        }
    }

    /// Resolves a minimize request: hide to tray or plain minimize.
    #[must_use]
    pub const fn on_minimize(self) -> MinimizeDecision {
        if self.minimize_to_tray {
            MinimizeDecision::HideToTray
        } else {
            MinimizeDecision::MinimizeWindow
        }
    }

    /// Resolves a close request: hide to tray, or destroy the window.
    ///
    /// `quit_requested` covers explicit Quit (tray menu, shortcut): it
    /// always destroys, ignoring `close_to_tray`.
    #[must_use]
    pub const fn on_close(self, quit_requested: bool) -> CloseDecision {
        if !quit_requested && self.close_to_tray {
            CloseDecision::HideToTray
        } else {
            CloseDecision::RemoveWindow
        }
    }
}

/// Outcome of a minimize request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinimizeDecision {
    /// `App::hide()`: window suspended, engine keeps running, Show cheap.
    HideToTray,
    /// `Window::minimize_window()`: plain OS taskbar minimize.
    MinimizeWindow,
}

/// Outcome of a close request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// `App::hide()`: tray keeps running with engine state intact.
    HideToTray,
    /// `Window::remove_window()`: Show must rebuild from monitor + DB.
    RemoveWindow,
}

/// Native tray backend. Implemented by the `gpui-tray` adapter (UI crate);
/// `core` only defines the contract plus test doubles.
pub trait TrayBackend {
    /// Replaces the native menu contents.
    fn apply_menu(&mut self, menu: &TrayMenu);
    /// Replaces the native tooltip text.
    fn apply_tooltip(&mut self, tooltip: &str);
    /// Backend identifier for diagnostics (`"memory"`, `"noop"`, `"gpui-tray"`).
    fn backend_name(&self) -> &'static str;
}

/// In-memory backend recording the last applied state (tests, previews).
#[derive(Debug, Default)]
pub struct MemoryTrayBackend {
    last_menu: Option<TrayMenu>,
    last_tooltip: Option<String>,
    apply_count: usize,
}

impl MemoryTrayBackend {
    /// Creates an empty backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Last applied menu, if any.
    #[must_use]
    pub fn last_menu(&self) -> Option<&TrayMenu> {
        self.last_menu.as_ref()
    }

    /// Last applied tooltip, if any.
    #[must_use]
    pub fn last_tooltip(&self) -> Option<&str> {
        self.last_tooltip.as_deref()
    }

    /// Number of `apply_*` calls (menu + tooltip each count once).
    #[must_use]
    pub const fn apply_count(&self) -> usize {
        self.apply_count
    }
}

impl TrayBackend for MemoryTrayBackend {
    fn apply_menu(&mut self, menu: &TrayMenu) {
        self.last_menu = Some(menu.clone());
        self.apply_count += 1;
    }

    fn apply_tooltip(&mut self, tooltip: &str) {
        self.last_tooltip = Some(tooltip.to_owned());
        self.apply_count += 1;
    }

    fn backend_name(&self) -> &'static str {
        "memory"
    }
}

/// Fallback backend that drops updates (headless, unsupported platforms).
///
/// Used until the native `gpui-tray` adapter lands; keeps tray-mode window
/// lifecycle fully functional without an OS icon.
#[derive(Debug, Default)]
pub struct NoopTrayBackend;

impl TrayBackend for NoopTrayBackend {
    fn apply_menu(&mut self, _menu: &TrayMenu) {}

    fn apply_tooltip(&mut self, _tooltip: &str) {}

    fn backend_name(&self) -> &'static str {
        "noop"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_menu_should_list_plan_items_with_state_flags() {
        let menu = TrayMenu::default_menu(true, false);
        let labels: Vec<String> = menu
            .items()
            .iter()
            .filter_map(|item| match item {
                TrayMenuItem::Action { label, .. } => Some(label.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(
            labels,
            vec![
                "Show Raijin",
                "Pause all",
                "Resume all",
                "Open downloads folder",
                "Settings",
                "Quit",
            ]
        );
        // Pause enabled (active work), Resume disabled (nothing paused).
        let enabled = |id| {
            menu.items().iter().any(|item| match item {
                TrayMenuItem::Action {
                    id: item_id,
                    enabled: true,
                    ..
                } => *item_id == id,
                _ => false,
            })
        };
        assert!(enabled(TrayActionId::PauseAll));
        assert!(!enabled(TrayActionId::ResumeAll));
        assert_eq!(menu.action_count(), 6);
        assert!(
            menu.items().contains(&TrayMenuItem::Separator),
            "menu must contain separators"
        );
    }

    #[test]
    fn resume_only_menu_should_flip_enabled_flags() {
        let menu = TrayMenu::default_menu(false, true);

        let enabled = |id| {
            menu.items().iter().any(|item| match item {
                TrayMenuItem::Action {
                    id: item_id,
                    enabled: true,
                    ..
                } => *item_id == id,
                _ => false,
            })
        };
        assert!(!enabled(TrayActionId::PauseAll));
        assert!(enabled(TrayActionId::ResumeAll));
    }

    #[test]
    fn submenu_should_count_nested_actions() {
        let mut menu = TrayMenu::new();
        menu.push(TrayMenuItem::Submenu {
            label: "Tools".to_owned(),
            enabled: true,
            items: vec![
                TrayMenuItem::action(TrayActionId::OpenFolder, "Open folder"),
                TrayMenuItem::Separator,
                TrayMenuItem::action(TrayActionId::OpenSettings, "Settings"),
            ],
        });

        assert_eq!(menu.action_count(), 2);
    }

    #[test]
    fn action_ids_should_map_to_matching_events() {
        assert_eq!(TrayActionId::Show.event(), TrayEvent::ShowRequested);
        assert_eq!(TrayActionId::PauseAll.event(), TrayEvent::PauseAllRequested);
        assert_eq!(TrayActionId::Quit.event(), TrayEvent::QuitRequested);
    }

    #[test]
    fn tooltip_should_show_idle_or_active_state() {
        assert_eq!(tray_tooltip("Raijin", 0, 0), "Raijin — Idle");
        assert_eq!(
            tray_tooltip("Raijin", 1, 512),
            "Raijin — 1 download · 512 B/s"
        );
        assert_eq!(
            tray_tooltip("Raijin", 2, 2 * 1024 * 1024),
            "Raijin — 2 downloads · 2.0 MiB/s"
        );
    }

    #[test]
    fn icon_policy_should_be_monochrome_only_on_macos() {
        assert_eq!(
            tray_icon_kind(),
            if cfg!(target_os = "macos") {
                TrayIconKind::Monochrome
            } else {
                TrayIconKind::Color
            }
        );
    }

    #[test]
    fn hide_then_show_should_preserve_state_without_rebuild() {
        let mut visibility = WindowVisibility::Visible;
        assert!(visibility.should_poll());

        visibility.hide_to_tray();
        assert!(visibility.is_hidden());
        assert!(!visibility.should_poll());

        let preserves = visibility.show();
        assert!(preserves, "hide path must preserve view state");
        assert_eq!(visibility, WindowVisibility::Visible);
        assert!(visibility.should_poll());
    }

    #[test]
    fn close_then_show_should_require_rebuild_from_monitor() {
        let mut visibility = WindowVisibility::Visible;
        visibility.close();
        assert_eq!(visibility, WindowVisibility::Closed);

        let preserves = visibility.show();
        assert!(!preserves, "closed path must rebuild from monitor + DB");
        assert!(!visibility.needs_rebuild_on_show());
        assert_eq!(visibility, WindowVisibility::Visible);
    }

    #[test]
    fn behavior_should_route_minimize_and_close_requests() {
        let tray_first = TrayBehavior::new(true, true);
        assert_eq!(tray_first.on_minimize(), MinimizeDecision::HideToTray);
        assert_eq!(tray_first.on_close(false), CloseDecision::HideToTray);
        // Explicit quit always destroys, even with close-to-tray on.
        assert_eq!(tray_first.on_close(true), CloseDecision::RemoveWindow);

        let plain = TrayBehavior::new(false, false);
        assert_eq!(plain.on_minimize(), MinimizeDecision::MinimizeWindow);
        assert_eq!(plain.on_close(false), CloseDecision::RemoveWindow);
    }

    #[test]
    fn memory_backend_should_record_last_menu_and_tooltip() {
        let mut backend = MemoryTrayBackend::new();
        let menu = TrayMenu::default_menu(true, true);

        backend.apply_menu(&menu);
        backend.apply_tooltip("Raijin — Idle");

        assert_eq!(backend.last_menu(), Some(&menu));
        assert_eq!(backend.last_tooltip(), Some("Raijin — Idle"));
        assert_eq!(backend.apply_count(), 2);
        assert_eq!(backend.backend_name(), "memory");
    }
}
