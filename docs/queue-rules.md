# Queue & Scheduler Rules

Status: **Basic queue is active MVP scope**. This supersedes the earlier Queue UI hold and hide-UI decision.

## Product model

- All/Finished/Unfinished and categories are library filters across queues.
- A queue owns execution policy: membership, execution order, concurrency, activation, and optional schedules. Library views and queue views can show the same download.
- Main is the default queue (`QueueId::MAIN`, id `0`) and cannot be deleted. New custom queues start stopped.
- Basic queue must be usable in MVP; do not wait for packaging or extension acceptance to expose it.

## MVP workflow

- Sidebar navigation, create, rename/settings, delete, start/stop, per-queue concurrency, and stop-on-empty.
- Add Download has a queue picker defaulting to Main. Add paused stays paused; Add to queue waits for activation and an available slot.
- A visible, keyboard-accessible Move to queue command is the primary membership workflow. Drag-and-drop is only a shortcut from a download row onto a sidebar queue.
- Move changes membership only: it does not move files, change categories, or resume paused/Added downloads. Reject the entire batch before mutation if a member is missing, finished, or still has a live worker. Users must pause running downloads first.
- Queue views use execution order, not library column sorting. Priority reordering operates on the selected queue via full-row drag-and-drop (with a translucent full-row insertion preview) and keyboard shortcuts (Alt+Up / Alt+Down). Reordering is disabled when a search query is active or status filters are applied. Conditional Move Up/Down buttons in the toolbar were removed to prevent toolbar layout shifts. A dedicated grip handle column (IconName::GripVertical) was evaluated and deferred: full-row drag avoids horizontal table column jumps and unifies with the existing drag-to-sidebar interaction.
- Deleting a custom queue requires explicit confirmation: pause its members, atomically transfer membership to Main and delete the queue. Keep download records and files; never interpret this transfer as Resume. The deleted queue's schedule is removed with that queue.
- Queue settings preserve the existing schedule, including opaque/invalid legacy JSON; editing basic properties is not schedule editing.
- Report command failures visibly. Forms keep their inputs on errors and reject duplicate submissions while a command is pending.

## Reorder presentation

The compact floating drag card remains separate from the insertion preview. During table drag, the dragged row is shown translucently at its proposed position and neighboring rows move immediately, without sliding. Multi-selection forms a block in existing queue order. Hover changes only UI presentation; drop sends one reorder command. Escape, release outside the table, and a sidebar membership drop discard the reorder preview. Progress snapshots preserve the preview; authoritative membership/order changes invalidate it rather than committing a stale item list. Command rejection restores the previous presentation and reports the error.

Sliding is deferred by explicit product choice. In the installed `gpui-component 0.8` / `gpui-pre 0.3.9`, `TableDelegate::render_tr` returns a `Stateful<Div>` that `UniformList` lays out as a root. Root relative insets do not move the rendered row, so applying `.top(offset)` does not animate it. Sliding the complete row would require a component seam change that separates the fixed slot from the row content. Dependency patches, custom table replacement, and content-only animation were not introduced. Insertion hit testing uses the scroll handle's measured viewport and row stride, not the nominal table size, because root margins are excluded from row measurement.

## Execution contract

- The queue actor is the only admission path that calls engine start. UI and integration code must not start workers directly.
- Global and per-queue concurrency limits both apply. Reducing a limit does not interrupt existing workers; it blocks new admission until capacity is available.
- Resume marks a download ready but never activates its stopped queue.
- Stop queue pauses running members. Start queue resumes those queue-suspended members in the same process, but does not resume independently paused downloads. Downloads still waiting remain queued.
- Activation and the queue-suspended set are runtime state, not durable settings. On process restart Main becomes active, custom queues remain stopped, and paused downloads remain paused. Membership, order, properties, and schedules persist in SQLite.
- Completed downloads leave queue membership and remain visible in the library. Queue counts describe the work list, not completed history.
- Queue logic stays out of download jobs/workers. UI renders projected monitor/queue snapshots and sends commands through the bridge.

## Delivery order and deferred scope

1. Complete and verify basic queue workflow and admission safety.
2. Add an injectable clock and fake-clock scheduler execution acceptance.
3. Expose weekly schedule editing and next-run only after those tests are verified.

Existing weekly schedules and the 10s ticker remain intact. Basic Queue UI does not silently disable or rewrite them. Weekly schedule editor/next-run, one-shot schedules, per-download overrides, and completion power actions are not part of this basic queue delivery; power actions remain M9.

Rejected alternatives: hiding an essential download-manager feature until after MVP, treating a queue as a file category, drag-only membership management, live worker ownership transfer for the initial UI, and rewriting the existing queue actor.
