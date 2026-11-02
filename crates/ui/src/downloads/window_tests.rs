use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{Sizable as _, WindowExt as _, v_flex};
use gpui_kit::test::{TestEventExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Context, Entity, FocusHandle,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render,
    ScrollDelta, Styled as _, TestAppContext, Window, WindowBounds, WindowOptions, point, px, size,
};
use raijin_core::download::{Bytes, BytesPerSecond, DownloadId, DownloadStatus, QueueId};
use raijin_core::monitor::DownloadView;

use super::{
    DownloadsDelegate, TABLE_HEADER_HEIGHT_PX, TABLE_ROW_HEIGHT_PX, build_rows,
    cancel_reorder_drag, reorder_surface,
};
use crate::bridge::EngineHandles;

struct FixtureView {
    table: Entity<TableState<DownloadsDelegate>>,
    focus: FocusHandle,
}

impl Render for FixtureView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && cx.has_active_drag() {
                    cancel_reorder_drag(&this.table, window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                reorder_surface(&self.table)
                    .size_full()
                    .overflow_hidden()
                    .child(
                        DataTable::new(&self.table)
                            .header_row_height(px(TABLE_HEADER_HEIGHT_PX))
                            .with_size(px(TABLE_ROW_HEIGHT_PX))
                            .stripe(false)
                            .bordered(false),
                    ),
            )
    }
}

fn fixture(
    cx: &mut TestAppContext,
    count: i64,
) -> (
    tokio::runtime::Runtime,
    AnyWindowHandle,
    Entity<FixtureView>,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime must start");
    let handles = runtime.block_on(async {
        let pool = raijin_core::db::bootstrap("sqlite::memory:").await.unwrap();
        let downloads = raijin_core::db::DownloadRepository::new(pool.clone());
        let parts = raijin_core::db::PartRepository::new(pool.clone());
        let (manager, _, events) = raijin_core::download::DownloadManagerHandle::spawn_with_events(
            downloads.clone(),
            parts,
            Default::default(),
        );
        let (monitor, _) = raijin_core::monitor::DownloadMonitorHandle::spawn(events, None);
        let (queues, _) = raijin_core::queue::QueueManagerHandle::spawn(
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
            default_folder: std::env::temp_dir(),
            category_root: std::env::temp_dir(),
            use_category_default: false,
        }
    });
    cx.update(gpui_kit::init);
    let (handle, view) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(800.), px(600.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let table = cx.new(|cx| {
                    TableState::new(DownloadsDelegate::new(handles), window, cx)
                        .row_selectable(false)
                        .col_selectable(false)
                        .sortable(false)
                });
                table.update(cx, |table, cx| {
                    let delegate = table.delegate_mut();
                    delegate.set_loaded(true);
                    delegate.set_queue_order(true);
                    delegate.set_reorderable_queue(Some(QueueId::new(999)));
                    delegate.set_rows(build_rows(&views(count), 0));
                    table.refresh(cx);
                });
                let focus = cx.focus_handle();
                window.focus(&focus, cx);
                cx.new(|_| FixtureView { table, focus })
            },
        )
        .unwrap()
    });
    (runtime, handle, view)
}

fn views(count: i64) -> Vec<DownloadView> {
    (1..=count)
        .map(|id| DownloadView {
            id: DownloadId::new(id),
            url: format!("https://example.com/item-{id}.zip"),
            file_name: format!("item-{id}.zip"),
            folder: PathBuf::from("Downloads"),
            category: None,
            status: DownloadStatus::Paused,
            downloaded_bytes: Bytes::new(0),
            total_bytes: None,
            speed: BytesPerSecond::ZERO,
            eta_seconds: None,
            active_part_count: 0,
            failure: None,
            created_at: id,
            updated_at: id,
        })
        .collect()
}

fn order(table: &TableState<DownloadsDelegate>) -> Vec<i64> {
    table
        .delegate()
        .rows
        .iter()
        .map(|row| row.id.get())
        .collect()
}

#[gpui_kit::test]
fn drag_should_show_shadow_preserve_metadata_and_cancel_on_escape(cx: &mut TestAppContext) {
    let (_runtime, handle, view) = fixture(cx, 3);
    cx.update_window(handle, |_, window, cx| {
        let table = view.read(cx).table.clone();
        window.render_frame(cx);
        window.click("download-row-1", cx);
        let start = window.find("download-row-1").bounds().center();
        let target = window.find("download-row-3").bounds().center();
        window.pointer_down(start, MouseButton::Left, cx);
        window.pointer_move(start + point(px(0.), px(6.)), Some(MouseButton::Left), cx);
        window.pointer_move(target, Some(MouseButton::Left), cx);
        assert_eq!(order(table.read(cx)), [2, 3, 1]);
        assert_eq!(
            table.read(cx).delegate().selected_id(),
            Some(DownloadId::new(1))
        );
        assert!(
            table
                .read(cx)
                .delegate()
                .reorder
                .is_placeholder(DownloadId::new(1))
        );
        let mut updated = views(3);
        updated[0].downloaded_bytes = Bytes::new(450);
        table.update(cx, |table, cx| {
            table.delegate_mut().set_rows(build_rows(&updated, 0));
            table.refresh(cx);
        });
        window.render_frame(cx);
        assert_eq!(order(table.read(cx)), [2, 3, 1]);
        assert_eq!(
            table.read(cx).delegate().selected_id(),
            Some(DownloadId::new(1))
        );
        assert_eq!(
            table
                .read(cx)
                .delegate()
                .row_by_id(DownloadId::new(1))
                .unwrap()
                .downloaded_bytes,
            450
        );
        window.focus(&view.read(cx).focus.clone(), cx);
        window.key_down("escape", false, cx);
        window.key_up("escape", cx);
        window.pointer_up(target, MouseButton::Left, cx);
        assert!(!cx.has_active_drag());
        assert_eq!(order(table.read(cx)), [1, 2, 3]);
        assert!(
            !table
                .read(cx)
                .delegate()
                .reorder
                .is_placeholder(DownloadId::new(1))
        );
        assert!(window.notifications(cx).is_empty());
    })
    .unwrap();
}

#[gpui_kit::test]
fn rejected_drop_should_rollback_and_report_failure(cx: &mut TestAppContext) {
    let (runtime, handle, view) = fixture(cx, 3);
    cx.update_window(handle, |_, window, cx| {
        let table = view.read(cx).table.clone();
        window.render_frame(cx);
        let start = window.find("download-row-1").bounds().center();
        let target = window.find("download-row-3").bounds().center();
        window.pointer_down(start, MouseButton::Left, cx);
        window.pointer_move(start + point(px(0.), px(6.)), Some(MouseButton::Left), cx);
        window.pointer_move(target, Some(MouseButton::Left), cx);
        assert_eq!(order(table.read(cx)), [2, 3, 1]);
        window.pointer_up(target, MouseButton::Left, cx);
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
        cx.run_until_parked();
        let complete = cx
            .update_window(handle, |_, window, cx| !window.notifications(cx).is_empty())
            .unwrap();
        if complete {
            break;
        }
        assert!(Instant::now() < deadline, "reorder rejection must complete");
    }
    cx.update_window(handle, |_, _, cx| {
        let table = view.read(cx).table.clone();
        assert_eq!(order(table.read(cx)), [1, 2, 3]);
        assert!(
            !table
                .read(cx)
                .delegate()
                .reorder
                .is_placeholder(DownloadId::new(1))
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn scrolling_during_drag_should_update_shadow_without_pointer_movement(cx: &mut TestAppContext) {
    let (_runtime, handle, view) = fixture(cx, 30);
    cx.update_window(handle, |_, window, cx| {
        let table = view.read(cx).table.clone();
        window.render_frame(cx);
        let start = window.find("download-row-1").bounds().center();
        let target = window.find("download-row-6").bounds().center();
        window.pointer_down(start, MouseButton::Left, cx);
        window.pointer_move(start + point(px(0.), px(6.)), Some(MouseButton::Left), cx);
        window.pointer_move(target, Some(MouseButton::Left), cx);
        assert_eq!(
            table.read(cx).delegate().row_position(DownloadId::new(1)),
            Some(5)
        );
        window.scroll("table", ScrollDelta::Pixels(point(px(0.), px(-96.))), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        let table = view.read(cx).table.clone();
        assert_eq!(
            table.read(cx).delegate().row_position(DownloadId::new(1)),
            Some(7)
        );
        assert!(
            table
                .read(cx)
                .delegate()
                .reorder
                .is_placeholder(DownloadId::new(1))
        );
        cancel_reorder_drag(&table, window, cx);
        assert_eq!(order(table.read(cx)), (1..=30).collect::<Vec<_>>());
    })
    .unwrap();
}

#[gpui_kit::test]
fn scrolled_drag_should_show_shadow_and_cancel_outside_table(cx: &mut TestAppContext) {
    let (_runtime, handle, view) = fixture(cx, 30);
    cx.update(|cx| cx.set_reduce_motion(true));
    cx.update_window(handle, |_, window, cx| {
        let table = view.read(cx).table.clone();
        window.render_frame(cx);
        window.scroll("table", ScrollDelta::Pixels(point(px(0.), px(-520.))), cx);
        let original_top = window.find("download-row-12").bounds().top();
        let start = window.find("download-row-12").bounds().center();
        let target = window.find("download-row-14").bounds().center();
        window.pointer_down(start, MouseButton::Left, cx);
        window.pointer_move(start + point(px(0.), px(6.)), Some(MouseButton::Left), cx);
        window.pointer_move(target, Some(MouseButton::Left), cx);
        assert_eq!(&order(table.read(cx))[11..14], [13, 14, 12]);
        assert_eq!(window.find("download-row-13").bounds().top(), original_top);
        window.pointer_up(point(px(900.), px(700.)), MouseButton::Left, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        let table = view.read(cx).table.clone();
        assert_eq!(order(table.read(cx)), (1..=30).collect::<Vec<_>>());
        assert!(
            !table
                .read(cx)
                .delegate()
                .reorder
                .is_placeholder(DownloadId::new(12))
        );
        assert!(window.notifications(cx).is_empty());
    })
    .unwrap();
}
