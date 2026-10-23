use std::collections::BTreeSet;
use std::path::PathBuf;

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::*;
use raijin_core::download::{DownloadId, DownloadStatus, FailureKind};
use raijin_core::monitor::DownloadView;

use crate::bridge::{EngineCommand, EngineHandles};
use crate::category::{CategoryFilter, FileCategory, StatusFilter, infer_category};
use crate::format::{
    format_bytes, format_eta, format_relative_time, format_speed, progress_percent,
};

/// Payload carried while dragging table rows onto a queue.
///
/// The drag carries ids, never engine state: the drop handler sends one queue
/// command, so the UI cannot mutate admission bookkeeping directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueDrag {
    /// Downloads being dragged, in selection order.
    pub downloads: Vec<DownloadId>,
}

/// Floating label shown while dragging rows toward a queue.
pub struct QueueDragPreview {
    label: String,
}

impl QueueDragPreview {
    /// Creates a preview carrying the drag summary text.
    #[must_use]
    pub fn new(label: String) -> Self {
        Self { label }
    }
}

impl Render for QueueDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded(px(8.))
            .bg(cx.theme().accent)
            .text_color(cx.theme().accent_foreground)
            .text_sm()
            .child(self.label.clone())
    }
}

/// Poll interval for monitor snapshots on the GPUI thread.
pub const MONITOR_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Fixed table body slot height in pixels.
///
/// In the hybrid card-table design, each slot is 52px high, accommodating a
/// 48px floating card row plus a 4px vertical gap (`my(px(2.))`). This preserves
/// the two-line Name and Status layouts while rendering distinct island cards.
pub const TABLE_ROW_HEIGHT_PX: f32 = 52.0;

/// Table column keys. Also used as stable [`Column`] keys.
pub mod col {
    /// File name + category cell.
    pub const NAME: &str = "name";
    /// Total (or downloaded) size cell.
    pub const SIZE: &str = "size";
    /// Status text + progress bar cell.
    pub const STATUS: &str = "status";
    /// Current throughput cell.
    pub const SPEED: &str = "speed";
    /// Estimated time left cell.
    pub const ETA: &str = "eta";
    /// Creation date cell.
    pub const ADDED: &str = "added";
}

/// One rendered table row derived from a [`DownloadView`].
#[derive(Debug, Clone)]
pub struct DownloadRow {
    /// Stable download id used for element identity and commands.
    pub id: DownloadId,
    /// File name shown in the name cell.
    pub file_name: String,
    /// Lowercase sort key for case-insensitive name sorting.
    pub name_key: String,
    /// Inferred file category for filtering and the name-cell subtitle.
    pub category: FileCategory,
    /// Destination folder for open/reveal actions.
    pub folder: PathBuf,
    /// Durable engine status.
    pub status: DownloadStatus,
    /// Status line shown in the status cell.
    pub status_label: String,
    /// True when the row represents an actively transferring download.
    pub is_active: bool,
    /// Progress 0–100 for the status-cell bar, when computable.
    pub progress: Option<f32>,
    /// Downloaded bytes (numeric sort key fallback).
    pub downloaded_bytes: u64,
    /// Total bytes when known (primary size sort key).
    pub total_bytes: Option<u64>,
    /// Size cell text.
    pub size_label: String,
    /// Throughput in bytes per second (numeric sort key).
    pub speed_bps: u64,
    /// Speed cell text (empty when idle).
    pub speed_label: String,
    /// ETA in seconds (numeric sort key, unknown sorts last).
    pub eta_seconds: Option<u64>,
    /// ETA cell text (empty when unknown).
    pub eta_label: String,
    /// Queue holding this download, when it is a queue member.
    pub queue_name: Option<String>,
    /// Creation timestamp in Unix milliseconds.
    pub created_at: i64,
    /// Date-added cell text.
    pub added_label: String,
}

impl DownloadRow {
    /// Derives a presentation row from a monitor view.
    ///
    /// Removed downloads have no row representation; use [`build_rows`] which
    /// filters them out.
    #[must_use]
    pub fn from_view(view: &DownloadView, now_ms: i64) -> Self {
        let total = view.total_bytes.map(|bytes| bytes.get());
        let downloaded = view.downloaded_bytes.get();
        let progress = progress_percent(downloaded, total);
        let is_active = matches!(view.status, DownloadStatus::Downloading);
        let show_progress = matches!(
            view.status,
            DownloadStatus::Downloading | DownloadStatus::Paused
        );
        let size_label = match total {
            Some(total) => format_bytes(total),
            None => format_bytes(downloaded),
        };
        Self {
            id: view.id,
            name_key: view.file_name.to_lowercase(),
            file_name: view.file_name.clone(),
            category: infer_category(&view.file_name),
            folder: view.folder.clone(),
            status: view.status,
            status_label: status_label(
                view.status,
                progress,
                view.failure.as_ref().map(|failure| failure.kind),
            ),
            is_active,
            progress: if show_progress { progress } else { None },
            downloaded_bytes: downloaded,
            total_bytes: total,
            size_label,
            speed_bps: view.speed.get(),
            speed_label: format_speed(view.speed.get()),
            eta_seconds: view.eta_seconds,
            eta_label: format_eta(view.eta_seconds),
            // Filled by the view, which owns the queue snapshot.
            queue_name: None,
            created_at: view.created_at,
            added_label: format_relative_time(now_ms, view.created_at),
        }
    }

    /// Absolute path of the final file (used by open/reveal actions).
    #[must_use]
    pub fn file_path(&self) -> PathBuf {
        self.folder.join(&self.file_name)
    }
}

fn status_label(
    status: DownloadStatus,
    progress: Option<f32>,
    failure: Option<FailureKind>,
) -> String {
    match status {
        DownloadStatus::Completed => "Finished".to_owned(),
        DownloadStatus::Downloading => match progress {
            Some(percent) => format!("{}% Downloading", percent.round() as u32),
            None => "Downloading".to_owned(),
        },
        DownloadStatus::Paused => match progress {
            Some(percent) => format!("{}% Paused", percent.round() as u32),
            None => "Paused".to_owned(),
        },
        DownloadStatus::Queued => "Queued".to_owned(),
        DownloadStatus::Added => "Waiting".to_owned(),
        DownloadStatus::Retrying => "Retrying".to_owned(),
        DownloadStatus::PreparingFile => "Preparing".to_owned(),
        DownloadStatus::Error => match failure {
            Some(kind) => format!("Error ({})", failure_kind_label(kind)),
            None => "Error".to_owned(),
        },
        DownloadStatus::Removed => "Removed".to_owned(),
    }
}

fn failure_kind_label(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Network => "network",
        FailureKind::Server => "server",
        FailureKind::Validation => "changed file",
        FailureKind::Disk => "disk",
        FailureKind::Canceled => "canceled",
        FailureKind::NoSpace => "no space",
        FailureKind::TooManyRetries => "retries",
    }
}

/// True when Resume applies to `status` (paused or failed).
#[must_use]
pub const fn can_resume(status: DownloadStatus) -> bool {
    matches!(status, DownloadStatus::Paused | DownloadStatus::Error)
}

/// True when Pause (and Pause-all) applies to `status`.
#[must_use]
pub const fn can_pause(status: DownloadStatus) -> bool {
    matches!(
        status,
        DownloadStatus::Added
            | DownloadStatus::Queued
            | DownloadStatus::Downloading
            | DownloadStatus::Retrying
            | DownloadStatus::PreparingFile
    )
}

/// Toolbar enablement derived from selection and full list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolbarActions {
    /// At least one target can resume.
    pub can_resume: bool,
    /// At least one target can pause.
    pub can_pause: bool,
    /// At least one target selected (remove always applies).
    pub can_remove: bool,
    /// At least one download anywhere can pause.
    pub can_pause_all: bool,
}

/// Computes toolbar enablement from target and full-list statuses.
#[must_use]
pub fn toolbar_actions(targets: &[DownloadStatus], all: &[DownloadStatus]) -> ToolbarActions {
    ToolbarActions {
        can_resume: targets.iter().any(|status| can_resume(*status)),
        can_pause: targets.iter().any(|status| can_pause(*status)),
        can_remove: !targets.is_empty(),
        can_pause_all: all.iter().any(|status| can_pause(*status)),
    }
}

/// Builds presentation rows from monitor views, skipping removed downloads.
#[must_use]
pub fn build_rows<'a>(
    views: impl IntoIterator<Item = &'a DownloadView>,
    now_ms: i64,
) -> Vec<DownloadRow> {
    views
        .into_iter()
        .filter(|view| !matches!(view.status, DownloadStatus::Removed))
        .map(|view| DownloadRow::from_view(view, now_ms))
        .collect()
}

/// Sortable table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Size,
    Status,
    Speed,
    Eta,
    DateAdded,
}

impl SortColumn {
    /// Maps a table column key to a sortable column. Unknown keys fall back
    /// to name sorting so a reorder can never break header clicks.
    #[must_use]
    pub fn from_key(key: &str) -> Self {
        match key {
            col::SIZE => Self::Size,
            col::STATUS => Self::Status,
            col::SPEED => Self::Speed,
            col::ETA => Self::Eta,
            col::ADDED => Self::DateAdded,
            _ => Self::Name,
        }
    }

    fn rank(status: DownloadStatus) -> u8 {
        match status {
            DownloadStatus::Downloading => 0,
            DownloadStatus::Retrying => 1,
            DownloadStatus::Queued => 2,
            DownloadStatus::Added => 3,
            DownloadStatus::PreparingFile => 4,
            DownloadStatus::Paused => 5,
            DownloadStatus::Error => 6,
            DownloadStatus::Completed => 7,
            DownloadStatus::Removed => 8,
        }
    }
}

/// Sort direction for a [`SortColumn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDir {
    #[default]
    Ascending,
    Descending,
}

impl SortDir {
    #[must_use]
    pub const fn from_column_sort(sort: ColumnSort) -> Self {
        match sort {
            ColumnSort::Descending => Self::Descending,
            ColumnSort::Ascending | ColumnSort::Default => Self::Ascending,
        }
    }
}

/// Filters rows by category, status, and a case-insensitive name query.
#[must_use]
pub fn filter_rows(
    rows: &[DownloadRow],
    category: CategoryFilter,
    status: StatusFilter,
    query: &str,
) -> Vec<DownloadRow> {
    let query = query.trim().to_lowercase();
    rows.iter()
        .filter(|row| category.matches(row.category))
        .filter(|row| status.matches(row.status))
        .filter(|row| query.is_empty() || row.name_key.contains(&query))
        .cloned()
        .collect()
}

/// Sorts rows in place. Unknown ETAs always sort last, in both directions.
pub fn sort_rows(rows: &mut [DownloadRow], column: SortColumn, dir: SortDir) {
    let descending = matches!(dir, SortDir::Descending);
    rows.sort_by(|a, b| {
        match column {
            SortColumn::Name => directed(a.name_key.cmp(&b.name_key), descending),
            SortColumn::Size => directed(size_key(a).cmp(&size_key(b)), descending),
            SortColumn::Status => directed(
                SortColumn::rank(a.status).cmp(&SortColumn::rank(b.status)),
                descending,
            ),
            SortColumn::Speed => directed(a.speed_bps.cmp(&b.speed_bps), descending),
            SortColumn::Eta => eta_cmp(a.eta_seconds, b.eta_seconds, descending),
            SortColumn::DateAdded => directed(a.created_at.cmp(&b.created_at), descending),
        }
        // Keep sort stable and deterministic for equal keys.
        .then_with(|| a.id.get().cmp(&b.id.get()))
    });
}

fn directed(ordering: std::cmp::Ordering, descending: bool) -> std::cmp::Ordering {
    if descending {
        ordering.reverse()
    } else {
        ordering
    }
}

/// Subtitle for the name cell: the owning queue when the row is a member.
///
/// A queued row that belongs to an inactive queue looks stuck otherwise, so the
/// queue name takes precedence over the file category.
fn queue_subtitle(row: &DownloadRow) -> String {
    match &row.queue_name {
        Some(queue) => queue.clone(),
        None => row.category.label().to_owned(),
    }
}

/// Computes the next sort state for a header click, mirroring the table's own
/// `Default → Descending → Ascending → default` cycle.
///
/// A first click on a new column sorts descending (newest/largest first,
/// matching the product default); clicking the sorted column toggles
/// direction, then resets to the default newest-first order.
fn next_sort(current: (SortColumn, SortDir), clicked: SortColumn) -> (SortColumn, SortDir) {
    let (column, dir) = current;
    if column != clicked {
        (clicked, SortDir::Descending)
    } else if matches!(dir, SortDir::Descending) {
        (column, SortDir::Ascending)
    } else {
        (SortColumn::DateAdded, SortDir::Descending)
    }
}
/// Compares ETAs with unknown values pinned last regardless of direction.
fn eta_cmp(a: Option<u64>, b: Option<u64>, descending: bool) -> std::cmp::Ordering {
    match (a, b) {
        (Some(left), Some(right)) => directed(left.cmp(&right), descending),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn size_key(row: &DownloadRow) -> u64 {
    row.total_bytes.unwrap_or(row.downloaded_bytes)
}

/// Maps a file category to a sidebar/table icon.
///
/// The bundled icon set has no archive/video/music/picture glyphs, so close
/// visual neighbours stand in (documented placeholders, not semantic claims).
#[must_use]
pub fn category_icon(category: FileCategory) -> IconName {
    match category {
        FileCategory::Compressed => IconName::Inbox,
        FileCategory::Programs => IconName::LayoutDashboard,
        FileCategory::Videos => IconName::Play,
        FileCategory::Music => IconName::Star,
        FileCategory::Pictures => IconName::GalleryVerticalEnd,
        FileCategory::Documents => IconName::FileText,
        FileCategory::Other => IconName::File,
    }
}
/// Checkbox multi-select set keyed by stable [`DownloadId`].
///
/// Pure state (no GPUI dependency) so it stays unit-testable: checks survive
/// re-sorting and monitor polls because ids are stable, while [`set_rows`]
/// prunes ids that left the visible set.
#[derive(Debug, Clone, Default)]
struct RowChecklist {
    checked: BTreeSet<DownloadId>,
}

impl RowChecklist {
    /// Returns true when `id` is checked.
    #[must_use]
    fn is_checked(&self, id: DownloadId) -> bool {
        self.checked.contains(&id)
    }

    /// Toggles the checked state of one id.
    fn toggle(&mut self, id: DownloadId) {
        if !self.checked.remove(&id) {
            self.checked.insert(id);
        }
    }

    /// Checks or unchecks every id in `visible`.
    fn set_all(&mut self, visible: impl IntoIterator<Item = DownloadId>, checked: bool) {
        if checked {
            self.checked.extend(visible);
        } else {
            for id in visible {
                self.checked.remove(&id);
            }
        }
    }

    /// Drops the given ids from the set.
    fn uncheck_ids(&mut self, ids: &[DownloadId]) {
        for id in ids {
            self.checked.remove(id);
        }
    }

    /// Drops every checked id that is not in `visible`.
    fn retain_visible(&mut self, visible: impl IntoIterator<Item = DownloadId>) {
        let visible: BTreeSet<DownloadId> = visible.into_iter().collect();
        self.checked.retain(|id| visible.contains(id));
    }

    /// Returns the checked ids in stable (ascending) order.
    #[must_use]
    fn ids(&self) -> Vec<DownloadId> {
        self.checked.iter().copied().collect()
    }

    /// Returns the number of checked ids (test helper).
    #[cfg(test)]
    #[must_use]
    fn len(&self) -> usize {
        self.checked.len()
    }
}

/// Wraps a framework `Checkbox` in a visible bordered box.
///
/// The theme `input` token (unchecked box bg + border) matches the table row
/// background, so a bare unchecked box is invisible on the rows. The wrapper
/// paints the ABDM-style outline while the inner control keeps its focus,
/// keyboard, and accessibility behavior.
fn checkbox_frame(checkbox: Checkbox, cx: &App) -> impl IntoElement {
    div()
        .size(px(18.))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .border_1()
        .border_color(cx.theme().border)
        .rounded(px(4.))
        .child(checkbox)
}

/// Column definition kept by the delegate (order = display order).
#[derive(Debug, Clone)]
struct ColumnDef {
    key: &'static str,
    name: &'static str,
    width: f32,
    /// Drag floor so a column stays readable (header label + cell content).
    min_width: f32,
}

impl ColumnDef {
    const fn new(key: &'static str, name: &'static str, width: f32, min_width: f32) -> Self {
        Self {
            key,
            name,
            width,
            min_width,
        }
    }

    /// Floors a dragged width at this column's readable minimum.
    fn clamp_width(&self, raw: f32) -> f32 {
        raw.max(self.min_width)
    }
}

/// `DataTable` delegate rendering [`DownloadRow`]s.
///
/// The delegate owns exactly the rows currently visible (already filtered and
/// sorted by the owning view) so `render_td` stays allocation-light. It also
/// owns the checkbox multi-select set ([`RowChecklist`]), keyed by stable
/// [`DownloadId`] so checks survive re-sorting, filtering, and monitor polls.
pub struct DownloadsDelegate {
    rows: Vec<DownloadRow>,
    columns: Vec<ColumnDef>,
    sort_column: SortColumn,
    sort_dir: SortDir,
    loaded: bool,
    checklist: RowChecklist,
    commands: EngineHandles,
    selected_id: Option<DownloadId>,
}

impl DownloadsDelegate {
    /// Creates a delegate with the default column layout from the sketch.
    #[must_use]
    pub fn new(commands: EngineHandles) -> Self {
        Self {
            rows: Vec::new(),
            columns: vec![
                ColumnDef::new(col::NAME, "Name", 390.0, 180.0),
                ColumnDef::new(col::SIZE, "Size", 110.0, 80.0),
                ColumnDef::new(col::STATUS, "Status", 210.0, 140.0),
                ColumnDef::new(col::SPEED, "Speed", 130.0, 80.0),
                ColumnDef::new(col::ETA, "Time Left", 130.0, 90.0),
                ColumnDef::new(col::ADDED, "Date Added", 150.0, 110.0),
            ],
            sort_column: SortColumn::DateAdded,
            sort_dir: SortDir::Descending,
            loaded: false,
            checklist: RowChecklist::default(),
            commands,
            selected_id: None,
        }
    }

    /// Replaces visible rows and re-applies the stored sort order.
    ///
    /// Checks for downloads that are no longer visible are dropped, so a
    /// removed or filtered-out row can never stay checked.
    pub fn set_rows(&mut self, mut rows: Vec<DownloadRow>) {
        sort_rows(&mut rows, self.sort_column, self.sort_dir);
        self.checklist.retain_visible(rows.iter().map(|row| row.id));
        if let Some(selected) = self.selected_id
            && !rows.iter().any(|row| row.id == selected)
        {
            self.selected_id = None;
        }
        self.rows = rows;
    }

    /// Marks the initial monitor snapshot as received (hides the skeleton).
    pub fn set_loaded(&mut self, loaded: bool) {
        self.loaded = loaded;
    }

    /// Applies resized column widths (in display order) so a later table
    /// refresh keeps the user's layout instead of resetting to defaults.
    pub fn set_column_widths(&mut self, widths: &[Pixels]) {
        for (def, width) in self.columns.iter_mut().zip(widths.iter()) {
            def.width = def.clamp_width(f32::from(*width));
        }
    }

    /// Returns the row at a visible index, if any.
    #[must_use]
    pub fn row(&self, row_ix: usize) -> Option<&DownloadRow> {
        self.rows.get(row_ix)
    }

    /// Returns the visible row for a download id, if currently shown.
    #[must_use]
    pub fn row_by_id(&self, id: DownloadId) -> Option<&DownloadRow> {
        self.rows.iter().find(|row| row.id == id)
    }

    /// Returns the visible index of a download id, if currently shown.
    ///
    /// Used to preserve selection by domain identity across filtering and
    /// reordering instead of pinning a positional index.
    #[must_use]
    pub fn row_position(&self, id: DownloadId) -> Option<usize> {
        self.rows.iter().position(|row| row.id == id)
    }

    /// Applies a header-click sort and re-sorts the visible rows.
    fn apply_sort(&mut self, column: SortColumn, dir: SortDir) {
        self.sort_column = column;
        self.sort_dir = dir;
        sort_rows(&mut self.rows, column, dir);
    }

    /// Advances the sort state for a header click, mirroring the table's own
    /// `Default → Descending → Ascending → default` cycle.
    ///
    /// Called from the header content click handler; the table's built-in
    /// sort glyph is disabled (see `MainView`) so the sketch's left-side icon
    /// is the single sort affordance.
    pub fn cycle_sort(&mut self, col_ix: usize) {
        let column = SortColumn::from_key(self.column_key(col_ix));
        let (column, dir) = next_sort((self.sort_column, self.sort_dir), column);
        self.apply_sort(column, dir);
    }

    /// Returns the stable key of the column at a visible index.
    fn column_key(&self, col_ix: usize) -> &'static str {
        self.columns
            .get(col_ix)
            .map(|def| def.key)
            .unwrap_or(col::NAME)
    }

    /// Returns true when `id` is checkbox-checked.
    #[must_use]
    pub fn is_checked(&self, id: DownloadId) -> bool {
        self.checklist.is_checked(id)
    }

    /// Toggles the checkbox for one download id.
    pub fn toggle_checked(&mut self, id: DownloadId) {
        self.checklist.toggle(id);
    }

    /// Checks or unchecks every currently visible row (header select-all).
    pub fn set_all_checked(&mut self, checked: bool) {
        self.checklist
            .set_all(self.rows.iter().map(|row| row.id), checked);
    }

    /// Unchecks the given ids (used right after dispatching Remove).
    pub fn uncheck_ids(&mut self, ids: &[DownloadId]) {
        self.checklist.uncheck_ids(ids);
    }

    /// Returns true when at least one visible row exists and all are checked.
    #[must_use]
    pub fn all_visible_checked(&self) -> bool {
        !self.rows.is_empty()
            && self
                .rows
                .iter()
                .all(|row| self.checklist.is_checked(row.id))
    }

    /// Returns the checked ids in stable (ascending) order.
    #[must_use]
    pub fn checked_ids(&self) -> Vec<DownloadId> {
        self.checklist.ids()
    }

    /// Returns the currently selected row id, if any.
    #[must_use]
    pub fn selected_id(&self) -> Option<DownloadId> {
        self.selected_id
    }

    /// Sets the selected download id.
    pub fn select_row(&mut self, id: DownloadId) {
        self.selected_id = Some(id);
    }

    /// Clears the single-row selection.
    pub fn clear_selection(&mut self) {
        self.selected_id = None;
    }

    fn cell_icon(category: FileCategory) -> IconName {
        category_icon(category)
    }
}

impl TableDelegate for DownloadsDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let def = &self.columns[col_ix];
        Column::new(def.key, def.name)
            .width(px(def.width))
            .min_width(px(def.min_width))
            .sortable()
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let key = self
            .columns
            .get(col_ix)
            .map(|def| def.key)
            .unwrap_or(col::NAME);
        // A `Default` sort resets to the product default (newest first).
        if matches!(sort, ColumnSort::Default) {
            self.apply_sort(SortColumn::DateAdded, SortDir::Descending);
        } else {
            self.apply_sort(SortColumn::from_key(key), SortDir::from_column_sort(sort));
        }
        cx.notify();
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        div()
            .id("downloads-table-header")
            .mb(px(3.))
            .rounded(px(8.))
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let key = self.column_key(col_ix);
        let name = self.columns[col_ix].name;
        // Sketch header: sort glyph left of the label in `$text-muted`, label
        // in `$text-secondary`. The table's built-in right-side glyph is
        // disabled, so this is the single sort affordance; the whole header
        // content is clickable and cycles the sort (see `cycle_sort`).
        // NOTE: per-cell card backgrounds were tried and reverted — the row
        // container owns the header paint; rounding belongs to the wrapper
        // in `MainView` (see its comment for the probe evidence).
        let sorted = SortColumn::from_key(key) == self.sort_column;
        let sort_icon = match (sorted, self.sort_dir) {
            (true, SortDir::Ascending) => Icon::new(IconName::ChevronUp)
                .xsmall()
                .text_color(cx.theme().accent),
            (true, SortDir::Descending) => Icon::new(IconName::ChevronDown)
                .xsmall()
                .text_color(cx.theme().accent),
            (false, _) => Icon::new(IconName::ChevronsUpDown)
                .xsmall()
                .text_color(cx.theme().muted_foreground),
        };
        let table = cx.entity();
        let sort_label = div()
            .id(SharedString::from(format!("sort-header-{key}")))
            .flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .gap_1()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .text_color(cx.theme().secondary)
            .on_click(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                table.update(cx, |table, cx| {
                    // Keep the current row anchored by id: sorting reorders
                    // positions, so re-select the same download afterwards.
                    let selected_id = table.delegate().selected_id();
                    table.delegate_mut().cycle_sort(col_ix);
                    if let Some(id) = selected_id
                        && let Some(new_ix) = table.delegate().row_position(id)
                    {
                        cx.emit(TableEvent::SelectRow(new_ix));
                    }
                    cx.notify();
                });
                cx.stop_propagation();
            })
            .child(sort_icon)
            .child(name)
            .into_any_element();
        // ABDM-style header select-all box. It lives outside the sort-click
        // area so checking all rows never re-sorts the table.
        if key == col::NAME {
            let header_table = cx.entity();
            let all_checked = self.all_visible_checked();
            let has_rows = !self.rows.is_empty();
            h_flex()
                .h_full()
                .items_center()
                .gap_2()
                .pl_3()
                .child(checkbox_frame(
                    Checkbox::new("header-check-name")
                        .checked(all_checked)
                        .disabled(!has_rows)
                        .on_click(move |_, _: &mut Window, cx: &mut App| {
                            header_table.update(cx, |table, cx| {
                                let check = !table.delegate().all_visible_checked();
                                table.delegate_mut().set_all_checked(check);
                                table.refresh(cx);
                                // Sync the owning view's toolbar state: the
                                // framework only emits click/keyboard events.
                                if check {
                                    if table.delegate().rows_count(cx) > 0 {
                                        if let Some(first_id) =
                                            table.delegate().row(0).map(|r| r.id)
                                        {
                                            table.delegate_mut().select_row(first_id);
                                        }
                                        cx.emit(TableEvent::SelectRow(0));
                                    }
                                } else {
                                    table.delegate_mut().clear_selection();
                                    cx.emit(TableEvent::ClearSelection);
                                }
                            });
                            cx.stop_propagation();
                        }),
                    cx,
                ))
                .child(sort_label)
                .into_any_element()
        } else if key == col::ADDED {
            h_flex()
                .h_full()
                .items_center()
                .pr_3()
                .child(sort_label)
                .into_any_element()
        } else {
            sort_label
        }
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        // Stable domain identity: reordering or filtering must not move row state.
        let row = self.rows.get(row_ix);
        let id_num = row.map(|r| r.id.get()).unwrap_or(-1);
        let id = row.map(|r| r.id);
        let is_selected = id.is_some_and(|id| self.selected_id == Some(id));
        let is_checked = id.is_some_and(|id| self.is_checked(id));

        let bg = if is_selected {
            crate::theme::row_card_selected_bg()
        } else {
            crate::theme::row_card_bg()
        };

        let border_color = if is_selected {
            cx.theme().accent
        } else if is_checked {
            cx.theme().accent.opacity(0.8)
        } else {
            cx.theme().border.opacity(0.6)
        };

        let table = cx.entity();

        let mut row_el = div()
            .id(SharedString::from(format!("download-row-{id_num}")))
            .my(px(2.))
            .h(px(48.))
            .rounded(px(8.))
            .border_1()
            .border_color(border_color)
            .bg(bg)
            .overflow_hidden();

        if let Some(id) = id {
            let drag_ids = {
                let checked = self.checked_ids();
                if checked.contains(&id) {
                    checked
                } else {
                    vec![id]
                }
            };
            row_el = row_el.on_drag(
                QueueDrag {
                    downloads: drag_ids,
                },
                move |drag: &QueueDrag, _offset, _window, cx| {
                    let label = match drag.downloads.len() {
                        0 => "Move".to_owned(),
                        1 => "1 download".to_owned(),
                        count => format!("{count} downloads"),
                    };
                    // The preview is a fresh entity per drag; keeping it
                    // state-free avoids leaking a view into the table.
                    cx.new(|_| QueueDragPreview::new(label.clone()))
                },
            );
            row_el = row_el.on_click(move |event: &ClickEvent, _: &mut Window, cx: &mut App| {
                table.update(cx, |table, cx| {
                    table.delegate_mut().select_row(id);
                    if event.click_count() == 2 {
                        cx.emit(TableEvent::DoubleClickedRow(row_ix));
                    } else {
                        cx.emit(TableEvent::SelectRow(row_ix));
                    }
                    cx.notify();
                });
                cx.stop_propagation();
            });
        }

        row_el
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.rows.get(row_ix) else {
            return div().into_any_element();
        };
        let key = self
            .columns
            .get(col_ix)
            .map(|def| def.key)
            .unwrap_or(col::NAME);
        match key {
            col::NAME => {
                // ABDM-style multi-select box. Clicking it also makes the row
                // current (emitted below) so the toolbar stays in step; the
                // framework itself only moves the single-row highlight.
                let id = row.id;
                let checked = self.is_checked(id);
                let table = cx.entity();
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .h_full()
                    .items_center()
                    .pl_3()
                    .child(checkbox_frame(
                        Checkbox::new(SharedString::from(format!("row-check-{}", id.get())))
                            .checked(checked)
                            .on_click(move |_, _: &mut Window, cx: &mut App| {
                                table.update(cx, |table, cx| {
                                    table.delegate_mut().toggle_checked(id);
                                    table.delegate_mut().select_row(id);
                                    table.refresh(cx);
                                    cx.emit(TableEvent::SelectRow(row_ix));
                                });
                                cx.stop_propagation();
                            }),
                        cx,
                    ))
                    .child(
                        Icon::new(Self::cell_icon(row.category))
                            .small()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().foreground)
                                    .child(row.file_name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(queue_subtitle(row)),
                            ),
                    )
                    .into_any_element()
            }
            col::SIZE => h_flex()
                .h_full()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(row.size_label.clone()),
                )
                .into_any_element(),
            col::STATUS => {
                let color = match row.status {
                    DownloadStatus::Completed => cx.theme().success,
                    DownloadStatus::Error | DownloadStatus::Paused => cx.theme().danger,
                    _ => cx.theme().foreground,
                };
                let mut cell = v_flex().h_full().justify_center().gap_1().child(
                    div()
                        .text_sm()
                        .text_color(color)
                        .child(row.status_label.clone()),
                );
                if let Some(progress) = row.progress {
                    let mut bar = gpui_kit::component::progress::Progress::new(SharedString::from(
                        format!("download-progress-{}", row.id.get()),
                    ))
                    .value(progress)
                    .small();
                    if row.status == DownloadStatus::Paused {
                        bar = bar.color(cx.theme().danger);
                    }
                    cell = cell.child(bar);
                }
                cell.into_any_element()
            }
            col::SPEED => h_flex()
                .h_full()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(row.speed_label.clone()),
                )
                .into_any_element(),
            col::ETA => h_flex()
                .h_full()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(row.eta_label.clone()),
                )
                .into_any_element(),
            _ => h_flex()
                .h_full()
                .items_center()
                .pr_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(row.added_label.clone()),
                )
                .into_any_element(),
        }
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(Icon::new(IconName::Inbox).large())
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child("No downloads here"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Use New Download to add your first link."),
            )
            .into_any_element()
    }

    fn loading(&self, _cx: &App) -> bool {
        !self.loaded
    }

    fn move_column(
        &mut self,
        col_ix: usize,
        to_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        if col_ix == to_ix || col_ix >= self.columns.len() || to_ix >= self.columns.len() {
            return;
        }
        let def = self.columns.remove(col_ix);
        self.columns.insert(to_ix, def);
        cx.notify();
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(row) = self.rows.get(row_ix) else {
            return menu;
        };
        let id = row.id;
        let status = row.status;
        let file_path = row.file_path();
        let commands = self.commands.clone();
        let dispatch = move |command: EngineCommand| {
            let commands = commands.clone();
            move |_: &ClickEvent, _: &mut Window, _: &mut App| {
                commands.dispatch(command.clone());
            }
        };

        let mut menu = menu;
        if can_resume(status) {
            menu = menu.item(
                PopupMenuItem::new("Resume")
                    .icon(Icon::new(IconName::Play))
                    .on_click(dispatch(EngineCommand::Resume(id))),
            );
        }
        if can_pause(status) {
            menu = menu.item(
                PopupMenuItem::new("Pause")
                    .icon(Icon::new(IconName::Pause))
                    .on_click(dispatch(EngineCommand::Pause(id))),
            );
        }
        if matches!(status, DownloadStatus::Error) {
            menu = menu.item(
                PopupMenuItem::new("Reset and retry")
                    .icon(Icon::new(IconName::RotateCw))
                    .on_click(dispatch(EngineCommand::Reset(id))),
            );
        }
        if matches!(status, DownloadStatus::Completed) {
            let path = file_path.clone();
            menu = menu.item(
                PopupMenuItem::new("Open file")
                    .icon(Icon::new(IconName::File))
                    .on_click(move |_: &ClickEvent, _: &mut Window, _: &mut App| {
                        if let Err(error) = raijin_core::platform::open_file(&path) {
                            tracing::warn!(?error, "open file failed");
                        }
                    }),
            );
        }
        let folder_path = file_path;
        menu = menu.item(
            PopupMenuItem::new("Open folder")
                .icon(Icon::new(IconName::Folder))
                .on_click(move |_: &ClickEvent, _: &mut Window, _: &mut App| {
                    if let Err(error) = raijin_core::platform::reveal_in_folder(&folder_path) {
                        tracing::warn!(?error, "reveal in folder failed");
                    }
                }),
        );
        menu.separator().item(
            PopupMenuItem::new("Remove")
                .icon(Icon::new(IconName::Delete))
                .on_click(dispatch(EngineCommand::Remove(id))),
        )
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports (see format.rs): a `use super::*` glob would shadow
    // the built-in `#[test]` attribute with GPUI's `test` macro.
    use std::path::PathBuf;

    use raijin_core::download::{
        Bytes, BytesPerSecond, DownloadFailure, DownloadId, DownloadStatus, FailureKind,
    };
    use raijin_core::monitor::DownloadView;

    use super::{
        ColumnDef, DownloadRow, QueueDrag, RowChecklist, SortColumn, SortDir, build_rows,
        can_pause, can_resume, filter_rows, next_sort, queue_subtitle, sort_rows, toolbar_actions,
    };
    use crate::category::{CategoryFilter, FileCategory, StatusFilter};
    use crate::format::format_bytes;

    #[test]
    fn rows_should_skip_removed_and_label_status() {
        let now = 1_700_000_000_000;
        let views = vec![
            view(1, DownloadStatus::Downloading, 140, Some(1_000), now),
            view(2, DownloadStatus::Completed, 1_000, Some(1_000), now),
            view(3, DownloadStatus::Removed, 0, None, now),
        ];

        let rows = build_rows(&views, now);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].status_label, "14% Downloading");
        let progress = rows[0].progress.expect("active row must have progress");
        assert!((progress - 14.0).abs() < 0.01, "progress was {progress}");
        assert_eq!(rows[1].status_label, "Finished");
        assert_eq!(rows[1].progress, None);
    }

    #[test]
    fn paused_download_should_retain_progress_and_label() {
        let now = 1_700_000_000_000;
        let with_total = view(1, DownloadStatus::Paused, 210, Some(1_000), now);
        let row = DownloadRow::from_view(&with_total, now);
        assert_eq!(row.status_label, "21% Paused");
        assert!(!row.is_active);
        let progress = row
            .progress
            .expect("paused row with total must have progress");
        assert!((progress - 21.0).abs() < 0.01);

        let without_total = view(2, DownloadStatus::Paused, 210, None, now);
        let row_blind = DownloadRow::from_view(&without_total, now);
        assert_eq!(row_blind.status_label, "Paused");
        assert!(!row_blind.is_active);
        assert_eq!(row_blind.progress, None);
    }

    #[test]
    fn rows_should_surface_error_kind_and_unknown_totals() {
        let now = 1_700_000_000_000;
        let mut failed = view(1, DownloadStatus::Error, 10, None, now);
        failed.failure = Some(DownloadFailure {
            kind: FailureKind::Validation,
            message: "remote ETag changed".to_owned(),
        });

        let row = DownloadRow::from_view(&failed, now);

        assert_eq!(row.status_label, "Error (changed file)");
        assert_eq!(row.progress, None);
        assert_eq!(row.size_label, format_bytes(10));
    }

    #[test]
    fn queue_member_row_should_show_its_queue_instead_of_the_category() {
        let now = 1_700_000_000_000;
        let mut row = DownloadRow::from_view(&view(1, DownloadStatus::Queued, 0, None, now), now);
        assert_eq!(
            queue_subtitle(&row),
            row.category.label(),
            "a non-member keeps the file category"
        );

        row.queue_name = Some("Night".to_owned());
        assert_eq!(
            queue_subtitle(&row),
            "Night",
            "the queue explains why a queued row is waiting"
        );
    }

    #[test]
    fn drag_payload_should_carry_the_checked_set_for_multi_move() {
        let drag = QueueDrag {
            downloads: vec![DownloadId::new(3), DownloadId::new(1)],
        };

        assert_eq!(drag.downloads.len(), 2);
        assert_eq!(drag.clone(), drag);
    }

    #[test]
    fn filter_should_combine_category_status_and_query() {
        let now = 1_700_000_000_000;
        let views = vec![
            view_named(1, "app.zip", DownloadStatus::Downloading, now),
            view_named(2, "movie.mkv", DownloadStatus::Completed, now),
            view_named(3, "notes.txt", DownloadStatus::Paused, now),
        ];
        let rows = build_rows(&views, now);

        let videos = filter_rows(
            &rows,
            CategoryFilter::Category(FileCategory::Videos),
            StatusFilter::All,
            "",
        );
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].file_name, "movie.mkv");

        let finished = filter_rows(&rows, CategoryFilter::All, StatusFilter::Finished, "");
        assert_eq!(finished.len(), 1);

        let queried = filter_rows(&rows, CategoryFilter::All, StatusFilter::All, "APP");
        assert_eq!(queried.len(), 1);
        assert_eq!(queried[0].file_name, "app.zip");
    }

    #[test]
    fn sort_should_order_by_column_and_keep_unknown_eta_last() {
        let now = 1_700_000_000_000;
        let views = vec![
            view_named(1, "b.zip", DownloadStatus::Paused, now),
            view_named(2, "a.zip", DownloadStatus::Downloading, now),
        ];
        let mut rows = build_rows(&views, now);

        sort_rows(&mut rows, SortColumn::Name, SortDir::Ascending);
        assert_eq!(rows[0].file_name, "a.zip");

        sort_rows(&mut rows, SortColumn::Status, SortDir::Ascending);
        // Active downloads sort before paused ones.
        assert_eq!(rows[0].status, DownloadStatus::Downloading);

        // Unknown ETAs always sort last.
        rows[0].eta_seconds = None;
        rows[1].eta_seconds = Some(30);
        sort_rows(&mut rows, SortColumn::Eta, SortDir::Ascending);
        assert_eq!(rows[0].eta_seconds, Some(30));
        sort_rows(&mut rows, SortColumn::Eta, SortDir::Descending);
        assert_eq!(rows[0].eta_seconds, Some(30));
    }

    #[test]
    fn sort_cycle_should_descend_first_then_reset_to_default() {
        use SortColumn as C;
        use SortDir as D;

        // First click on a new column sorts descending.
        assert_eq!(
            next_sort((C::DateAdded, D::Descending), C::Name),
            (C::Name, D::Descending)
        );
        // Clicking the sorted column toggles direction…
        assert_eq!(
            next_sort((C::Name, D::Descending), C::Name),
            (C::Name, D::Ascending)
        );
        // …then resets to the default newest-first order.
        assert_eq!(
            next_sort((C::Name, D::Ascending), C::Name),
            (C::DateAdded, D::Descending)
        );
    }

    #[test]
    fn checklist_should_toggle_and_list_ids_in_stable_order() {
        let mut checklist = RowChecklist::default();
        assert_eq!(checklist.len(), 0);

        checklist.toggle(DownloadId::new(2));
        checklist.toggle(DownloadId::new(1));
        assert!(checklist.is_checked(DownloadId::new(1)));
        assert_eq!(
            checklist.ids(),
            vec![DownloadId::new(1), DownloadId::new(2)]
        );

        // Toggling twice unchecks.
        checklist.toggle(DownloadId::new(1));
        assert!(!checklist.is_checked(DownloadId::new(1)));
        assert_eq!(checklist.len(), 1);
    }

    #[test]
    fn checklist_should_check_all_visible_and_prune_stale_ids() {
        let mut checklist = RowChecklist::default();
        checklist.set_all([DownloadId::new(1), DownloadId::new(2)], true);
        assert_eq!(checklist.len(), 2);

        // Unchecking all visible clears only those ids.
        checklist.toggle(DownloadId::new(9));
        checklist.set_all([DownloadId::new(1), DownloadId::new(2)], false);
        assert_eq!(checklist.ids(), vec![DownloadId::new(9)]);

        // Rows that left the visible set (removed/filtered) are pruned.
        checklist.toggle(DownloadId::new(1));
        checklist.retain_visible([DownloadId::new(9)]);
        assert_eq!(checklist.ids(), vec![DownloadId::new(9)]);

        checklist.uncheck_ids(&[DownloadId::new(9)]);
        assert_eq!(checklist.len(), 0);
    }

    #[test]
    fn toolbar_finished_selection_should_enable_only_remove() {
        let actions = toolbar_actions(&[DownloadStatus::Completed], &[DownloadStatus::Completed]);

        assert!(!actions.can_resume);
        assert!(!actions.can_pause);
        assert!(actions.can_remove);
        assert!(!actions.can_pause_all);
    }

    #[test]
    fn toolbar_should_enable_per_target_capability() {
        assert!(can_resume(DownloadStatus::Paused));
        assert!(can_resume(DownloadStatus::Error));
        assert!(!can_resume(DownloadStatus::Completed));
        assert!(!can_resume(DownloadStatus::Downloading));

        assert!(can_pause(DownloadStatus::Downloading));
        assert!(can_pause(DownloadStatus::Queued));
        assert!(can_pause(DownloadStatus::Added));
        assert!(can_pause(DownloadStatus::Retrying));
        assert!(can_pause(DownloadStatus::PreparingFile));
        assert!(!can_pause(DownloadStatus::Completed));
        assert!(!can_pause(DownloadStatus::Paused));
        assert!(!can_pause(DownloadStatus::Error));

        let mixed = toolbar_actions(
            &[DownloadStatus::Completed, DownloadStatus::Paused],
            &[DownloadStatus::Completed, DownloadStatus::Paused],
        );
        assert!(mixed.can_resume);
        assert!(!mixed.can_pause);
        assert!(mixed.can_remove);
        assert!(!mixed.can_pause_all);

        let downloading = toolbar_actions(
            &[DownloadStatus::Downloading],
            &[DownloadStatus::Completed, DownloadStatus::Downloading],
        );
        assert!(!downloading.can_resume);
        assert!(downloading.can_pause);
        assert!(downloading.can_pause_all);

        let empty = toolbar_actions(&[], &[DownloadStatus::Completed]);
        assert!(!empty.can_resume);
        assert!(!empty.can_pause);
        assert!(!empty.can_remove);
        assert!(!empty.can_pause_all);
    }

    #[test]
    fn column_widths_should_floor_at_readable_minimum() {
        let def = ColumnDef::new("size", "Size", 110.0, 80.0);

        // The old 20px floor let columns collapse to a sliver.
        assert_eq!(def.clamp_width(20.0), 80.0);
        assert_eq!(def.clamp_width(200.0), 200.0);
    }

    #[test]
    fn sort_should_keep_row_identity_for_selection_remap() {
        let now = 1_700_000_000_000;
        let views = vec![
            view_named(1, "c.zip", DownloadStatus::Downloading, now),
            view_named(2, "a.zip", DownloadStatus::Downloading, now),
            view_named(3, "b.zip", DownloadStatus::Downloading, now),
        ];
        let mut rows = build_rows(&views, now);

        // Handler logic: capture id before sort, re-lookup afterwards.
        let selected_id = rows[0].id;
        sort_rows(&mut rows, SortColumn::Name, SortDir::Ascending);

        let new_ix = rows
            .iter()
            .position(|row| row.id == selected_id)
            .expect("selected id must survive sorting");
        assert_eq!(rows[new_ix].file_name, "c.zip");
        assert_eq!(new_ix, 2);
    }

    fn view(
        id: i64,
        status: DownloadStatus,
        downloaded: u64,
        total: Option<u64>,
        now: i64,
    ) -> DownloadView {
        view_named(id, &format!("file-{id}.bin"), status, now).with_progress(downloaded, total)
    }

    fn view_named(id: i64, name: &str, status: DownloadStatus, now: i64) -> DownloadView {
        DownloadView {
            id: DownloadId::new(id),
            url: format!("https://example.com/{name}"),
            file_name: name.to_owned(),
            folder: PathBuf::from("C:/Downloads"),
            status,
            downloaded_bytes: Bytes::new(0),
            total_bytes: None,
            speed: BytesPerSecond::ZERO,
            eta_seconds: None,
            active_part_count: 0,
            failure: None,
            created_at: now - id * 1_000,
            updated_at: now,
        }
    }

    trait WithProgress {
        fn with_progress(self, downloaded: u64, total: Option<u64>) -> DownloadView;
    }

    impl WithProgress for DownloadView {
        fn with_progress(mut self, downloaded: u64, total: Option<u64>) -> DownloadView {
            self.downloaded_bytes = Bytes::new(downloaded);
            self.total_bytes = total.map(Bytes::new);
            self
        }
    }
}
