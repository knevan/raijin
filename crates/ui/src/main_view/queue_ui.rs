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
enum QueueFormKind {
    Create,
    Edit(QueueSnapshot),
    Delete(QueueSnapshot),
    Move(Vec<DownloadId>),
}

struct QueueForm {
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
    fn open(
        kind: QueueFormKind,
        handles: EngineHandles,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let (title, commit) = match &kind {
            QueueFormKind::Create => ("New queue".to_owned(), "Create"),
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
                InputState::new(window, cx)
                    .default_value(initial.map(|q| q.name.clone()).unwrap_or_default())
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
        window.open_dialog(cx, move |dialog, _, cx| {
            let submit = dialog_form.clone();
            let busy = dialog_form.read(cx).busy;
            let destructive = matches!(dialog_form.read(cx).kind, QueueFormKind::Delete(_));
            dialog
                .title(title.clone())
                .child(dialog_form.clone())
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|button| button.label("Cancel")))
                        .child(
                            DialogAction::new().child(
                                Button::new("queue-commit")
                                    .label(commit)
                                    .primary()
                                    .when(destructive, |button| button.danger())
                                    .disabled(busy),
                            ),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    submit.update(cx, |form, cx| form.submit(window, cx));
                    false
                })
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
            .when(matches!(self.kind, QueueFormKind::Create | QueueFormKind::Edit(_)), |this| {
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
    fn new_queue(&self, window: &mut Window, cx: &mut App) {
        QueueForm::open(QueueFormKind::Create, self.handles.clone(), window, cx);
    }

    fn edit_queue(&self, delete: bool, window: &mut Window, cx: &mut App) {
        if let Some(snapshot) = self
            .queue_state
            .iter()
            .find(|s| Some(s.queue.id) == self.selected_queue)
        {
            let kind = if delete {
                QueueFormKind::Delete(snapshot.clone())
            } else {
                QueueFormKind::Edit(snapshot.clone())
            };
            QueueForm::open(kind, self.handles.clone(), window, cx);
        }
    }

    pub(super) fn move_to_queue(&self, window: &mut Window, cx: &mut App) {
        let ids = self.target_ids(cx);
        if !ids.is_empty() {
            QueueForm::open(QueueFormKind::Move(ids), self.handles.clone(), window, cx);
        }
    }

    pub(super) fn reorder_selected(&self, up: bool, window: &mut Window, cx: &mut App) {
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

    pub(super) fn render_queue_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self
            .queue_state
            .iter()
            .find(|s| Some(s.queue.id) == self.selected_queue);
        v_flex().gap_2()
            .child(h_flex().gap_2()
                .child(div().flex_1().text_lg().child(self.header_title()))
                .child(Button::new("new-queue").small().label("New queue…")
                    .on_click(cx.listener(|this, _, window, cx| this.new_queue(window, cx)))))
            .when_some(selected, |this, snapshot| {
                let id = snapshot.queue.id;
                let active = snapshot.active;
                let running = self.cache.iter().filter(|r| snapshot.items.contains(&r.id) && matches!(r.status,
                    DownloadStatus::Downloading | DownloadStatus::PreparingFile | DownloadStatus::Retrying)).count();
                let waiting = self.cache.iter().filter(|r| snapshot.items.contains(&r.id) && r.status == DownloadStatus::Queued).count();
                this.child(div().text_sm().text_color(cx.theme().muted_foreground).child(format!(
                    "{} · {running} running · {waiting} waiting · limit {} · execution order",
                    if active { "Active" } else { "Stopped" }, snapshot.queue.max_concurrent,
                )))
                .child(h_flex().gap_2()
                    .child(Button::new("queue-toggle").small().label(if active { "Stop queue" } else { "Start queue" })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let action = if active { QueueAction::Stop(id) } else { QueueAction::Start(id) };
                            this.handles.dispatch_in(EngineCommand::Queue(action), window, cx);
                        })))
                    .child(Button::new("queue-settings").small().label("Queue settings…")
                        .on_click(cx.listener(|this, _, window, cx| this.edit_queue(false, window, cx))))
                    .child(Button::new("queue-delete").small().label("Delete queue…").disabled(id == QueueId::MAIN)
                        .on_click(cx.listener(|this, _, window, cx| this.edit_queue(true, window, cx)))))
            })
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
