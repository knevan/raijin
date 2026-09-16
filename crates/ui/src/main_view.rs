use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::sidebar::{Sidebar, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Root, Sizable as _, TitleBar,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::*;
use raijin_core::download::DownloadStatus;
use raijin_core::monitor::MonitorState;
use raijin_core::platform::DiskUsage;
use raijin_core::platform::tray::CloseDecision;

use crate::add_download::open_add_download_window;
use crate::bridge::{EngineCommand, EngineHandles};
use crate::category::{CategoryFilter, FileCategory, StatusFilter};
use crate::downloads::{
    DownloadRow, DownloadsDelegate, MONITOR_POLL_INTERVAL, build_rows, can_pause, can_resume,
    category_icon, filter_rows, toolbar_actions,
};
use crate::format::{format_bytes, format_speed, now_ms};
use crate::tray::{TraySession, summarize_monitor};

/// Default sidebar width in pixels, matching the product sketch.
const SIDEBAR_WIDTH: f32 = 248.0;
/// Sidebar drag limits in pixels.
const SIDEBAR_MIN_WIDTH: f32 = 200.0;
/// Sidebar drag limits in pixels.
const SIDEBAR_MAX_WIDTH: f32 = 360.0;
/// Disk usage refresh throttle; capacity barely moves at monitor tick rates.
const STORAGE_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Main downloads page owning the shell regions and table state.
pub struct MainView {
    handles: EngineHandles,
    tray: TraySession,
    search: Entity<InputState>,
    table: Entity<TableState<DownloadsDelegate>>,
    /// Last-seen monitor snapshot; compared to skip no-op refreshes.
    last_snapshot: MonitorState,
    /// Unfiltered presentation rows rebuilt on every snapshot change.
    cache: Vec<DownloadRow>,
    category: CategoryFilter,
    status: StatusFilter,
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
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search downloads..."));
        let table = cx.new(|cx| {
            // `sortable(false)` hides the table's built-in right-side sort
            // glyph; sorting is driven by the sketch's left-side header
            // content via `DownloadsDelegate::cycle_sort`.
            // `col_selectable(false)` keeps header clicks from entering the
            // framework's column-selection mode (which would wash the whole
            // column with `table_active`); headers only sort.
            TableState::new(DownloadsDelegate::new(handles.clone()), window, cx)
                .row_selectable(true)
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
        let poll_tray = tray.clone();
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(MONITOR_POLL_INTERVAL).await;
                // Low-resource tray mode: the engine keeps running, but the
                // UI sheds table/storage work. Only activity + tooltip
                // (essential timers) refresh; no entity is touched here.
                if !poll_tray.should_poll() {
                    let (active, paused, speed) = summarize_monitor(&monitor.state());
                    poll_tray.update_activity(active, paused, speed);
                    continue;
                }
                let snapshot = monitor.state();
                let alive = weak
                    .update(cx, |view, cx| view.apply_snapshot(snapshot, cx))
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
            search,
            table,
            last_snapshot: MonitorState::default(),
            cache: Vec::new(),
            category: CategoryFilter::All,
            status: StatusFilter::All,
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
    fn apply_snapshot(&mut self, snapshot: MonitorState, cx: &mut Context<Self>) {
        let storage_changed = self.maybe_refresh_storage();
        if self.loaded && !storage_changed && snapshot == self.last_snapshot {
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
        let filtered = filter_rows(&self.cache, self.category, self.status, &self.query);
        self.visible_count = filtered.len();
        self.active_count = self.cache.iter().filter(|row| row.is_active).count();
        self.total_speed_bps = self.cache.iter().map(|row| row.speed_bps).sum();
        // Keep tray activity + tooltip in step with the visible totals; the
        // hidden tick calls `update_activity` directly (see the poll loop).
        let paused_count = self
            .cache
            .iter()
            .filter(|row| matches!(row.status, DownloadStatus::Paused))
            .count();
        self.tray
            .update_activity(self.active_count, paused_count, self.total_speed_bps);
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
        self.selected = None;
        self.table.update(cx, |table, cx| {
            table.clear_selection(cx);
        });
        self.recompute(cx);
    }

    /// Clears the row highlight, e.g. after clicking empty table space.
    ///
    /// Goes through [`TableState::clear_selection`] so the emitted
    /// `ClearSelection` keeps `selected` in step via `on_table_event`.
    fn clear_table_selection(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table.clear_selection(cx);
        });
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

    /// Renders the sidebar as a `Sidebar` of collapsible filter dropdowns.
    ///
    /// `All`, `Finished`, and `Unfinished` are dropdown parents whose children
    /// are the file categories scoped to that status, matching the sketch:
    /// selecting `Finished → Videos` shows finished videos only.
    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, category) = (self.status, self.category);
        let menu = SidebarMenu::new()
            .child(filter_parent(
                "All",
                IconName::Folder,
                StatusFilter::All,
                true,
                status,
                category,
                cx,
            ))
            .child(filter_parent(
                "Finished",
                IconName::CircleCheck,
                StatusFilter::Finished,
                false,
                status,
                category,
                cx,
            ))
            .child(filter_parent(
                "Unfinished",
                IconName::Loader,
                StatusFilter::Unfinished,
                false,
                status,
                category,
                cx,
            ));
        Sidebar::new("main-sidebar")
            .w_full()
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
            .child(menu)
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
                    .on_click(cx.listener(|_, _, window, cx| {
                        window
                            .push_notification("Settings page follows in the M5 continuation", cx);
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
                                    // Sketch HeaderRow has cornerRadius 8 with no outer
                                    // border. Proven by visual probe
                                    // (`examples/radius_probe.rs`): child
                                    // backgrounds ignore wrapper radii, but a
                                    // full border loop follows them — so a 1px
                                    // loop in the header color draws the rounded
                                    // top corners with a near-invisible seam
                                    // (a lone top border renders straight).
                                    .child({
                                        let view = cx.entity();
                                        div()
                                            .id("downloads-table-wrap")
                                            .flex_1()
                                            .min_h_0()
                                            .rounded_tl(px(8.))
                                            .rounded_tr(px(8.))
                                            .border_1()
                                            .border_color(cx.theme().table_head)
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
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

/// Builds one sidebar filter dropdown: a status parent with the file
/// categories scoped to that status as children.
///
/// Parent clicks select the whole status; child clicks select one category
/// within it. Chevron toggles collapse without changing the filter.
fn filter_parent(
    label: &'static str,
    icon: IconName,
    status: StatusFilter,
    open: bool,
    current_status: StatusFilter,
    current_category: CategoryFilter,
    cx: &mut Context<MainView>,
) -> SidebarMenuItem {
    SidebarMenuItem::new(label)
        .icon(icon)
        .active(current_status == status && matches!(current_category, CategoryFilter::All))
        .default_open(open)
        .on_click(cx.listener(move |this, _, _, cx| {
            this.set_filter(status, CategoryFilter::All, cx);
        }))
        .children(FileCategory::all().map(|item| {
            SidebarMenuItem::new(item.label())
                .icon(category_icon(item))
                .active(
                    current_status == status
                        && matches!(current_category, CategoryFilter::Category(current) if current == item),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_filter(status, CategoryFilter::Category(item), cx);
                }))
        }))
}
