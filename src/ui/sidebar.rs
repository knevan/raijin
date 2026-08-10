use freya::icons;
use freya::prelude::*;

use super::services::AppServices;
use super::theme;
use crate::download::QueueId;

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueSummary {
    id: QueueId,
    name: String,
    item_count: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IconKind {
    Archive,
    Boxes,
    ChevronDown,
    ChevronUp,
    FileText,
    Folder,
    FolderCheck,
    FolderDown,
    Image,
    Music,
    Panels,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarFilter {
    All,
    Category(&'static str),
    Finished,
    Unfinished,
}

#[derive(PartialEq)]
pub(crate) struct Sidebar {
    pub(crate) filter: State<SidebarFilter>,
}

impl Component for Sidebar {
    fn render(&self) -> impl IntoElement {
        rect()
            .expanded()
            .background(theme::RAIJIN_BACKGROUND)
            .padding(Gaps::new(12., 12., 20., 20.))
            .spacing(12.)
            .vertical()
            .child(CategoriesCard {
                filter: self.filter,
            })
            .child(QueuesCard)
    }
}

#[derive(PartialEq)]
struct CategoriesCard {
    filter: State<SidebarFilter>,
}

impl Component for CategoriesCard {
    fn render(&self) -> impl IntoElement {
        let all_expanded = use_state(|| true);
        let finished_expanded = use_state(|| false);
        let unfinished_expanded = use_state(|| false);

        let selected_filter = (self.filter)();

        sidebar_card()
            .child(section_header(
                "All",
                IconKind::Folder,
                all_expanded,
                selected_filter == SidebarFilter::All,
                Some((self.filter, SidebarFilter::All)),
            ))
            .maybe(all_expanded(), |el| {
                el.child(indent_row(
                    "Compressed",
                    IconKind::Archive,
                    selected_filter,
                    self.filter,
                ))
                .child(indent_row(
                    "Programs",
                    IconKind::Boxes,
                    selected_filter,
                    self.filter,
                ))
                .child(indent_row(
                    "Videos",
                    IconKind::Video,
                    selected_filter,
                    self.filter,
                ))
                .child(indent_row(
                    "Music",
                    IconKind::Music,
                    selected_filter,
                    self.filter,
                ))
                .child(indent_row(
                    "Pictures",
                    IconKind::Image,
                    selected_filter,
                    self.filter,
                ))
                .child(indent_row(
                    "Documents",
                    IconKind::FileText,
                    selected_filter,
                    self.filter,
                ))
            })
            .child(section_header(
                "Finished",
                IconKind::FolderCheck,
                finished_expanded,
                selected_filter == SidebarFilter::Finished,
                Some((self.filter, SidebarFilter::Finished)),
            ))
            .maybe(finished_expanded(), |el| {
                el.child(indent_row(
                    "Finished",
                    IconKind::FolderCheck,
                    selected_filter,
                    self.filter,
                ))
            })
            .child(section_header(
                "Unfinished",
                IconKind::FolderDown,
                unfinished_expanded,
                selected_filter == SidebarFilter::Unfinished,
                Some((self.filter, SidebarFilter::Unfinished)),
            ))
            .maybe(unfinished_expanded(), |el| {
                el.child(indent_row(
                    "Unfinished",
                    IconKind::FolderDown,
                    selected_filter,
                    self.filter,
                ))
            })
    }
}

#[derive(PartialEq)]
struct QueuesCard;

impl Component for QueuesCard {
    fn render(&self) -> impl IntoElement {
        let services = use_consume::<AppServices>();
        let queues_expanded = use_state(|| true);
        let summaries = use_state(Vec::<QueueSummary>::new);

        use_hook(move || {
            let services = services.clone();
            spawn(async move {
                refresh_queues(&services, summaries).await;
                let mut events = services.queues.subscribe();
                while events.recv().await.is_ok() {
                    refresh_queues(&services, summaries).await;
                }
            });
        });

        let queues = summaries.read().clone();

        sidebar_card()
            .child(section_header(
                "Queues",
                IconKind::Panels,
                queues_expanded,
                false,
                None,
            ))
            .maybe(queues_expanded(), |el| {
                el.children(
                    queues
                        .into_iter()
                        .map(|queue| queue_row(queue).into_element()),
                )
            })
    }
}

async fn refresh_queues(services: &AppServices, mut summaries: State<Vec<QueueSummary>>) {
    let queues = match services.queues.list_queues().await {
        Ok(queues) => queues,
        Err(error) => {
            tracing::warn!(?error, "queue list refresh failed");
            return;
        }
    };
    let mut next = Vec::with_capacity(queues.len());
    for queue in queues {
        let item_count = match services.queues.list_items(queue.id).await {
            Ok(items) => items.len(),
            Err(error) => {
                tracing::warn!(queue_id = %queue.id, ?error, "queue item refresh failed");
                0
            }
        };
        next.push(QueueSummary {
            id: queue.id,
            name: queue.name,
            item_count,
        });
    }
    summaries.set_if_modified(next);
}

fn sidebar_card() -> Rect {
    rect()
        .width(Size::fill())
        .background(theme::SURFACE)
        .corner_radius(9.)
        .border(card_border())
        .padding(Gaps::new(0., 0., 0., 0.))
        .vertical()
}

fn section_header(
    title: &'static str,
    icon: IconKind,
    mut expanded: State<bool>,
    selected: bool,
    filter_selection: Option<(State<SidebarFilter>, SidebarFilter)>,
) -> impl IntoElement {
    let is_expanded = expanded();

    rect()
        .height(Size::px(39.))
        .width(Size::fill())
        .horizontal()
        .corner_radius(7.)
        .background(if selected {
            theme::SURFACE_ELEVATED
        } else {
            theme::SURFACE
        })
        .border(card_border())
        .on_press(move |_| {
            if let Some((mut filter, filter_value)) = filter_selection {
                filter.set(filter_value);
            }
            expanded.toggle();
        })
        .child(
            rect()
                .width(Size::px(3.))
                .height(Size::fill())
                .background(if selected {
                    theme::ACCENT
                } else {
                    Color::TRANSPARENT
                }),
        )
        .child(
            rect()
                .expanded()
                .horizontal()
                .cross_align(Alignment::Center)
                .padding(Gaps::new(0., 40., 0., 17.))
                .spacing(8.)
                .child(icon_view(icon, theme::TEXT_PRIMARY, 18.))
                .child(
                    label()
                        .text(title)
                        .font_size(16.)
                        .font_weight(FontWeight::BOLD)
                        .color(theme::TEXT_PRIMARY)
                        .width(Size::fill()),
                )
                .child(icon_view(
                    if is_expanded {
                        IconKind::ChevronUp
                    } else {
                        IconKind::ChevronDown
                    },
                    theme::TEXT_MUTED,
                    14.,
                )),
        )
}

fn indent_row(
    title: &'static str,
    icon: IconKind,
    selected_filter: SidebarFilter,
    mut filter: State<SidebarFilter>,
) -> impl IntoElement {
    let filter_value = match title {
        "Finished" => SidebarFilter::Finished,
        "Unfinished" => SidebarFilter::Unfinished,
        _ => SidebarFilter::Category(title),
    };
    let selected = selected_filter == filter_value;
    rect()
        .height(Size::px(39.))
        .width(Size::fill())
        .horizontal()
        .cross_align(Alignment::Center)
        .padding(Gaps::new(0., 12., 0., 56.))
        .spacing(8.)
        .corner_radius(7.)
        .background(if selected {
            theme::SURFACE_ELEVATED
        } else {
            Color::TRANSPARENT
        })
        .on_press(move |_| filter.set(filter_value))
        .child(icon_view(
            icon,
            if selected {
                theme::TEXT_PRIMARY
            } else {
                theme::TEXT_SUBTLE
            },
            17.,
        ))
        .child(label().text(title).font_size(14.).color(if selected {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_SUBTLE
        }))
}

fn queue_row(queue: QueueSummary) -> impl IntoElement {
    rect()
        .key(queue.id.get())
        .height(Size::px(42.))
        .width(Size::fill())
        .horizontal()
        .cross_align(Alignment::Center)
        .padding(Gaps::new(0., 40., 0., 34.))
        .spacing(8.)
        .child(icon_view(IconKind::Folder, theme::TEXT_SUBTLE, 17.))
        .child(
            label()
                .text(queue.name)
                .font_size(14.)
                .color(theme::TEXT_SUBTLE)
                .width(Size::fill()),
        )
        .child(
            label()
                .text(queue.item_count.to_string())
                .font_size(13.)
                .color(theme::TEXT_SUBTLE),
        )
}

fn card_border() -> Border {
    Border::new().width(1.).fill(theme::BORDER)
}

fn icon_view(kind: IconKind, color: Color, size: f32) -> SvgViewer {
    SvgViewer::new(match kind {
        IconKind::Archive => icons::lucide::file_archive(),
        IconKind::Boxes => icons::lucide::boxes(),
        IconKind::ChevronDown => icons::lucide::chevron_down(),
        IconKind::ChevronUp => icons::lucide::chevron_up(),
        IconKind::FileText => icons::lucide::file_text(),
        IconKind::Folder => icons::lucide::folder(),
        IconKind::FolderCheck => icons::lucide::folder_check(),
        IconKind::FolderDown => icons::lucide::folder_down(),
        IconKind::Image => icons::lucide::image(),
        IconKind::Music => icons::lucide::music(),
        IconKind::Panels => icons::lucide::panels_top_left(),
        IconKind::Video => icons::lucide::video(),
    })
    .color(color)
    .width(Size::px(size))
    .height(Size::px(size))
}
