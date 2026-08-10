use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use freya::animation::*;
use freya::icons;
use freya::prelude::*;

use super::services::AppServices;
use super::theme;
use crate::download::{Bytes, DownloadId, DownloadStatus, NewDownload, QueueId};
use crate::monitor::{DownloadView, MonitorState};

const COLUMN_COUNT: usize = 6;
const SELECTION_COLUMN_WIDTH: f32 = 44.;
const ROW_HEIGHT: f32 = 52.;
const ROW_DETAILS_HEIGHT: f32 = 48.;

#[derive(Clone, PartialEq)]
struct DownloadRowData {
    id: DownloadId,
    file_name: String,
    kind: String,
    size_bytes: Option<Bytes>,
    status: DownloadStatus,
    speed_bps: u64,
    time_left: Option<u64>,
    date_added: String,
    progress: f32,
    connections: u16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableColumn {
    Name,
    Size,
    Status,
    Speed,
    TimeLeft,
    DateAdded,
}

impl TableColumn {
    const ALL: [Self; COLUMN_COUNT] = [
        Self::Name,
        Self::Size,
        Self::Status,
        Self::Speed,
        Self::TimeLeft,
        Self::DateAdded,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Size => "Size",
            Self::Status => "Status",
            Self::Speed => "Speed",
            Self::TimeLeft => "Time Left",
            Self::DateAdded => "Date Added",
        }
    }

    const fn min_width(self) -> f32 {
        match self {
            Self::Name => 240.,
            Self::Size => 90.,
            Self::Status => 110.,
            Self::Speed => 110.,
            Self::TimeLeft => 120.,
            Self::DateAdded => 130.,
        }
    }

    const fn default_width(self) -> f32 {
        match self {
            Self::Name => 360.,
            Self::Size => 120.,
            Self::Status => 150.,
            Self::Speed => 140.,
            Self::TimeLeft => 160.,
            Self::DateAdded => 180.,
        }
    }

    const fn fills_remaining_width(self) -> bool {
        matches!(self, Self::DateAdded)
    }
}

#[derive(Clone, Copy, PartialEq)]
struct ColumnWidths {
    name: State<f32>,
    size: State<f32>,
    status: State<f32>,
    speed: State<f32>,
    time_left: State<f32>,
    date_added: State<f32>,
}

impl ColumnWidths {
    const fn width_for(self, column: TableColumn) -> State<f32> {
        match column {
            TableColumn::Name => self.name,
            TableColumn::Size => self.size,
            TableColumn::Status => self.status,
            TableColumn::Speed => self.speed,
            TableColumn::TimeLeft => self.time_left,
            TableColumn::DateAdded => self.date_added,
        }
    }

    fn min_table_width(self) -> f32 {
        SELECTION_COLUMN_WIDTH
            + TableColumn::ALL
                .iter()
                .map(|column| (self.width_for(*column))().max(column.min_width()))
                .sum::<f32>()
    }
}

#[derive(Clone, PartialEq)]
struct VirtualRowsData {
    rows: Vec<DownloadRowData>,
    widths: ColumnWidths,
    expanded_ids: State<Vec<DownloadId>>,
    selected_ids: State<Vec<DownloadId>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectBoxState {
    Checked,
    Partial,
    Unchecked,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortDirection {
    Ascending,
    Descending,
}

impl SortDirection {
    const fn toggled(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SortState {
    column: TableColumn,
    direction: SortDirection,
}

impl SortState {
    fn next_for(self, column: TableColumn) -> Self {
        if self.column == column {
            Self {
                column,
                direction: self.direction.toggled(),
            }
        } else {
            Self {
                column,
                direction: SortDirection::Ascending,
            }
        }
    }

    fn compare(self, a: &DownloadRowData, b: &DownloadRowData) -> Ordering {
        let ordering = match self.column {
            TableColumn::Name => a.file_name.cmp(&b.file_name),
            TableColumn::Size => byte_sort_value(a.size_bytes).cmp(&byte_sort_value(b.size_bytes)),
            TableColumn::Status => status_order(a.status).cmp(&status_order(b.status)),
            TableColumn::Speed => a.speed_bps.cmp(&b.speed_bps),
            TableColumn::TimeLeft => a
                .time_left
                .unwrap_or(u64::MAX)
                .cmp(&b.time_left.unwrap_or(u64::MAX)),
            TableColumn::DateAdded => a.date_added.cmp(&b.date_added),
        };

        match self.direction {
            SortDirection::Ascending => ordering,
            SortDirection::Descending => ordering.reverse(),
        }
    }
}

#[derive(PartialEq)]
pub(crate) struct DownloadsTable;

impl Component for DownloadsTable {
    fn render(&self) -> impl IntoElement {
        let services = use_consume::<AppServices>();
        let mut monitor_state = use_state(MonitorState::default);
        let monitor_services = services.clone();
        use_hook(move || {
            let mut receiver = monitor_services.monitor.subscribe();
            spawn(async move {
                monitor_state.set(receiver.borrow().clone());
                while receiver.changed().await.is_ok() {
                    monitor_state.set(receiver.borrow().clone());
                }
            });
        });

        let column_widths = ColumnWidths {
            name: use_state(|| TableColumn::Name.default_width()),
            size: use_state(|| TableColumn::Size.default_width()),
            status: use_state(|| TableColumn::Status.default_width()),
            speed: use_state(|| TableColumn::Speed.default_width()),
            time_left: use_state(|| TableColumn::TimeLeft.default_width()),
            date_added: use_state(|| TableColumn::DateAdded.default_width()),
        };
        let mut sort = use_state(|| SortState {
            column: TableColumn::Name,
            direction: SortDirection::Ascending,
        });
        let sorted_rows = use_memo(move || {
            let mut rows = rows_from_state(&monitor_state.read());
            rows.sort_by(|a, b| sort().compare(a, b));
            rows
        });
        let rows = sorted_rows.read().clone();
        let expanded_ids = use_state(|| {
            rows.iter()
                .filter(|item| item.status == DownloadStatus::Downloading)
                .map(|item| item.id)
                .collect::<Vec<_>>()
        });
        let selected_ids = use_state(Vec::<DownloadId>::new);
        let selected_count = selected_ids.read().len();

        rect()
            .expanded()
            .background(theme::RAIJIN_BACKGROUND)
            .border(left_border())
            .padding(Gaps::new(10., 10., 12., 10.))
            .vertical()
            .spacing(8.)
            .child(DownloadsToolbar {
                services,
                selected_ids,
                selected_count,
                snapshot: monitor_state.read().clone(),
            })
            .child(
                ScrollView::new()
                    .direction(Direction::Horizontal)
                    .child(TableSurface {
                        rows,
                        widths: column_widths,
                        expanded_ids,
                        selected_ids,
                        sort: sort(),
                        on_sort: (move |column| sort.set(sort().next_for(column))).into(),
                    }),
            )
    }
}

#[derive(Clone)]
struct DownloadsToolbar {
    services: AppServices,
    selected_ids: State<Vec<DownloadId>>,
    selected_count: usize,
    snapshot: MonitorState,
}

impl PartialEq for DownloadsToolbar {
    fn eq(&self, other: &Self) -> bool {
        self.selected_count == other.selected_count && self.snapshot == other.snapshot
    }
}

impl Component for DownloadsToolbar {
    fn render(&self) -> impl IntoElement {
        let has_selection = self.selected_count > 0;
        let services = self.services.clone();
        let open_services = self.services.clone();
        let stop_snapshot = self.snapshot.clone();

        rect()
            .width(Size::fill())
            .height(Size::px(52.))
            .horizontal()
            .cross_align(Alignment::Center)
            .spacing(10.)
            .padding(Gaps::new(0., 8., 0., 0.))
            .background(theme::RAIJIN_BACKGROUND)
            .child(toolbar_button(
                "New Download",
                icons::lucide::link_2(),
                true,
                true,
                (move |()| open_new_download_window(open_services.clone())).into(),
            ))
            .child(toolbar_divider())
            .child(toolbar_button(
                "Resume",
                icons::lucide::play(),
                has_selection,
                false,
                {
                    let selected_ids = self.selected_ids;
                    (move |()| {
                        run_for_selected(
                            services.clone(),
                            selected_ids.read().clone(),
                            DownloadAction::Resume,
                        )
                    })
                    .into()
                },
            ))
            .child(toolbar_button(
                "Pause",
                icons::lucide::pause(),
                has_selection,
                false,
                {
                    let services = self.services.clone();
                    let selected_ids = self.selected_ids;
                    (move |()| {
                        run_for_selected(
                            services.clone(),
                            selected_ids.read().clone(),
                            DownloadAction::Pause,
                        )
                    })
                    .into()
                },
            ))
            .child(toolbar_button(
                "Stop All",
                icons::lucide::square_stop(),
                true,
                false,
                {
                    let services = self.services.clone();
                    (move |()| stop_all_active(services.clone(), stop_snapshot.clone())).into()
                },
            ))
            .child(toolbar_button(
                "Delete",
                icons::lucide::trash_2(),
                has_selection,
                false,
                {
                    let services = self.services.clone();
                    let mut selected_ids = self.selected_ids;
                    (move |()| {
                        let selected = selected_ids.read().clone();
                        selected_ids.set(Vec::new());
                        run_for_selected(services.clone(), selected, DownloadAction::Remove);
                    })
                    .into()
                },
            ))
            .child(toolbar_divider())
            .child(toolbar_button(
                "Settings",
                icons::lucide::settings(),
                true,
                false,
                noop_toolbar_action(),
            ))
    }
}

#[derive(PartialEq)]
struct TableSurface {
    rows: Vec<DownloadRowData>,
    widths: ColumnWidths,
    expanded_ids: State<Vec<DownloadId>>,
    selected_ids: State<Vec<DownloadId>>,
    sort: SortState,
    on_sort: EventHandler<TableColumn>,
}

impl Component for TableSurface {
    fn render(&self) -> impl IntoElement {
        let rows = self.rows.clone();
        let expanded_ids = self.expanded_ids;
        let data = VirtualRowsData {
            rows: rows.clone(),
            widths: self.widths,
            expanded_ids,
            selected_ids: self.selected_ids,
        };
        let row_heights = rows.clone();
        let current_expanded = expanded_ids.read().clone();

        rect()
            .width(Size::fill())
            .min_width(Size::px(self.widths.min_table_width()))
            .height(Size::fill())
            .vertical()
            .spacing(8.)
            .child(TableHeader {
                column_widths: self.widths,
                rows: rows.clone(),
                selected_ids: self.selected_ids,
                sort: self.sort,
                on_sort: self.on_sort.clone(),
            })
            .child(
                VirtualScrollView::new_with_data(data, move |item, data: &VirtualRowsData| {
                    DownloadTableRow::new(
                        data.rows[item.index].clone(),
                        data.widths,
                        data.expanded_ids,
                        data.selected_ids,
                    )
                    .key(data.rows[item.index].id.get())
                    .into_element()
                })
                .length(rows.len())
                .item_size(move |index: usize| {
                    let base = ROW_HEIGHT;
                    if current_expanded.contains(&row_heights[index].id) {
                        base + ROW_DETAILS_HEIGHT
                    } else {
                        base
                    }
                })
                .height(Size::fill()),
            )
    }
}

#[derive(PartialEq)]
struct TableHeader {
    column_widths: ColumnWidths,
    rows: Vec<DownloadRowData>,
    selected_ids: State<Vec<DownloadId>>,
    sort: SortState,
    on_sort: EventHandler<TableColumn>,
}

impl Component for TableHeader {
    fn render(&self) -> impl IntoElement {
        let all_ids = self.rows.iter().map(|row| row.id).collect::<Vec<_>>();
        let selected = self.selected_ids.read();
        let selected_count = all_ids.iter().filter(|id| selected.contains(id)).count();
        let selection = match selected_count {
            0 => SelectBoxState::Unchecked,
            count if count == all_ids.len() => SelectBoxState::Checked,
            _ => SelectBoxState::Partial,
        };
        let mut selected_ids = self.selected_ids;

        rect()
            .height(Size::px(40.))
            .width(Size::fill())
            .horizontal()
            .background(theme::SURFACE_ELEVATED)
            .corner_radius(7.)
            .border(Border::new().width(1.).fill(theme::BORDER))
            .overflow(Overflow::Clip)
            .child(selection_box(
                selection,
                (move |()| {
                    if selected_count == all_ids.len() {
                        selected_ids.set(Vec::new());
                    } else {
                        selected_ids.set(all_ids.clone());
                    }
                })
                .into(),
            ))
            .children(TableColumn::ALL.iter().map(|column| {
                HeaderCell {
                    column: *column,
                    widths: self.column_widths,
                    sort: self.sort,
                    on_sort: self.on_sort.clone(),
                }
                .into_element()
            }))
    }
}

#[derive(PartialEq)]
struct HeaderCell {
    column: TableColumn,
    widths: ColumnWidths,
    sort: SortState,
    on_sort: EventHandler<TableColumn>,
}

#[derive(Clone, Copy, PartialEq)]
struct ResizeDrag {
    start_x: f32,
    start_width: f32,
}

impl Component for HeaderCell {
    fn render(&self) -> impl IntoElement {
        let mut drag = use_state(|| None::<ResizeDrag>);
        let mut width_state = self.widths.width_for(self.column);
        let column = self.column;
        let on_sort = self.on_sort.clone();
        let width = width_state();
        let start_width = width;

        let on_resize = move |e: Event<PointerEventData>| {
            let Some(drag_state) = drag() else {
                return;
            };
            e.prevent_default();
            let delta = e.global_location().x as f32 - drag_state.start_x;
            let new_width = (drag_state.start_width + delta)
                .max(column.min_width())
                .round();
            let current_width = width_state();
            if (current_width - new_width).abs() < f32::EPSILON {
                return;
            }
            width_state.set_if_modified(new_width);
        };

        rect()
            .height(Size::fill())
            .width(column_width(self.column, width))
            .min_width(Size::px(column_min_width(self.column, width)))
            .horizontal()
            .cross_align(Alignment::Center)
            .padding(Gaps::new(0., 0., 0., 10.))
            .on_press(move |_| on_sort.call(column))
            .child(sort_icon(self.sort, self.column))
            .child(
                label()
                    .text(self.column.label())
                    .font_size(14.)
                    .color(theme::TEXT_MUTED)
                    .width(Size::fill()),
            )
            .child(
                rect()
                    .width(Size::px(7.))
                    .height(Size::fill())
                    .center()
                    .on_pointer_down(move |e: Event<PointerEventData>| {
                        if e.data().is_primary() {
                            e.stop_propagation();
                            e.prevent_default();
                            drag.set(Some(ResizeDrag {
                                start_x: e.global_location().x as f32,
                                start_width,
                            }));
                        }
                    })
                    .on_global_pointer_press(move |_| drag.set(None))
                    .on_capture_global_pointer_move(on_resize)
                    .child(
                        rect()
                            .width(Size::px(1.))
                            .height(Size::px(20.))
                            .background(theme::BORDER),
                    ),
            )
    }
}

#[derive(PartialEq)]
struct DownloadTableRow {
    item: DownloadRowData,
    column_widths: ColumnWidths,
    expanded_ids: State<Vec<DownloadId>>,
    selected_ids: State<Vec<DownloadId>>,
    key: DiffKey,
}

impl DownloadTableRow {
    fn new(
        item: DownloadRowData,
        column_widths: ColumnWidths,
        expanded_ids: State<Vec<DownloadId>>,
        selected_ids: State<Vec<DownloadId>>,
    ) -> Self {
        Self {
            item,
            column_widths,
            expanded_ids,
            selected_ids,
            key: DiffKey::None,
        }
    }
}

impl KeyExt for DownloadTableRow {
    fn write_key(&mut self) -> &mut DiffKey {
        &mut self.key
    }
}

impl Component for DownloadTableRow {
    fn render(&self) -> impl IntoElement {
        let expanded = self.expanded_ids.read().contains(&self.item.id);
        let selected = self.selected_ids.read().contains(&self.item.id);
        let mut selected_ids = self.selected_ids;
        let item_id = self.item.id;

        rect()
            .width(Size::fill())
            .vertical()
            .corner_radius(7.)
            .background(if expanded {
                Color::from_rgb(29, 30, 34)
            } else {
                theme::RAIJIN_BACKGROUND
            })
            .child(
                rect()
                    .height(Size::px(52.))
                    .width(Size::fill())
                    .horizontal()
                    .cross_align(Alignment::Center)
                    .child(selection_box(
                        if selected {
                            SelectBoxState::Checked
                        } else {
                            SelectBoxState::Unchecked
                        },
                        (move |()| {
                            let mut next = selected_ids.read().clone();
                            if let Some(index) = next.iter().position(|id| *id == item_id) {
                                next.remove(index);
                            } else {
                                next.push(item_id);
                            }
                            selected_ids.set_if_modified(next);
                        })
                        .into(),
                    ))
                    .children(TableColumn::ALL.iter().map(|column| {
                        RowCell {
                            column: *column,
                            widths: self.column_widths,
                            item: self.item.clone(),
                            expanded_ids: self.expanded_ids,
                        }
                        .into_element()
                    })),
            )
            .maybe(expanded, |el| {
                el.child(RowDetails {
                    item: self.item.clone(),
                    widths: self.column_widths,
                })
            })
    }

    fn render_key(&self) -> DiffKey {
        self.key.clone().or(self.default_key())
    }
}

#[derive(PartialEq)]
struct RowDetails {
    item: DownloadRowData,
    widths: ColumnWidths,
}

impl Component for RowDetails {
    fn render(&self) -> impl IntoElement {
        let left_offset = (self.widths.name)() + 44.;

        rect()
            .width(Size::fill())
            .height(Size::px(48.))
            .padding(Gaps::new(4., 24., 10., left_offset))
            .vertical()
            .spacing(7.)
            .child(
                label()
                    .text(format!(
                        "{} connections | {:.0}% complete",
                        self.item.connections,
                        self.item.progress * 100.
                    ))
                    .font_size(11.)
                    .color(theme::TEXT_SUBTLE),
            )
    }
}

#[derive(PartialEq)]
struct ProgressBar {
    color: Color,
    progress: f32,
}

impl Component for ProgressBar {
    fn render(&self) -> impl IntoElement {
        let progress = self.progress.clamp(0., 1.);
        let animation = use_animation_transition(progress, |from, to| {
            AnimNum::new(from, to).time(450).ease(Ease::InOut)
        });
        let width = animation.get().value() * 100.;

        rect()
            .width(Size::fill())
            .height(Size::px(7.))
            .corner_radius(99.)
            .background(Color::from_rgb(42, 44, 50))
            .overflow(Overflow::Clip)
            .child(
                rect()
                    .height(Size::fill())
                    .width(Size::percent(width))
                    .corner_radius(99.)
                    .background(self.color),
            )
    }
}

#[derive(PartialEq)]
struct RowCell {
    column: TableColumn,
    widths: ColumnWidths,
    item: DownloadRowData,
    expanded_ids: State<Vec<DownloadId>>,
}

impl Component for RowCell {
    fn render(&self) -> impl IntoElement {
        let width = (self.widths.width_for(self.column))();
        let content = match self.column {
            TableColumn::Name => name_cell(self.item.clone(), self.expanded_ids).into_element(),
            TableColumn::Size => plain_cell(format_size(self.item.size_bytes)).into_element(),
            TableColumn::Status => status_cell(&self.item).into_element(),
            TableColumn::Speed => plain_cell(format_speed(self.item.speed_bps)).into_element(),
            TableColumn::TimeLeft => plain_cell(format_eta(self.item.time_left)).into_element(),
            TableColumn::DateAdded => plain_cell(self.item.date_added.clone()).into_element(),
        };

        rect()
            .height(Size::fill())
            .width(column_width(self.column, width))
            .min_width(Size::px(column_min_width(self.column, width)))
            .padding(Gaps::new(0., 10., 0., 10.))
            .main_align(Alignment::Center)
            .cross_align(Alignment::Center)
            .overflow(Overflow::Clip)
            .child(content)
    }
}

#[derive(Clone)]
struct NewDownloadWindow {
    services: AppServices,
}

impl PartialEq for NewDownloadWindow {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Component for NewDownloadWindow {
    fn render(&self) -> impl IntoElement {
        use_init_theme(theme::raijin_theme);

        let link = use_state(String::new);
        let has_link = !link.read().trim().is_empty();
        let services = self.services.clone();

        rect()
            .expanded()
            .background(theme::RAIJIN_BACKGROUND)
            .color(theme::TEXT_PRIMARY)
            .vertical()
            .child(
                rect()
                    .expanded()
                    .padding(Gaps::new(40., 20., 0., 20.))
                    .spacing(12.)
                    .vertical()
                    .child(
                        Input::new(link)
                            .placeholder("Download link")
                            .width(Size::fill())
                            .theme_colors(dialog_input_colors())
                            .theme_layout(InputLayoutThemePartial {
                                corner_radius: Some(CornerRadius::new_all(8.).into()),
                                inner_margin: Some(Gaps::new(10., 12., 10., 12.).into()),
                            })
                            .leading(
                                SvgViewer::new(icons::lucide::link_2())
                                    .width(Size::px(18.))
                                    .height(Size::px(18.))
                                    .color(theme::TEXT_SUBTLE),
                            )
                            .trailing(
                                SvgViewer::new(icons::lucide::clipboard())
                                    .width(Size::px(18.))
                                    .height(Size::px(18.))
                                    .color(theme::TEXT_MUTED),
                            ),
                    )
                    .child(
                        rect()
                            .horizontal()
                            .main_align(Alignment::SpaceBetween)
                            .cross_align(Alignment::Center)
                            .width(Size::fill())
                            .child(dropdown_button("Auto"))
                            .child(
                                rect()
                                    .horizontal()
                                    .spacing(10.)
                                    .child(dialog_button(
                                        "OK",
                                        has_link,
                                        add_download_action(
                                            services,
                                            link.read().trim().to_owned(),
                                        ),
                                    ))
                                    .child(dialog_button(
                                        "Cancel",
                                        true,
                                        close_current_window_action(),
                                    )),
                            ),
                    ),
            )
    }
}

pub(crate) fn new_download_window_config(services: AppServices) -> WindowConfig {
    WindowConfig::new(move || NewDownloadWindow {
        services: services.clone(),
    })
    .with_title("New Download")
    .with_size(430., 145.)
    .with_min_size(430., 145.)
    .with_resizable(true)
    .with_background(theme::RAIJIN_BACKGROUND)
}

#[derive(Clone, Copy)]
enum DownloadAction {
    Pause,
    Resume,
    Remove,
}

fn rows_from_state(state: &MonitorState) -> Vec<DownloadRowData> {
    state
        .active
        .values()
        .chain(state.completed.values())
        .map(row_from_view)
        .collect()
}

fn row_from_view(view: &DownloadView) -> DownloadRowData {
    DownloadRowData {
        id: view.id,
        file_name: display_name(view),
        kind: kind_from_name(&view.file_name).to_owned(),
        size_bytes: view.total_bytes,
        status: view.status,
        speed_bps: view.speed.get(),
        time_left: view.eta_seconds,
        date_added: format_date_added(view.created_at),
        progress: progress_ratio(view),
        connections: view.active_part_count.max(1),
    }
}

fn run_for_selected(services: AppServices, ids: Vec<DownloadId>, action: DownloadAction) {
    spawn(async move {
        for id in ids {
            let result = match action {
                DownloadAction::Pause => services.downloads.pause(id).await.map(|_| ()),
                DownloadAction::Resume => services.downloads.resume(id).await.map(|_| ()),
                DownloadAction::Remove => services.downloads.remove(id).await.map(|_| ()),
            };
            if let Err(error) = result {
                tracing::warn!(%id, ?error, "download command from UI failed");
            }
        }
    });
}

fn stop_all_active(services: AppServices, snapshot: MonitorState) {
    let ids = snapshot.active.keys().copied().collect::<Vec<_>>();
    run_for_selected(services, ids, DownloadAction::Pause);
}

fn add_download_action(services: AppServices, url: String) -> EventHandler<()> {
    (move |()| {
        let services = services.clone();
        let url = url.clone();
        spawn(async move {
            if url.trim().is_empty() {
                return;
            }
            let file_name = file_name_from_url(&url);
            let request = NewDownload::http(url, file_name, services.default_folder.clone());
            match services.downloads.add(request).await {
                Ok(item) => {
                    if let Err(error) = services.queues.enqueue(QueueId::MAIN, item.id).await {
                        tracing::warn!(%error, "enqueue from UI failed");
                    }
                    if let Err(error) = services.queues.start(QueueId::MAIN).await {
                        tracing::warn!(%error, "queue start from UI failed");
                    }
                    close_current_window();
                }
                Err(error) => tracing::warn!(?error, "add download from UI failed"),
            }
        });
    })
    .into()
}

fn open_new_download_window(services: AppServices) {
    spawn(async move {
        Platform::get()
            .launch_window(new_download_window_config(services))
            .await;
    });
}

fn close_current_window_action() -> EventHandler<()> {
    (|()| close_current_window()).into()
}

fn close_current_window() {
    let platform = Platform::get();
    Platform::get().with_window(None, move |window| {
        platform.close_window(window.id());
    });
}

fn column_width(column: TableColumn, width: f32) -> Size {
    if column.fills_remaining_width() {
        Size::fill()
    } else {
        Size::px(width)
    }
}

fn column_min_width(column: TableColumn, width: f32) -> f32 {
    if column.fills_remaining_width() {
        width
    } else {
        column.min_width()
    }
}

fn name_cell(item: DownloadRowData, mut expanded_ids: State<Vec<DownloadId>>) -> impl IntoElement {
    let expanded = expanded_ids.read().contains(&item.id);

    rect()
        .height(Size::fill())
        .width(Size::fill())
        .horizontal()
        .cross_align(Alignment::Center)
        .spacing(8.)
        .on_press(move |_| {
            let mut next = expanded_ids.read().clone();
            if let Some(index) = next.iter().position(|id| *id == item.id) {
                next.remove(index);
            } else {
                next.push(item.id);
            }
            expanded_ids.set_if_modified(next);
        })
        .child(icon_button(if expanded {
            icons::lucide::chevron_down()
        } else {
            icons::lucide::chevron_right()
        }))
        .child(row_kind_icon(&item.kind))
        .child(
            rect()
                .vertical()
                .spacing(2.)
                .width(Size::fill())
                .child(
                    label()
                        .text(item.file_name)
                        .font_size(14.)
                        .max_lines(1)
                        .color(theme::TEXT_PRIMARY),
                )
                .child(
                    label()
                        .text(item.kind)
                        .font_size(10.)
                        .max_lines(1)
                        .color(theme::TEXT_SUBTLE),
                ),
        )
}

fn plain_cell(text: impl Into<String>) -> impl IntoElement {
    label()
        .text(text.into())
        .font_size(14.)
        .max_lines(1)
        .color(theme::TEXT_PRIMARY)
}

fn status_cell(item: &DownloadRowData) -> impl IntoElement {
    rect()
        .width(Size::fill())
        .height(Size::fill())
        .center()
        .maybe(status_uses_progress(item.status), |el| {
            el.child(ProgressBar {
                progress: item.progress,
                color: match item.status {
                    DownloadStatus::Downloading
                    | DownloadStatus::Queued
                    | DownloadStatus::PreparingFile => theme::ACCENT,
                    DownloadStatus::Paused => Color::from_rgb(100, 105, 116),
                    _ => theme::ACCENT,
                },
            })
        })
        .maybe(!status_uses_progress(item.status), |el| {
            el.child(
                label()
                    .text(status_label(item.status))
                    .font_size(14.)
                    .color(status_color(item.status)),
            )
        })
}

fn selection_box(state: SelectBoxState, on_toggle: EventHandler<()>) -> impl IntoElement {
    rect()
        .width(Size::px(44.))
        .height(Size::fill())
        .center()
        .on_press(move |_| on_toggle.call(()))
        .child(
            rect()
                .width(Size::px(14.))
                .height(Size::px(14.))
                .corner_radius(4.)
                .border(Border::new().width(1.6).fill(match state {
                    SelectBoxState::Checked | SelectBoxState::Partial => theme::ACCENT,
                    SelectBoxState::Unchecked => Color::from_rgb(86, 89, 98),
                }))
                .background(match state {
                    SelectBoxState::Checked | SelectBoxState::Partial => theme::ACCENT,
                    SelectBoxState::Unchecked => Color::TRANSPARENT,
                })
                .center()
                .maybe(state == SelectBoxState::Checked, |el| {
                    el.child(
                        SvgViewer::new(icons::lucide::check())
                            .width(Size::px(11.))
                            .height(Size::px(11.))
                            .color(theme::RAIJIN_BACKGROUND)
                            .stroke_width(3.),
                    )
                })
                .maybe(state == SelectBoxState::Partial, |el| {
                    el.child(
                        SvgViewer::new(icons::lucide::minus())
                            .width(Size::px(11.))
                            .height(Size::px(11.))
                            .color(theme::RAIJIN_BACKGROUND)
                            .stroke_width(3.),
                    )
                }),
        )
}

fn toolbar_button(
    label_text: &'static str,
    icon: freya::prelude::Bytes,
    enabled: bool,
    prominent: bool,
    on_press: EventHandler<()>,
) -> impl IntoElement {
    let icon_color = if enabled {
        theme::TEXT_PRIMARY
    } else {
        Color::from_rgb(105, 108, 116)
    };
    let text_color = if enabled {
        theme::TEXT_PRIMARY
    } else {
        theme::TEXT_SUBTLE
    };

    rect()
        .height(Size::px(40.))
        .padding(if prominent {
            Gaps::new(0., 10., 0., 10.)
        } else {
            Gaps::new(0., 12., 0., 12.)
        })
        .horizontal()
        .cross_align(Alignment::Center)
        .spacing(8.)
        .corner_radius(8.)
        .background(if prominent {
            theme::SURFACE_ELEVATED
        } else {
            Color::TRANSPARENT
        })
        .border(if prominent {
            Border::new().width(1.).fill(theme::BORDER)
        } else {
            Border::default()
        })
        .maybe(enabled, |el| el.on_press(move |_| on_press.call(())))
        .child(
            SvgViewer::new(icon)
                .width(Size::px(18.))
                .height(Size::px(18.))
                .color(icon_color),
        )
        .child(
            label()
                .text(label_text)
                .font_size(12.)
                .color(text_color)
                .max_lines(1),
        )
        .maybe(prominent, |el| {
            el.child(
                rect()
                    .width(Size::px(26.))
                    .height(Size::px(26.))
                    .center()
                    .corner_radius(8.)
                    .background(theme::ACCENT)
                    .child(
                        SvgViewer::new(icons::lucide::cloud_download())
                            .width(Size::px(15.))
                            .height(Size::px(15.))
                            .color(theme::RAIJIN_BACKGROUND)
                            .stroke_width(2.6),
                    ),
            )
        })
}

fn noop_toolbar_action() -> EventHandler<()> {
    (|()| {}).into()
}

fn dialog_input_colors() -> InputColorsThemePartial {
    InputColorsThemePartial {
        background: Some(theme::SURFACE.into()),
        focus_background: Some(theme::SURFACE.into()),
        border_fill: Some(theme::BORDER.into()),
        focus_border_fill: Some(theme::BORDER.into()),
        color: Some(theme::TEXT_PRIMARY.into()),
        placeholder_color: Some(theme::TEXT_SUBTLE.into()),
    }
}

fn dialog_button(
    text: &'static str,
    enabled: bool,
    on_press: EventHandler<()>,
) -> impl IntoElement {
    rect()
        .width(Size::px(92.))
        .height(Size::px(36.))
        .center()
        .corner_radius(8.)
        .background(theme::SURFACE)
        .border(Border::new().width(1.).fill(theme::BORDER))
        .maybe(enabled, |el| el.on_press(move |_| on_press.call(())))
        .child(label().text(text).font_size(14.).color(if enabled {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_SUBTLE
        }))
}

fn dropdown_button(text: &'static str) -> impl IntoElement {
    rect()
        .width(Size::px(112.))
        .height(Size::px(36.))
        .horizontal()
        .center()
        .spacing(8.)
        .corner_radius(8.)
        .background(theme::SURFACE)
        .border(Border::new().width(1.).fill(theme::BORDER))
        .child(label().text(text).font_size(14.).color(theme::TEXT_PRIMARY))
        .child(
            SvgViewer::new(icons::lucide::chevron_down())
                .width(Size::px(14.))
                .height(Size::px(14.))
                .color(theme::TEXT_MUTED),
        )
}

fn toolbar_divider() -> impl IntoElement {
    rect()
        .width(Size::px(1.))
        .height(Size::px(32.))
        .background(theme::BORDER)
}

fn sort_icon(sort: SortState, column: TableColumn) -> impl IntoElement {
    SvgViewer::new(if sort.column == column {
        match sort.direction {
            SortDirection::Ascending => icons::lucide::chevron_up(),
            SortDirection::Descending => icons::lucide::chevron_down(),
        }
    } else {
        icons::lucide::chevrons_up_down()
    })
    .width(Size::px(13.))
    .height(Size::px(13.))
    .color(Color::from_rgb(104, 107, 116))
}

fn icon_button(icon: freya::prelude::Bytes) -> impl IntoElement {
    SvgViewer::new(icon)
        .width(Size::px(14.))
        .height(Size::px(14.))
        .color(theme::TEXT_MUTED)
}

fn row_kind_icon(kind: &str) -> impl IntoElement {
    let icon = match kind {
        "Compressed" => icons::lucide::file_archive(),
        "Programs" => icons::lucide::boxes(),
        "Videos" => icons::lucide::video(),
        "Music" => icons::lucide::music(),
        "Pictures" => icons::lucide::image(),
        _ => icons::lucide::file_text(),
    };

    SvgViewer::new(icon)
        .width(Size::px(18.))
        .height(Size::px(18.))
        .color(theme::TEXT_MUTED)
}

fn status_label(status: DownloadStatus) -> &'static str {
    match status {
        DownloadStatus::Added => "Added",
        DownloadStatus::Queued => "Queued",
        DownloadStatus::Downloading => "Downloading",
        DownloadStatus::Paused => "Paused",
        DownloadStatus::Retrying => "Retrying",
        DownloadStatus::PreparingFile => "Preparing",
        DownloadStatus::Completed => "Finished",
        DownloadStatus::Error => "Error",
        DownloadStatus::Removed => "Removed",
    }
}

const fn status_order(status: DownloadStatus) -> u8 {
    match status {
        DownloadStatus::Downloading => 0,
        DownloadStatus::Queued => 1,
        DownloadStatus::Retrying => 2,
        DownloadStatus::PreparingFile => 3,
        DownloadStatus::Paused => 4,
        DownloadStatus::Completed => 5,
        DownloadStatus::Error => 6,
        DownloadStatus::Added => 7,
        DownloadStatus::Removed => 8,
    }
}

fn status_uses_progress(status: DownloadStatus) -> bool {
    matches!(
        status,
        DownloadStatus::Downloading
            | DownloadStatus::Queued
            | DownloadStatus::Paused
            | DownloadStatus::Retrying
            | DownloadStatus::PreparingFile
    )
}

fn status_color(status: DownloadStatus) -> Color {
    match status {
        DownloadStatus::Completed => Color::from_rgb(71, 210, 128),
        DownloadStatus::Error => Color::from_rgb(235, 112, 112),
        DownloadStatus::Paused => Color::from_rgb(255, 190, 100),
        DownloadStatus::Downloading | DownloadStatus::Queued | DownloadStatus::PreparingFile => {
            theme::ACCENT
        }
        _ => theme::TEXT_PRIMARY,
    }
}

fn format_size(size: Option<Bytes>) -> String {
    size.map(|value| human_bytes(value.get()))
        .unwrap_or_else(|| "-".to_owned())
}

fn format_speed(bytes_per_second: u64) -> String {
    if bytes_per_second == 0 {
        "-".to_owned()
    } else {
        format!("{}/s", human_bytes(bytes_per_second))
    }
}

fn format_eta(eta_seconds: Option<u64>) -> String {
    let Some(seconds) = eta_seconds else {
        return "-".to_owned();
    };
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

fn format_date_added(created_at: i64) -> String {
    if created_at <= 0 {
        return "-".to_owned();
    }
    let Ok(duration) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return "-".to_owned();
    };
    let Ok(now_ms) = i64::try_from(duration.as_millis()) else {
        return "-".to_owned();
    };
    let days = now_ms.saturating_sub(created_at) / 86_400_000;
    if days <= 0 {
        "today".to_owned()
    } else if days == 1 {
        "1 day ago".to_owned()
    } else if days < 30 {
        format!("{days} days ago")
    } else {
        format!("{} months ago", days / 30)
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

fn byte_sort_value(size: Option<Bytes>) -> u64 {
    size.map(Bytes::get).unwrap_or(0)
}

fn display_name(view: &DownloadView) -> String {
    if view.file_name.is_empty() {
        format!("Download {}", view.id.get())
    } else {
        view.file_name.clone()
    }
}

fn progress_ratio(view: &DownloadView) -> f32 {
    let Some(total) = view.total_bytes else {
        return 0.;
    };
    if total == Bytes::ZERO {
        return 0.;
    }
    view.downloaded_bytes.get() as f32 / total.get() as f32
}

fn kind_from_name(file_name: &str) -> &'static str {
    let extension = file_name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "zip" | "7z" | "rar" | "tar" | "gz" => "Compressed",
        "exe" | "msi" | "appimage" | "dmg" | "deb" | "rpm" => "Programs",
        "mp4" | "mkv" | "webm" | "avi" => "Videos",
        "mp3" | "flac" | "wav" | "ogg" => "Music",
        "png" | "jpg" | "jpeg" | "gif" | "webp" => "Pictures",
        _ => "Documents",
    }
}

fn file_name_from_url(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    without_query
        .rsplit('/')
        .find(|part| !part.trim().is_empty())
        .map(sanitize_file_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "download.bin".to_owned())
}

fn sanitize_file_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect()
}

fn left_border() -> Border {
    Border::new()
        .width(BorderWidth {
            left: 1.,
            ..Default::default()
        })
        .fill(theme::BORDER)
}
