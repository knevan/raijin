use std::num::{NonZeroU16, NonZeroU32};
use std::path::PathBuf;

use freya::icons;
use freya::prelude::*;

use super::services::AppServices;
use super::theme;
use crate::config::DesktopSettings;
use crate::download::QueueId;

const MIN_THREADS: u16 = 1;
const MAX_THREADS: u16 = 32;
const MIN_CONCURRENT_DOWNLOADS: u16 = 1;
const MAX_CONCURRENT_DOWNLOADS: u16 = 16;
const MIN_RETRIES: u32 = 1;
const MAX_RETRIES: u32 = 10;
const SETTINGS_CONTENT_WIDTH: f32 = 620.;
const SETTING_COPY_WIDTH: f32 = 340.;
const SETTING_CONTROL_WIDTH: f32 = 150.;
const FOLDER_CONTROL_WIDTH: f32 = 230.;
const SETTING_ROW_RIGHT_PADDING: f32 = 120.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Appearance,
    DownloadEngine,
    BrowserIntegration,
}

#[derive(Clone)]
pub(crate) struct SettingsWindow {
    services: AppServices,
}

impl PartialEq for SettingsWindow {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Component for SettingsWindow {
    fn render(&self) -> impl IntoElement {
        use_init_theme(theme::raijin_theme);

        let selected_tab = use_state(|| SettingsTab::DownloadEngine);
        let mut settings = use_state(|| {
            DesktopSettings::from_defaults(
                self.services.default_folder.clone(),
                self.services.category_root.clone(),
            )
        });
        let mut folder = use_state(|| self.services.default_folder.to_string_lossy().into_owned());
        let mut load_error = use_state(|| Option::<String>::None);
        let save_status = use_state(|| Option::<String>::None);
        let services = self.services.clone();

        use_hook(move || {
            let services = services.clone();
            spawn(async move {
                match services.load_desktop_settings().await {
                    Ok(loaded) => {
                        folder.set(
                            loaded
                                .default_download_folder
                                .to_string_lossy()
                                .into_owned(),
                        );
                        settings.set(loaded);
                        load_error.set(None);
                    }
                    Err(error) => load_error.set(Some(error.to_string())),
                }
            });
        });

        rect()
            .expanded()
            .background(theme::RAIJIN_BACKGROUND)
            .color(theme::TEXT_PRIMARY)
            .horizontal()
            .child(settings_sidebar(selected_tab, selected_tab()))
            .child(
                rect()
                    .width(Size::px(1.))
                    .height(Size::fill())
                    .background(theme::BORDER),
            )
            .child(
                rect()
                    .expanded()
                    .padding(Gaps::new(12., 36., 12., 12.))
                    .child(ScrollView::new().direction(Direction::Vertical).child(
                        settings_content(
                            self.services.clone(),
                            selected_tab(),
                            settings,
                            folder,
                            save_status,
                            load_error.read().clone(),
                        ),
                    )),
            )
    }
}

pub(crate) fn settings_window_config(services: AppServices) -> WindowConfig {
    WindowConfig::new(move || SettingsWindow {
        services: services.clone(),
    })
    .with_title("Settings")
    .with_size(960., 540.)
    .with_min_size(760., 460.)
    .with_resizable(true)
    .with_background(theme::RAIJIN_BACKGROUND)
}

fn settings_sidebar(selected_tab: State<SettingsTab>, current: SettingsTab) -> impl IntoElement {
    rect()
        .width(Size::px(306.))
        .height(Size::fill())
        .padding(Gaps::new(12., 8., 12., 8.))
        .vertical()
        .spacing(6.)
        .child(settings_nav_row(
            "Appearance",
            SettingsTab::Appearance,
            current,
            selected_tab,
        ))
        .child(settings_nav_row(
            "Download Engine",
            SettingsTab::DownloadEngine,
            current,
            selected_tab,
        ))
        .child(settings_nav_row(
            "Browser Integration",
            SettingsTab::BrowserIntegration,
            current,
            selected_tab,
        ))
}

fn settings_nav_row(
    title: &'static str,
    tab: SettingsTab,
    current: SettingsTab,
    mut selected_tab: State<SettingsTab>,
) -> impl IntoElement {
    let selected = current == tab;
    rect()
        .height(Size::px(43.))
        .width(Size::fill())
        .horizontal()
        .cross_align(Alignment::Center)
        .corner_radius(8.)
        .background(if selected {
            theme::SURFACE_ELEVATED
        } else {
            Color::TRANSPARENT
        })
        .on_press(move |_| selected_tab.set(tab))
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
                .spacing(8.)
                .padding(Gaps::new(0., 14., 0., 18.))
                .child(
                    SvgViewer::new(match tab {
                        SettingsTab::Appearance => icons::lucide::settings(),
                        SettingsTab::DownloadEngine => icons::lucide::download(),
                        SettingsTab::BrowserIntegration => icons::lucide::link_2(),
                    })
                    .width(Size::px(18.))
                    .height(Size::px(18.))
                    .color(if selected {
                        theme::TEXT_PRIMARY
                    } else {
                        theme::TEXT_SUBTLE
                    }),
                )
                .child(
                    label()
                        .text(title)
                        .font_size(16.)
                        .font_weight(if selected {
                            FontWeight::BOLD
                        } else {
                            FontWeight::NORMAL
                        })
                        .color(if selected {
                            theme::TEXT_PRIMARY
                        } else {
                            theme::TEXT_SUBTLE
                        }),
                ),
        )
}

fn settings_content(
    services: AppServices,
    tab: SettingsTab,
    settings: State<DesktopSettings>,
    folder: State<String>,
    save_status: State<Option<String>>,
    load_error: Option<String>,
) -> impl IntoElement {
    rect()
        .width(Size::px(SETTINGS_CONTENT_WIDTH))
        .vertical()
        .spacing(14.)
        .maybe_child(load_error.map(status_banner))
        .maybe_child(save_status.read().clone().map(status_banner))
        .child(match tab {
            SettingsTab::Appearance => placeholder_panel("Appearance", "Theme and density settings are not wired yet."),
            SettingsTab::DownloadEngine => download_engine_panel(services, settings, folder, save_status).into_element(),
            SettingsTab::BrowserIntegration => placeholder_panel(
                "Browser Integration",
                "Browser capture server and extension settings will be enabled after integration API is wired.",
            ),
        })
}

fn download_engine_panel(
    services: AppServices,
    settings: State<DesktopSettings>,
    folder: State<String>,
    save_status: State<Option<String>>,
) -> impl IntoElement {
    let current = settings.read().clone();
    rect()
        .width(Size::fill())
        .vertical()
        .spacing(20.)
        .child(
            settings_card()
                .child(folder_setting_row(
                    services.clone(),
                    settings,
                    folder,
                    save_status,
                ))
                .child(setting_row(
                    "Use Category By Default",
                    enabled_text(current.use_category_by_default),
                    toggle_control(current.use_category_by_default, {
                        let services = services.clone();
                        move |()| {
                            update_settings(settings, services.clone(), save_status, |settings| {
                                settings.use_category_by_default =
                                    !settings.use_category_by_default;
                            })
                        }
                    }),
                )),
        )
        .child(
            settings_card()
                .child(setting_row(
                    "Global Speed Limiter",
                    speed_limit_text(current.global_speed_limit_bps),
                    toggle_control(current.global_speed_limit_bps.is_some(), {
                        let services = services.clone();
                        move |()| {
                            update_settings(settings, services.clone(), save_status, |settings| {
                                settings.global_speed_limit_bps =
                                    if settings.global_speed_limit_bps.is_some() {
                                        None
                                    } else {
                                        Some(1_048_576)
                                    };
                            })
                        }
                    }),
                ))
                .child(setting_row(
                    "Thread Count",
                    format!(
                        "A download can have up to {} threads",
                        current.thread_count.get()
                    ),
                    number_stepper(
                        current.thread_count.get().to_string(),
                        current.thread_count.get() > MIN_THREADS,
                        current.thread_count.get() < MAX_THREADS,
                        {
                            let services = services.clone();
                            move |()| {
                                update_settings(
                                    settings,
                                    services.clone(),
                                    save_status,
                                    |settings| {
                                        let next = settings
                                            .thread_count
                                            .get()
                                            .saturating_sub(1)
                                            .max(MIN_THREADS);
                                        if let Some(value) = NonZeroU16::new(next) {
                                            settings.thread_count = value;
                                        }
                                    },
                                )
                            }
                        },
                        {
                            let services = services.clone();
                            move |()| {
                                update_settings(
                                    settings,
                                    services.clone(),
                                    save_status,
                                    |settings| {
                                        let next = settings
                                            .thread_count
                                            .get()
                                            .saturating_add(1)
                                            .min(MAX_THREADS);
                                        if let Some(value) = NonZeroU16::new(next) {
                                            settings.thread_count = value;
                                        }
                                    },
                                )
                            }
                        },
                    ),
                ))
                .child(setting_row(
                    "Maximum Concurrent Downloads",
                    current.max_concurrent_downloads.get().to_string(),
                    number_stepper(
                        current.max_concurrent_downloads.get().to_string(),
                        current.max_concurrent_downloads.get() > MIN_CONCURRENT_DOWNLOADS,
                        current.max_concurrent_downloads.get() < MAX_CONCURRENT_DOWNLOADS,
                        {
                            let services = services.clone();
                            move |()| {
                                update_max_concurrent(settings, services.clone(), save_status, -1)
                            }
                        },
                        {
                            let services = services.clone();
                            move |()| {
                                update_max_concurrent(settings, services.clone(), save_status, 1)
                            }
                        },
                    ),
                ))
                .child(setting_row(
                    "Maximum Download Retries",
                    format!(
                        "Failed downloads will be retried {} time(s)",
                        current.max_download_retries.get()
                    ),
                    number_stepper(
                        current.max_download_retries.get().to_string(),
                        current.max_download_retries.get() > MIN_RETRIES,
                        current.max_download_retries.get() < MAX_RETRIES,
                        {
                            let services = services.clone();
                            move |()| {
                                update_settings(
                                    settings,
                                    services.clone(),
                                    save_status,
                                    |settings| {
                                        let next =
                                            settings.max_download_retries.get().saturating_sub(1);
                                        if let Some(value) = NonZeroU32::new(next) {
                                            settings.max_download_retries = value;
                                        }
                                    },
                                )
                            }
                        },
                        {
                            let services = services.clone();
                            move |()| {
                                update_settings(
                                    settings,
                                    services.clone(),
                                    save_status,
                                    |settings| {
                                        let next = settings
                                            .max_download_retries
                                            .get()
                                            .saturating_add(1)
                                            .min(MAX_RETRIES);
                                        if let Some(value) = NonZeroU32::new(next) {
                                            settings.max_download_retries = value;
                                        }
                                    },
                                )
                            }
                        },
                    ),
                ))
                .child(setting_row(
                    "Dynamic Part Creation",
                    enabled_text(current.dynamic_part_creation),
                    toggle_control(current.dynamic_part_creation, {
                        let services = services.clone();
                        move |()| {
                            update_settings(settings, services.clone(), save_status, |settings| {
                                settings.dynamic_part_creation = !settings.dynamic_part_creation;
                            })
                        }
                    }),
                )),
        )
}

fn folder_setting_row(
    services: AppServices,
    settings: State<DesktopSettings>,
    mut folder: State<String>,
    save_status: State<Option<String>>,
) -> impl IntoElement {
    rect()
        .height(Size::px(74.))
        .width(Size::fill())
        .horizontal()
        .main_align(Alignment::Start)
        .cross_align(Alignment::Center)
        .padding(Gaps::new(0., SETTING_ROW_RIGHT_PADDING, 0., 22.))
        .child(setting_copy(
            "Default Download Folder",
            "New manual downloads use this folder unless category routing is enabled.",
        ))
        .child(
            rect()
                .width(Size::px(FOLDER_CONTROL_WIDTH))
                .horizontal()
                .cross_align(Alignment::Center)
                .spacing(8.)
                .padding(Gaps::new(0., 25., 0., 0.))
                .child(folder_input(folder))
                .child(small_icon_button(icons::lucide::folder_open(), true, {
                    move |()| {
                        spawn(async move {
                            let Some(handle) = rfd::AsyncFileDialog::new().pick_folder().await
                            else {
                                return;
                            };
                            folder.set(handle.path().to_string_lossy().into_owned());
                        });
                    }
                }))
                .child(small_text_button("Save", true, move |()| {
                    save_folder_setting(services.clone(), settings, folder, save_status);
                })),
        )
}

fn folder_input(folder: State<String>) -> impl IntoElement {
    Input::new(folder)
        .placeholder("Default folder")
        .width(Size::fill())
        .theme_colors(InputColorsThemePartial {
            background: Some(theme::SURFACE.into()),
            focus_background: Some(theme::SURFACE.into()),
            border_fill: Some(theme::BORDER.into()),
            focus_border_fill: Some(theme::ACCENT.into()),
            color: Some(theme::TEXT_PRIMARY.into()),
            placeholder_color: Some(theme::TEXT_SUBTLE.into()),
        })
        .theme_layout(InputLayoutThemePartial {
            corner_radius: Some(CornerRadius::new_all(8.).into()),
            inner_margin: Some(Gaps::new(7., 10., 7., 10.).into()),
        })
}

fn setting_row(
    title: &'static str,
    description: impl Into<String>,
    control: impl IntoElement,
) -> impl IntoElement {
    rect()
        .height(Size::px(94.))
        .width(Size::fill())
        .horizontal()
        .main_align(Alignment::Start)
        .cross_align(Alignment::Center)
        .padding(Gaps::new(0., 25., 0., 22.))
        .child(setting_copy(title, description))
        .child(
            rect()
                .width(Size::px(SETTING_CONTROL_WIDTH))
                .main_align(Alignment::End)
                .padding(Gaps::new(0., 25., 0., 0.))
                .child(control),
        )
}

fn setting_copy(title: &'static str, description: impl Into<String>) -> impl IntoElement {
    rect()
        .width(Size::px(SETTING_COPY_WIDTH))
        .vertical()
        .spacing(5.)
        .child(
            rect()
                .horizontal()
                .cross_align(Alignment::Center)
                .spacing(8.)
                .child(
                    label()
                        .text(title)
                        .font_size(15.)
                        .font_weight(FontWeight::BOLD),
                )
                .child(help_badge()),
        )
        .child(
            label()
                .text(description.into())
                .font_size(14.)
                .color(theme::TEXT_MUTED),
        )
}

fn help_badge() -> impl IntoElement {
    rect()
        .width(Size::px(22.))
        .height(Size::px(22.))
        .corner_radius(11.)
        .center()
        .background(theme::SURFACE)
        .child(label().text("?").font_size(13.).color(theme::TEXT_MUTED))
}

fn settings_card() -> Rect {
    rect()
        .width(Size::fill())
        .background(theme::SURFACE_ELEVATED)
        .corner_radius(8.)
        .vertical()
}

fn toggle_control(enabled: bool, on_toggle: impl Fn(()) + 'static) -> impl IntoElement {
    rect()
        .width(Size::px(54.))
        .height(Size::px(31.))
        .corner_radius(16.)
        .padding(Gaps::new(3., 3., 3., 3.))
        .main_align(if enabled {
            Alignment::End
        } else {
            Alignment::Start
        })
        .background(if enabled {
            theme::ACCENT
        } else {
            theme::SURFACE
        })
        .on_press(move |_| on_toggle(()))
        .child(
            rect()
                .width(Size::px(25.))
                .height(Size::px(25.))
                .corner_radius(13.)
                .background(theme::TEXT_PRIMARY),
        )
}

fn number_stepper(
    value: String,
    can_decrement: bool,
    can_increment: bool,
    on_decrement: impl Fn(()) + 'static,
    on_increment: impl Fn(()) + 'static,
) -> impl IntoElement {
    rect()
        .width(Size::px(126.))
        .height(Size::px(32.))
        .horizontal()
        .cross_align(Alignment::Center)
        .corner_radius(8.)
        .border(Border::new().width(1.).fill(theme::BORDER))
        .background(theme::SURFACE)
        .child(
            label()
                .text(value)
                .font_size(14.)
                .width(Size::fill())
                .padding(Gaps::new(0., 0., 0., 10.)),
        )
        .child(small_icon_button(
            icons::lucide::chevron_down(),
            can_decrement,
            on_decrement,
        ))
        .child(small_icon_button(
            icons::lucide::chevron_up(),
            can_increment,
            on_increment,
        ))
}

fn small_icon_button(
    icon: freya::prelude::Bytes,
    enabled: bool,
    on_press: impl Fn(()) + 'static,
) -> impl IntoElement {
    rect()
        .width(Size::px(30.))
        .height(Size::px(30.))
        .center()
        .corner_radius(6.)
        .background(Color::TRANSPARENT)
        .maybe(enabled, |el| el.on_press(move |_| on_press(())))
        .child(
            SvgViewer::new(icon)
                .width(Size::px(16.))
                .height(Size::px(16.))
                .color(if enabled {
                    theme::TEXT_MUTED
                } else {
                    theme::BORDER
                }),
        )
}

fn small_text_button(
    text: &'static str,
    enabled: bool,
    on_press: impl Fn(()) + 'static,
) -> impl IntoElement {
    rect()
        .height(Size::px(32.))
        .padding(Gaps::new(0., 12., 0., 12.))
        .center()
        .corner_radius(8.)
        .border(Border::new().width(1.).fill(theme::BORDER))
        .background(if enabled {
            theme::SURFACE
        } else {
            Color::TRANSPARENT
        })
        .maybe(enabled, |el| el.on_press(move |_| on_press(())))
        .child(label().text(text).font_size(13.).color(if enabled {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_SUBTLE
        }))
}

fn placeholder_panel(title: &'static str, body: &'static str) -> Element {
    settings_card()
        .height(Size::px(170.))
        .padding(Gaps::new(22., 22., 22., 22.))
        .spacing(10.)
        .child(
            label()
                .text(title)
                .font_size(18.)
                .font_weight(FontWeight::BOLD),
        )
        .child(label().text(body).font_size(14.).color(theme::TEXT_MUTED))
        .into_element()
}

fn status_banner(message: String) -> impl IntoElement {
    rect()
        .width(Size::fill())
        .padding(Gaps::new(10., 14., 10., 14.))
        .corner_radius(8.)
        .background(theme::SURFACE)
        .border(Border::new().width(1.).fill(theme::BORDER))
        .child(
            label()
                .text(message)
                .font_size(13.)
                .color(theme::TEXT_MUTED),
        )
}

fn enabled_text(enabled: bool) -> &'static str {
    if enabled { "Enabled" } else { "Disabled" }
}

fn speed_limit_text(limit: Option<u64>) -> String {
    match limit {
        Some(limit) => format!("{} KiB/s", limit / 1024),
        None => "Unlimited".to_owned(),
    }
}

fn update_settings(
    mut state: State<DesktopSettings>,
    services: AppServices,
    save_status: State<Option<String>>,
    update: impl FnOnce(&mut DesktopSettings) + Send + 'static,
) {
    let mut next = state.read().clone();
    update(&mut next);
    state.set(next.clone());
    save_settings(services, next, save_status);
}

fn update_max_concurrent(
    mut state: State<DesktopSettings>,
    services: AppServices,
    save_status: State<Option<String>>,
    delta: i16,
) {
    let mut next = state.read().clone();
    let current = i32::from(next.max_concurrent_downloads.get());
    let updated = (current + i32::from(delta)).clamp(
        i32::from(MIN_CONCURRENT_DOWNLOADS),
        i32::from(MAX_CONCURRENT_DOWNLOADS),
    ) as u16;
    let Some(max_concurrent) = NonZeroU16::new(updated) else {
        return;
    };
    next.max_concurrent_downloads = max_concurrent;
    state.set(next.clone());
    spawn(async move {
        let result = async {
            services
                .queues
                .set_max_concurrent(QueueId::MAIN, max_concurrent)
                .await?;
            services.save_desktop_settings(&next).await
        }
        .await;
        set_save_status(save_status, result);
    });
}

fn save_folder_setting(
    services: AppServices,
    mut state: State<DesktopSettings>,
    folder: State<String>,
    mut save_status: State<Option<String>>,
) {
    let candidate = PathBuf::from(folder.read().trim());
    if candidate.as_os_str().is_empty() {
        save_status.set(Some("Default folder cannot be empty".to_owned()));
        return;
    }
    let mut next = state.read().clone();
    next.default_download_folder = candidate;
    state.set(next.clone());
    save_settings(services, next, save_status);
}

fn save_settings(
    services: AppServices,
    settings: DesktopSettings,
    save_status: State<Option<String>>,
) {
    spawn(async move {
        let result = services.save_desktop_settings(&settings).await;
        set_save_status(save_status, result);
    });
}

fn set_save_status(
    mut save_status: State<Option<String>>,
    result: Result<(), super::services::AppServicesError>,
) {
    match result {
        Ok(()) => save_status.set(Some("Settings saved".to_owned())),
        Err(error) => save_status.set(Some(format!("Settings save failed: {error}"))),
    }
}
