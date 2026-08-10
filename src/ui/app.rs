use freya::icons;
use freya::prelude::*;

use super::downloads_table::DownloadsTable;
use super::services::AppServices;
use super::sidebar::{Sidebar, SidebarFilter};
use super::theme;

pub(crate) fn root(services: AppServices) -> impl IntoElement {
    use_init_theme(theme::raijin_theme);
    use_provide_context(|| services);
    let sidebar_filter = use_state(|| SidebarFilter::All);

    rect()
        .expanded()
        .background(theme::RAIJIN_BACKGROUND)
        .color(theme::TEXT_PRIMARY)
        .vertical()
        .child(MenuBar)
        .child(
            ResizableContainer::new()
                .direction(Direction::Horizontal)
                .panel(
                    ResizablePanel::new(PanelSize::px(264.))
                        .min_size(230.)
                        .child(Sidebar {
                            filter: sidebar_filter,
                        }),
                )
                .panel(
                    ResizablePanel::new(PanelSize::percent(100.))
                        .min_size(25.)
                        .child(DownloadsTable {
                            filter: sidebar_filter,
                        }),
                ),
        )
}

#[derive(PartialEq)]
struct MenuBar;

impl Component for MenuBar {
    fn render(&self) -> impl IntoElement {
        rect()
            .height(Size::px(40.))
            .width(Size::fill())
            .horizontal()
            .cross_align(Alignment::Center)
            .padding(Gaps::new(0., 18., 0., 18.))
            .spacing(22.)
            .background(theme::RAIJIN_BACKGROUND)
            .border(
                Border::new()
                    .width(BorderWidth {
                        bottom: 1.,
                        ..Default::default()
                    })
                    .fill(theme::BORDER),
            )
            .child(
                SvgViewer::new(icons::lucide::download())
                    .color(theme::ACCENT)
                    .width(Size::px(22.))
                    .height(Size::px(22.)),
            )
            .child(menu_label("File"))
            .child(menu_label("Tasks"))
            .child(menu_label("Tools"))
            .child(menu_label("Help"))
    }
}

fn menu_label(text: &'static str) -> impl IntoElement {
    label().text(text).font_size(14.).color(theme::TEXT_PRIMARY)
}
