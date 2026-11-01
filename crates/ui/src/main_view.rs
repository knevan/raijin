mod queue_ui;

use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::collapsible::Collapsible as UiCollapsible;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::sidebar::{Sidebar, SidebarItem};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Collapsible, Disableable as _, Icon, IconName, Sizable as _, TitleBar,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use raijin_core::download::{DownloadStatus, QueueId};
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
use crate::{ReorderQueueDown, ReorderQueueUp};

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
    /// Last-seen queue snapshot; supplies filters and drop targets.
    queue_state: Vec<QueueSnapshot>,
    /// Unfiltered presentation rows rebuilt on every snapshot change.
    cache: Vec<DownloadRow>,
    category: CategoryFilter,
    status: StatusFilter,
    selected_queue: Option<QueueId>,
    categories_open: bool,
    queues_open: bool,
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
                        view.apply_queue_state(queues, cx);
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
            categories_open: true,
            queues_open: true,
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
        self.cache = build_rows(views, now);
        self.last_snapshot = snapshot;
        self.loaded = true;
        self.recompute(cx);
    }

    /// Stores the latest queue snapshot.
    fn apply_queue_state(&mut self, queues: Vec<QueueSnapshot>, cx: &mut Context<Self>) {
        if queues == self.queue_state {
            return;
        }
        self.queue_state = queues;
        if self
            .selected_queue
            .is_some_and(|id| !self.queue_state.iter().any(|s| s.queue.id == id))
        {
            self.selected_queue = None;
        }
        self.recompute(cx);
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
                let positions: std::collections::HashMap<_, _> = snapshot
                    .items
                    .iter()
                    .enumerate()
                    .map(|(ix, id)| (*id, ix))
                    .collect();
                filtered.sort_by_key(|row| positions.get(&row.id).copied());
            } else {
                filtered.clear();
            }
        }
        self.visible_count = filtered.len();
        self.active_count = self.cache.iter().filter(|row| row.is_active).count();
        self.total_speed_bps = self.cache.iter().map(|row| row.speed_bps).sum();
        let reorderable_queue = if self.query.trim().is_empty() && self.status == StatusFilter::All
        {
            self.selected_queue
        } else {
            None
        };
        self.table.update(cx, |table, cx| {
            table
                .delegate_mut()
                .set_queue_order(self.selected_queue.is_some());
            table
                .delegate_mut()
                .set_reorderable_queue(reorderable_queue);
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

    /// Toggles the categories accordion expansion in the sidebar.
    pub fn toggle_categories(&mut self, cx: &mut Context<Self>) {
        self.categories_open = !self.categories_open;
        cx.notify();
    }

    /// Toggles the queues accordion expansion in the sidebar.
    pub fn toggle_queues(&mut self, cx: &mut Context<Self>) {
        self.queues_open = !self.queues_open;
        cx.notify();
    }

    /// Applies a category filter from the sidebar, preserving the current status filter.
    fn set_category_filter(&mut self, category: CategoryFilter, cx: &mut Context<Self>) {
        if self.category == category && self.selected_queue.is_none() {
            return;
        }
        self.category = category;
        self.selected_queue = None;
        self.selected = None;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            table.clear_selection(cx);
        });
        self.recompute(cx);
    }

    /// Applies a status filter from the status pill selector, preserving the current category/queue.
    fn set_status_filter(&mut self, status: StatusFilter, cx: &mut Context<Self>) {
        if self.status == status {
            return;
        }
        self.status = status;
        self.selected = None;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            table.clear_selection(cx);
        });
        self.recompute(cx);
    }

    /// Filters downloads by queue membership or toggles back to all downloads.
    fn select_queue(&mut self, queue_id: Option<QueueId>, cx: &mut Context<Self>) {
        self.selected_queue = queue_id;
        self.category = CategoryFilter::All;
        self.status = StatusFilter::All;
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

    fn pause_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            self.handles
                .dispatch_in(EngineCommand::Pause(id), window, cx);
        }
        cx.notify();
    }

    fn resume_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            self.handles
                .dispatch_in(EngineCommand::Resume(id), window, cx);
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
            (CategoryFilter::All, StatusFilter::All) => "All Downloads".to_owned(),
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
        let categories_section = CategoriesSection {
            category: self.category,
            selected_queue: self.selected_queue,
            open: self.categories_open,
            view: cx.entity(),
            collapsed: false,
        };

        let queue_section = QueueSection {
            queues: self.queue_state.clone(),
            selected_queue: self.selected_queue,
            open: self.queues_open,
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
            .child(MainSidebarSection::Categories(Box::new(categories_section)))
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
}

/// Renders a vertical action button for the ABDM-styled toolbar (icon on top, label on bottom).
fn toolbar_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    label: &'static str,
    disabled: bool,
    tooltip: Option<&'static str>,
    cx: &App,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let text_color = if disabled {
        cx.theme().muted_foreground.opacity(0.4)
    } else {
        cx.theme().foreground
    };
    let icon_color = if disabled {
        cx.theme().muted_foreground.opacity(0.4)
    } else {
        cx.theme().foreground
    };

    v_flex()
        .id(id.into())
        .items_center()
        .justify_center()
        .gap(px(2.))
        .py_1()
        .px(px(6.))
        .min_w(px(54.))
        .h(px(48.))
        .rounded_md()
        .when(!disabled, |this| {
            this.cursor_pointer()
                .hover(|s| {
                    s.bg(cx.theme().list_hover)
                        .text_color(cx.theme().foreground)
                })
                .active(|s| s.bg(cx.theme().list_active))
                .on_click(on_click)
        })
        .when(disabled, |this| this.cursor_not_allowed())
        .when_some(tooltip, |this, tip| {
            this.tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        })
        .child(icon.into().size(px(18.)).text_color(icon_color))
        .child(div().text_xs().text_color(text_color).child(label))
}

/// Subtle vertical divider separating toolbar action groups.
fn toolbar_divider(cx: &App) -> impl IntoElement {
    div()
        .w(px(1.0))
        .h(px(24.0))
        .self_center()
        .mx(px(2.0))
        .bg(cx.theme().border.opacity(0.6))
}

impl MainView {
    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let targets = self.target_statuses(cx);
        let has_targets = !targets.is_empty();
        let all: Vec<DownloadStatus> = self.cache.iter().map(|row| row.status).collect();
        let actions = toolbar_actions(&targets, &all);

        let (all_count, unfinished_count, finished_count) = self.status_counts();
        let current_status = self.status;
        let view = cx.entity();

        let (active_label, active_count) = match current_status {
            StatusFilter::All => ("All", all_count),
            StatusFilter::Unfinished => ("Downloading", unfinished_count),
            StatusFilter::Finished => ("Finished", finished_count),
        };

        let status_dropdown = {
            let view_all = view.clone();
            let view_unf = view.clone();
            let view_fin = view;
            Button::new("status-filter-dropdown")
                .ghost()
                .small()
                .icon(Icon::new(gpui_kit::assets::IconName::ListFilter))
                .label(format!("{active_label} ({active_count})"))
                .dropdown_caret(true)
                .tooltip("Filter by status")
                .dropdown_menu(move |menu, _window, _cx| {
                    let view_all = view_all.clone();
                    let view_unf = view_unf.clone();
                    let view_fin = view_fin.clone();
                    menu.item(
                        PopupMenuItem::new(format!("All ({all_count})"))
                            .checked(current_status == StatusFilter::All)
                            .on_click(move |_, _, cx| {
                                view_all.update(cx, |this, cx| {
                                    this.set_status_filter(StatusFilter::All, cx);
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new(format!("Downloading ({unfinished_count})"))
                            .checked(current_status == StatusFilter::Unfinished)
                            .on_click(move |_, _, cx| {
                                view_unf.update(cx, |this, cx| {
                                    this.set_status_filter(StatusFilter::Unfinished, cx);
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new(format!("Finished ({finished_count})"))
                            .checked(current_status == StatusFilter::Finished)
                            .on_click(move |_, _, cx| {
                                view_fin.update(cx, |this, cx| {
                                    this.set_status_filter(StatusFilter::Finished, cx);
                                });
                            }),
                    )
                })
        };

        h_flex()
            .flex_wrap()
            .gap(px(2.))
            .items_center()
            .p(px(2.5))
            .rounded_lg()
            .bg(cx.theme().muted.opacity(0.3))
            .border_1()
            .border_color(cx.theme().border.opacity(0.5))
            // Group 1: Item Actions
            .child(toolbar_button(
                "toolbar-resume",
                IconName::Play,
                "Resume",
                !actions.can_resume,
                Some("Resume selected download"),
                cx,
                cx.listener(|this, _, window, cx| this.resume_selected(window, cx)),
            ))
            .child(toolbar_button(
                "toolbar-pause",
                IconName::Pause,
                "Pause",
                !actions.can_pause,
                Some("Pause selected download"),
                cx,
                cx.listener(|this, _, window, cx| this.pause_selected(window, cx)),
            ))
            // Divider 1
            .child(toolbar_divider(cx))
            // Group 2: Queue Management
            .child(toolbar_button(
                "move-to-queue",
                Icon::new(gpui_kit::assets::IconName::SquareArrowRight),
                "Move to queue",
                !has_targets,
                Some("Move selected download(s) to a queue"),
                cx,
                cx.listener(|this, _, window, cx| this.move_to_queue(window, cx)),
            ))
            .child(toolbar_button(
                "new-queue",
                Icon::new(gpui_kit::assets::IconName::LayersPlus),
                "New queue",
                false,
                Some("Create a new download queue"),
                cx,
                cx.listener(|this, _, window, cx| this.new_queue(window, cx)),
            ))
            // Divider 2
            .child(toolbar_divider(cx))
            // Group 3: Global Actions
            .child(toolbar_button(
                "toolbar-pause-all",
                gpui_kit::assets::IconName::CirclePause,
                "Stop all",
                !actions.can_pause_all,
                Some("Pause all active downloads"),
                cx,
                cx.listener(|this, _, _, cx| this.pause_all(cx)),
            ))
            .child(toolbar_button(
                "toolbar-remove",
                gpui_kit::assets::IconName::Trash,
                "Delete",
                !actions.can_remove,
                Some("Remove selected download(s)"),
                cx,
                cx.listener(|this, _, _, cx| this.remove_selected(cx)),
            ))
            // Divider 3
            .child(toolbar_divider(cx))
            // Spacer to keep view filter and settings on the right
            .child(div().flex_1())
            // Status Filter Dropdown
            .child(status_dropdown)
            .child(toolbar_divider(cx))
            // Group 5: Settings
            .child(toolbar_button(
                "toolbar-settings",
                IconName::Settings,
                "Settings",
                false,
                Some("Settings"),
                cx,
                cx.listener(|this, _, _window, cx| {
                    open_settings_window(cx, &this.settings);
                }),
            ))
    }

    fn status_counts(&self) -> (usize, usize, usize) {
        let mut all = 0;
        let mut unfinished = 0;
        let mut finished = 0;

        let queue_member_ids: Option<std::collections::HashSet<_>> =
            self.selected_queue.and_then(|qid| {
                self.queue_state
                    .iter()
                    .find(|s| s.queue.id == qid)
                    .map(|s| s.items.iter().copied().collect())
            });

        for row in &self.cache {
            if !self.category.matches(&row.category_label) {
                continue;
            }
            if let Some(ref members) = queue_member_ids
                && !members.contains(&row.id)
            {
                continue;
            }
            if StatusFilter::All.matches(row.status) {
                all += 1;
            }
            if StatusFilter::Unfinished.matches(row.status) {
                unfinished += 1;
            }
            if StatusFilter::Finished.matches(row.status) {
                finished += 1;
            }
        }
        (all, unfinished, finished)
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
            .on_action(cx.listener(|this, _: &ReorderQueueUp, window, cx| {
                this.reorder_selected(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ReorderQueueDown, window, cx| {
                this.reorder_selected(false, window, cx);
            }))
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
                                    .when_some(self.render_queue_banner(cx), |this, banner| {
                                        this.child(banner)
                                    })
                                    .child(self.render_toolbar(cx))
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
                                                    .header_row_height(px(
                                                        crate::downloads::TABLE_HEADER_HEIGHT_PX,
                                                    ))
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

/// Dedicated sidebar section for managing and filtering category downloads.
#[derive(Clone)]
struct CategoriesSection {
    category: CategoryFilter,
    selected_queue: Option<QueueId>,
    open: bool,
    view: Entity<MainView>,
    collapsed: bool,
}

impl Collapsible for CategoriesSection {
    fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }
}

impl SidebarItem for CategoriesSection {
    fn render(
        self,
        id: impl Into<ElementId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let id = id.into();
        let is_collapsed = self.collapsed;
        let is_open = !is_collapsed && self.open;
        let is_parent_active =
            matches!(self.category, CategoryFilter::All) && self.selected_queue.is_none();

        let header_text_color = if is_parent_active {
            cx.theme().foreground
        } else {
            cx.theme().sidebar_foreground
        };

        let view_click = self.view.clone();
        let view_toggle = self.view.clone();

        let header = h_flex()
            .id("categories-header")
            .w_full()
            .h(px(30.))
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .overflow_hidden()
            .when(!is_parent_active, |this| {
                this.hover(|this| {
                    this.bg(cx.theme().list_hover)
                        .text_color(cx.theme().foreground)
                })
            })
            .when(is_parent_active, |this| {
                this.font_weight(FontWeight::MEDIUM)
                    .bg(cx.theme().list_active)
                    .text_color(cx.theme().foreground)
            })
            .when(is_collapsed, |this| {
                this.justify_center().child(
                    Icon::new(IconName::Folder)
                        .size_4()
                        .text_color(header_text_color),
                )
            })
            .when(!is_collapsed, |this| {
                this.child(
                    div()
                        .w(px(3.0))
                        .h(px(15.0))
                        .rounded_full()
                        .when(is_parent_active, |this| this.bg(cx.theme().info))
                        .when(!is_parent_active, |this| this.bg(gpui::transparent_black())),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_2()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::Folder)
                                        .size_4()
                                        .text_color(header_text_color),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(header_text_color)
                                        .child("All Downloads"),
                                ),
                        )
                        .child(
                            Button::new("categories-caret")
                                .xsmall()
                                .ghost()
                                .icon(
                                    Icon::new(IconName::ChevronRight)
                                        .size_4()
                                        .when(is_open, |this| this.rotate(percentage(90. / 360.))),
                                )
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    view_toggle.update(cx, |this, cx| this.toggle_categories(cx));
                                }),
                        ),
                )
            })
            .on_click(move |_, _, cx| {
                view_click.update(cx, |this, cx| {
                    let was_active = matches!(this.category, CategoryFilter::All)
                        && this.selected_queue.is_none();
                    this.set_category_filter(CategoryFilter::All, cx);
                    if was_active {
                        this.toggle_categories(cx);
                    } else {
                        this.categories_open = true;
                        cx.notify();
                    }
                });
            });

        let view_items = self.view.clone();
        let current_category = self.category;
        let selected_queue = self.selected_queue;

        let submenu = v_flex()
            .id("categories-submenu")
            .w_full()
            .gap(px(2.))
            .pt(px(2.))
            .children(
                FileCategory::all()
                    .into_iter()
                    .chain([FileCategory::General])
                    .map(move |item| {
                        let is_child_active = selected_queue.is_none()
                            && matches!(
                                current_category,
                                CategoryFilter::Category(current) if current == item
                            );

                        let child_text_color = if is_child_active {
                            cx.theme().foreground
                        } else {
                            cx.theme().sidebar_foreground
                        };

                        let view = view_items.clone();

                        h_flex()
                            .id(SharedString::from(format!("cat-{}", item.label())))
                            .w_full()
                            .h(px(28.))
                            .items_center()
                            .rounded_md()
                            .cursor_pointer()
                            .overflow_hidden()
                            .when(!is_child_active, |this| {
                                this.hover(|this| {
                                    this.bg(cx.theme().list_hover)
                                        .text_color(cx.theme().foreground)
                                })
                            })
                            .when(is_child_active, |this| {
                                this.font_weight(FontWeight::MEDIUM)
                                    .bg(cx.theme().list_active)
                                    .text_color(cx.theme().foreground)
                            })
                            .child(
                                div()
                                    .w(px(3.0))
                                    .h(px(14.0))
                                    .rounded_full()
                                    .when(is_child_active, |this| this.bg(cx.theme().info))
                                    .when(!is_child_active, |this| {
                                        this.bg(gpui::transparent_black())
                                    }),
                            )
                            .child(
                                h_flex()
                                    .flex_1()
                                    .items_center()
                                    .gap_2()
                                    .pl_7()
                                    .pr_2()
                                    .child(
                                        Icon::new(category_icon(item))
                                            .size_4()
                                            .text_color(child_text_color),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(child_text_color)
                                            .child(item.label()),
                                    ),
                            )
                            .on_click(move |_, _, cx| {
                                view.update(cx, |this, cx| {
                                    this.set_category_filter(CategoryFilter::Category(item), cx);
                                });
                            })
                    }),
            );

        v_flex().id(id).w_full().child(
            UiCollapsible::new()
                .motion_id("categories-motion")
                .open(is_open)
                .child(header)
                .content(submenu),
        )
    }
}

/// Top-level section items rendered in the main navigation sidebar.
#[derive(Clone)]
enum MainSidebarSection {
    Categories(Box<CategoriesSection>),
    Queues(Box<QueueSection>),
}

impl Collapsible for MainSidebarSection {
    fn is_collapsed(&self) -> bool {
        match self {
            Self::Categories(categories) => categories.is_collapsed(),
            Self::Queues(queues) => queues.is_collapsed(),
        }
    }

    fn collapsed(self, collapsed: bool) -> Self {
        match self {
            Self::Categories(categories) => {
                Self::Categories(Box::new(categories.collapsed(collapsed)))
            }
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
            Self::Categories(categories) => categories.render(id, window, cx).into_any_element(),
            Self::Queues(queues) => queues.render(id, window, cx).into_any_element(),
        }
    }
}

/// Dedicated sidebar section for managing and filtering download queues.
#[derive(Clone)]
struct QueueSection {
    queues: Vec<QueueSnapshot>,
    selected_queue: Option<QueueId>,
    open: bool,
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
        let is_open = !is_collapsed && self.open;

        let view_toggle = self.view.clone();
        let view_click = self.view.clone();
        let view_add = self.view.clone();

        let header = h_flex()
            .id("queues-header")
            .w_full()
            .h(px(30.))
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .overflow_hidden()
            .hover(|this| {
                this.bg(cx.theme().list_hover)
                    .text_color(cx.theme().foreground)
            })
            .when(is_collapsed, |this| {
                this.justify_center().child(
                    Icon::new(IconName::Menu)
                        .size_4()
                        .text_color(cx.theme().sidebar_foreground),
                )
            })
            .when(!is_collapsed, |this| {
                this.child(
                    div()
                        .w(px(3.0))
                        .h(px(15.0))
                        .rounded_full()
                        .bg(gpui::transparent_black()),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_2()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::Menu)
                                        .size_4()
                                        .text_color(cx.theme().sidebar_foreground),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().sidebar_foreground)
                                        .child("Queues"),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    Button::new("add-queue-header")
                                        .xsmall()
                                        .ghost()
                                        .icon(IconName::Plus)
                                        .tooltip("New queue")
                                        .on_click(move |_, window, cx| {
                                            cx.stop_propagation();
                                            view_add
                                                .update(cx, |this, cx| this.new_queue(window, cx));
                                        }),
                                )
                                .child(
                                    Button::new("queues-caret")
                                        .xsmall()
                                        .ghost()
                                        .icon(
                                            Icon::new(IconName::ChevronRight)
                                                .size_4()
                                                .when(is_open, |this| {
                                                    this.rotate(percentage(90. / 360.))
                                                }),
                                        )
                                        .on_click(move |_, _, cx| {
                                            cx.stop_propagation();
                                            view_toggle
                                                .update(cx, |this, cx| this.toggle_queues(cx));
                                        }),
                                ),
                        ),
                )
            })
            .on_click(move |_, _, cx| {
                view_click.update(cx, |this, cx| this.toggle_queues(cx));
            });

        let view_items = self.view.clone();
        let selected_queue = self.selected_queue;
        let handles = self.handles.clone();

        let submenu = v_flex()
            .id("queues-submenu")
            .w_full()
            .gap(px(2.))
            .pt(px(2.))
            .when(self.queues.is_empty(), |this| {
                this.child(
                    div()
                        .w_full()
                        .pl_7()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("No queues"),
                )
            })
            .children(self.queues.into_iter().map(move |snapshot| {
                let queue_id = snapshot.queue.id;
                let count = snapshot.items.len();
                let running = snapshot.active;
                let is_active = selected_queue == Some(queue_id);
                let name = snapshot.queue.name;
                let handles = handles.clone();
                let view = view_items.clone();

                let row_text_color = if is_active {
                    cx.theme().foreground
                } else {
                    cx.theme().sidebar_foreground
                };

                let item_id = SharedString::from(format!("queue-item-{}", queue_id.get()));
                let label = format!(
                    "{name} · {count} · {}",
                    if running { "Active" } else { "Stopped" }
                );

                h_flex()
                    .id(item_id)
                    .w_full()
                    .h(px(28.))
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .overflow_hidden()
                    .when(!is_active, |this| {
                        this.hover(|this| {
                            this.bg(cx.theme().list_hover)
                                .text_color(cx.theme().foreground)
                        })
                    })
                    .when(is_active, |this| {
                        this.font_weight(FontWeight::MEDIUM)
                            .bg(cx.theme().list_active)
                            .text_color(cx.theme().foreground)
                    })
                    .child(
                        div()
                            .w(px(3.0))
                            .h(px(14.0))
                            .rounded_full()
                            .when(is_active, |this| this.bg(cx.theme().info))
                            .when(!is_active, |this| this.bg(gpui::transparent_black())),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .items_center()
                            .gap_2()
                            .pl_7()
                            .pr_2()
                            .child(
                                Icon::new(IconName::Menu)
                                    .size_4()
                                    .text_color(row_text_color),
                            )
                            .child(div().text_sm().text_color(row_text_color).child(label)),
                    )
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| this.select_queue(Some(queue_id), cx));
                    })
                    .can_drop(|payload, _, _| payload.is::<crate::downloads::QueueDrag>())
                    .on_drop(move |drag: &crate::downloads::QueueDrag, window, cx| {
                        handles.dispatch_in(
                            EngineCommand::MoveToQueue {
                                queue_id,
                                downloads: drag.downloads.clone(),
                            },
                            window,
                            cx,
                        );
                    })
            }));

        v_flex().id(id).w_full().pt(px(6.)).child(
            UiCollapsible::new()
                .motion_id("queues-motion")
                .open(is_open)
                .child(header)
                .content(submenu),
        )
    }
}
