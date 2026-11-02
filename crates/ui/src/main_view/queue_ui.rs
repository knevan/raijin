use std::num::NonZeroU16;

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::select::{SearchableVec, Select, SelectState};
use gpui_kit::component::{IndexPath, WindowExt as _};
use raijin_core::download::DownloadId;

use super::*;
use crate::bridge::QueueAction;
use crate::queue_picker::QueueChoice;

#[derive(Clone)]
#[allow(dead_code)]
pub(super) enum QueueFormKind {
    Create,
    Edit(QueueSnapshot),
    Delete(QueueSnapshot),
    Move(Vec<DownloadId>),
}

pub(super) struct QueueForm {
    handles: EngineHandles,
    kind: QueueFormKind,
    name: Entity<InputState>,
    limit: Entity<InputState>,
    destination: Entity<SelectState<SearchableVec<QueueChoice>>>,
    stop_on_empty: bool,
    busy: bool,
    error: Option<String>,
}

impl QueueForm {
    pub(super) fn open(
        kind: QueueFormKind,
        handles: EngineHandles,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let is_create = matches!(kind, QueueFormKind::Create);
        let form_kind = kind.clone();
        let handles_clone = handles.clone();
        let (title, commit) = match &kind {
            QueueFormKind::Create => ("Add New Queue".to_owned(), "Add"),
            QueueFormKind::Edit(s) => (format!("Queue settings: {}", s.queue.name), "Save"),
            QueueFormKind::Delete(s) => (format!("Delete “{}”?", s.queue.name), "Delete"),
            QueueFormKind::Move(ids) => {
                (format!("Move {} download(s) to queue", ids.len()), "Move")
            }
        };
        let form = cx.new(|cx| {
            let choices = handles.queues.state();
            let labels: Vec<_> = choices
                .iter()
                .map(|s| QueueChoice::new(s.queue.id, s.queue.name.clone()))
                .collect();
            let initial = match &kind {
                QueueFormKind::Edit(s) => Some(&s.queue),
                _ => None,
            };
            let name = cx.new(|cx| {
                let mut input = InputState::new(window, cx);
                if is_create {
                    input = input.placeholder("Queue Name");
                } else {
                    input =
                        input.default_value(initial.map(|q| q.name.clone()).unwrap_or_default());
                }
                input
            });
            let limit = cx.new(|cx| {
                InputState::new(window, cx).default_value(
                    initial
                        .map(|q| q.max_concurrent.to_string())
                        .unwrap_or_else(|| "2".to_owned()),
                )
            });
            let stop_on_empty = initial.is_some_and(|q| q.stop_on_empty);
            let destination = cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(labels),
                    (!choices.is_empty()).then_some(IndexPath::new(0)),
                    window,
                    cx,
                )
            });
            QueueForm {
                handles,
                kind,
                name,
                limit,
                destination,
                stop_on_empty,
                busy: false,
                error: None,
            }
        });
        let dialog_form = form.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            let submit = dialog_form.clone();
            let busy = dialog_form.read(cx).busy;
            let destructive = matches!(dialog_form.read(cx).kind, QueueFormKind::Delete(_));
            let commit_btn = Button::new("queue-commit")
                .label(commit)
                .primary()
                .when(destructive, |button| button.danger())
                .disabled(busy);
            let cancel_btn = DialogClose::new().trigger(|button| button.label("Cancel"));

            let delete_btn = match &form_kind {
                QueueFormKind::Edit(s) if s.queue.id != QueueId::MAIN => {
                    let s = s.clone();
                    let handles = handles_clone.clone();
                    Some(
                        Button::new("queue-delete-trigger")
                            .label("Delete queue…")
                            .danger()
                            .ghost()
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                QueueForm::open(
                                    QueueFormKind::Delete(s.clone()),
                                    handles.clone(),
                                    window,
                                    cx,
                                );
                            }),
                    )
                }
                _ => None,
            };

            let footer = if is_create {
                DialogFooter::new()
                    .child(DialogAction::new().child(commit_btn))
                    .child(cancel_btn)
            } else {
                DialogFooter::new()
                    .when_some(delete_btn, |footer, btn| footer.child(btn))
                    .child(cancel_btn)
                    .child(DialogAction::new().child(commit_btn))
            };

            let mut dialog = dialog
                .title(title.clone())
                .child(dialog_form.clone())
                .footer(footer)
                .on_ok(move |_, window, cx| {
                    submit.update(cx, |form, cx| form.submit(window, cx));
                    false
                });

            if is_create {
                let view_height = window.viewport_size().height;
                let estimated_dialog_height = px(150.);
                let margin_top = ((view_height - estimated_dialog_height) / 2.0).max(px(40.));
                dialog = dialog.width(px(360.)).margin_top(margin_top);
            }

            dialog
        });
        form
    }

    fn command(&self, cx: &App) -> Result<EngineCommand, String> {
        let action = match &self.kind {
            QueueFormKind::Create | QueueFormKind::Edit(_) => {
                let name = self.name.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    return Err("Enter a queue name.".to_owned());
                }
                match &self.kind {
                    QueueFormKind::Edit(s) => {
                        let limit = self
                            .limit
                            .read(cx)
                            .value()
                            .trim()
                            .parse::<NonZeroU16>()
                            .map_err(|_| {
                                "Concurrent downloads must be between 1 and 65535.".to_owned()
                            })?;
                        QueueAction::Save {
                            id: s.queue.id,
                            name,
                            limit,
                            stop_on_empty: self.stop_on_empty,
                        }
                    }
                    _ => QueueAction::Create(name),
                }
            }
            QueueFormKind::Delete(s) => QueueAction::Delete(s.queue.id),
            QueueFormKind::Move(ids) => {
                let queue_id = self
                    .destination
                    .read(cx)
                    .selected_value()
                    .copied()
                    .ok_or_else(|| "Choose a destination queue.".to_owned())?;
                return Ok(EngineCommand::MoveToQueue {
                    queue_id,
                    downloads: ids.clone(),
                });
            }
        };
        Ok(EngineCommand::Queue(action))
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let command = match self.command(cx) {
            Ok(command) => command,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        let weak = cx.entity().downgrade();
        self.handles
            .run_in(command, window, cx, move |result, window, cx| {
                let _ = weak.update(cx, |form, cx| {
                    form.busy = false;
                    match result {
                        Ok(()) => window.close_dialog(cx),
                        Err(error) => form.error = Some(error.to_string()),
                    }
                    cx.notify();
                });
            });
    }
}

impl Render for QueueForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().gap_3()
            .when(matches!(self.kind, QueueFormKind::Create), |this| {
                this.child(Input::new(&self.name).disabled(self.busy))
            })
            .when(matches!(self.kind, QueueFormKind::Edit(_)), |this| {
                this.child("Name").child(Input::new(&self.name).disabled(self.busy))
            })
            .when(matches!(self.kind, QueueFormKind::Edit(_)), |this| {
                this.child("Concurrent downloads")
                    .child(Input::new(&self.limit).disabled(self.busy))
                    .child(div().text_sm().text_color(cx.theme().muted_foreground)
                        .child("The global download limit also applies. Lowering this limit lets running downloads finish."))
                    .child(Checkbox::new("queue-stop-empty").label("Stop when empty")
                        .checked(self.stop_on_empty).disabled(self.busy)
                        .on_click(cx.listener(|this, checked, _, cx| {
                            this.stop_on_empty = *checked;
                            cx.notify();
                        })))
            })
            .when(matches!(self.kind, QueueFormKind::Delete(_)), |this| {
                this.child("Downloads will be paused and moved to Main. Download records and files will be kept. The queue's schedule will be removed.")
            })
            .when(matches!(self.kind, QueueFormKind::Move(_)), |this| {
                this.child("Destination queue")
                    .child(Select::new(&self.destination).disabled(self.busy))
                    .child(div().text_sm().text_color(cx.theme().muted_foreground)
                        .child("Pause running downloads first. Moving does not resume paused downloads or move files."))
            })
            .when(self.busy, |this| this.child("Applying changes…"))
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
    }
}

impl MainView {
    pub(super) fn new_queue(&self, window: &mut Window, cx: &mut App) {
        QueueForm::open(QueueFormKind::Create, self.handles.clone(), window, cx);
    }

    pub(super) fn move_to_queue(&self, window: &mut Window, cx: &mut App) {
        let ids = self.target_ids(cx);
        if !ids.is_empty() {
            QueueForm::open(QueueFormKind::Move(ids), self.handles.clone(), window, cx);
        }
    }

    pub(super) fn reorder_selected(&self, up: bool, window: &mut Window, cx: &mut App) {
        if !self.query.trim().is_empty() || self.status != StatusFilter::All {
            return;
        }
        let Some(id) = self.selected_id(cx) else {
            return;
        };
        let Some(snapshot) = self
            .queue_state
            .iter()
            .find(|s| Some(s.queue.id) == self.selected_queue)
        else {
            return;
        };
        let Some(order) = moved_order(&snapshot.items, id, up) else {
            return;
        };
        self.handles.dispatch_in(
            EngineCommand::Queue(QueueAction::Reorder {
                id: snapshot.queue.id,
                order,
            }),
            window,
            cx,
        );
    }
}

fn moved_order(items: &[DownloadId], id: DownloadId, up: bool) -> Option<Vec<DownloadId>> {
    let ix = items.iter().position(|item| *item == id)?;
    let target = if up {
        ix.checked_sub(1)?
    } else {
        ix.checked_add(1).filter(|next| *next < items.len())?
    };
    let mut order = items.to_vec();
    order.swap(ix, target);
    Some(order)
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext};

    use super::{DownloadId, QueueForm, QueueFormKind, moved_order};

    #[gpui_kit::test]
    fn create_dialog_should_validate_and_submit_once(cx: &mut TestAppContext) {
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
        let form = cx
            .update_window(window, |_, window, cx| {
                let form = QueueForm::open(QueueFormKind::Create, handles.clone(), window, cx);
                window.render_frame(cx);
                window.click("queue-commit", cx);
                assert!(form.read(cx).error.is_some());
                let name = form.read(cx).name.clone();
                name.update(cx, |state, cx| state.set_value("Night", window, cx));
                window.render_frame(cx);
                window.click("queue-commit", cx);
                assert!(form.read(cx).busy);
                window.click("queue-commit", cx);
                form
            })
            .expect("dialog");
        runtime.block_on(async {
            for _ in 0..100 {
                if queues.list().await.expect("queues").len() == 2 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("create did not complete");
        });
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(form.read(cx).name.read(cx).value().as_ref(), "Night"));
        runtime.block_on(async {
            assert_eq!(queues.list().await.expect("queues").len(), 2);
            queues.shutdown().await.expect("queue shutdown");
            handles.manager.shutdown().await.expect("engine shutdown");
        });
    }

    #[gpui_kit::test]
    fn edit_dialog_should_populate_and_save(cx: &mut TestAppContext) {
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
        let initial_snapshot = handles.queues.state().first().cloned().expect("main queue");
        let _form = cx
            .update_window(window, |_, window, cx| {
                let form = QueueForm::open(
                    QueueFormKind::Edit(initial_snapshot),
                    handles.clone(),
                    window,
                    cx,
                );
                window.render_frame(cx);
                assert_eq!(form.read(cx).limit.read(cx).value().as_ref(), "2");
                let limit = form.read(cx).limit.clone();
                limit.update(cx, |state, cx| state.set_value("4", window, cx));
                window.render_frame(cx);
                window.click("queue-commit", cx);
                assert!(form.read(cx).busy);
                form
            })
            .expect("dialog");
        runtime.block_on(async {
            for _ in 0..100 {
                if queues.list().await.expect("queues")[0]
                    .queue
                    .max_concurrent
                    .get()
                    == 4
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("edit did not complete");
        });
        cx.run_until_parked();
        runtime.block_on(async {
            assert_eq!(
                queues.list().await.expect("queues")[0]
                    .queue
                    .max_concurrent
                    .get(),
                4
            );
            queues.shutdown().await.expect("queue shutdown");
            handles.manager.shutdown().await.expect("engine shutdown");
        });
    }

    #[test]
    fn reorder_should_preserve_members_and_handle_boundaries() {
        let ids = [DownloadId::new(1), DownloadId::new(2), DownloadId::new(3)];
        assert_eq!(
            moved_order(&ids, ids[1], true),
            Some(vec![ids[1], ids[0], ids[2]])
        );
        assert_eq!(moved_order(&ids, ids[0], true), None);
        assert_eq!(moved_order(&ids, ids[2], false), None);
        assert_eq!(moved_order(&[], ids[0], false), None);
    }
}
