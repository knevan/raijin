#![windows_subsystem = "windows"]
#![deny(
    clippy::panic,
    clippy::panicking_unwrap,
    clippy::redundant_clone,
    clippy::implicit_clone,
    clippy::perf,
    clippy::large_types_passed_by_value,
    clippy::large_futures,
    clippy::trivially_copy_pass_by_ref,
    clippy::clone_on_ref_ptr
)]
#![cfg_attr(test, allow(clippy::panic))]

mod add_download;
mod bridge;
mod category;
mod downloads;
mod format;
mod integration_bridge;
mod main_view;
mod queue_picker;
mod settings;
mod theme;
mod tray;

use std::time::Duration;

use gpui_kit::component::{Theme, ThemeMode, TitleBar};
use gpui_kit::*;
use raijin_core::integration::DEFAULT_PORT;
use raijin_core::platform::deeplink::{ProtocolAction, parse_protocol_args, set_registered};
use raijin_core::platform::tray::CloseDecision;

use crate::add_download::open_add_download_window;
use crate::bridge::{EngineCommand, EngineHandles};
use crate::integration_bridge::Started;
use crate::main_view::MainView;
use crate::settings::{SettingsHandles, apply_autostart, open_settings_window};
use crate::tray::{
    OsCloseOutcome, TraySession, parse_add_url, parse_start_from_autostart, parse_start_minimized,
    start_windowless,
};

gpui_kit::actions!(
    tray,
    [
        ShowWindow,
        HideWindowToTray,
        NewDownloadWindow,
        PauseAllDownloads,
        ResumeAllDownloads,
        OpenDownloadsFolder,
        OpenAppSettings,
        QuitFromTray,
    ]
);

fn startup_failed(context: &str, error: impl std::fmt::Display) -> ! {
    eprintln!("{}", raijin_core::logging::record_fatal(context, error));
    std::process::exit(1);
}

#[hotpath::main(format = "json", output_path = "hotpath-ui.json")]
fn main() {
    // The GUI binary has no console (GUI subsystem), so routine logging goes
    // to a file in debug builds and every crash goes to `crash.log`.
    raijin_core::logging::install_panic_hook();
    raijin_core::logging::init_app_tracing();
    let args: Vec<String> = std::env::args().collect();
    let protocol_action = parse_protocol_args(&args)
        .unwrap_or_else(|error| startup_failed("invalid app link arguments", error));
    if let Some(action @ (ProtocolAction::Register | ProtocolAction::Unregister)) = protocol_action
    {
        let executable = std::env::current_exe()
            .unwrap_or_else(|error| startup_failed("could not locate Raijin executable", error));
        set_registered(&executable, action == ProtocolAction::Register)
            .unwrap_or_else(|error| startup_failed("protocol registration failed", error));
        tracing::info!(?action, "protocol registration command completed");
        return;
    }
    let open_integration = protocol_action == Some(ProtocolAction::OpenSettings);
    #[cfg(not(windows))]
    if open_integration {
        startup_failed(
            "app links are currently supported only on Windows",
            "unsupported platform",
        );
    }
    let start_minimized = parse_start_minimized(args.iter().cloned());
    let start_from_autostart = parse_start_from_autostart(args.iter().cloned());
    // Standalone Add Download entry (browser-extension catch without the
    // main table): only the engine + tray icon boot, never `MainView`.
    let add_url = parse_add_url(args.iter().cloned());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("raijin-engine")
        .worker_threads(4)
        .build()
        .unwrap_or_else(|error| {
            eprintln!(
                "{}",
                raijin_core::logging::record_fatal("failed to start engine runtime", error)
            );
            std::process::exit(1);
        });
    #[cfg(windows)]
    let mut instance = {
        use raijin_core::platform::instance::{DesktopCommand, claim};
        let command = if open_integration {
            DesktopCommand::BrowserIntegration
        } else if let Some(url) = &add_url {
            DesktopCommand::Add(url.clone())
        } else if start_minimized || start_from_autostart {
            DesktopCommand::Background
        } else {
            DesktopCommand::Activate
        };
        let data_dir = raijin_core::platform::app_data_dir().unwrap_or_else(|error| {
            startup_failed("could not resolve instance data directory", error)
        });
        match runtime
            .block_on(claim(&data_dir, &command))
            .unwrap_or_else(|error| {
                startup_failed("could not claim or contact the desktop instance", error)
            }) {
            Some(instance) => instance,
            None => return,
        }
    };
    #[cfg(windows)]
    let instance_receiver = instance
        .take_receiver()
        .expect("new instance owns a receiver");
    let (handles, tray, integration, settings) = runtime
        .block_on(bootstrap_engine())
        .unwrap_or_else(|error| {
            eprintln!(
                "{}",
                raijin_core::logging::record_fatal("failed to bootstrap engine", error)
            );
            std::process::exit(1);
        });
    // `run` blocks until explicit quit, so the local runtime stays alive for
    // the whole application lifetime (including windowless tray mode).
    // Views only keep its `Handle`.
    //
    // `with_assets` is required: without the icon bundle every `Icon` renders
    // blank. Dark mode matches the product sketch until a theme setting lands.
    //
    // `QuitMode::Explicit` is required for tray mode: the event loop must
    // survive `remove_window` (Windows/Linux hide path) and run windowless
    // under `--minimized`. Quit flows through the tray menu / OS requests.
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            Theme::change(ThemeMode::Dark, None, cx);
            crate::theme::apply_product_theme(cx);
            // Freshness marker + effective behavior for close-path diagnosis.
            let behavior = tray.behavior();
            tracing::info!(
                minimize_to_tray = behavior.minimize_to_tray,
                close_to_tray = behavior.close_to_tray,
                start_minimized,
                start_from_autostart,
                "raijin starting"
            );
            // OS quit requests (dock, taskbar, session end) must stand the
            // window-closed observer down, or it would reopen mid-quit.
            let quit_session = tray.clone();
            let quit_subscription = cx.on_app_quit(move |_| {
                quit_session.set_quitting();
                async {}
            });
            std::mem::forget(quit_subscription);
            // OS chrome close bypasses in-app handlers on Windows/macOS, so
            // enforce `close_to_tray` here: reopen from monitor + DB, or quit
            // for plain closes. The subscription must outlive `run`, hence
            // the deliberate `forget` (it dies with the process).
            let reopen_handles = handles.clone();
            let reopen_tray = tray.clone();
            let reopen_settings = settings.clone();
            let closed_subscription = cx.on_window_closed(move |cx, _| {
                // Add Download windows are standalone: their close must not
                // drive the main-window lifecycle. While any window remains,
                // only disarm a stale hide flag and stand down.
                if !cx.windows().is_empty() {
                    reopen_tray.cancel_programmatic_close();
                    return;
                }
                let outcome = reopen_tray.os_window_closed();
                tracing::debug!(?outcome, "window closed");
                match outcome {
                    OsCloseOutcome::Reopen => {
                        open_main_window(cx, &reopen_handles, &reopen_tray, &reopen_settings);
                    }
                    OsCloseOutcome::Quit => cx.quit(),
                    OsCloseOutcome::Nothing => {}
                }
            });
            std::mem::forget(closed_subscription);
            let native_tray = match build_native_tray(cx, &tray) {
                Ok(icon) => Some(icon),
                Err(error) => {
                    // Without an icon a windowless run is a trap (nothing to
                    // restore from), so degrade to plain app behavior: the
                    // close button quits instead of hiding to tray.
                    tracing::warn!(?error, "tray icon unavailable, hiding to tray disabled");
                    tray.set_behavior(raijin_core::platform::tray::TrayBehavior::new(false, false));
                    None
                }
            };
            register_tray_actions(cx, &handles, &tray, &settings, native_tray.clone());
            #[cfg(windows)]
            crate::bridge::spawn_launch_dispatcher(
                cx,
                instance_receiver,
                handles.clone(),
                tray.clone(),
                settings.clone(),
            );
            // Browser-extension requests arrive over localhost; the dispatcher
            // stays on this thread and only hands work to the engine runtime.
            if let Some(started) = integration {
                let dialog_handles = handles.clone();
                crate::integration_bridge::spawn_dispatcher(
                    cx,
                    started,
                    Box::new(move |cx, prepared, pending| {
                        let mut windows: Vec<AnyWindowHandle> = Vec::new();
                        for item in prepared {
                            match crate::add_download::open_integration_download_window(
                                cx,
                                &dialog_handles,
                                item,
                                pending.clone(),
                            ) {
                                Ok(window) => windows.push(window),
                                Err(error) => {
                                    for handle in windows {
                                        let _ = handle
                                            .update(cx, |_, window, _| window.remove_window());
                                    }
                                    return Err(error);
                                }
                            }
                        }
                        Ok(())
                    }),
                );
            }
            // A windowless start needs a live tray icon; without one there
            // would be no way to restore or quit. `--startup` (autostart)
            // additionally defers to `close_to_tray`: with the close button
            // set to quit, login opens the main window instead of hiding.
            let windowless_start = start_windowless(
                start_minimized,
                start_from_autostart,
                tray.behavior().close_to_tray,
                native_tray.is_some(),
            );
            if let Some(icon) = native_tray {
                tray.set_tray_icon_live();
                spawn_tray_sync(cx, icon, tray.clone(), handles.monitor.clone());
            }
            // `--add` opens only the Add Download window: the main table
            // (DataTable, monitor poll, storage scan) never loads, so an
            // extension-caught link needs no main-window cost. An explicit
            // `--add` wins over `--minimized`. Closing the Add window falls
            // back to tray (or reopens main when no icon is live).
            if open_integration {
                crate::settings::open_browser_integration(cx, &settings);
            } else if let Some(url) = add_url.clone() {
                let handles = handles.clone();
                cx.spawn(async move |cx| {
                    cx.update(|cx| open_add_download_window(cx, &handles, &url));
                })
                .detach();
            } else if !windowless_start {
                let handles = handles.clone();
                let tray = tray.clone();
                let settings = settings.clone();
                cx.spawn(async move |cx| {
                    cx.update(|cx| {
                        open_main_window(cx, &handles, &tray, &settings);
                    })
                })
                .detach();
            }
        });
}

/// Registers the GPUI actions dispatched by tray menu clicks and the primary
/// (left-click) action. All handlers are `'static` clones of the engine and
/// session handles; native callbacks never touch engine state directly.
fn register_tray_actions(
    cx: &mut App,
    handles: &EngineHandles,
    session: &TraySession,
    settings: &SettingsHandles,
    native_tray: Option<gpui_tray::Tray>,
) {
    let show_handles = handles.clone();
    let show_session = session.clone();
    let show_settings = settings.clone();
    cx.on_action(move |_: &ShowWindow, cx: &mut App| {
        // Show-only: reopen when windowless, otherwise restore + focus.
        // Window-level `activate` restores taskbar-minimized windows on
        // Windows (app-level `activate` is a no-op there).
        if cx.windows().is_empty() {
            tracing::debug!("tray Show: reopening window");
            open_main_window(cx, &show_handles, &show_session, &show_settings);
        } else {
            tracing::debug!("tray Show: activating window");
            show_session.mark_shown();
            for window in cx.windows() {
                let _ = window.update(cx, |_, window, _| window.activate_window());
            }
        }
    });

    let hide_session = session.clone();
    cx.on_action(move |_: &HideWindowToTray, cx: &mut App| {
        tracing::debug!("tray Hide: hiding window");
        hide_window_to_tray(&hide_session, cx);
    });

    // Standalone Add Download window from the tray: works with the main
    // window hidden or never opened (extension catch without main cost).
    // An empty seed lets the window fall back to a valid clipboard URL.
    let add_handles = handles.clone();
    cx.on_action(move |_: &NewDownloadWindow, cx: &mut App| {
        tracing::debug!("tray New Download: opening Add window");
        open_add_download_window(cx, &add_handles, "");
    });

    let pause_handles = handles.clone();
    cx.on_action(move |_: &PauseAllDownloads, _: &mut App| {
        pause_handles.dispatch(EngineCommand::PauseAll);
    });

    let resume_handles = handles.clone();
    cx.on_action(move |_: &ResumeAllDownloads, _: &mut App| {
        resume_handles.dispatch(EngineCommand::ResumeAll);
    });

    let folder_handles = handles.clone();
    cx.on_action(move |_: &OpenDownloadsFolder, _: &mut App| {
        if let Err(error) = raijin_core::platform::open_folder(&folder_handles.default_folder) {
            tracing::warn!(?error, "tray open folder failed");
        }
    });

    // Settings opens as its own window so it works from the tray menu even
    // when the main window was never created.
    let settings_handles = settings.clone();
    cx.on_action(move |_: &OpenAppSettings, cx: &mut App| {
        tracing::debug!("tray Settings: opening Settings window");
        open_settings_window(cx, &settings_handles);
    });

    let quit_session = session.clone();
    let quit_tray = native_tray;
    cx.on_action(move |_: &QuitFromTray, cx: &mut App| {
        tracing::debug!("tray Quit requested");
        // Deterministic icon removal first: dropping it during teardown
        // would leave a ghost icon until the next cursor hover.
        if let Some(tray) = quit_tray.as_ref() {
            let _ = tray.close(cx);
        }
        quit_session.set_quitting();
        cx.quit();
    });
}

/// Hides the window to tray: suspend on macOS, deferred programmatic
/// destroy + rebuild-on-show elsewhere (GPUI implements `hide()` on macOS
/// only; Windows carries a `// todo(windows)` stub).
///
/// The destroy is deferred to a spawned task because close hooks may run
/// with the window checked out of GPUI's map, where a nested remove
/// silently no-ops and would strand an armed stand-down flag. Each failed
/// attempt disarms first and retries boundedly.
///
/// `cfg!` (not `#[cfg]`) keeps one compiled code path on all platforms.
fn hide_window_to_tray(session: &TraySession, cx: &mut App) {
    if cfg!(target_os = "macos") {
        session.note_hidden_to_tray();
        cx.hide();
        return;
    }
    tracing::debug!("tray Hide: deferring remove");
    let deferred = session.clone();
    cx.spawn(async move |cx| {
        for attempt in 0..20_u32 {
            if deferred.is_quitting() {
                break;
            }
            let removed = cx.update(|cx| {
                if cx.windows().is_empty() {
                    return false;
                }
                deferred.begin_programmatic_close();
                let mut removed = false;
                for window in cx.windows() {
                    if window
                        .update(cx, |_, window, _| window.remove_window())
                        .is_ok()
                    {
                        removed = true;
                    }
                }
                if !removed {
                    deferred.cancel_programmatic_close();
                }
                removed
            });
            if removed {
                tracing::debug!("tray Hide: window removed");
                break;
            }
            tracing::debug!(attempt, "tray Hide: remove deferred, retrying");
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
        }
    })
    .detach();
}

/// Builds the native tray icon from the session's activity cache.
fn build_native_tray(cx: &mut App, session: &TraySession) -> gpui_tray::Result<gpui_tray::Tray> {
    let menu_session = session.clone();
    gpui_tray::Tray::builder()
        .icon(tray_icon()?)
        .title("Raijin")
        .tooltip(session.update_activity(0, 0, 0))
        .on_activate(ShowWindow)
        .menu(move |cx: &mut App| {
            // Show is show-only (restore/focus); hiding is the explicit
            // item below, so a minimized window is never hidden by accident.
            let has_window = !cx.windows().is_empty();
            vec![
                MenuItem::action("Show window", ShowWindow),
                MenuItem::action("Hide to tray", HideWindowToTray).disabled(!has_window),
                MenuItem::action("New Download…", NewDownloadWindow),
                MenuItem::separator(),
                MenuItem::action("Pause all", PauseAllDownloads)
                    .disabled(!menu_session.has_active()),
                MenuItem::action("Resume all", ResumeAllDownloads)
                    .disabled(!menu_session.has_paused()),
                MenuItem::separator(),
                MenuItem::action("Open downloads folder", OpenDownloadsFolder),
                MenuItem::action("Settings", OpenAppSettings),
                MenuItem::separator(),
                MenuItem::action("Quit", QuitFromTray),
            ]
        })
        .build(cx)
}

/// Keeps the native icon in step with engine activity.
///
/// The [`gpui_tray::Tray`] diffs tooltip snapshots internally, so
/// pushing the cached text every second is cheap; menu rebuilds are gated on
/// activity-flag changes. The loop exits on explicit quit; dropping the last
/// window does not stop it ([`QuitMode::Explicit`]).
fn spawn_tray_sync(
    cx: &mut App,
    icon: gpui_tray::Tray,
    session: TraySession,
    monitor: raijin_core::monitor::DownloadMonitorHandle,
) {
    cx.spawn(async move |cx| {
        let mut last_flags = (false, false);
        let mut last_tooltip = String::new();
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if session.is_quitting() {
                break;
            }
            session.update_from_monitor(&monitor.state());
            let tooltip = session.last_tooltip();
            let flags = (session.has_active(), session.has_paused());
            let tooltip_changed = tooltip != last_tooltip;
            // `has_window` is read by the menu builder, which already holds
            // `App`; the session only reports that a transition happened, so
            // this loop never borrows `App` while nothing changed.
            let menu_changed = flags != last_flags || session.take_menu_dirty();
            if tooltip_changed {
                last_tooltip = tooltip;
            }
            last_flags = flags;
            if !tooltip_changed && !menu_changed {
                continue;
            }
            // Errors only mean the icon is gone; the session cache stays warm.
            cx.update(|cx| {
                if tooltip_changed
                    && let Err(error) = icon.set_tooltip(Some(last_tooltip.clone()), cx)
                {
                    tracing::debug!(?error, "tray tooltip update failed");
                }
                if menu_changed && let Err(error) = icon.refresh_menu(cx) {
                    tracing::debug!(?error, "tray menu refresh failed");
                }
            });
        }
    })
    .detach();
}

/// Opens (or re-opens) the main window from monitor state + DB.
///
/// Rebuild path for `remove_window` closes: fresh entities subscribe to the
/// monitor feed anew; engine state is never rebuilt by the UI.
fn open_main_window(
    cx: &mut App,
    handles: &EngineHandles,
    tray: &TraySession,
    settings: &SettingsHandles,
) {
    let root_handles = handles.clone();
    let root_tray = tray.clone();
    let root_settings = settings.clone();
    match gpui_kit::open_window(main_window_options(cx), cx, |window, cx| {
        build_main_view(
            window,
            cx,
            root_handles.clone(),
            root_tray.clone(),
            root_settings.clone(),
        )
    }) {
        Ok((handle, _view)) => {
            tray.mark_shown();
            // Window-level activate foregrounds the reopened window.
            // App-level `activate` is a Windows no-op, and a background
            // reopen makes the first click merely focus the window
            // (indistinguishable from a dead X button).
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        Err(error) => tracing::error!(?error, "failed to open main window"),
    }
}

/// Default main-window options: centered windowed frame, never fullscreen.
fn main_window_options(cx: &App) -> WindowOptions {
    let mut options = TitleBar::window_options();
    options.window_bounds = Some(WindowBounds::centered(size(px(900.), px(600.)), cx));
    options.window_min_size = Some(size(px(600.), px(300.)));
    options
}

/// Builds the main window view and wires the close veto.
///
/// Every opened main window (initial + rebuilds) goes through here so the
/// close behavior stays identical across the app lifetime.
fn build_main_view(
    window: &mut Window,
    cx: &mut App,
    handles: EngineHandles,
    session: TraySession,
    settings: SettingsHandles,
) -> Entity<MainView> {
    register_should_close(window, cx, &session);
    cx.new(|cx| MainView::new(window, cx, handles, session, settings))
}

/// Vetoes the OS close for tray hides (X, Alt+F4, taskbar close) and converts
/// it into a deferred programmatic hide.
///
/// Rationale: an OS-initiated destroy tears the window down outside GPUI's
/// frame processing and logs teardown noise (`window not found` + invalid
/// handles) on this GPUI revision; the minimal repro proves a programmatic
/// `remove_window` closes silently. So the veto keeps the HWND alive and a
/// spawned task removes the window on the next executor turn — the proven
/// clean path — while the closed-observer stands down via
/// [`TraySession::begin_programmatic_close`]. Plain closes proceed to the
/// observer, which quits the app.
fn register_should_close(window: &mut Window, cx: &mut App, session: &TraySession) {
    let hook_session = session.clone();
    window.on_window_should_close(cx, move |_, cx| {
        // Quitting must never be vetoed: taskbar/dock quit and session-end
        // tear windows down through this same hook.
        if hook_session.is_quitting() {
            return true;
        }
        // Entry marker: a failed X with no log at all means WM_CLOSE never
        // reached this hook (OS/modal-loop level), not a logic failure.
        tracing::debug!("tray should_close intercepted");
        match hook_session.close_requested(false) {
            CloseDecision::HideToTray => {
                hide_window_to_tray(&hook_session, cx);
                false
            }
            CloseDecision::RemoveWindow => true,
        }
    });
}

/// Generates the 32x32 RGBA pixel buffer: indigo rounded square with a white "R".
fn tray_icon_rgba() -> Vec<u8> {
    const SIZE: u32 = 32;
    // 5x7 bitmap glyph for "R".
    const GLYPH: [&str; 7] = [
        ".###.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#",
    ];
    const SCALE: u32 = 2;
    const GLYPH_W: u32 = 5 * SCALE;
    const GLYPH_H: u32 = 7 * SCALE;
    const BG: [u8; 4] = [0x4F, 0x6E, 0xFF, 0xFF];
    const FG: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];
    let radius = 7i32;
    for y in 0..SIZE {
        for x in 0..SIZE {
            // Rounded-rect mask.
            let dx = (x as i32 - radius).min((SIZE as i32 - 1 - x as i32) - radius);
            let dy = (y as i32 - radius).min((SIZE as i32 - 1 - y as i32) - radius);
            let inside = dx.max(dy) >= 0
                || dx.max(dy) >= -(radius) && dx * dx + dy * dy <= radius * radius + radius;
            if !inside {
                continue;
            }
            let mut pixel = BG;
            // Centered scaled glyph.
            let gx = x as i32 - (SIZE as i32 - GLYPH_W as i32) / 2;
            let gy = y as i32 - (SIZE as i32 - GLYPH_H as i32) / 2;
            if gx >= 0 && gy >= 0 {
                let cell_x = gx as u32 / SCALE;
                let cell_y = gy as u32 / SCALE;
                if cell_x < 5
                    && cell_y < 7
                    && GLYPH[cell_y as usize].as_bytes()[cell_x as usize] == b'#'
                {
                    pixel = FG;
                }
            }
            let offset = ((y * SIZE + x) * 4) as usize;
            rgba[offset..offset + 4].copy_from_slice(&pixel);
        }
    }
    rgba
}

/// Generates the 32x32 tray icon: indigo rounded square with a white "R".
///
/// Pure pixels, unit-tested below; the native backends convert to
/// `HICON` / SNI pixmap / `NSImage`.
fn tray_icon() -> gpui_tray::Result<gpui_tray::Icon> {
    const SIZE: u32 = 32;
    gpui_tray::Icon::from_rgba(tray_icon_rgba(), SIZE, SIZE).build()
}

/// Bootstraps persistence, settings, the download manager actor, and the monitor.
type Bootstrap = (EngineHandles, TraySession, Option<Started>, SettingsHandles);

async fn bootstrap_engine() -> Result<Bootstrap, String> {
    let database_url = raijin_core::platform::desktop_database_url()
        .map_err(|error| format!("database path unavailable: {error}"))?;
    let pool = raijin_core::db::bootstrap(&database_url)
        .await
        .map_err(|error| format!("database bootstrap failed: {error}"))?;
    let downloads = raijin_core::db::DownloadRepository::new(pool.clone());
    let parts = raijin_core::db::PartRepository::new(pool.clone());
    let settings_repository = raijin_core::db::SettingsRepository::new(pool.clone());
    let (manager, _manager_task, events) =
        raijin_core::download::DownloadManagerHandle::spawn_with_events(
            downloads.clone(),
            parts,
            raijin_core::download::DownloadManagerOptions::default(),
        );
    // Queue admission and the monitor both need the stream; the manager owns
    // the sender, so a second receiver costs one clone of the handle.
    let queue_events = manager.subscribe();
    // The monitor subscribes before boot publishes `BootLoaded`, so the first
    // poll already contains unfinished downloads restored from SQLite.
    let (monitor, _monitor_task) = raijin_core::monitor::DownloadMonitorHandle::spawn(events, None);
    let platform_folder = raijin_core::platform::default_download_dir().unwrap_or_else(|error| {
        tracing::warn!(
            ?error,
            "default download dir unavailable, using current dir"
        );
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    });
    let category_root = raijin_core::platform::download_category_root_dir()
        .unwrap_or_else(|_| platform_folder.clone());
    let fallback =
        raijin_core::config::DesktopSettings::from_defaults(platform_folder, category_root);
    let desktop = raijin_core::config::load_desktop_settings(&settings_repository, fallback).await;
    if let Err(error) = apply_autostart(&desktop) {
        tracing::warn!(%error, "autostart apply on boot failed");
    }
    let tray = TraySession::new(desktop.tray_behavior());
    // The token slot is created regardless of the integration toggle: the
    // Settings window reads and rotates the persisted token either way.
    let token_cell = raijin_core::integration::TokenCell::new(
        raijin_core::integration::load_or_create_token(&settings_repository)
            .await
            .map_err(|error| format!("integration token unavailable: {error}"))?,
    );
    // Queue admission is the only path that starts downloads, so the actor
    // must exist before any add/reorder command can reach the engine.
    let (queue_manager, _queue_task) = raijin_core::queue::QueueManagerHandle::spawn(
        downloads,
        raijin_core::db::QueueRepository::new(pool.clone()),
        manager.clone(),
        queue_events,
        queue_options(&desktop),
    );
    let integration = if integration_enabled(&desktop) {
        start_integration(
            token_cell.clone(),
            desktop.default_download_folder.clone(),
            manager.clone(),
            queue_manager.clone(),
        )
        .await
        .map(Some)?
    } else {
        None
    };
    let port = integration.as_ref().map(|started| started.port);
    let settings = SettingsHandles {
        token: token_cell,
        repository: settings_repository,
        runtime: tokio::runtime::Handle::current(),
        port,
        tray: tray.clone(),
        desktop: std::sync::Arc::new(std::sync::RwLock::new(desktop.clone())),
        window: std::sync::Arc::new(std::sync::Mutex::new(None)),
    };
    Ok((
        EngineHandles {
            default_folder: desktop.default_download_folder.clone(),
            category_root: desktop.category_root.clone(),
            use_category_default: desktop.use_category_by_default,
            manager,
            queues: queue_manager,
            monitor,
            runtime: tokio::runtime::Handle::current(),
        },
        tray,
        integration,
        settings,
    ))
}

/// Builds queue runtime options from the persisted concurrency setting.
///
/// A zero value would mean "no downloads", so it is floored to one slot.
fn queue_options(
    desktop: &raijin_core::config::DesktopSettings,
) -> raijin_core::queue::QueueManagerOptions {
    raijin_core::queue::QueueManagerOptions {
        global_limit: std::num::NonZeroU16::new(desktop.max_concurrent_downloads)
            .unwrap_or(std::num::NonZeroU16::MIN),
        ..raijin_core::queue::QueueManagerOptions::default()
    }
}

/// Returns whether the localhost integration server should run.
///
/// The persisted toggle has no Settings UI yet, so `RAIJIN_INTEGRATION=1` lets
/// the extension be exercised end-to-end before that page lands. The setting
/// stays authoritative once it is on.
fn integration_enabled(settings: &raijin_core::config::DesktopSettings) -> bool {
    if settings.browser_integration_enabled {
        return true;
    }
    matches!(
        std::env::var("RAIJIN_INTEGRATION").as_deref(),
        Ok("1" | "true" | "TRUE" | "yes")
    )
}

/// Binds the integration server and logs the effective port.
///
/// The token is no longer logged: the Settings window is the supported way to
/// read it, so a copy in the tracing output would only be a leak.
async fn start_integration(
    token: raijin_core::integration::TokenCell,
    folder: std::path::PathBuf,
    manager: raijin_core::download::DownloadManagerHandle,
    queue_manager: raijin_core::queue::QueueManagerHandle,
) -> Result<crate::integration_bridge::Started, String> {
    let bridge = crate::integration_bridge::bridge();
    let address = raijin_core::integration::serve(bridge.handle.clone(), token, DEFAULT_PORT)
        .await
        .map_err(|error| format!("integration server bind failed: {error}"))?;
    tracing::info!(
        %address,
        port = address.port(),
        "browser integration listening (Authorization: Bearer <token from Settings>)"
    );
    Ok(crate::integration_bridge::Started {
        receiver: bridge.receiver,
        port: address.port(),
        engine: crate::integration_bridge::IntegrationEngine {
            manager,
            queues: queue_manager,
            runtime: tokio::runtime::Handle::current(),
            default_folder: folder,
        },
    })
}

#[cfg(test)]
mod tests {
    // Explicit imports: a `use super::*` glob would shadow the built-in
    // `#[test]` attribute with GPUI's `test` macro.
    use super::{tray_icon, tray_icon_rgba};

    #[test]
    fn tray_icon_should_be_opaque_centered_mark_with_transparent_corners() {
        let _ = tray_icon().expect("icon must build");
        let rgba = tray_icon_rgba();
        assert_eq!(rgba.len(), 32 * 32 * 4);

        // Corners stay transparent (rounded mask).
        assert_eq!(&rgba[0..4], &[0, 0, 0, 0]);
        // Center carries the white glyph.
        let center = (16 * 32 + 16) * 4;
        assert_eq!(&rgba[center..center + 4], &[0xFF, 0xFF, 0xFF, 0xFF]);
        // At least the background ring is opaque indigo.
        let opaque = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[3] == 0xFF)
            .count();
        assert!(opaque > 32 * 32 / 2, "opaque was {opaque}");
    }
}
