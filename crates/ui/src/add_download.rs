use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Sizable as _, TitleBar,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::bridge::{EngineCommand, EngineHandles, build_new_download, is_http_url_str};
use crate::category::{FileCategory, infer_category};
use crate::downloads::category_icon;
use crate::format::format_bytes;

/// Width of the category dropdown, matching the sketch (300).
const CATEGORY_DROP_WIDTH: f32 = 300.0;
/// Size of the square icon buttons (+, reset, gear). Matches the `h_8`
/// (= 32px) that medium `Input`/`Select`/text buttons render at, so the icon
/// buttons sit flush with their row's field. (An explicit `.w()/.h()` on
/// `Button` wins over the size preset, while `.h()` on a single-line `Input`
/// is ignored by the framework — hence no `.h()` on the inputs below.)
const ICON_BUTTON: f32 = 32.0;
/// Upper bound for probe waits; the badge degrades to "unknown" past it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum suggested/sanitized file-name length in chars.
const MAX_FILE_NAME_CHARS: usize = 128;
/// Fallback file name when the URL yields no usable segment.
const FALLBACK_FILE_NAME: &str = "download.bin";

/// Remote-size probe state shown in the category-row badge.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeState {
    /// No URL to probe yet.
    Idle,
    /// A probe task is in flight for the current URL.
    Checking,
    /// Probe answered; size may still be unknown (blind servers).
    Ready { total_bytes: Option<u64> },
    /// Probe failed (offline, 404, HTML page, timeout); download stays allowed.
    Failed,
}

/// Standalone Add Download window state.
pub struct AddDownloadView {
    handles: EngineHandles,
    url: Entity<InputState>,
    folder: Entity<InputState>,
    file: Entity<InputState>,
    categories: Entity<SelectState<SearchableVec<SharedString>>>,
    use_category: bool,
    category_root: PathBuf,
    /// True once the user edits the file name by hand (stops URL auto-fill).
    file_manual: bool,
    /// True once the user picks a category by hand (stops filename inference).
    category_manual: bool,
    /// True once the user edits the folder by hand (stops category auto-fill).
    folder_manual: bool,
    probe: ProbeState,
    /// Last server-suggested file name from the probe (header over final URL).
    probe_name: Option<String>,
    /// Monotonic id; only the newest probe task may write `probe`.
    probe_gen: u64,
    integration: Option<raijin_core::integration::PreparedItem>,
    _pending: Option<std::sync::Arc<tokio::sync::OwnedSemaphorePermit>>,
    probe_url: Option<String>,
    probe_abort: Option<tokio::task::AbortHandle>,
    _subscriptions: [Subscription; 4],
}

impl AddDownloadView {
    /// Creates the view, pre-filling the URL from `initial_url` or clipboard.
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        handles: EngineHandles,
        initial_url: &str,
        integration: Option<raijin_core::integration::PreparedItem>,
        pending: Option<std::sync::Arc<tokio::sync::OwnedSemaphorePermit>>,
    ) -> Self {
        let seed_url = if initial_url.trim().is_empty() {
            clipboard_url(cx).unwrap_or_default()
        } else {
            initial_url.trim().to_owned()
        };
        let url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://example.com/file.zip")
                .default_value(seed_url.as_str())
        });
        let folder = cx.new(|cx| InputState::new(window, cx).placeholder("Download folder"));
        let file = cx.new(|cx| InputState::new(window, cx).placeholder("File name"));
        let labels = category_labels();
        let selected = initial_category(&seed_url);
        let categories = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(labels),
                Some(IndexPath::new(category_position(selected))),
                window,
                cx,
            )
        });
        let subscriptions = [
            cx.subscribe_in(&url, window, Self::on_url_event),
            cx.subscribe_in(&folder, window, Self::on_folder_event),
            cx.subscribe_in(&file, window, Self::on_file_event),
            cx.subscribe_in(&categories, window, Self::on_category_event),
        ];
        let mut view = Self {
            handles,
            url,
            folder,
            file,
            categories,
            use_category: true,
            category_root: PathBuf::new(),
            file_manual: false,
            category_manual: false,
            folder_manual: false,
            probe: ProbeState::Idle,
            probe_name: None,
            probe_gen: 0,
            integration,
            _pending: pending,
            probe_url: None,
            probe_abort: None,
            _subscriptions: subscriptions,
        };
        // Seed category root + checkbox default from engine handles, then run
        // one derivation pass so folder/file/probe match the seeded URL.
        view.category_root = view.handles.category_root.clone();
        view.use_category = view.handles.use_category_default;
        if let Some(item) = &view.integration {
            view.file.update(cx, |state, cx| {
                state.set_value(item.request.file_name.as_str(), window, cx);
            });
            view.probe_name = item
                .source
                .suggested_name
                .as_ref()
                .map(|_| item.request.file_name.clone());
        }
        view.refresh_derived(window, cx);
        view
    }

    /// Current URL text, trimmed.
    fn url_text(&self, cx: &App) -> String {
        self.url.read(cx).value().to_string().trim().to_owned()
    }

    /// True when the form may be submitted (URL valid; folder falls back).
    fn is_valid(&self, cx: &App) -> bool {
        is_http_url_str(&self.url_text(cx))
    }

    /// Currently selected file category from the dropdown value.
    fn selected_category(&self, cx: &App) -> FileCategory {
        let label = self
            .categories
            .read(cx)
            .selected_value()
            .map(|value| value.to_string())
            .unwrap_or_default();
        category_from_label(&label)
    }

    /// Folder the download will use: manual input, else category resolution.
    fn resolved_folder(&self, cx: &App) -> PathBuf {
        let typed = self.folder.read(cx).value().to_string().trim().to_owned();
        if !typed.is_empty() {
            return PathBuf::from(typed);
        }
        resolve_folder(
            self.use_category,
            self.selected_category(cx),
            &self.category_root,
            &self.handles.default_folder,
        )
    }

    /// Re-derives file suggestion, category inference, folder default, and
    /// the size probe after any URL/category/toggle change.
    ///
    /// Programmatic `set_value` calls suppress `Change` events, so this is
    /// the single place that writes derived values without feedback loops.
    fn refresh_derived(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.url_text(cx);
        if self
            .probe_url
            .as_deref()
            .is_some_and(|previous| previous != url)
        {
            self.probe_name = None;
        }
        let url_valid = is_http_url_str(&url);
        // File name follows the URL until the user takes over.
        if !self.file_manual {
            let suggestion = if url_valid {
                self.probe_name
                    .clone()
                    .unwrap_or_else(|| crate::bridge::suggest_filename(&url))
            } else {
                String::new()
            };
            self.file.update(cx, |state, cx| {
                state.set_value(suggestion.as_str(), window, cx);
            });
        }
        // Category follows the file name until the user takes over.
        if !self.category_manual {
            let file_name = self.file.read(cx).value().to_string();
            if !file_name.trim().is_empty() {
                let inferred = infer_category(file_name.trim());
                self.set_category(inferred, window, cx);
            }
        }
        // Folder follows the category toggle until the user takes over.
        if !self.folder_manual {
            let folder = resolve_folder(
                self.use_category,
                self.selected_category(cx),
                &self.category_root,
                &self.handles.default_folder,
            );
            self.folder.update(cx, |state, cx| {
                state.set_value(folder.to_string_lossy().as_ref(), window, cx);
            });
        }
        if self.probe_url.as_deref() != Some(url.as_str()) {
            self.schedule_probe(url, window, cx);
        }
        cx.notify();
    }

    /// Sets the dropdown selection to `category` without marking manual input.
    fn set_category(
        &mut self,
        category: FileCategory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = category_position(category);
        self.categories.update(cx, |state, cx| {
            state.set_selected_index(Some(IndexPath::new(position)), window, cx);
        });
    }

    /// Starts a bounded metadata probe for `url`; stale results are dropped.
    ///
    /// A successful probe also upgrades the file suggestion (header over final
    /// URL) unless the user already typed a name by hand. Field writes need a
    /// live window, so they run through its handle; the badge works windowless.
    fn schedule_probe(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(abort) = self.probe_abort.take() {
            abort.abort();
        }
        self.probe_url = Some(url.clone());
        self.probe_gen += 1;
        let generation = self.probe_gen;
        let hinted_size = self
            .integration
            .as_ref()
            .filter(|item| item.source.link.trim() == url)
            .and_then(|item| item.source.total_bytes);
        if hinted_size.is_some() && self.probe_name.is_some() {
            self.probe = ProbeState::Ready {
                total_bytes: hinted_size,
            };
            return;
        }
        if !is_http_url_str(&url) {
            self.probe = ProbeState::Idle;
            return;
        }
        self.probe = hinted_size.map_or(ProbeState::Checking, |size| ProbeState::Ready {
            total_bytes: Some(size),
        });
        let task = self
            .handles
            .runtime
            .spawn(async move { probe_download_info(&url).await });
        self.probe_abort = Some(task.abort_handle());
        let window_handle = window.window_handle();
        cx.spawn(async move |weak, cx| {
            let probed = match task.await {
                Ok(info) => info,
                Err(error) => {
                    if !error.is_cancelled() {
                        tracing::warn!(%error, "metadata probe task failed");
                    }
                    None
                }
            };
            let info = weak
                .update(cx, |view, cx| {
                    if view.probe_gen != generation {
                        return None;
                    }
                    match probed {
                        Some(info) => {
                            view.probe = ProbeState::Ready {
                                total_bytes: info.total_bytes.or(hinted_size),
                            };
                            cx.notify();
                            Some(info)
                        }
                        None => {
                            view.probe =
                                hinted_size.map_or(ProbeState::Failed, |size| ProbeState::Ready {
                                    total_bytes: Some(size),
                                });
                            cx.notify();
                            None
                        }
                    }
                })
                .ok()
                .flatten();
            let Some(info) = info else {
                return;
            };
            let _ = window_handle.update(cx, |_, window, cx| {
                let _ = weak.update(cx, |view, cx| {
                    if view.probe_gen != generation {
                        return;
                    }
                    view.apply_probe_name(info, window, cx);
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// Applies a probe result to the file/category/folder derivation.
    ///
    /// Programmatic `set_value` calls suppress `Change` events, so manual
    /// flags stay untouched: a probe upgrade never locks the user out.
    fn apply_probe_name(&mut self, info: ProbeInfo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = info.file_name {
            let sanitized = sanitize_filename(&name);
            if !sanitized.is_empty() {
                self.probe_name = Some(sanitized);
            }
        }
        if self.file_manual {
            return;
        }
        let current = self.file.read(cx).value().to_string();
        let Some(upgrade) = select_probe_filename(
            &current,
            self.probe_name.as_deref(),
            &info.final_url,
            &info.typed_url,
        ) else {
            return;
        };
        self.probe_name = Some(upgrade.clone());
        self.file.update(cx, |state, cx| {
            state.set_value(upgrade.as_str(), window, cx);
        });
        if !self.category_manual {
            self.set_category(infer_category(upgrade.trim()), window, cx);
        }
        if !self.folder_manual {
            self.refresh_folder(window, cx);
        }
    }

    fn on_url_event(
        &mut self,
        _state: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => self.refresh_derived(window, cx),
            InputEvent::PressEnter { .. } => self.submit(true, window, cx),
            _ => {}
        }
    }

    fn on_file_event(
        &mut self,
        _state: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                self.file_manual = true;
                // A hand-typed name re-infers the category unless locked.
                if !self.category_manual {
                    let file_name = self.file.read(cx).value().to_string();
                    if !file_name.trim().is_empty() {
                        let inferred = infer_category(file_name.trim());
                        self.set_category(inferred, window, cx);
                        if !self.folder_manual {
                            self.refresh_folder(window, cx);
                        }
                        cx.notify();
                    }
                }
            }
            InputEvent::PressEnter { .. } => self.submit(true, window, cx),
            _ => {}
        }
    }

    fn on_folder_event(
        &mut self,
        _state: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, InputEvent::Change) {
            self.folder_manual = true;
            cx.notify();
        }
    }

    fn on_category_event(
        &mut self,
        _state: &Entity<SelectState<SearchableVec<SharedString>>>,
        event: &SelectEvent<SearchableVec<SharedString>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, SelectEvent::Confirm(_)) {
            self.category_manual = true;
            if !self.folder_manual {
                self.refresh_folder(window, cx);
            }
            cx.notify();
        }
    }

    /// Rewrites the folder field from the current category toggle.
    fn refresh_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let folder = resolve_folder(
            self.use_category,
            self.selected_category(cx),
            &self.category_root,
            &self.handles.default_folder,
        );
        self.folder.update(cx, |state, cx| {
            state.set_value(folder.to_string_lossy().as_ref(), window, cx);
        });
    }

    fn choose_folder(&self, window: &mut Window, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select download folder".into()),
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = selection
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = cx.update(|window, cx| {
                view.update(cx, |view, cx| match result {
                    Ok(Some(paths)) => {
                        let Some(path) = paths.first() else {
                            return;
                        };
                        let Some(path) = path.to_str() else {
                            window.push_notification("Folder path is not valid UTF-8", cx);
                            return;
                        };
                        view.folder_manual = true;
                        view.folder.update(cx, |state, cx| {
                            state.set_value(path, window, cx);
                        });
                        cx.notify();
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "folder picker failed");
                        window.push_notification(format!("Could not select folder: {error}"), cx);
                    }
                })
            });
        })
        .detach();
    }

    /// Pastes a clipboard URL into the URL field, when one exists.
    fn paste_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = clipboard_text(cx) else {
            window.push_notification("Clipboard has no text to paste", cx);
            return;
        };
        let url = text.trim().to_owned();
        self.url.update(cx, |state, cx| {
            state.set_value(url.as_str(), window, cx);
        });
        self.refresh_derived(window, cx);
    }

    /// Submits the form: `autostart` selects Download (resume) vs Add (idle).
    fn submit(&self, autostart: bool, window: &mut Window, cx: &mut App) {
        let url = self.url.read(cx).value().to_string();
        let raw_file = self.file.read(cx).value().to_string();
        let file_name = sanitize_filename(&raw_file);
        let file_name = if file_name.is_empty() {
            self.probe_name
                .as_deref()
                .map(sanitize_filename)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| crate::bridge::suggest_filename(url.trim()))
        } else {
            file_name
        };
        let folder = self.resolved_folder(cx);
        match build_new_download(url.trim(), &file_name, folder) {
            Ok(mut request) => {
                request.category = download_category(self.use_category, self.selected_category(cx));
                preserve_integration_credentials(&mut request, self.integration.as_ref());
                let command = if autostart {
                    EngineCommand::Add(request)
                } else {
                    EngineCommand::AddPaused(request)
                };
                self.handles.dispatch(command);
                window.remove_window();
            }
            Err(error) => window.push_notification(error.to_string(), cx),
        }
    }

    fn render_probe_badge(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let category = self.selected_category(cx);
        match &self.probe {
            ProbeState::Idle => h_flex()
                .gap_2()
                .items_center()
                .child(
                    Icon::new(category_icon(category))
                        .small()
                        .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Size appears here"),
                ),
            ProbeState::Checking => h_flex()
                .gap_2()
                .items_center()
                .child(
                    Icon::new(IconName::LoaderCircle)
                        .small()
                        .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Checking…"),
                ),
            ProbeState::Ready { total_bytes } => {
                let label = total_bytes
                    .map(format_bytes)
                    .unwrap_or_else(|| "Unknown size".to_owned());
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(category_icon(category))
                            .small()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(label),
                    )
                    .child(
                        Icon::new(IconName::Check)
                            .small()
                            .text_color(cx.theme().success),
                    )
            }
            ProbeState::Failed => h_flex()
                .gap_2()
                .items_center()
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .small()
                        .text_color(cx.theme().warning),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Size unknown"),
                ),
        }
    }
}

impl Render for AddDownloadView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let valid = self.is_valid(cx);
        let url_invalid = !self.url_text(cx).is_empty() && !valid;
        // Frame background is the sketch `$panel` (theme `sidebar`), so the
        // default input boxes (`$bg`) read as dark cards like the sketch and
        // the ABDM reference. `background` ($bg) would swallow them whole.
        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Icon::new(IconName::ArrowDown)
                                .small()
                                .text_color(cx.theme().primary),
                        )
                        .child(
                            div()
                                .text_base()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(cx.theme().foreground)
                                .child("Add Download"),
                        ),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .gap_3()
                    .p_5()
                    .child(
                        Input::new(&self.url).suffix(
                            Button::new("add-paste")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Copy)
                                .tooltip("Paste URL from clipboard")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.paste_clipboard(window, cx);
                                })),
                        ),
                    )
                    .when(url_invalid, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child("Enter a valid http(s) URL"),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                Checkbox::new("add-use-category")
                                    .label("Use Category")
                                    .checked(self.use_category)
                                    .on_click(cx.listener(|this, checked, window, cx| {
                                        this.use_category = *checked;
                                        if !this.folder_manual {
                                            this.refresh_folder(window, cx);
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div().w(px(CATEGORY_DROP_WIDTH)).child(
                                    Select::new(&self.categories)
                                        .menu_max_h(px(130.))
                                        .disabled(!self.use_category),
                                ),
                            )
                            .child(
                                Button::new("add-detect-category")
                                    .outline()
                                    .secondary()
                                    .w(px(ICON_BUTTON))
                                    .h(px(ICON_BUTTON))
                                    .icon(IconName::Plus)
                                    .tooltip("Detect category from file name")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let file_name = this.file.read(cx).value().to_string();
                                        if !file_name.trim().is_empty() {
                                            this.category_manual = false;
                                            this.set_category(
                                                infer_category(file_name.trim()),
                                                window,
                                                cx,
                                            );
                                            if !this.folder_manual {
                                                this.refresh_folder(window, cx);
                                            }
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .justify_end()
                                    .child(self.render_probe_badge(cx)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(div().flex_1().child(Input::new(&self.folder)))
                            .child(
                                Button::new("add-folder-reset")
                                    .outline()
                                    .secondary()
                                    .w(px(ICON_BUTTON))
                                    .h(px(ICON_BUTTON))
                                    .icon(IconName::RotateCw)
                                    .tooltip("Reset folder to default")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.folder_manual = false;
                                        this.refresh_folder(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("add-folder-open")
                                    .outline()
                                    .secondary()
                                    .w(px(ICON_BUTTON))
                                    .h(px(ICON_BUTTON))
                                    .icon(IconName::Folder)
                                    .tooltip("Choose download folder…")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.choose_folder(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("add-download-options")
                                    .outline()
                                    .secondary()
                                    .w(px(ICON_BUTTON))
                                    .h(px(ICON_BUTTON))
                                    .icon(IconName::Settings)
                                    .disabled(true)
                                    .tooltip("Download options are not available yet"),
                            ),
                    )
                    .child(Input::new(&self.file))
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                Button::new("add-cancel")
                                    .outline()
                                    .secondary()
                                    .w(px(130.))
                                    .label("Cancel")
                                    .on_click(|_, window, _| {
                                        window.remove_window();
                                    }),
                            )
                            .child(
                                h_flex()
                                    .gap_3()
                                    .items_center()
                                    .child(
                                        Button::new("add-submit-idle")
                                            .outline()
                                            .secondary()
                                            .w(px(130.))
                                            .label("Add paused")
                                            .disabled(!valid)
                                            .tooltip("Add without starting; resume manually")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.submit(false, window, cx);
                                            })),
                                    )
                                    .child(
                                        Button::new("add-submit-start")
                                            .primary()
                                            .w(px(170.))
                                            .label("Download")
                                            .disabled(!valid)
                                            .tooltip("Add and start downloading")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.submit(true, window, cx);
                                            })),
                                    ),
                            ),
                    ),
            )
    }
}

/// Opens the standalone Add Download window.
///
/// Safe to call with no main window open: only engine handles are needed,
/// so tray-only runs and `--add <url>` launches never construct the downloads table.
/// Failures only mean the window could not open and are logged.
pub fn open_add_download_window(cx: &mut App, handles: &EngineHandles, initial_url: &str) {
    if let Err(error) = open_download_window(cx, handles, initial_url, None, None) {
        tracing::error!(%error, "failed to open Add Download window");
    }
}

pub fn open_integration_download_window(
    cx: &mut App,
    handles: &EngineHandles,
    item: raijin_core::integration::PreparedItem,
    pending: Option<std::sync::Arc<tokio::sync::OwnedSemaphorePermit>>,
) -> Result<AnyWindowHandle, String> {
    let url = item.request.url.clone();
    open_download_window(cx, handles, &url, Some(item), pending)
}

fn open_download_window(
    cx: &mut App,
    handles: &EngineHandles,
    initial_url: &str,
    integration: Option<raijin_core::integration::PreparedItem>,
    pending: Option<std::sync::Arc<tokio::sync::OwnedSemaphorePermit>>,
) -> Result<AnyWindowHandle, String> {
    // Icon-button rows sit at the 32px field height (down from 44px), so the
    // frame shrinks by the same 2 x 12px to stay snug.
    let dialog_size = size(px(500.), px(284.));
    let mut options = TitleBar::window_options();
    options.window_bounds = Some(WindowBounds::centered(dialog_size, cx));
    options.window_min_size = Some(dialog_size);
    options.is_resizable = false;
    let focus = pending.is_none();
    options.focus = focus;
    let handles = handles.clone();
    let initial_url = initial_url.to_owned();
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| {
            AddDownloadView::new(
                window,
                cx,
                handles.clone(),
                &initial_url,
                integration,
                pending,
            )
        })
    }) {
        Ok((handle, _)) => {
            if focus {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
            }
            Ok(handle)
        }
        Err(error) => Err(error.to_string()),
    }
}

fn preserve_integration_credentials(
    request: &mut raijin_core::download::NewDownload,
    integration: Option<&raijin_core::integration::PreparedItem>,
) {
    if let Some(item) = integration
        && request.url == item.request.url
    {
        request.headers = item.request.headers.clone();
        request.download_page = item.request.download_page.clone();
    }
}

/// Probe result feeding the size badge plus the file-name suggestion.
struct ProbeInfo {
    /// Probed total size; `None` covers blind servers.
    total_bytes: Option<u64>,
    /// Sanitized server-suggested name (header over final URL), if any.
    file_name: Option<String>,
    /// Final URL after redirects.
    final_url: String,
    /// Typed URL the probe ran for (stale-result guard lives on `probe_gen`).
    typed_url: String,
}

/// Picks a probe-driven file upgrade, if the probe knows better.
///
/// Priority mirrors ABDM: `Content-Disposition` header over the final URL
/// after redirects over the typed URL. Returns `None` when the current value
/// already wins (or is equally good), so callers never churn the field.
fn select_probe_filename(
    current: &str,
    header_name: Option<&str>,
    final_url: &str,
    typed_url: &str,
) -> Option<String> {
    let header = header_name
        .map(sanitize_filename)
        .filter(|name| !name.is_empty());
    if let Some(header) = header {
        if header != current.trim() {
            return Some(header);
        }
        return None;
    }
    if final_url == typed_url {
        return None;
    }
    let suggestion = crate::bridge::suggest_filename(final_url);
    if suggestion != current.trim() && suggestion != crate::bridge::suggest_filename(typed_url) {
        return Some(suggestion);
    }
    None
}

/// Probes `url` for size plus naming hints; `None` covers failures.
///
/// A short timeout keeps the badge from hanging the submit path: the form
/// stays submittable either way because blind servers are still downloadable.
async fn probe_download_info(url: &str) -> Option<ProbeInfo> {
    let client = match raijin_core::download::ReqwestHttpClient::new() {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "metadata probe client initialization failed");
            return None;
        }
    };
    let request = raijin_core::download::ProbeRequest::new(url.to_owned());
    let metadata = match tokio::time::timeout(
        PROBE_TIMEOUT,
        raijin_core::download::probe_http(&client, request),
    )
    .await
    {
        Ok(Ok(metadata)) => metadata,
        Ok(Err(error)) => {
            tracing::debug!(%error, "metadata probe failed");
            return None;
        }
        Err(_) => {
            tracing::debug!("metadata probe timed out");
            return None;
        }
    };
    Some(ProbeInfo {
        total_bytes: metadata.total_bytes.map(|bytes| bytes.get()),
        file_name: metadata.file_name,
        final_url: metadata.url,
        typed_url: url.to_owned(),
    })
}

impl Drop for AddDownloadView {
    fn drop(&mut self) {
        if let Some(abort) = self.probe_abort.take() {
            abort.abort();
        }
    }
}

/// Reads clipboard text, if any, on the GPUI thread.
fn clipboard_text(cx: &App) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

/// Returns the clipboard text when it is an `http(s)` URL.
fn clipboard_url(cx: &App) -> Option<String> {
    clipboard_text(cx).filter(|text| is_http_url_str(text.trim()))
}

/// Dropdown labels in sidebar order.
fn category_labels() -> Vec<SharedString> {
    FileCategory::all()
        .iter()
        .map(|category| SharedString::from(category.label().to_owned()))
        .collect()
}

/// Position of `category` inside [`category_labels`].
fn category_position(category: FileCategory) -> usize {
    FileCategory::all()
        .iter()
        .position(|item| *item == category)
        .unwrap_or(FileCategory::all().len() - 1)
}

/// Maps a dropdown label back to its category; unknown labels are `Other`.
fn category_from_label(label: &str) -> FileCategory {
    FileCategory::all()
        .iter()
        .find(|category| category.label() == label)
        .copied()
        .unwrap_or(FileCategory::Other)
}

/// Initial dropdown selection: inferred from the URL file name, if any.
fn initial_category(url: &str) -> FileCategory {
    if is_http_url_str(url.trim()) {
        infer_category(&crate::bridge::suggest_filename(url.trim()))
    } else {
        FileCategory::Other
    }
}

fn download_category(use_category: bool, category: FileCategory) -> Option<String> {
    use_category.then(|| category.label().to_owned())
}

/// Resolves the destination folder for the form.
///
/// A checked "Use Category" stores into `<root>/<Label>` (sketch example:
/// `…\ABDM\Programs`); unchecked it stores into the default folder. An
/// empty root falls back to the default folder so the form never proposes
/// a bare relative label.
fn resolve_folder(
    use_category: bool,
    category: FileCategory,
    category_root: &Path,
    default_folder: &Path,
) -> PathBuf {
    if use_category && !category_root.as_os_str().is_empty() {
        category_root.join(category.label())
    } else {
        default_folder.to_path_buf()
    }
}

/// Sanitizes a user-typed file name for Windows/macOS/Linux.
///
/// Strips path components, replaces reserved characters and controls with
/// `_`, trims trailing dots/spaces (Windows), and caps length. Returns an
/// empty string when nothing usable remains (caller falls back to the URL
/// suggestion, then to `download.bin`).
fn sanitize_filename(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Keep only the last path component so `a/b.exe` cannot escape the folder.
    let base = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    let mut cleaned: String = base
        .chars()
        .map(|ch| {
            if matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || ch.is_control()
            {
                '_'
            } else {
                ch
            }
        })
        .collect();
    // Windows rejects trailing dots/spaces; strip them everywhere for parity.
    while cleaned.ends_with(['.', ' ']) {
        cleaned.pop();
    }
    if cleaned.is_empty() || matches!(cleaned.as_str(), "." | "..") {
        return String::new();
    }
    let truncated: String = cleaned.chars().take(MAX_FILE_NAME_CHARS).collect();
    let mut truncated = truncated;
    while truncated.ends_with(['.', ' ']) {
        truncated.pop();
    }
    if truncated.is_empty() {
        return String::new();
    }
    if truncated.is_empty() {
        FALLBACK_FILE_NAME.to_owned()
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports: a `use super::*` glob would shadow the built-in
    // `#[test]` attribute with GPUI's `test` macro.
    use std::path::PathBuf;

    use super::{
        category_from_label, category_position, initial_category, resolve_folder,
        sanitize_filename, select_probe_filename,
    };
    use crate::category::FileCategory;

    mod window;

    #[tokio::test]
    async fn metadata_probe_should_resolve_redirect_filename_and_size_without_saving_a_file() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let url = format!("{origin}/download");
        let server = tokio::spawn(async move {
            for response in [
                "HTTP/1.1 302 Found\r\nLocation: /installer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 206 Partial Content\r\nContent-Disposition: attachment; filename=setup.exe\r\nContent-Range: bytes 0-0/123456\r\nContent-Length: 1\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\nx",
            ] {
                let (mut stream, _) = listener.accept().await.expect("connection");
                let mut request = [0; 4096];
                let count = stream.read(&mut request).await.expect("request");
                assert!(count > 0);
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("response");
            }
        });
        let info = super::probe_download_info(&url).await.expect("metadata");
        assert_eq!(info.file_name.as_deref(), Some("setup.exe"));
        assert_eq!(info.total_bytes, Some(123456));
        assert_eq!(info.final_url, format!("{origin}/installer"));
        assert_eq!(info.typed_url, url);
        server.await.expect("server");
    }

    #[test]
    fn integration_credentials_should_only_follow_the_approved_original_url() {
        use raijin_core::integration::{DownloadRequestItem, DownloadType, prepare_item};

        let source = DownloadRequestItem {
            link: "https://example.com/file.zip".to_owned(),
            download_page: Some("https://example.com".to_owned()),
            headers: Some([("Cookie".to_owned(), "session=secret".to_owned())].into()),
            description: None,
            suggested_name: None,
            total_bytes: None,
            r#type: DownloadType::Http,
        };
        let folder = PathBuf::from("C:/Downloads");
        let prepared = prepare_item(&source, &folder).expect("prepared");
        for url in [source.link.as_str(), "https://different.example/file.zip"] {
            let mut request = crate::bridge::build_new_download(url, "file.zip", folder.clone())
                .expect("request");
            super::preserve_integration_credentials(&mut request, Some(&prepared));
            assert_eq!(request.headers.contains_key("Cookie"), url == source.link);
            assert_eq!(request.download_page.is_some(), url == source.link);
        }
    }

    #[test]
    fn sanitize_should_strip_paths_and_reserved_chars() {
        assert_eq!(
            sanitize_filename("  dir/sub\\Antigravity-x64.exe  "),
            "Antigravity-x64.exe"
        );
        // Path separators split components first, so only the tail survives.
        assert_eq!(sanitize_filename("a/b\\g|h?i*j"), "g_h_i_j");
        assert_eq!(sanitize_filename("a<b>c:d\"e|h?i*j"), "a_b_c_d_e_h_i_j");
        assert_eq!(sanitize_filename("trailing...   "), "trailing");
        assert_eq!(sanitize_filename(""), "");
        assert_eq!(sanitize_filename("..."), "");
        assert_eq!(sanitize_filename(".."), "");
    }

    #[test]
    fn sanitize_should_cap_length_without_trailing_dots() {
        let long = format!("{}.exe", "a".repeat(200));
        let cleaned = sanitize_filename(&long);
        assert!(cleaned.chars().count() <= 128, "cleaned was {cleaned}");
        assert!(!cleaned.ends_with(['.', ' ']), "cleaned was {cleaned}");
        assert!(cleaned.starts_with("aaaa"), "cleaned was {cleaned}");
    }

    #[test]
    fn resolve_should_join_category_or_fall_back() {
        let root = PathBuf::from("C:/Users/wahyu/Downloads/ABDM");
        let default = PathBuf::from("C:/Users/wahyu/Downloads/raijin/Downloads");
        assert_eq!(
            resolve_folder(true, FileCategory::Programs, &root, &default),
            PathBuf::from("C:/Users/wahyu/Downloads/ABDM/Programs")
        );
        assert_eq!(
            resolve_folder(false, FileCategory::Programs, &root, &default),
            default
        );
        // Empty root never proposes a bare relative label.
        assert_eq!(
            resolve_folder(
                true,
                FileCategory::Programs,
                PathBuf::new().as_path(),
                &default
            ),
            default
        );
    }

    #[test]
    fn download_category_should_follow_checkbox_and_selection() {
        assert_eq!(
            super::download_category(true, FileCategory::Compressed),
            Some("Compressed".to_owned())
        );
        assert_eq!(
            super::download_category(true, FileCategory::Other),
            Some("Other".to_owned())
        );
        assert_eq!(
            super::download_category(false, FileCategory::Programs),
            None
        );
    }

    #[test]
    fn category_labels_should_round_trip() {
        for category in FileCategory::all() {
            assert_eq!(category_from_label(category.label()), category);
        }
        assert_eq!(category_from_label("Nope"), FileCategory::Other);
        assert_eq!(category_position(FileCategory::Programs), 1);
    }

    #[test]
    fn initial_category_should_infer_exe_and_default_other() {
        assert_eq!(
            initial_category("https://example.com/Antigravity-x64.exe"),
            FileCategory::Programs
        );
        assert_eq!(initial_category(""), FileCategory::Other);
        assert_eq!(initial_category("not a url"), FileCategory::Other);
    }

    #[test]
    fn probe_filename_should_prefer_header_over_urls() {
        assert_eq!(
            select_probe_filename(
                "stat",
                Some("Antigravity IDE.exe"),
                "https://cdn.example.com/files/Antigravity%20IDE.exe",
                "https://example.com/stat?id=1",
            ),
            Some("Antigravity IDE.exe".to_owned())
        );
        // Identical header never churns the field.
        assert_eq!(
            select_probe_filename(
                "Antigravity IDE.exe",
                Some("Antigravity IDE.exe"),
                "https://cdn.example.com/files/Antigravity%20IDE.exe",
                "https://example.com/stat?id=1",
            ),
            None
        );
    }

    #[test]
    fn probe_filename_should_upgrade_from_final_url_after_redirect() {
        assert_eq!(
            select_probe_filename(
                "download.bin",
                None,
                "https://cdn.example.com/files/Antigravity%20IDE.exe",
                "https://example.com/go?id=1",
            ),
            Some("Antigravity IDE.exe".to_owned())
        );
        // No redirect and no header means the typed suggestion already won.
        assert_eq!(
            select_probe_filename(
                "app.zip",
                None,
                "https://example.com/files/app.zip",
                "https://example.com/files/app.zip",
            ),
            None
        );
    }
}
