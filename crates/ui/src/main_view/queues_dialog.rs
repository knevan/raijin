use std::num::NonZeroU16;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, NumberInput};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::gpui::*;
use gpui_kit::prelude::FluentBuilder as _;
use raijin_core::download::QueueId;
use raijin_core::queue::QueueSnapshot;

use super::queue_ui::{QueueForm, QueueFormKind};
use crate::bridge::{EngineCommand, EngineHandles, QueueAction};

pub struct QueuesDialog {
    handles: EngineHandles,
    selected_id: QueueId,
    snapshots: Vec<QueueSnapshot>,
    name: Entity<InputState>,
    limit: Entity<InputState>,
    stop_on_empty: bool,
    busy: bool,
    error: Option<String>,
    _subscriptions: [Subscription; 2],
}

impl QueuesDialog {
    pub fn open(
        handles: EngineHandles,
        initial_queue_id: Option<QueueId>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let snapshots = handles.queues.state();
        let selected_id = initial_queue_id
            .filter(|id| snapshots.iter().any(|s| s.queue.id == *id))
            .unwrap_or_else(|| {
                snapshots
                    .first()
                    .map(|s| s.queue.id)
                    .unwrap_or(QueueId::MAIN)
            });
        let initial_snapshot = snapshots.iter().find(|s| s.queue.id == selected_id);
        let initial_name = initial_snapshot
            .map(|s| s.queue.name.clone())
            .unwrap_or_else(|| "Main".to_owned());
        let initial_limit = initial_snapshot
            .map(|s| s.queue.max_concurrent.to_string())
            .unwrap_or_else(|| "2".to_owned());
        let initial_stop_on_empty = initial_snapshot.is_some_and(|s| s.queue.stop_on_empty);

        let name = cx.new(|cx| InputState::new(window, cx).default_value(initial_name));
        let limit = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(initial_limit)
                .step(1.0)
                .min(1.0)
                .max(65535.0)
        });

        let view = cx.new(|cx| {
            let subscriptions = [
                cx.subscribe_in(&name, window, Self::on_name_event),
                cx.subscribe_in(&limit, window, Self::on_limit_event),
            ];

            let mut rx = handles.queues.subscribe();
            let queue_handle = handles.queues.clone();
            cx.spawn_in(window, async move |weak, cx| {
                while let Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) =
                    rx.recv().await
                {
                    let latest = queue_handle.state();
                    let alive = weak
                        .update_in(cx, |this, window, cx| {
                            this.update_snapshots(latest, window, cx);
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
                selected_id,
                snapshots,
                name,
                limit,
                stop_on_empty: initial_stop_on_empty,
                busy: false,
                error: None,
                _subscriptions: subscriptions,
            }
        });

        let dialog_view = view.clone();
        window.open_dialog(cx, move |dialog, window, _cx| {
            let view_height = window.viewport_size().height;
            let dialog_height = px(440.);
            let margin_top = ((view_height - dialog_height) / 2.0).max(px(30.));
            dialog
                .title(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Icon::new(AssetIconName::Layers))
                        .child("Queues"),
                )
                .width(px(720.))
                .margin_top(margin_top)
                .child(dialog_view.clone())
        });

        view
    }

    fn on_name_event(
        &mut self,
        _state: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_id == QueueId::MAIN {
            return;
        }
        match event {
            InputEvent::Blur | InputEvent::PressEnter { .. } => {
                let current_name = self.name.read(cx).value().trim().to_owned();
                let saved_name = self
                    .snapshots
                    .iter()
                    .find(|s| s.queue.id == self.selected_id)
                    .map(|s| s.queue.name.as_str())
                    .unwrap_or_default();
                if current_name.is_empty() {
                    self.error = Some("Queue name cannot be empty.".to_owned());
                    cx.notify();
                } else if current_name != saved_name {
                    self.auto_save(window, cx);
                }
            }
            _ => {}
        }
    }

    fn on_limit_event(
        &mut self,
        _state: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let val_str = self.limit.read(cx).value().trim().to_owned();
                if let Ok(limit_val) = val_str.parse::<NonZeroU16>() {
                    let saved_limit = self
                        .snapshots
                        .iter()
                        .find(|s| s.queue.id == self.selected_id)
                        .map(|s| s.queue.max_concurrent);
                    if Some(limit_val) != saved_limit {
                        self.auto_save(window, cx);
                    }
                }
            }
            InputEvent::Blur => {
                let val_str = self.limit.read(cx).value().trim().to_owned();
                if val_str.parse::<NonZeroU16>().is_err() {
                    self.error =
                        Some("Concurrent downloads must be between 1 and 65535.".to_owned());
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn auto_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.error = Some("Queue name cannot be empty.".to_owned());
            cx.notify();
            return;
        }
        let limit = match self.limit.read(cx).value().trim().parse::<NonZeroU16>() {
            Ok(limit) => limit,
            Err(_) => {
                self.error = Some("Concurrent downloads must be between 1 and 65535.".to_owned());
                cx.notify();
                return;
            }
        };

        self.busy = true;
        self.error = None;
        cx.notify();

        let command = EngineCommand::Queue(QueueAction::Save {
            id: self.selected_id,
            name,
            limit,
            stop_on_empty: self.stop_on_empty,
        });

        let weak = cx.entity().downgrade();
        self.handles
            .run_in(command, window, cx, move |result, _window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.busy = false;
                    match result {
                        Ok(()) => {
                            this.error = None;
                        }
                        Err(err) => {
                            this.error = Some(err.to_string());
                        }
                    }
                    cx.notify();
                });
            });
    }

    fn select_queue(&mut self, id: QueueId, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_id == id {
            return;
        }
        self.selected_id = id;
        self.reload_inputs(window, cx);
    }

    fn reload_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(snapshot) = self
            .snapshots
            .iter()
            .find(|s| s.queue.id == self.selected_id)
        {
            let name_val = snapshot.queue.name.clone();
            let limit_val = snapshot.queue.max_concurrent.to_string();
            let stop_on_empty_val = snapshot.queue.stop_on_empty;
            self.name
                .update(cx, |name, cx| name.set_value(&name_val, window, cx));
            self.limit
                .update(cx, |limit, cx| limit.set_value(&limit_val, window, cx));
            self.stop_on_empty = stop_on_empty_val;
            self.error = None;
        }
        cx.notify();
    }

    fn update_snapshots(
        &mut self,
        snapshots: Vec<QueueSnapshot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.snapshots = snapshots;
        if !self
            .snapshots
            .iter()
            .any(|s| s.queue.id == self.selected_id)
            && let Some(first) = self.snapshots.first()
        {
            self.selected_id = first.queue.id;
            self.reload_inputs(window, cx);
            return;
        }
        let name_focused = self.name.read(cx).focus_handle(cx).is_focused(window);
        let limit_focused = self.limit.read(cx).focus_handle(cx).is_focused(window);
        if let Some(current) = self
            .snapshots
            .iter()
            .find(|s| s.queue.id == self.selected_id)
        {
            if !name_focused {
                self.name.update(cx, |name, cx| {
                    name.set_value(&current.queue.name, window, cx)
                });
            }
            if !limit_focused {
                self.limit.update(cx, |limit, cx| {
                    limit.set_value(current.queue.max_concurrent.to_string(), window, cx)
                });
            }
            if !self.busy {
                self.stop_on_empty = current.queue.stop_on_empty;
            }
        }
        cx.notify();
    }

    fn render_left_column(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_main_selected = self.selected_id == QueueId::MAIN;
        let selected_snapshot = self
            .snapshots
            .iter()
            .find(|s| s.queue.id == self.selected_id)
            .cloned();

        v_flex()
            .w(px(220.))
            .h_full()
            .min_h_0()
            .border_r_1()
            .border_color(cx.theme().border.opacity(0.6))
            .bg(cx.theme().sidebar.opacity(0.3))
            // Scrollable list of queues
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .p_2()
                    .gap(px(2.))
                    .children(self.snapshots.iter().map(|snapshot| {
                        let is_selected = snapshot.queue.id == self.selected_id;
                        let is_active = snapshot.active;
                        let queue_id = snapshot.queue.id;
                        let queue_name = snapshot.queue.name.clone();

                        h_flex()
                            .id(SharedString::from(format!(
                                "queue-list-row-{}",
                                queue_id.get()
                            )))
                            .w_full()
                            .h(px(32.))
                            .px_2()
                            .items_center()
                            .justify_between()
                            .rounded_md()
                            .cursor_pointer()
                            .when(!is_selected, |this| {
                                this.hover(|this| {
                                    this.bg(cx.theme().list_hover)
                                        .text_color(cx.theme().foreground)
                                })
                            })
                            .when(is_selected, |this| {
                                this.font_weight(FontWeight::MEDIUM)
                                    .bg(cx.theme().list_active)
                                    .text_color(cx.theme().foreground)
                            })
                            .child(
                                h_flex()
                                    .flex_1()
                                    .items_center()
                                    .gap_2()
                                    .overflow_hidden()
                                    .child(Icon::new(IconName::Folder).size_4().text_color(
                                        if is_selected {
                                            cx.theme().foreground
                                        } else {
                                            cx.theme().muted_foreground
                                        },
                                    ))
                                    .child(div().text_sm().truncate().child(queue_name)),
                            )
                            .child(div().size(px(7.)).rounded_full().bg(if is_active {
                                cx.theme().success
                            } else {
                                cx.theme().muted_foreground.opacity(0.4)
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_queue(queue_id, window, cx);
                            }))
                    })),
            )
            // Bottom action bar
            .child(
                h_flex()
                    .flex_shrink_0()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(cx.theme().border.opacity(0.6))
                    .items_center()
                    .child(
                        Button::new("add-queue-button")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip("Add Queue")
                            .on_click(cx.listener(|this, _, window, cx| {
                                QueueForm::open(
                                    QueueFormKind::Create,
                                    this.handles.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("delete-queue-button")
                            .ghost()
                            .small()
                            .icon(gpui_kit::assets::IconName::Trash)
                            .disabled(is_main_selected)
                            .tooltip(if is_main_selected {
                                "Cannot delete Main queue"
                            } else {
                                "Delete Queue"
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(snapshot) = selected_snapshot.clone() {
                                    QueueForm::open(
                                        QueueFormKind::Delete(snapshot),
                                        this.handles.clone(),
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    ),
            )
    }

    fn render_right_column(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_main = self.selected_id == QueueId::MAIN;
        let queue_name = if is_main {
            "Main".to_owned()
        } else {
            self.name.read(cx).value().to_string()
        };
        let limit_val = self.limit.read(cx).value().to_string();

        v_flex()
            .flex_1()
            .min_h_0()
            .h_full()
            .overflow_y_scrollbar()
            .p_4()
            .gap_4()
            // Header: Tab pill "Config" with IconName::Settings
            .child(
                h_flex().items_center().gap_2().child(
                    h_flex()
                        .items_center()
                        .gap_1_5()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(cx.theme().muted)
                        .text_color(cx.theme().foreground)
                        .child(
                            Icon::new(IconName::Settings)
                                .size_4()
                                .text_color(cx.theme().primary),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .pr(px(3.))
                                .child("Config"),
                        ),
                ),
            )
            // Card 1 (General)
            .child(
                self.render_card(
                    "General",
                    [
                        self.render_setting_row(
                            "Name",
                            format!("Queue name is {queue_name}"),
                            div()
                                .w(px(160.))
                                .child(Input::new(&self.name).disabled(is_main || self.busy)),
                            cx,
                        ),
                        self.render_setting_row(
                            "Max Concurrent Download",
                            format!("Queue limit is {limit_val}"),
                            div()
                                .w(px(100.))
                                .child(NumberInput::new(&self.limit).disabled(self.busy)),
                            cx,
                        ),
                    ],
                    cx,
                ),
            )
            // Card 2 (On Completion)
            .child(
                self.render_card(
                    "On Completion",
                    [
                        self.render_setting_row(
                            "Automatic Stop",
                            if self.stop_on_empty {
                                "Enabled"
                            } else {
                                "Disabled"
                            },
                            Switch::new("queue-auto-stop")
                                .checked(self.stop_on_empty)
                                .disabled(self.busy)
                                .on_click(cx.listener(|this, checked: &bool, window, cx| {
                                    this.stop_on_empty = *checked;
                                    this.auto_save(window, cx);
                                })),
                            cx,
                        ),
                        self.render_setting_row(
                            "Shutdown System On Completion",
                            "Disabled",
                            Switch::new("queue-shutdown").checked(false).disabled(true),
                            cx,
                        ),
                    ],
                    cx,
                ),
            )
            // Card 3 (Scheduler)
            .child(self.render_card(
                "Scheduler",
                [self.render_setting_row(
                    "Enable Scheduler",
                    "Disabled",
                    Switch::new("queue-scheduler").checked(false).disabled(true),
                    cx,
                )],
                cx,
            ))
    }

    fn render_card(
        &self,
        title: impl Into<SharedString>,
        rows: impl IntoIterator<Item = impl IntoElement>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let title_str = title.into();
        v_flex()
            .w_full()
            .rounded_lg()
            .bg(cx.theme().sidebar.opacity(0.4))
            .border_1()
            .border_color(cx.theme().border.opacity(0.6))
            .p_3()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(title_str),
            )
            .children(rows)
    }

    fn render_setting_row(
        &self,
        title: impl IntoElement,
        subtitle: impl IntoElement,
        control: impl IntoElement,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .py_1()
            .gap_4()
            .child(
                v_flex()
                    .flex_1()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().foreground)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(subtitle),
                    ),
            )
            .child(control)
    }

    fn render_footer(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_active = self
            .snapshots
            .iter()
            .find(|s| s.queue.id == self.selected_id)
            .map(|s| s.active)
            .unwrap_or(false);
        let selected_id = self.selected_id;

        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .pt_3()
            .child(div().flex_1().when_some(self.error.clone(), |this, err| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(err))
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("queue-start-stop")
                            .label(if is_active {
                                "Stop Queue"
                            } else {
                                "Start Queue"
                            })
                            .when(!is_active, |btn| btn.primary())
                            .when(is_active, |btn| btn.outline().secondary())
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let cmd = if is_active {
                                    EngineCommand::Queue(QueueAction::Stop(selected_id))
                                } else {
                                    EngineCommand::Queue(QueueAction::Start(selected_id))
                                };
                                this.handles.dispatch_in(cmd, window, cx);
                            })),
                    )
                    .child(
                        Button::new("queue-dialog-close")
                            .label("Close")
                            .outline()
                            .secondary()
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    ),
            )
    }
}

impl Render for QueuesDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .h(px(380.))
            .w_full()
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .border_1()
                    .border_color(cx.theme().border.opacity(0.6))
                    .rounded_lg()
                    .child(self.render_left_column(window, cx))
                    .child(self.render_right_column(window, cx)),
            )
            .child(self.render_footer(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext};
    use raijin_core::download::QueueId;

    use super::QueuesDialog;

    #[gpui_kit::test]
    fn queues_dialog_should_populate_and_toggle_stop_on_empty(cx: &mut TestAppContext) {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let (manager, queues) = runtime.block_on(crate::bridge::queue_tests::managers(2));
        let (monitor, _task) = {
            let _guard = runtime.enter();
            raijin_core::monitor::DownloadMonitorHandle::spawn(manager.subscribe(), None)
        };
        let handles = crate::bridge::EngineHandles {
            manager,
            queues: queues.clone(),
            monitor,
            runtime: runtime.handle().clone(),
            default_folder: std::env::temp_dir(),
            category_root: std::env::temp_dir(),
            use_category_default: false,
        };
        cx.update(gpui_kit::init);
        let window = cx.update(|cx| {
            crate::add_download::open_add_download_window(cx, &handles, "");
            *cx.windows().last().expect("window")
        });

        let dialog = cx
            .update_window(window, |_, window, cx| {
                let dialog = QueuesDialog::open(handles.clone(), Some(QueueId::MAIN), window, cx);
                window.render_frame(cx);
                assert_eq!(dialog.read(cx).selected_id, QueueId::MAIN);
                assert!(!dialog.read(cx).stop_on_empty);
                // Toggle stop on empty switch
                dialog.update(cx, |this, cx| {
                    this.stop_on_empty = true;
                    this.auto_save(window, cx);
                });
                window.render_frame(cx);
                dialog
            })
            .expect("dialog");

        runtime.block_on(async {
            for _ in 0..100 {
                if queues.list().await.expect("queues")[0].queue.stop_on_empty {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("auto-save did not complete");
        });

        cx.run_until_parked();
        cx.update(|cx| assert!(dialog.read(cx).stop_on_empty));

        runtime.block_on(async {
            assert!(queues.list().await.expect("queues")[0].queue.stop_on_empty);
            queues.shutdown().await.expect("queue shutdown");
            handles.manager.shutdown().await.expect("engine shutdown");
        });
    }
}
