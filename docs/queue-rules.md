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
- Queue views use execution order, not library column sorting. Move up/down operate on one selected item and are disabled during search or multi-selection.
- Deleting a custom queue requires explicit confirmation: pause its members, atomically transfer membership to Main and delete the queue. Keep download records and files; never interpret this transfer as Resume. The deleted queue's schedule is removed with that queue.
- Queue settings preserve the existing schedule, including opaque/invalid legacy JSON; editing basic properties is not schedule editing.
- Report command failures visibly. Forms keep their inputs on errors and reject duplicate submissions while a command is pending.

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
