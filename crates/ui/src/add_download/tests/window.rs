use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, TestAppContext, base};

use crate::bridge::EngineHandles;

fn test_handles(runtime: &tokio::runtime::Runtime, folder: std::path::PathBuf) -> EngineHandles {
    runtime.block_on(async {
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
            default_folder: folder.clone(),
            category_root: folder,
            use_category_default: false,
        }
    })
}

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
    let handles = test_handles(&runtime, missing);
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

#[gpui_kit::test]
fn integration_metadata_should_seed_size_probe_missing_fields_and_preserve_user_edits(
    cx: &mut TestAppContext,
) {
    use raijin_core::integration::{DownloadRequestItem, prepare_item};

    use super::super::{AddDownloadView, ProbeInfo, ProbeState, open_integration_download_window};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let handles = test_handles(&runtime, std::env::temp_dir());
    cx.update(gpui_kit::init);
    for complete in [true, false] {
        let source = DownloadRequestItem {
            link: "https://example.com/download".into(),
            download_page: None,
            headers: None,
            description: None,
            suggested_name: complete.then(|| "setup.exe".into()),
            total_bytes: complete.then_some(123456),
            r#type: raijin_core::integration::DownloadType::Http,
        };
        let prepared = prepare_item(&source, &handles.default_folder).expect("prepared");
        let handle = cx.update(|cx| {
            open_integration_download_window(cx, &handles, prepared, None).expect("window")
        });
        cx.update_window(handle, |_, window, cx| {
            let view = base::Root::read(window, cx)
                .view()
                .clone()
                .downcast::<AddDownloadView>()
                .expect("form");
            view.update(cx, |view, cx| {
                assert!(!view.file_manual);
                if complete {
                    assert_eq!(
                        view.probe,
                        ProbeState::Ready {
                            total_bytes: Some(123456)
                        }
                    );
                    assert!(view.probe_abort.is_none());
                    assert_eq!(view.file.read(cx).value().as_str(), "setup.exe");
                } else {
                    assert_eq!(view.probe, ProbeState::Checking);
                    assert!(view.probe_abort.is_some());
                    view.apply_probe_name(
                        ProbeInfo {
                            total_bytes: Some(987),
                            file_name: None,
                            final_url: "https://cdn.example.com/resolved.exe".into(),
                            typed_url: source.link.clone(),
                        },
                        window,
                        cx,
                    );
                    assert_eq!(view.file.read(cx).value().as_str(), "resolved.exe");
                }
                let generation = view.probe_gen;
                view.refresh_derived(window, cx);
                assert_eq!(view.probe_gen, generation);
                assert_eq!(
                    view.file.read(cx).value().as_str(),
                    if complete {
                        "setup.exe"
                    } else {
                        "resolved.exe"
                    }
                );
                view.file
                    .update(cx, |state, cx| state.set_value("chosen.zip", window, cx));
                view.file_manual = true;
                view.apply_probe_name(
                    ProbeInfo {
                        total_bytes: None,
                        file_name: Some("server.exe".into()),
                        final_url: source.link.clone(),
                        typed_url: source.link.clone(),
                    },
                    window,
                    cx,
                );
                assert_eq!(view.file.read(cx).value().as_str(), "chosen.zip");
                view.url.update(cx, |state, cx| {
                    state.set_value("https://other.example.com/file", window, cx)
                });
                view.refresh_derived(window, cx);
                assert_eq!(view.probe, ProbeState::Checking);
                assert!(view.probe_name.is_none());
            });
            window.remove_window();
        })
        .expect("window update");
    }
}
