use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, TestAppContext, base};

use crate::bridge::EngineHandles;

#[gpui_kit::test]
fn folder_picker_should_preserve_category_and_handle_selection_and_cancel(cx: &mut TestAppContext) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime must start");
    let missing = std::env::temp_dir()
        .join(format!("raijin-folder-error-{}", std::process::id()))
        .join("missing");
    assert!(!missing.exists());
    let handles = runtime.block_on(async {
        let pool = raijin_core::db::bootstrap("sqlite::memory:")
            .await
            .expect("test database must bootstrap");
        let downloads = raijin_core::db::DownloadRepository::new(pool.clone());
        let parts = raijin_core::db::PartRepository::new(pool.clone());
        let (manager, _manager_task, events) =
            raijin_core::download::DownloadManagerHandle::spawn_with_events(
                downloads.clone(),
                parts,
                Default::default(),
            );
        let (monitor, _monitor_task) =
            raijin_core::monitor::DownloadMonitorHandle::spawn(events, None);
        let (queues, _queue_task) = raijin_core::queue::QueueManagerHandle::spawn(
            downloads,
            raijin_core::db::QueueRepository::new(pool),
            manager.clone(),
            manager.subscribe(),
            Default::default(),
        );
        EngineHandles {
            manager,
            queues,
            monitor,
            runtime: runtime.handle().clone(),
            default_folder: missing.clone(),
            category_root: missing,
            use_category_default: false,
        }
    });
    cx.update(gpui_kit::init);
    let handle = cx.update(|cx| {
        super::super::open_add_download_window(cx, &handles, "");
        *cx.windows().last().expect("Add Download must open")
    });
    let view = cx
        .update_window(handle, |_, window, cx| {
            assert!(window.root::<base::Root>().flatten().is_some());
            base::Root::read(window, cx)
                .view()
                .clone()
                .downcast::<super::super::AddDownloadView>()
                .expect("Add Download root must contain the form")
        })
        .expect("Add Download must open");
    let selected = std::env::temp_dir();
    for use_category in [false, true] {
        let (original, category) = cx
            .update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.use_category = use_category;
                    view.folder_manual = false;
                    view.refresh_folder(window, cx);
                    cx.notify();
                });
                let original = view.read(cx).resolved_folder(cx);
                let category = view.read(cx).selected_category(cx);
                window.render_frame(cx);
                window.click("add-folder-open", cx);
                (original, category)
            })
            .expect("folder picker must open");
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|options| {
            assert!(!options.files);
            assert!(options.directories);
            assert!(!options.multiple);
            None
        });
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            assert_eq!(view.read(cx).resolved_folder(cx), original);
            assert!(!view.read(cx).folder_manual);
            window.click("add-folder-open", cx);
        })
        .expect("Cancel must leave the form open");
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|_| Some(vec![selected.clone()]));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            assert_eq!(view.read(cx).resolved_folder(cx), selected);
            assert!(view.read(cx).folder_manual);
            assert_eq!(view.read(cx).use_category, use_category);
            assert_eq!(view.read(cx).selected_category(cx), category);
            assert!(window.notifications(cx).is_empty());
            window.click("add-use-category", cx);
            assert_eq!(view.read(cx).use_category, !use_category);
            assert_eq!(view.read(cx).resolved_folder(cx), selected);
            window.click("add-folder-reset", cx);
            assert!(!view.read(cx).folder_manual);
        })
        .expect("folder selection must leave the form usable");
    }
    cx.update_window(handle, |_, window, cx| {
        window.push_notification("Folder picker error notification", cx);
        assert_eq!(window.notifications(cx).len(), 1);
        window.remove_window();
    })
    .expect("Add Download must support notifications");
}
