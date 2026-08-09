mod app;
mod downloads_table;
mod sidebar;
mod theme;

pub(crate) use app::root;
use downloads_table::new_download_window_config;
use freya::prelude::*;

/// Runs the desktop UI shell.
pub fn run() {
    use freya::tray::menu::{Menu, MenuEvent, MenuItem};
    use freya::tray::{TrayEvent, TrayIconBuilder};

    let tray_icon = || {
        let tray_menu = Menu::new();
        let _ = tray_menu.append(&MenuItem::with_id("show", "Show Raijin", true, None));
        let _ = tray_menu.append(&MenuItem::with_id(
            "new-download",
            "New Download",
            true,
            None,
        ));
        let _ = tray_menu.append(&MenuItem::with_id("exit", "Exit", true, None));
        TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu))
            .with_tooltip("Raijin")
            .build()
            .expect("failed to build tray icon")
    };

    let tray_handler = |event, mut context: RendererContext| match event {
        TrayEvent::Menu(MenuEvent { id }) if id == "show" => {
            for window in context.windows_mut().values_mut() {
                window.window_mut().set_visible(true);
                window.window().focus_window();
            }
        }
        TrayEvent::Menu(MenuEvent { id }) if id == "new-download" => {
            context.launch_window(new_download_window_config());
        }
        TrayEvent::Menu(MenuEvent { id }) if id == "exit" => {
            context.exit();
        }
        _ => {}
    };

    launch(
        LaunchConfig::new()
            .with_window(
                WindowConfig::new(root)
                    .with_title("Raijin")
                    .with_size(900., 620.)
                    .with_min_size(560., 420.)
                    .with_resizable(true)
                    .with_background(theme::RAIJIN_BACKGROUND)
                    .with_on_close(|mut context, window_id| {
                        if let Some(window) = context.windows_mut().get_mut(&window_id) {
                            window.window_mut().set_visible(false);
                        }
                        CloseDecision::KeepOpen
                    }),
            )
            .with_tray(tray_icon, tray_handler),
    );
}
