use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, TestAppContext, base};
use raijin_core::config::DesktopSettings;
use raijin_core::db::SettingsRepository;
use raijin_core::integration::{TokenCell, load_or_create_token};
use tokio::runtime::Runtime;

use super::super::{
    Section, SettingsHandles, SettingsView, open_browser_integration, open_settings_window,
};
use crate::tray::TraySession;

fn settings_runtime() -> Runtime {
    // GPUI's deterministic scheduler requires join wakeups on the test thread.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime must start")
}

fn settings_handles(runtime: &Runtime) -> SettingsHandles {
    let repository = runtime.block_on(async {
        SettingsRepository::new(
            raijin_core::db::bootstrap("sqlite::memory:")
                .await
                .expect("test database must bootstrap"),
        )
    });
    let token = runtime
        .block_on(load_or_create_token(&repository))
        .expect("test token must persist");
    let desktop = DesktopSettings::from_defaults("downloads".into(), "categories".into());
    SettingsHandles {
        token: TokenCell::new(token),
        repository,
        runtime: runtime.handle().clone(),
        port: Some(18765),
        tray: TraySession::new(desktop.tray_behavior()),
        desktop: Arc::new(RwLock::new(desktop)),
        window: Arc::new(Mutex::new(None)),
    }
}

fn settings_window(handles: &SettingsHandles) -> AnyWindowHandle {
    handles
        .window
        .lock()
        .expect("test window lock must not be poisoned")
        .expect("Settings must open")
}

#[gpui_kit::test]
fn regenerate_should_confirm_before_rotating_and_persist_the_result(cx: &mut TestAppContext) {
    let runtime = settings_runtime();
    let handles = settings_handles(&runtime);
    let original = handles.token.get();
    cx.update(gpui_kit::init);
    cx.update(|cx| open_settings_window(cx, &handles));
    let handle = settings_window(&handles);

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("settings-regenerate", cx);
        assert!(window.has_active_dialog(cx));
        assert!(window.find("ok").visible());
        assert_eq!(handles.token.get(), original);
        window.click("cancel", cx);
        assert!(!window.has_active_dialog(cx));
        assert_eq!(handles.token.get(), original);
    })
    .expect("Settings window must remain open");
    assert_eq!(
        runtime
            .block_on(load_or_create_token(&handles.repository))
            .expect("cancelled token must remain readable"),
        original
    );

    cx.update_window(handle, |_, window, cx| {
        window.click("settings-regenerate", cx);
        window.press("escape", cx);
        assert!(!window.has_active_dialog(cx));
        assert_eq!(handles.token.get(), original);
        window.click("settings-regenerate", cx);
        window.click("ok", cx);
        assert!(!window.has_active_dialog(cx));
    })
    .expect("Settings window must remain open");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        let finished = cx
            .update_window(handle, |_, window, cx| {
                let root = base::Root::read(window, cx);
                let view = root.view().clone().downcast::<SettingsView>().unwrap();
                let view = view.read(cx);
                !view.busy && view.token_input.read(cx).value().as_ref() != original
            })
            .expect("Settings must survive token rotation");
        if finished {
            break;
        }
        assert!(Instant::now() < deadline, "token rotation did not complete");
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(5)).await });
    }
    let rotated = handles.token.get();
    assert_ne!(rotated, original);
    assert_eq!(
        runtime
            .block_on(load_or_create_token(&handles.repository))
            .expect("rotated token must remain readable"),
        rotated
    );
}

#[gpui_kit::test]
fn settings_should_reuse_its_root_and_reopen_after_close(cx: &mut TestAppContext) {
    let runtime = settings_runtime();
    let handles = settings_handles(&runtime);
    cx.update(gpui_kit::init);
    cx.update(|cx| open_settings_window(cx, &handles));
    let first = settings_window(&handles);
    let view = cx
        .update_window(first, |_, window, cx| {
            let view = base::Root::read(window, cx)
                .view()
                .clone()
                .downcast::<SettingsView>()
                .unwrap();
            view.update(cx, |view, cx| {
                view.section = Section::General;
                cx.notify();
            });
            view
        })
        .expect("Settings must have a Base Root");

    cx.update(|cx| open_settings_window(cx, &handles));
    assert_eq!(settings_window(&handles), first);
    cx.update(|cx| assert_eq!(view.read(cx).section, Section::General));
    cx.update(|cx| open_browser_integration(cx, &handles));
    assert_eq!(settings_window(&handles), first);
    cx.update(|cx| assert_eq!(view.read(cx).section, Section::BrowserIntegration));
    cx.update_window(first, |_, window, _| window.remove_window())
        .expect("Settings must close");
    cx.update(|cx| open_settings_window(cx, &handles));
    assert_ne!(settings_window(&handles), first);
    cx.update_window(settings_window(&handles), |_, window, cx| {
        assert!(window.root::<base::Root>().flatten().is_some());
        window.render_frame(cx);
        window.click("settings-regenerate", cx);
        assert!(window.has_active_dialog(cx));
    })
    .expect("reopened Settings must support dialogs");
}

#[gpui_kit::test]
fn stopped_integration_should_not_open_regenerate_confirmation(cx: &mut TestAppContext) {
    let runtime = settings_runtime();
    let mut handles = settings_handles(&runtime);
    handles.port = None;
    let original = handles.token.get();
    cx.update(gpui_kit::init);
    cx.update(|cx| open_settings_window(cx, &handles));
    cx.update_window(settings_window(&handles), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("settings-regenerate").disabled(), Some(true));
        window.click("settings-regenerate", cx);
        assert!(!window.has_active_dialog(cx));
        assert_eq!(handles.token.get(), original);
    })
    .expect("stopped integration Settings must remain open");
}
