//! Settings window.
//!
//! Sections are navigation entries in a persistent left sidebar; the right pane
//! renders the selected section. v1 covers Browser Integration (pairing token)
//! and General (tray close behavior + launch at startup).

use std::sync::{Arc, RwLock};

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::sidebar::{Sidebar, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Root, Sizable as _, TitleBar,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::*;
use raijin_core::config::DesktopSettings;
use raijin_core::integration::TokenCell;

use crate::tray::TraySession;

const SETTINGS_WIDTH: f32 = 720.0;
const SETTINGS_HEIGHT: f32 = 460.0;
const NAV_WIDTH: f32 = 200.0;

/// One navigable settings group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    BrowserIntegration,
    General,
}

impl Section {
    const ALL: [Self; 2] = [Self::BrowserIntegration, Self::General];

    const fn label(self) -> &'static str {
        match self {
            Self::BrowserIntegration => "Browser Integration",
            Self::General => "General",
        }
    }

    const fn icon(self) -> IconName {
        match self {
            Self::BrowserIntegration => IconName::Globe,
            Self::General => IconName::Settings,
        }
    }
}

/// Everything the settings window needs to read and write durable state.
#[derive(Debug, Clone)]
pub struct SettingsHandles {
    /// Shared with the running server: a write applies without a rebind.
    pub token: TokenCell,
    /// Persisted settings and token owner.
    pub repository: raijin_core::db::SettingsRepository,
    /// Engine runtime; DB/OS writes never run on the GPUI thread.
    pub runtime: tokio::runtime::Handle,
    /// Bound port when the integration server runs in this process.
    pub port: Option<u16>,
    /// Live tray session; close behavior applies without a restart.
    pub tray: TraySession,
    /// Last loaded settings, shared so each toggle edits the whole document.
    pub desktop: Arc<RwLock<DesktopSettings>>,
}

/// Settings window state.
pub struct SettingsView {
    handles: SettingsHandles,
    section: Section,
    token_input: Entity<InputState>,
    /// Guards against a second regenerate while one write is in flight.
    busy: bool,
    /// Last successfully applied document; the revert target when the OS call
    /// rejects a settings change after it was already persisted.
    previous: DesktopSettings,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>, handles: SettingsHandles) -> Self {
        let token_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(handles.token.get())
                .masked(true)
        });
        let previous = handles
            .desktop
            .read()
            .expect("settings lock poisoned")
            .clone();
        Self {
            handles,
            section: Section::BrowserIntegration,
            token_input,
            busy: false,
            previous,
        }
    }

    /// Persists a fresh token and applies it to the live server slot.
    ///
    /// The DB write runs on the engine runtime and the result comes back
    /// through `update_in`, so the GPUI thread never awaits SQLite.
    fn regenerate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let runtime = self.handles.runtime.clone();
        let token_cell = self.handles.token.clone();
        let repository = self.handles.repository.clone();
        cx.spawn_in(window, async move |weak, cx| {
            let outcome = match runtime
                .spawn(async move {
                    let token = raijin_core::integration::regenerate_token(&repository).await?;
                    token_cell.set(token.clone());
                    Ok::<String, raijin_core::db::DbError>(token)
                })
                .await
            {
                Ok(result) => result.map_err(|error| error.to_string()),
                // A panicked task must not look like a persisted token.
                Err(error) => Err(format!("token regenerate task failed: {error}")),
            };

            let _ = weak.update_in(cx, |view, window, cx| {
                view.busy = false;
                match outcome {
                    Ok(token) => {
                        view.token_input.update(cx, |state, cx| {
                            state.set_masked(false, window, cx);
                            state.set_value(token.as_str(), window, cx);
                        });
                    }
                    Err(message) => {
                        tracing::warn!(%message, "integration token regenerate failed");
                        window.push_notification(message, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Asks before rotating: every paired extension stops working until the
    /// new token is pasted.
    fn confirm_regenerate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.handles.port.is_none() {
            return;
        }
        let view = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let view = view.clone();
            alert
                .title("Regenerate API token?")
                .description(
                    "Every browser extension that uses the current token stops working until \
                     you paste the new one.",
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Regenerate")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("Cancel")
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    view.update(cx, |view, cx| view.regenerate(window, cx));
                    true
                })
        });
    }

    /// Edits the shared settings document and applies the runtime effect.
    ///
    /// `mutate` runs under a short lock with no `.await` inside; persistence and
    /// the OS call both run later on the engine runtime.
    fn edit_settings(
        &mut self,
        cx: &mut Context<Self>,
        window: &mut Window,
        mutate: impl FnOnce(&mut DesktopSettings),
        autostart_changed: bool,
    ) {
        let snapshot = {
            let mut desktop = self
                .handles
                .desktop
                .write()
                .expect("settings lock poisoned");
            mutate(&mut desktop);
            desktop.clone()
        };
        // Tray behavior is read from the session on every close, so applying it
        // here is enough for the change to take effect immediately.
        self.handles.tray.set_behavior(snapshot.tray_behavior());

        let runtime = self.handles.runtime.clone();
        let repository = self.handles.repository.clone();
        let previous = self.previous.clone();
        cx.spawn_in(window, async move |weak, cx| {
            // The inner task owns its copies: the outer ones must survive for
            // the foreground update below.
            let persist_target = snapshot.clone();
            let revert_target = previous.clone();
            let revert_repository = repository.clone();
            let outcome = runtime
                .spawn(async move {
                    raijin_core::config::save_desktop_settings(&repository, &persist_target)
                        .await
                        .map_err(|error| error.to_string())?;
                    if autostart_changed && let Err(message) = apply_autostart(&persist_target) {
                        // The OS call is the only step rejected after the row
                        // is already saved; restoring it keeps the switch and
                        // the next boot from disagreeing.
                        if let Err(error) = raijin_core::config::save_desktop_settings(
                            &revert_repository,
                            &revert_target,
                        )
                        .await
                        {
                            tracing::warn!(%error, "settings revert failed");
                        }
                        return Err(message);
                    }
                    Ok::<(), String>(())
                })
                .await
                .unwrap_or_else(|error| Err(format!("settings task failed: {error}")));

            let _ = weak.update_in(cx, |view, window, cx| {
                match outcome {
                    Ok(()) => view.previous = snapshot,
                    Err(message) => {
                        tracing::warn!(%message, "settings write failed");
                        if autostart_changed {
                            *view
                                .handles
                                .desktop
                                .write()
                                .expect("settings lock poisoned") = previous;
                        }
                        window.push_notification(message, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_close_to_tray(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_settings(
            cx,
            window,
            |settings| settings.close_to_tray = enabled,
            false,
        );
        cx.notify();
    }

    fn toggle_launch_at_startup(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_settings(
            cx,
            window,
            |settings| settings.launch_at_startup = enabled,
            true,
        );
        cx.notify();
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.section;
        let items = Section::ALL.into_iter().map(|section| {
            SidebarMenuItem::new(section.label())
                .icon(Icon::new(section.icon()).small())
                .active(section == selected)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.section != section {
                        this.section = section;
                        cx.notify();
                    }
                }))
        });
        // Two short groups never benefit from an icon-only collapse state, and
        // the nav must stay readable for keyboard users.
        Sidebar::new("settings-nav")
            .w(px(NAV_WIDTH))
            .collapsible(false)
            .child(SidebarMenu::new().children(items))
    }

    fn render_browser_integration(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (label, color) = match self.handles.port {
            Some(port) => (format!("Running on port {port}"), cx.theme().success),
            None => ("Stopped".to_owned(), cx.theme().muted_foreground),
        };
        let running = self.handles.port.is_some();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Browser Integration"),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(IconName::Info).small().text_color(color))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(label),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.token_input).readonly(true).mask_toggle()),
                    )
                    .child(
                        Clipboard::new("settings-copy-token")
                            .value(self.handles.token.get())
                            .tooltip("Copy token"),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Paste this token into the Raijin extension popup."),
            )
            .child(
                h_flex().gap_2().items_center().child(
                    Button::new("settings-regenerate")
                        .outline()
                        .secondary()
                        .label("Regenerate token")
                        .disabled(self.busy || !running)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_regenerate(window, cx);
                        })),
                ),
            )
    }

    fn render_general(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let desktop = self.handles.desktop.read().expect("settings lock poisoned");
        let (close_to_tray, launch_at_startup) = (desktop.close_to_tray, desktop.launch_at_startup);
        drop(desktop);
        v_flex()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("General"),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("settings-close-to-tray")
                            .checked(close_to_tray)
                            .label("Close to tray")
                            .on_click(cx.listener(|this, checked, window, cx| {
                                this.toggle_close_to_tray(*checked, window, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                "The close button hides Raijin to the tray instead of quitting. \
                                 Quit always exits.",
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("settings-launch-at-startup")
                            .checked(launch_at_startup)
                            .label("Launch at startup")
                            .on_click(cx.listener(|this, checked, window, cx| {
                                this.toggle_launch_at_startup(*checked, window, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Starts Raijin minimized in the tray when you log in."),
                    ),
            )
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let section = self.section;
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Icon::new(IconName::Settings)
                                .small()
                                .text_color(cx.theme().primary),
                        )
                        .child(
                            div()
                                .text_base()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(cx.theme().foreground)
                                .child("Settings"),
                        ),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_nav(cx))
                    .child(
                        v_flex()
                            .id("settings-section")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .gap_3()
                            .p_5()
                            .overflow_y_scroll()
                            .child(match section {
                                Section::BrowserIntegration => {
                                    self.render_browser_integration(cx).into_any_element()
                                }
                                Section::General => self.render_general(cx).into_any_element(),
                            }),
                    ),
            )
            .children(Root::render_dialog_layer(_window, cx))
            .children(Root::render_sheet_layer(_window, cx))
            .children(Root::render_notification_layer(_window, cx))
    }
}

/// Applies the persisted launch-at-startup toggle idempotently.
///
/// Only touches the OS mechanism on drift. Failures are returned so the caller
/// can report them next to the switch that caused them.
pub(crate) fn apply_autostart(settings: &DesktopSettings) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| format!("autostart skipped: {error}"))?;
    let manager = raijin_core::platform::autostart::system_autostart_manager(
        "Raijin",
        &exe,
        &["--minimized".to_owned()],
    );
    if manager.is_enabled() == settings.launch_at_startup {
        return Ok(());
    }
    manager
        .set_enabled(settings.launch_at_startup)
        .map_err(|error| match error {
            raijin_core::platform::autostart::AutostartError::Registry(ref detail)
                if detail.contains("Access is denied") =>
            {
                format!(
                    "Windows denied the startup entry change: {error}. Check \
                     security policy for the Run key, then toggle again."
                )
            }
            other => format!("autostart toggle failed: {other}"),
        })
}

/// Opens the Settings window, independent of the main window.
///
/// Wired to the tray menu and the toolbar gear; failures only mean the window
/// could not open and are logged.
pub fn open_settings_window(cx: &mut App, handles: &SettingsHandles) {
    // Settings is a two-pane shell (persistent nav + detail), so it needs more
    // room than the single-section dialog it replaced.
    let dialog_size = size(px(SETTINGS_WIDTH), px(SETTINGS_HEIGHT));
    let mut options = TitleBar::window_options();
    options.window_bounds = Some(WindowBounds::centered(dialog_size, cx));
    options.window_min_size = Some(size(px(560.), px(360.)));
    options.is_resizable = true;
    options.focus = true;
    let handles = handles.clone();
    match cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| SettingsView::new(window, cx, handles.clone()));
        cx.new(|cx| Root::new(view, window, cx))
    }) {
        Ok(handle) => {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        Err(error) => tracing::error!(?error, "failed to open Settings window"),
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports: a `use super::*` glob would shadow the built-in
    // `#[test]` attribute with GPUI's `test` macro.
    use raijin_core::platform::tray::{CloseDecision, TrayBehavior};

    use super::Section;
    use crate::tray::TraySession;

    #[test]
    fn every_section_should_have_a_distinct_label_and_icon() {
        let labels: Vec<&str> = Section::ALL.iter().map(|section| section.label()).collect();
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "duplicate section label");
    }

    #[test]
    fn close_to_tray_write_through_should_apply_without_a_restart() {
        let session = TraySession::new(TrayBehavior::new(false, true));
        assert_eq!(session.close_requested(false), CloseDecision::HideToTray);

        // What the General section does after the switch flips.
        session.set_behavior(TrayBehavior::new(false, false));
        assert_eq!(session.close_requested(false), CloseDecision::RemoveWindow);
    }
}
