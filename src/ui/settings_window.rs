use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::Duration;

use freya::animation::{AnimNum, Ease, use_animation_transition};
use freya::icons;
use freya::prelude::*;

use super::services::AppServices;
use super::theme;
use crate::config::DesktopSettings;
use crate::download::QueueId;

const MIN_THREADS: u16 = 1;
const MAX_THREADS: u16 = 32;
const MIN_CONCURRENT_DOWNLOADS: u16 = 0;
const MAX_CONCURRENT_DOWNLOADS: u16 = 16;
const MIN_RETRIES: u32 = 0;
const MAX_RETRIES: u32 = 10;
const SETTING_COPY_WIDTH: f32 = 340.;
const SETTING_CONTROL_WIDTH: f32 = 150.;
const FOLDER_INPUT_WIDTH: f32 = 255.;
const FOLDER_CONTROL_WIDTH: f32 = FOLDER_INPUT_WIDTH + 38.;

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
        let mut folder_autosave_enabled = use_state(|| false);
        let mut load_error = use_state(|| Option::<String>::None);
        let mut save_status = use_state(|| Option::<String>::None);
        let mut visible_save_status = use_state(|| Option::<String>::None);
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
                        folder_autosave_enabled.set(true);
                        load_error.set(None);
                    }
                    Err(error) => load_error.set(Some(error.to_string())),
                }
            });
        });

        use_side_effect(move || {
            let Some(message) = save_status.read().clone() else {
                return;
            };
            visible_save_status.set(Some(message.clone()));
            save_status.set(None);

            spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if visible_save_status.read().as_ref() == Some(&message) {
                    visible_save_status.set(None);
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
                            folder_autosave_enabled,
                            save_status,
                            SettingsBanners {
                                saved: visible_save_status.read().clone(),
                                load_error: load_error.read().clone(),
                            },
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
    folder_autosave_enabled: State<bool>,
    save_status: State<Option<String>>,
    banners: SettingsBanners,
) -> impl IntoElement {
    rect()
        .width(Size::fill())
        .vertical()
        .spacing(14.)
        .maybe_child(banners.load_error.map(status_banner))
        .maybe_child(banners.saved.map(status_banner))
        .child(match tab {
            SettingsTab::Appearance => placeholder_panel("Appearance", "Theme and density settings are not wired yet."),
            SettingsTab::DownloadEngine => {
                download_engine_panel(
                    services,
                    settings,
                    folder,
                    folder_autosave_enabled,
                    save_status,
                )
                .into_element()
            }
            SettingsTab::BrowserIntegration => placeholder_panel(
                "Browser Integration",
                "Browser capture server and extension settings will be enabled after integration API is wired.",
            ),
        })
}

struct SettingsBanners {
    saved: Option<String>,
    load_error: Option<String>,
}

fn download_engine_panel(
    services: AppServices,
    settings: State<DesktopSettings>,
    folder: State<String>,
    folder_autosave_enabled: State<bool>,
    save_status: State<Option<String>>,
) -> impl IntoElement {
    let current = settings.read().clone();
    rect()
        .width(Size::fill())
        .vertical()
        .spacing(20.)
        .child(
            settings_card()
                .child(FolderSettingRow {
                    services: services.clone(),
                    settings,
                    folder,
                    autosave_enabled: folder_autosave_enabled,
                    save_status,
                })
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
                        u32::from(current.thread_count.get()),
                        u32::from(MIN_THREADS),
                        u32::from(MAX_THREADS),
                        {
                            let services = services.clone();
                            move |value| {
                                update_settings(
                                    settings,
                                    services.clone(),
                                    save_status,
                                    move |settings| {
                                        if let Ok(value) = u16::try_from(value)
                                            && let Some(value) = NonZeroU16::new(value)
                                        {
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
                    current.max_concurrent_downloads.to_string(),
                    number_stepper(
                        u32::from(current.max_concurrent_downloads),
                        u32::from(MIN_CONCURRENT_DOWNLOADS),
                        u32::from(MAX_CONCURRENT_DOWNLOADS),
                        {
                            let services = services.clone();
                            move |value| {
                                update_max_concurrent(
                                    settings,
                                    services.clone(),
                                    save_status,
                                    value,
                                )
                            }
                        },
                    ),
                ))
                .child(setting_row(
                    "Maximum Download Retries",
                    format!(
                        "Failed downloads will be retried {} time(s)",
                        current.max_download_retries
                    ),
                    number_stepper(current.max_download_retries, MIN_RETRIES, MAX_RETRIES, {
                        let services = services.clone();
                        move |value| {
                            update_settings(
                                settings,
                                services.clone(),
                                save_status,
                                move |settings| {
                                    settings.max_download_retries = value;
                                },
                            )
                        }
                    }),
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

#[derive(Clone)]
struct FolderSettingRow {
    services: AppServices,
    settings: State<DesktopSettings>,
    folder: State<String>,
    autosave_enabled: State<bool>,
    save_status: State<Option<String>>,
}

impl PartialEq for FolderSettingRow {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Component for FolderSettingRow {
    fn render(&self) -> impl IntoElement {
        let services = self.services.clone();
        let settings = self.settings;
        let mut folder = self.folder;
        let autosave_enabled = self.autosave_enabled;
        let save_status = self.save_status;

        use_side_effect(move || {
            if !autosave_enabled() {
                return;
            }
            let candidate = folder.read().trim().to_owned();
            let saved = settings
                .read()
                .default_download_folder
                .to_string_lossy()
                .into_owned();
            if candidate == saved {
                return;
            }
            save_folder_path(services.clone(), settings, candidate, save_status);
        });

        rect()
            .height(Size::px(74.))
            .width(Size::fill())
            .horizontal()
            .main_align(Alignment::SpaceBetween)
            .cross_align(Alignment::Center)
            .padding(Gaps::new(0., 25., 0., 22.))
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
                    })),
            )
    }
}

fn folder_input(folder: State<String>) -> impl IntoElement {
    Input::new(folder)
        .placeholder("Default folder")
        .width(Size::px(FOLDER_INPUT_WIDTH))
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
        .main_align(Alignment::SpaceBetween)
        .cross_align(Alignment::Center)
        .padding(Gaps::new(0., 25., 0., 22.))
        .child(setting_copy(title, description))
        .child(
            rect()
                .width(Size::px(SETTING_CONTROL_WIDTH))
                .horizontal()
                .main_align(Alignment::End)
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
    ToggleControl {
        enabled,
        on_toggle: on_toggle.into(),
    }
}

#[derive(PartialEq)]
struct ToggleControl {
    enabled: bool,
    on_toggle: EventHandler<()>,
}

impl Component for ToggleControl {
    fn render(&self) -> impl IntoElement {
        let progress = if self.enabled { 1. } else { 0. };
        let animation = use_animation_transition(progress, |from, to| {
            AnimNum::new(from, to).time(180).ease(Ease::InOut)
        });
        let knob_left = 3. + (animation.get().value() * 23.);
        let on_toggle = self.on_toggle.clone();

        rect()
            .width(Size::px(54.))
            .height(Size::px(31.))
            .corner_radius(16.)
            .background(if self.enabled {
                theme::ACCENT
            } else {
                theme::SURFACE
            })
            .on_press(move |_| on_toggle.call(()))
            .child(
                rect()
                    .position(Position::new_absolute().left(knob_left).top(3.))
                    .width(Size::px(25.))
                    .height(Size::px(25.))
                    .corner_radius(13.)
                    .background(theme::TEXT_PRIMARY),
            )
    }
}

fn number_stepper(
    value: u32,
    min: u32,
    max: u32,
    on_change: impl Fn(u32) + 'static,
) -> impl IntoElement {
    NumberStepper {
        value,
        min,
        max,
        on_change: on_change.into(),
    }
}

#[derive(PartialEq)]
struct NumberStepper {
    value: u32,
    min: u32,
    max: u32,
    on_change: EventHandler<u32>,
}

impl Component for NumberStepper {
    fn render(&self) -> impl IntoElement {
        let mut text = use_state(|| self.value.to_string());
        let mut synced_value = use_state(|| self.value);
        let mut was_focused = use_state(|| false);
        let a11y_id = use_a11y();
        let focus = use_focus(a11y_id);
        let value_text = self.value.to_string();
        let min = self.min;
        let max = self.max;
        let value = self.value.clamp(min, max);
        let can_decrement = value > min;
        let can_increment = value < max;
        let on_change = self.on_change.clone();

        use_side_effect(move || {
            if synced_value() != value {
                synced_value.set(value);
                text.set(value_text.clone());
            }

            if focus().is_focused() {
                was_focused.set_if_modified(true);
                return;
            }

            if !was_focused() {
                return;
            }

            was_focused.set(false);
            let next = stepper_text_value(&text.read(), min, max);
            text.set(next.to_string());

            if next != value {
                on_change.call(next);
            }
        });

        number_stepper_input(
            text,
            a11y_id,
            can_decrement,
            can_increment,
            {
                let on_change = self.on_change.clone();
                move |()| {
                    let next = value.saturating_sub(1).max(min);
                    on_change.call(next);
                }
            },
            {
                let on_change = self.on_change.clone();
                move |()| {
                    let next = value.saturating_add(1).min(max);
                    on_change.call(next);
                }
            },
            {
                let on_change = self.on_change.clone();
                move |text| {
                    let next = stepper_text_value(&text, min, max);
                    on_change.call(next);
                }
            },
        )
    }
}

fn stepper_text_value(text: &str, min: u32, max: u32) -> u32 {
    text.trim()
        .parse::<u32>()
        .map_or(min, |value| value.clamp(min, max))
}

fn number_stepper_input(
    text: State<String>,
    a11y_id: AccessibilityId,
    can_decrement: bool,
    can_increment: bool,
    on_decrement: impl Fn(()) + 'static,
    on_increment: impl Fn(()) + 'static,
    on_submit: impl Fn(String) + 'static,
) -> impl IntoElement {
    Input::new(text)
        .width(Size::px(126.))
        .a11y_id(a11y_id)
        .trailing(
            rect()
                .horizontal()
                .spacing(2.)
                .child(stepper_icon_button(
                    icons::lucide::chevron_down(),
                    can_decrement,
                    on_decrement,
                ))
                .child(stepper_icon_button(
                    icons::lucide::chevron_up(),
                    can_increment,
                    on_increment,
                )),
        )
        .on_validate(|validator: InputValidator| {
            validator.set_valid(
                validator
                    .text()
                    .chars()
                    .all(|character| character.is_ascii_digit()),
            );
        })
        .on_submit(on_submit)
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
            inner_margin: Some(Gaps::new(6., 0., 6., 0.).into()),
        })
}

fn stepper_icon_button(
    icon: freya::prelude::Bytes,
    enabled: bool,
    on_press: impl Fn(()) + 'static,
) -> impl IntoElement {
    rect()
        .width(Size::px(22.))
        .height(Size::px(30.))
        .center()
        .corner_radius(6.)
        .background(Color::TRANSPARENT)
        .maybe(enabled, |el| {
            el.on_pointer_down(move |event: Event<PointerEventData>| {
                event.stop_propagation();
                event.prevent_default();
                on_press(());
            })
        })
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
    value: u32,
) {
    let mut next = state.read().clone();
    let updated = value.clamp(
        u32::from(MIN_CONCURRENT_DOWNLOADS),
        u32::from(MAX_CONCURRENT_DOWNLOADS),
    ) as u16;
    next.max_concurrent_downloads = updated;
    let queue_max_concurrent = NonZeroU16::new(updated).unwrap_or(NonZeroU16::MAX);
    state.set(next.clone());
    spawn(async move {
        let result = async {
            services
                .queues
                .set_max_concurrent(QueueId::MAIN, queue_max_concurrent)
                .await?;
            services.save_desktop_settings(&next).await
        }
        .await;
        set_save_status(save_status, result);
    });
}

fn save_folder_path(
    services: AppServices,
    mut state: State<DesktopSettings>,
    folder: String,
    mut save_status: State<Option<String>>,
) {
    let candidate = PathBuf::from(folder.trim());
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
