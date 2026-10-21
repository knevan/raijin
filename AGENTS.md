# Raijin Agent Rulebook

## Project Direction

Raijin is a Rust desktop download manager inspired by AB Download Manager.

Primary architecture:

- Native HTTP download engine first.
- Aria2 only as optional future adapter behind engine trait.
- Queue system separate from download engine (ON HOLD — see Queue Rules).
- Monitor/projection layer separate from engine and UI.
- `gpui-kit` views render projected monitor state and send
  commands, never mutate engine internals.
- SQLite is source of durable state.
- No Freya anywhere in this repo. Old Freya code lives only in the
  archived `rnd/raijin` reference.

## Required References

Before changing architecture, read `plan.md`.

When implementation conflicts with this file, follow this priority:

1. Explicit user instruction in current conversation.
2. `AGENTS.md`.
3. `plan.md`.
4. Existing code conventions.

`plan.md` Section 7 milestones carry a `Source:` line (COPY from old
`rnd/raijin` vs REFERENCE ABDM vs BUILD NEW). Obey it; do not copy Freya
UI code under any circumstance.

## Tools

> [!IMPORTANT]
> Always use these tools to create migration file:
>   - `sqlx migrate add <migration_filename>` (empty file)

Do not create another migration file in development environment just write directly, because always reset database manually with `sqlx database reset`.

## Skills

> [!IMPORTANT]
> Always use these skills:
>   - rust-best-practices
>   - rust-skills
>   - rust-async-patterns
>   - gpui-kit
>   - gpui-kit-design-guides

Use Context7 (`gpui-kit` docs) when touching `gpui-kit` views,
`TableDelegate`, theming, or window lifecycle. There is no Freya skill in
this repo.

## Workspace Rules

Current layout:

```text
Cargo.toml            # workspace
crates/
  core/               # UI-free: config, db, download, queue, monitor,
                      # integration models, platform paths
  ui/                 # gpui-kit shell + component views (depends on core)
  cli/                # optional headless/debug client (depends on core)
```

Rules:

- Core must build and test headless: `cargo test -p core`.
- `core` must never depend on `gpui-kit`, `gpui`, or any window-system
  crate. The compiler enforces this; do not work around it.
- `ui` owns all `gpui-kit` code. `cli` owns flags/subcommands.
- `ui` depends only on `gpui-kit` (single declaration, e.g.
  `gpui-kit = "0.6"`); never list `gpui` / `gpui-base` / `gpui-component`
  separately. Import via `gpui_kit::{*, platform, base, component, assets}`.
- Pin `gpui-kit` (+ tray crate) to an explicit version/commit and commit
  `Cargo.lock`. Never float `main`.
- Binary is named `raijin` (lives in `cli`).

## Module Boundaries

Boundary rules (inside `core`):

- `download/`: engine traits, manager actor, jobs, parts, destination, retry, validation, speed limiting.
- `queue/`: queue manager, queue model, scheduler, queue commands.
- `monitor/`: UI-safe projected state, speed meter, event aggregation.
- `db/`: SQLite models, repositories, migrations, transaction helpers.
- `integration/`: local API request/response models and server glue, UI-free.
- `platform/`: filesystem paths, tray abstraction, autostart, and OS actions
  usable headless where possible.

Boundary rules (inside `ui`):

- `shell/`: GPUI `Application` (via `gpui_kit::application()`), window
  lifecycle, root/titlebar/statusbar. Call `gpui_kit::init(cx)` first
  inside the `run` closure, before any component use.
- `views/`: `DataTable` downloads page, dialogs, settings, sidebar.
- `bridge/`: Tokio channel -> GPUI `Entity` forwarding. The only place that
  touches both runtimes.

## Async Rules

- Use actor-style managers with bounded `mpsc` commands and `oneshot` replies.
- Use `broadcast` or `watch` for event/state publication.
- Use `CancellationToken` for pause/cancel/shutdown.
- Use `JoinSet` or owned task handles for dynamic worker sets.
- Never hold `Mutex` or `RwLock` guards across `.await`.
- Prefer message passing over shared mutable state.
- Do not spawn unbounded tasks; use limits/backpressure.
- Use Tokio tasks for engine, API server, DB/network workers.
- GPUI owns the main thread. Never update an `Entity` from a Tokio worker.
  Pattern: Tokio produces data -> forward via channel -> GPUI foreground
  task applies via `entity.update(cx, ...)` + `cx.notify()`.
- Keep tray event handling + essential timers alive in tray mode; suspend
  table subscriptions, repaints, and animations.

## GPUI Rules

- Views render from projected monitor state and send commands to `core`
  handles. Never reach into job/part internals.
- `DataTable` for the main download list (`TableDelegate`, virtual rows,
  resizable/sortable columns, persisted widths/order).
- `VirtualList` for variable-height lists (queue sidebar, logs, details).
- Wrap `gpui-component` widgets (via `gpui_kit::component`) in local
  components so upstream churn stays behind one boundary.
- Window lifecycle: prefer hiding/suspending the main window for tray
  minimize. Reserve `remove_window()` for the real close/Quit path, where
  Show must rebuild from monitor state + DB.
- Tray backend is `domenkozar/gpui-tray` behind `platform::tray` trait.
  Fallback: `Tryanks/gpui-tray`. Never `Yamrc/gpui-tray` (archived, macOS
  stub). UI code depends on the trait, not the crate.

## Download Engine Rules

- Native HTTP engine is default implementation target.
- Download item state and part state must persist separately.
- Persist progress on interval and on pause/completion, not every chunk.
- Use incomplete destination file and atomic rename on completion.
- Validate content length, ETag, range support, content range, and content type when resuming.
- If remote file changed, pause with validation error instead of silently restarting.
- If range support disappears after ranged state exists, pause and require reset/manual action.
- If total size unknown or range unsupported, use single blind/non-resumable part.
- Keep dynamic + in-flight worker-ack splitting with transactional commits
  and no-gap/no-overlap tests.
- Keep adaptive connections with manual `preferred_connections` override.
- Queue logic must not live inside download jobs.
- UI state must come from monitor/projection, not direct job internals.

## Queue Rules

- Queue/scheduler (M3 in `plan.md`) is ON HOLD until engine (M2), monitor
  (M4), main UI (M5), packaging (M8), and browser extension (M7) are robust.
- Until unheld: downloads run directly via the engine, no scheduling. Do not
  add queue UI (picker, queue pages).
- When unheld: port old `rnd/raijin` `src/queue/` as-is, then add the typed
  `QueueSchedule` boundary + scheduler timer per `plan.md` Section 6.

## SQLite Rules

- Use sqlx with SQLite.
- Enable WAL mode during DB bootstrap.
- Use transactions for multi-table state changes.
- Store durable domain state in SQLite, not ad hoc JSON files.
- `queues.schedule_json` stays a TEXT column (no migration); the API boundary
  is the typed `QueueSchedule` (strict on write, tolerant on read).
- Use typed Rust enums/newtypes at boundaries; avoid stringly typed logic outside DB mapping layer.
- Avoid per-chunk database writes.

## Error Handling Rules

- Use `thiserror` for core/library error enums.
- Use `anyhow` only at binary/application boundary if needed.
- No `unwrap()` in production code.
- `expect()` only for internal invariants that indicate bugs.
- Preserve source error chains.
- Persist last error kind/message for downloads that fail.

## Testing Rules

Minimum verification before marking core work done:

- `cargo test -p core`
- `cargo clippy --all-targets -p core -- -D warnings`

For UI work, also run when feasible:

- `cargo check -p ui`
- `cargo clippy --all-targets -p ui -- -D warnings`

Tests should cover:

- Range splitting.
- Resume validation.
- Duplicate filename strategy.
- Queue max-concurrent behavior (when M3 is unheld).
- Speed limiter behavior.
- Pause/resume persistence with local HTTP test server.
- Schedule next-start/stop with fake clock (when M3 is unheld).

## API Rules

- Local API binds to localhost by default.
- Use token auth for mutating endpoints once API is enabled.
- MVP endpoints are unversioned: `/add`, `/downloads`, `/queues`, `/ping`.
- Add `/api/v1` aliases later only when API becomes stable external contract.
- API must not expose arbitrary file operations.

## Dependency Rules

- Keep core dependencies UI-free.
- Prefer proven crates: tokio, sqlx, reqwest, tokio-util, serde, thiserror, tracing.
- Pinned UI deps: `gpui-kit`, `domenkozar/gpui-tray` (+ `cargo tree` must
  show a single `gpui` version shared by both).
- Never add Freya (or `rfd`-via-Freya) to this repo.
- Do not add aria2 integration until native HTTP MVP and engine trait boundary exist.
- Do not add HLS/ffmpeg before HTTP multi-part engine is stable.
- Do not add proxy/updater/power-actions before MVP milestones land.

## Code Style

- Prefer small, explicit types and newtypes for IDs/bytes/speeds.
- Prefer `&str`/slices in parameters where ownership not needed.
- Prefer enums over strings for domain state.
- Keep abstractions minimal until duplication proves need.
- Comments explain why, not what.
- Public APIs should be documented once stabilized.
- Use `tracing`, not `println!`, outside temporary debugging.

## Do Not Do

- Do not bind GPUI views directly to engine jobs.
- Do not update `Entity` state from Tokio workers.
- Do not put queue scheduling inside download workers.
- Do not implement queue UI while M3 is on hold.
- Do not write migration files manually.
- Do not persist download state as JSON sidecars.
- Do not make window-system crates required for core tests.
- Do not introduce aria2 as default engine.
- Do not use `Yamrc/gpui-tray`.
- Do not silently discard validation errors or restart changed remote files.
- Do not hold locks across `.await`.