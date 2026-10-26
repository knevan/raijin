use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::sidebar::{Sidebar, SidebarItem, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Collapsible, Disableable as _, Icon, IconName, Sizable as _, TitleBar,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use raijin_core::download::{DownloadId, DownloadStatus, QueueId};
use raijin_core::monitor::MonitorState;
use raijin_core::platform::DiskUsage;
use raijin_core::platform::tray::CloseDecision;
use raijin_core::queue::QueueSnapshot;

use crate::add_download::open_add_download_window;
use crate::bridge::{EngineCommand, EngineHandles};
use crate::category::{CategoryFilter, FileCategory, StatusFilter};
use crate::downloads::{
    DownloadRow, DownloadsDelegate, MONITOR_POLL_INTERVAL, build_rows, can_pause, can_resume,
    category_icon, filter_rows, toolbar_actions,
};
use crate::format::{format_bytes, format_speed, now_ms};
use crate::settings::{SettingsHandles, open_settings_window};
use crate::tray::TraySession;

/// Default sidebar width in pixels, matching the product sketch.
const SIDEBAR_WIDTH: f32 = 220.0;
/// Sidebar drag limits in pixels.
const SIDEBAR_MIN_WIDTH: f32 = 200.0;
/// Sidebar drag limits in pixels.
const SIDEBAR_MAX_WIDTH: f32 = 300.0;
/// Disk usage refresh throttle; capacity barely moves at monitor tick rates.
const STORAGE_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Main downloads page owning the shell regions and table state.
pub struct MainView {
    handles: EngineHandles,
    tray: TraySession,
    /// Token plumbing for the Settings window opened from the toolbar.
    settings: SettingsHandles,
    search: Entity<InputState>,
    table: Entity<TableState<DownloadsDelegate>>,
    /// Last-seen monitor snapshot; compared to skip no-op refreshes.
    last_snapshot: std::sync::Arc<MonitorState>,
    /// Last-seen queue snapshot; supplies row labels and drop targets.
    queue_state: Vec<QueueSnapshot>,
    /// Unfiltered presentation rows rebuilt on every snapshot change.
    cache: Vec<DownloadRow>,
    category: CategoryFilter,
    status: StatusFilter,
    selected_queue: Option<QueueId>,
    query: String,
    /// Visible index of the selected row, if any.
    selected: Option<usize>,
    loaded: bool,
    visible_count: usize,
    active_count: usize,
    total_speed_bps: u64,
    /// Drag state for the sidebar/content split; sizes persist across renders.
    resize_state: Entity<ResizableState>,
    /// Last detected disk usage for the download folder.
    storage: Option<DiskUsage>,
    /// Throttle marker for disk detection.
    storage_checked_at: Option<Instant>,
    /// Kept alive for the view lifetime; dropping them unsubscribes events.
    _subscriptions: [Subscription; 2],
}

impl MainView {
    /// Creates the main view, wires subscriptions, and starts monitor polling.
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        handles: EngineHandles,
        tray: TraySession,
        settings: SettingsHandles,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search downloads..."));
        let table = cx.new(|cx| {
            // `sortable(false)` hides the table's built-in right-side sort
            // glyph; sorting is driven by the sketch's left-side header
            // content via `DownloadsDelegate::cycle_sort`.
            // Row selection is managed directly on the hybrid card container
            // in `DownloadsDelegate::render_tr` so that corner radius (8px)
            // and 1px borders are preserved without framework's square overlay.
            TableState::new(DownloadsDelegate::new(handles.clone()), window, cx)
                .row_selectable(false)
                .col_selectable(false)
                .sortable(false)
                .col_movable(true)
                .col_resizable(true)
        });
        let resize_state = cx.new(|_| ResizableState::default());
        let subscriptions = [
            cx.subscribe_in(&search, window, Self::on_search_event),
            cx.subscribe(&table, Self::on_table_event),
        ];

        let monitor = handles.monitor.clone();
        // Seed once so the first paint already labels queue members; the poll
        // loop refreshes it on every snapshot change.
        let initial_queues = handles.queues.state();
        let queue_handle = handles.queues.clone();
        let poll_tray = tray.clone();
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(MONITOR_POLL_INTERVAL).await;
                if !poll_tray.should_poll() {
                    let is_alive = weak.update(cx, |_, _| ()).is_ok();
                    if !is_alive {
                        break;
                    }
                    continue;
                }
                let snapshot = monitor.state();
                let queues = queue_handle.state();
                let alive = weak
                    .update(cx, |view, cx| {
                        view.apply_queue_state(queues);
                        view.apply_snapshot(snapshot, cx)
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        Self {
            handles,
            tray,
            settings,
            search,
            table,
            last_snapshot: std::sync::Arc::new(MonitorState::default()),
            queue_state: initial_queues,
            cache: Vec::new(),
            category: CategoryFilter::All,
            status: StatusFilter::All,
            selected_queue: None,
            query: String::new(),
            selected: None,
            loaded: false,
            visible_count: 0,
            active_count: 0,
            total_speed_bps: 0,
            resize_state,
            storage: None,
            storage_checked_at: None,
            _subscriptions: subscriptions,
        }
    }

    /// Applies a monitor snapshot when it differs from the last one.
    ///
    /// The first poll always applies, even when the database is empty, so the
    /// table leaves its skeleton loading state on a fresh install. Disk
    /// detection runs on its own throttle and can force a refresh alone.
    #[hotpath::measure(impl_type = "MainView")]
    fn apply_snapshot(&mut self, snapshot: std::sync::Arc<MonitorState>, cx: &mut Context<Self>) {
        let storage_changed = self.maybe_refresh_storage();
        if self.loaded
            && !storage_changed
            && (std::sync::Arc::ptr_eq(&snapshot, &self.last_snapshot)
                || *snapshot == *self.last_snapshot)
        {
            return;
        }
        let now = now_ms();
        let mut views: Vec<_> = snapshot
            .active
            .values()
            .chain(snapshot.completed.values())
            .collect();
        views.sort_by_key(|view| view.created_at);
        let labels = self.queue_labels();
        self.cache = build_rows(views, now);
        for row in &mut self.cache {
            row.queue_name = labels.get(&row.id).cloned();
        }
        self.last_snapshot = snapshot;
        self.loaded = true;
        self.recompute(cx);
    }

    /// Stores the latest queue snapshot and relabels rows when it changed.
    fn apply_queue_state(&mut self, queues: Vec<QueueSnapshot>) {
        if queues == self.queue_state {
            return;
        }
        self.queue_state = queues;
        let labels = self.queue_labels();
        for row in &mut self.cache {
            row.queue_name = labels.get(&row.id).cloned();
        }
    }

    /// Maps download id to owning queue name for the current snapshot.
    fn queue_labels(&self) -> HashMap<DownloadId, String> {
        let mut labels = std::collections::HashMap::new();
        for snapshot in &self.queue_state {
            for id in &snapshot.items {
                labels.insert(*id, snapshot.queue.name.clone());
            }
        }
        labels
    }

    /// Refreshes detected disk usage at most every [`STORAGE_REFRESH_INTERVAL`].
    ///
    /// Returns true when the displayed value changed and the view must repaint
    /// even with an otherwise identical monitor snapshot.
    fn maybe_refresh_storage(&mut self) -> bool {
        if let Some(checked_at) = self.storage_checked_at
            && checked_at.elapsed() < STORAGE_REFRESH_INTERVAL
        {
            return false;
        }
        self.storage_checked_at = Some(Instant::now());
        let usage = raijin_core::platform::disk_usage_for(&self.handles.default_folder);
        if usage != self.storage {
            self.storage = usage;
            true
        } else {
            false
        }
    }

    /// Rebuilds visible rows from cache + filters, preserving selection by id.
    fn recompute(&mut self, cx: &mut Context<Self>) {
        let selected_id = self
            .selected
            .and_then(|ix| self.table.read(cx).delegate().row(ix))
            .map(|row| row.id);
        let mut filtered = filter_rows(&self.cache, self.category, self.status, &self.query);
        if let Some(queue_id) = self.selected_queue {
            if let Some(snapshot) = self.queue_state.iter().find(|s| s.queue.id == queue_id) {
                let member_ids: std::collections::HashSet<_> =
                    snapshot.items.iter().copied().collect();
                filtered.retain(|row| member_ids.contains(&row.id));
            } else {
                filtered.clear();
            }
        }
        self.visible_count = filtered.len();
        self.active_count = self.cache.iter().filter(|row| row.is_active).count();
        self.total_speed_bps = self.cache.iter().map(|row| row.speed_bps).sum();
        self.table.update(cx, |table, cx| {
            table.delegate_mut().set_rows(filtered);
            table.delegate_mut().set_loaded(true);
            table.refresh(cx);
        });
        // Restore selection by stable id; drop it when the row filtered out.
        self.selected = selected_id.and_then(|id| self.table.read(cx).delegate().row_position(id));
        cx.notify();
    }

    fn on_search_event(
        &mut self,
        state: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, InputEvent::Change) {
            return;
        }
        self.query = state.read(cx).value().to_string();
        self.recompute(cx);
    }

    fn on_table_event(
        &mut self,
        _table: Entity<TableState<DownloadsDelegate>>,
        event: &TableEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TableEvent::SelectRow(ix) => {
                self.selected = Some(*ix);
                self.table.update(cx, |table, cx| {
                    if let Some(id) = table.delegate().row(*ix).map(|r| r.id) {
                        table.delegate_mut().select_row(id);
                    }
                    cx.notify();
                });
                cx.notify();
            }
            TableEvent::DoubleClickedRow(ix) => {
                self.open_row(*ix, cx);
            }
            TableEvent::ColumnWidthsChanged(widths) => {
                self.table.update(cx, |table, _| {
                    table.delegate_mut().set_column_widths(widths);
                });
            }
            TableEvent::ClearSelection => {
                self.selected = None;
                self.table.update(cx, |table, cx| {
                    table.delegate_mut().clear_selection();
                    cx.notify();
                });
                cx.notify();
            }
            _ => {}
        }
    }

    /// Returns the selected download id, if the selection is still valid.
    fn selected_id(&self, cx: &App) -> Option<raijin_core::download::DownloadId> {
        self.selected
            .and_then(|ix| self.table.read(cx).delegate().row(ix))
            .map(|row| row.id)
    }

    /// Returns the ids toolbar actions apply to: the checked set when
    /// non-empty (ABDM multi-select), else the single current row.
    fn target_ids(&self, cx: &App) -> Vec<raijin_core::download::DownloadId> {
        let checked = self.table.read(cx).delegate().checked_ids();
        if checked.is_empty() {
            self.selected_id(cx).into_iter().collect()
        } else {
            checked
        }
    }

    /// Returns the statuses toolbar actions apply to: the checked set when
    /// non-empty (ABDM multi-select), else the single current row.
    fn target_statuses(&self, cx: &App) -> Vec<DownloadStatus> {
        let ids = self.target_ids(cx);
        let table = self.table.read(cx);
        let delegate = table.delegate();
        ids.iter()
            .filter_map(|id| delegate.row_by_id(*id))
            .map(|row| row.status)
            .collect()
    }

    /// Applies a combined status + category filter from the sidebar tree.
    ///
    /// Navigation invalidates the positional row highlight, so the current
    /// row is cleared (checkboxes survive via the delegate's id-keyed set).
    fn set_filter(
        &mut self,
        status: StatusFilter,
        category: CategoryFilter,
        cx: &mut Context<Self>,
    ) {
        self.status = status;
        self.category = category;
        self.selected_queue = None;
        self.selected = None;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            table.clear_selection(cx);
        });
        self.recompute(cx);
    }

    /// Filters downloads by queue membership or toggles back to all downloads.
    fn select_queue(&mut self, queue_id: Option<QueueId>, cx: &mut Context<Self>) {
        if self.selected_queue == queue_id {
            self.selected_queue = None;
        } else {
            self.selected_queue = queue_id;
        }
        self.selected = None;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            table.clear_selection(cx);
        });
        self.recompute(cx);
    }

    /// Clears the row highlight, e.g. after clicking empty table space.
    ///
    /// Goes through [`TableState::clear_selection`] so the emitted
    /// `ClearSelection` keeps `selected` in step via `on_table_event`.
    fn clear_table_selection(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            table.clear_selection(cx);
            cx.notify();
        });
        cx.notify();
    }

    fn pause_selected(&mut self, cx: &mut Context<Self>) {
        let ids = self.target_ids(cx);
        let targets: Vec<_> = {
            let table = self.table.read(cx);
            let delegate = table.delegate();
            ids.into_iter()
                .filter(|id| {
                    delegate
                        .row_by_id(*id)
                        .is_some_and(|row| can_pause(row.status))
                })
                .collect()
        };
        for id in targets {
            self.handles.dispatch(EngineCommand::Pause(id));
        }
        cx.notify();
    }

    fn resume_selected(&mut self, cx: &mut Context<Self>) {
        let ids = self.target_ids(cx);
        let targets: Vec<_> = {
            let table = self.table.read(cx);
            let delegate = table.delegate();
            ids.into_iter()
                .filter(|id| {
                    delegate
                        .row_by_id(*id)
                        .is_some_and(|row| can_resume(row.status))
                })
                .collect()
        };
        for id in targets {
            self.handles.dispatch(EngineCommand::Resume(id));
        }
        cx.notify();
    }

    fn remove_selected(&mut self, cx: &mut Context<Self>) {
        let targets = self.target_ids(cx);
        if targets.is_empty() {
            return;
        }
        for id in &targets {
            // Immediate removal keeps list management fast; the engine only
            // deletes the download's own final and partial files. A confirm
            // step follows with the AlertDialog pass.
            self.handles.dispatch(EngineCommand::Remove(*id));
        }
        // Uncheck at once; the next monitor poll prunes them as well.
        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            table.delegate_mut().uncheck_ids(&targets);
            table.refresh(cx);
        });
        self.selected = None;
        cx.notify();
    }

    fn pause_all(&mut self, cx: &mut Context<Self>) {
        // Single actor command; per-row dispatch would cost N round-trips
        // and interleave with concurrent completions.
        self.handles.dispatch(EngineCommand::PauseAll);
        cx.notify();
    }

    fn open_row(&self, row_ix: usize, cx: &App) {
        let Some(row) = self.table.read(cx).delegate().row(row_ix) else {
            return;
        };
        let path = row.file_path();
        let result = if matches!(row.status, DownloadStatus::Completed) {
            raijin_core::platform::open_file(&path)
        } else {
            raijin_core::platform::reveal_in_folder(&path)
        };
        if let Err(error) = result {
            tracing::warn!(?error, "open action failed");
        }
    }

    /// Opens the standalone Add Download window (sketch: New Download opens
    /// a new window, also spawnable from tray / `--add` without main cost).
    fn open_add_window(&self, cx: &mut App) {
        open_add_download_window(cx, &self.handles, "");
    }

    fn header_title(&self) -> String {
        if let Some(queue_id) = self.selected_queue
            && let Some(snapshot) = self.queue_state.iter().find(|s| s.queue.id == queue_id)
        {
            return format!("Queue: {}", snapshot.queue.name);
        }
        match (self.category, self.status) {
            (CategoryFilter::All, StatusFilter::All) => "Recent".to_owned(),
            (category, StatusFilter::All) => category.label().to_owned(),
            (CategoryFilter::All, status) => status.label().to_owned(),
            (category, status) => format!("{} · {}", category.label(), status.label()),
        }
    }

    fn render_titlebar(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Linux exposes a close hook: hide to tray via a programmatic
        // destroy (GPUI has no `hide()` there) instead of destroying +
        // reopening through the observer. Other platforms fall through to
        // the `on_window_closed` path in `main.rs`. The constructor is a
        // no-op there by design, so one code path covers all OSes.
        let close_tray = self.tray.clone();
        TitleBar::new()
            .on_close_window(
                move |_, window, _| match close_tray.close_requested(false) {
                    CloseDecision::HideToTray => {
                        close_tray.begin_programmatic_close();
                        window.remove_window();
                    }
                    CloseDecision::RemoveWindow => window.remove_window(),
                },
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .size_7()
                            .rounded_md()
                            .bg(cx.theme().primary)
                            .text_color(cx.theme().primary_foreground)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .child("R"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground)
                            .child("Raijin"),
                    ),
            )
            .child(div().flex_1())
            .child(
                div().w(px(240.)).child(
                    Input::new(&self.search)
                        .prefix(Icon::new(IconName::Search).small())
                        .cleanable(true)
                        .small(),
                ),
            )
    }

    /// Renders the sidebar with collapsible filter dropdowns and a dedicated
    /// Queues section.
    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = ActiveSidebarFilter {
            status: self.status,
            category: self.category,
            queue: self.selected_queue,
        };
        let menu = SidebarMenu::new()
            .child(filter_parent(
                "All",
                IconName::Folder,
                StatusFilter::All,
                true,
                filter,
                cx,
            ))
            .child(filter_parent(
                "Finished",
                IconName::CircleCheck,
                StatusFilter::Finished,
                false,
                filter,
                cx,
            ))
            .child(filter_parent(
                "Unfinished",
                IconName::Loader,
                StatusFilter::Unfinished,
                false,
                filter,
                cx,
            ));

        let queue_section = QueueSection {
            queues: self.queue_state.clone(),
            selected_queue: self.selected_queue,
            handles: self.handles.clone(),
            view: cx.entity(),
            collapsed: false,
        };

        Sidebar::new("main-sidebar")
            .w_full()
            .collapsible(false)
            .header(
                Button::new("new-download")
                    .primary()
                    .small()
                    .w_full()
                    .icon(IconName::Plus)
                    .label("New Download")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_add_window(cx);
                    })),
            )
            .child(MainSidebarSection::Menu(Box::new(menu)))
            .child(MainSidebarSection::Queues(Box::new(queue_section)))
            .footer(self.render_storage(cx))
    }

    /// Renders the storage card from real detected disk capacity.
    fn render_storage(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (sub, fraction) = match self.storage {
            Some(usage) => (
                format!(
                    "{} of {}",
                    format_bytes(usage.used_bytes()),
                    format_bytes(usage.total_bytes)
                ),
                usage.used_fraction() * 100.0,
            ),
            None => ("Detecting…".to_owned(), 0.0),
        };
        v_flex()
            .w_full()
            .gap_1()
            .p_2()
            .rounded_md()
            .bg(cx.theme().muted)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child("Storage"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(sub),
            )
            .child(Progress::new("storage-bar").value(fraction).small())
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // Toolbar acts on the checked set when non-empty, else the current
        // row — Resume/Pause enable only when a target actually supports the
        // action, so a Finished selection leaves only Remove enabled.
        let targets = self.target_statuses(cx);
        let all: Vec<DownloadStatus> = self.cache.iter().map(|row| row.status).collect();
        let actions = toolbar_actions(&targets, &all);
        h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground)
                    .child(self.header_title()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{} items", self.visible_count)),
            )
            .child(div().w_2())
            .child(
                Button::new("toolbar-resume")
                    .secondary()
                    .outline()
                    .small()
                    .rounded(px(999.))
                    .icon(IconName::Play)
                    .label("Resume")
                    .disabled(!actions.can_resume)
                    .on_click(cx.listener(|this, _, _, cx| this.resume_selected(cx))),
            )
            .child(
                Button::new("toolbar-pause")
                    .secondary()
                    .outline()
                    .small()
                    .rounded(px(999.))
                    .icon(IconName::Pause)
                    .label("Pause")
                    .disabled(!actions.can_pause)
                    .on_click(cx.listener(|this, _, _, cx| this.pause_selected(cx))),
            )
            .child(div().flex_1())
            .child(
                Button::new("toolbar-pause-all")
                    .secondary()
                    .outline()
                    .small()
                    .w(px(36.))
                    .h(px(36.))
                    .rounded(px(10.))
                    .icon(IconName::Close)
                    .tooltip("Pause all active downloads")
                    .disabled(!actions.can_pause_all)
                    .on_click(cx.listener(|this, _, _, cx| this.pause_all(cx))),
            )
            .child(
                Button::new("toolbar-remove")
                    .secondary()
                    .outline()
                    .small()
                    .w(px(36.))
                    .h(px(36.))
                    .rounded(px(10.))
                    .icon(IconName::Delete)
                    .tooltip("Remove selected download")
                    .disabled(!actions.can_remove)
                    .on_click(cx.listener(|this, _, _, cx| this.remove_selected(cx))),
            )
            .child(
                Button::new("toolbar-settings")
                    .secondary()
                    .outline()
                    .small()
                    .w(px(36.))
                    .h(px(36.))
                    .rounded(px(10.))
                    .icon(IconName::Settings)
                    .tooltip("Settings")
                    .on_click(cx.listener(|this, _, _window, cx| {
                        open_settings_window(cx, &this.settings);
                    })),
            )
    }

    fn render_statusbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let speed = if self.total_speed_bps == 0 {
            "Idle".to_owned()
        } else {
            format_speed(self.total_speed_bps)
        };
        StatusBar::new()
            .left(format!("{} items", self.visible_count))
            .left(format!("{} active", self.active_count))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.header_title()),
            )
            .right(speed)
    }
}

impl Render for MainView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_titlebar(window, cx))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("main-split")
                        .with_state(&self.resize_state)
                        .on_resize(|_, _, _| {})
                        .child(
                            resizable_panel()
                                .size(px(SIDEBAR_WIDTH))
                                .size_range(px(SIDEBAR_MIN_WIDTH)..px(SIDEBAR_MAX_WIDTH))
                                .child(self.render_sidebar(cx)),
                        )
                        .child(
                            resizable_panel().child(
                                v_flex()
                                    .size_full()
                                    .min_w_0()
                                    .gap_3()
                                    .p_4()
                                    .child(self.render_toolbar(cx))
                                    // Hybrid Card-Table: the header renders as a floating rounded island
                                    // with an 8px vertical gap to the body rows, and each row renders as
                                    // an independent card with smooth borders and a 6px gap.
                                    // with a compact gap to the body rows, and each row renders as
                                    // an independent card with smooth borders and a 4px vertical gap.
                                    .child({
                                        let view = cx.entity();
                                        div()
                                            .id("downloads-table-wrap")
                                            .flex_1()
                                            .min_h_0()
                                            .overflow_hidden()
                                            // Empty table space clears the row highlight.
                                            // Row clicks stop propagation in the framework
                                            // (`set_selected_row`) and header clicks stop
                                            // in the delegate, so only genuine empty-space
                                            // clicks arrive here.
                                            .on_click(
                                                move |_: &ClickEvent,
                                                      _: &mut Window,
                                                      cx: &mut App| {
                                                    view.update(cx, |this, cx| {
                                                        this.clear_table_selection(cx);
                                                    });
                                                },
                                            )
                                            .child(
                                                DataTable::new(&self.table)
                                                    .with_size(px(
                                                        crate::downloads::TABLE_ROW_HEIGHT_PX,
                                                    ))
                                                    .stripe(false)
                                                    .bordered(false),
                                            )
                                    }),
                            ),
                        ),
                ),
            )
            .child(self.render_statusbar(cx))
    }
}

/// Active sidebar filter state passed to menu builders.
#[derive(Clone, Copy)]
struct ActiveSidebarFilter {
    status: StatusFilter,
    category: CategoryFilter,
    queue: Option<QueueId>,
}

/// Builds one sidebar filter dropdown: a status parent with the file
/// categories scoped to that status as children.
///
/// Parent clicks select the whole status and toggle dropdown expansion;
/// child clicks select one category within it. Chevron also toggles
/// collapse without changing the filter.
fn filter_parent(
    label: &'static str,
    icon: IconName,
    status: StatusFilter,
    open: bool,
    filter: ActiveSidebarFilter,
    cx: &mut Context<MainView>,
) -> SidebarMenuItem {
    let is_parent_active = filter.status == status
        && matches!(filter.category, CategoryFilter::All)
        && filter.queue.is_none();

    let parent_text_color = if is_parent_active {
        cx.theme().sidebar_accent_foreground
    } else {
        cx.theme().sidebar_foreground
    };

    SidebarMenuItem::new(label)
        .icon(Icon::new(icon).text_color(parent_text_color))
        .label_style(StyleRefinement::default().text_color(parent_text_color))
        .active(is_parent_active)
        .text_color(parent_text_color)
        .default_open(open)
        .click_to_toggle(true)
        .on_click(cx.listener(move |this, _, _, cx| {
            this.set_filter(status, CategoryFilter::All, cx);
        }))
        .children(FileCategory::all().map(|item| {
            let is_child_active = filter.status == status
                && filter.queue.is_none()
                && matches!(
                    filter.category,
                    CategoryFilter::Category(current) if current == item
                );

            let child_text_color = if is_child_active {
                cx.theme().sidebar_accent_foreground
            } else {
                cx.theme().sidebar_foreground
            };

            SidebarMenuItem::new(item.label())
                .icon(Icon::new(category_icon(item)).text_color(child_text_color))
                .label_style(StyleRefinement::default().text_color(child_text_color))
                .active(is_child_active)
                .text_color(child_text_color)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_filter(status, CategoryFilter::Category(item), cx);
                }))
        }))
}

/// Top-level section items rendered in the main navigation sidebar.
#[derive(Clone)]
enum MainSidebarSection {
    Menu(Box<SidebarMenu>),
    Queues(Box<QueueSection>),
}

impl Collapsible for MainSidebarSection {
    fn is_collapsed(&self) -> bool {
        match self {
            Self::Menu(menu) => menu.is_collapsed(),
            Self::Queues(queues) => queues.is_collapsed(),
        }
    }

    fn collapsed(self, collapsed: bool) -> Self {
        match self {
            Self::Menu(menu) => Self::Menu(Box::new(menu.collapsed(collapsed))),
            Self::Queues(queues) => Self::Queues(Box::new(queues.collapsed(collapsed))),
        }
    }
}

impl SidebarItem for MainSidebarSection {
    fn render(
        self,
        id: impl Into<ElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let id = id.into();
        match self {
            Self::Menu(menu) => menu.render(id, window, cx).into_any_element(),
            Self::Queues(queues) => queues.render(id, window, cx).into_any_element(),
        }
    }
}

/// Dedicated sidebar section for managing and filtering download queues.
#[derive(Clone)]
struct QueueSection {
    queues: Vec<QueueSnapshot>,
    selected_queue: Option<QueueId>,
    handles: EngineHandles,
    view: Entity<MainView>,
    collapsed: bool,
}

impl Collapsible for QueueSection {
    fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }
}

impl SidebarItem for QueueSection {
    fn render(
        self,
        id: impl Into<ElementId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let id = id.into();
        let is_collapsed = self.collapsed;

        v_flex()
            .id(id)
            .w_full()
            .gap_1()
            .pt_2()
            .when(!is_collapsed, |this| {
                this.child(
                    h_flex()
                        .px_2()
                        .h_6()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(cx.theme().sidebar_foreground.opacity(0.6))
                                .child("QUEUES"),
                        ),
                )
            })
            .when(self.queues.is_empty(), |this| {
                this.when(!is_collapsed, |this| {
                    this.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("No queues"),
                    )
                })
            })
            .children(self.queues.into_iter().map(|snapshot| {
                let queue_id = snapshot.queue.id;
                let count = snapshot.items.len();
                let running = snapshot.active;
                let is_active = self.selected_queue == Some(queue_id);
                let name = snapshot.queue.name;
                let handles = self.handles.clone();
                let view = self.view.clone();

                let text_color = if is_active {
                    cx.theme().sidebar_accent_foreground
                } else {
                    cx.theme().sidebar_foreground
                };

                let count_badge_bg = if is_active {
                    cx.theme().sidebar_accent_foreground.opacity(0.15)
                } else {
                    cx.theme().muted.opacity(0.8)
                };

                let item_id = ElementId::NamedInteger("queue-item".into(), queue_id.get() as u64);

                h_flex()
                    .id(item_id)
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded(cx.theme().radius)
                    .text_sm()
                    .cursor_pointer()
                    .when(is_active, |this| {
                        this.font_weight(FontWeight::MEDIUM)
                            .bg(cx.theme().tokens.sidebar_accent)
                            .text_color(cx.theme().sidebar_accent_foreground)
                    })
                    .when(!is_active, |this| {
                        this.text_color(cx.theme().sidebar_foreground)
                            .hover(|this| {
                                this.bg(cx.theme().sidebar_accent.opacity(0.8))
                                    .text_color(cx.theme().sidebar_accent_foreground)
                            })
                    })
                    .when(is_collapsed, |this| {
                        this.justify_center()
                            .child(Icon::new(IconName::Menu).size_4().text_color(text_color))
                    })
                    .when(!is_collapsed, |this| {
                        this.child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .overflow_hidden()
                                .child(Icon::new(IconName::Menu).size_4().text_color(text_color))
                                .child(div().truncate().text_sm().child(name))
                                .when(running, |this| {
                                    this.child(
                                        div().size_1p5().rounded_full().bg(cx.theme().accent),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded(px(4.))
                                .bg(count_badge_bg)
                                .text_xs()
                                .text_color(if is_active {
                                    cx.theme().sidebar_accent_foreground
                                } else {
                                    cx.theme().muted_foreground
                                })
                                .child(format!("{count}")),
                        )
                    })
                    .on_click(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        view.update(cx, |this, cx| {
                            this.select_queue(Some(queue_id), cx);
                        });
                    })
                    .can_drop(|payload, _, _| payload.is::<crate::downloads::QueueDrag>())
                    .on_drop(move |drag: &crate::downloads::QueueDrag, _, _| {
                        handles.dispatch(EngineCommand::MoveToQueue {
                            queue_id,
                            downloads: drag.downloads.clone(),
                        });
                    })
            }))
    }
}
