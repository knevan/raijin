# core

Rules for `crates/core`. The root `AGENTS.md` applies too.

`crates/core` is the headless domain engine of Raijin. It manages downloads, local database persistence, background actors, system integrations, and state projections.

## Skills — On-Demand Reference

Load the following skills on demand when relevant to the task:
- `rust-async-patterns`: when designing or refactoring concurrency flows, Tokio actors, channels, or cancellation.
- `rust-best-practices`: when reviewing ownership, borrowing, or idiomatic error handling.
- `rust-skills`: deep reference for Rust patterns and rules.

**Skills are reference material, not repo rules.** Where a skill contradicts, narrows, or differs from an `AGENTS.md` file or `docs/`, the repository rules are authoritative and win.

---

## Headless Invariant

- **Zero UI Dependencies**: `crates/core` must never depend on `gpui-kit`, `gpui`, or any window-system crate. The compiler enforces this; do not work around it.
- **Verification**:
  - Run only touched or relevant unit tests (the full test can takes long time):
    ```bash
    cargo test -p raijin-core <module/filter_name>
    ```
  - Do not restrict threads (avoid `--test-threads=1`) for fast parallel execution.
  - Compilation check & clippy:
    ```bash
    cargo check -p raijin-core
    cargo clippy --all-targets -p raijin-core -- -D warnings
    ```

---

## Module Boundaries

- `download/`: Engine traits, manager actor, jobs, parts, file allocation, retry, validation, speed limiting (see `docs/download-engine.md`).
- `queue/`: Queue manager, scheduler, queue models, and commands (ON HOLD — see `docs/queue-rules.md`).
- `monitor/`: UI-safe projected state, speed meters, event aggregation. UI views consume monitor state, never direct engine internals.
- `db/`: SQLite models, repositories, migrations, transaction helpers (see `docs/database.md`).
- `integration/`: Local API (`axum`) request/response models and server glue, completely UI-free (see `docs/api-and-integration.md`).
- `platform/`: Filesystem paths, tray abstraction trait, autostart management, and OS actions usable headless where possible.

---

## Async Architecture & Tokio Patterns

- **Actor Pattern**: Managers use actor-style architectures with bounded `tokio::sync::mpsc` for commands and `tokio::sync::oneshot` for replies.
- **State Publication**: Use `tokio::sync::broadcast` or `watch` for event and state distribution.
- **Cancellation**: Use `tokio_util::sync::CancellationToken` for deterministic pause, cancellation, and shutdown.
- **Task Management**: Use `tokio::task::JoinSet` or owned task handles for dynamic worker pools. Never spawn unbounded tasks without backpressure.
- **No Locks Across Await**: **NEVER hold `std::sync::Mutex` or `parking_lot::Mutex` guards across an `.await` point.** Prefer message passing or Tokio async mutexes where unavoidable.

---

## Error Handling & Code Quality

- **Error Types**: Use `thiserror` for all core library error enums. `anyhow` is reserved only for top-level application boundaries.
- **No Unwraps**: No `unwrap()` in production code. `expect()` is permitted only for verified internal invariants that represent unrecoverable bugs.
- **Preserve Error Chains**: Always wrap underlying errors using `#[source]` or `#[from]` attributes.
- **Failure Persistence**: Persist the last error kind and user-friendly error message in SQLite for downloads that fail.
- **Domain Modeling**: Use small, explicit types and newtypes for IDs, byte counts, and transfer speeds. Prefer typed Rust enums over raw strings for domain states.
- **Observability**: Use `tracing` macros (`trace!`, `debug!`, `info!`, `warn!`, `error!`), never `println!`.
