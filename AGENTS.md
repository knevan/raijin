# raijin

Native, multi-platform desktop download manager in Rust inspired by AB Download Manager (ABDM).

Key architectural invariants:

- Native HTTP download engine first (dynamic multi-connection range splitting).
- Aria2-next only as optional future adapter behind engine trait.
- Queue system separate from download engine (basic queue is MVP scope; see `docs/queue-rules.md`).
- Monitor/projection layer separate from engine and UI.
- `gpui-kit` views render projected monitor state and send commands, never mutate engine internals.
- SQLite is the sole durable source of truth (WAL mode). No ad-hoc JSON sidecar files.
- No Freya anywhere in this repo. Old Freya code lives only in the archived `rnd/raijin` reference.

Current phase: **MVP / active development**. Breaking changes during development are fine: to modify a model or schema, edit the migration directly and reset the local database with `sqlx database reset --force`.

---

## Where the rest of the rules live

This file holds only what applies to every task. Detailed rules are split into an `AGENTS.md` per crate, and design decisions/deferred work are recorded in `docs/`. Before touching an area, read its rulebook and relevant documentation:

| When you… | Read |
|---|---|
| read or touch anything under `crates/core/` | `crates/core/AGENTS.md`, then the skills it lists |
| read or touch anything under `crates/ui/` | `crates/ui/AGENTS.md`, then the skills it lists |
| touch the download engine, range splitting, or resume validation | `docs/download-engine.md` |
| add or change a migration, table, or SQLite repository | `docs/database.md` |
| touch queue scheduling or queue models | `docs/queue-rules.md` |
| touch the system tray, window minimize/hide, or autostart lifecycle | `docs/tray.md` and `docs/startup-deferred.md` |
| touch app links, protocol registration, or single-instance forwarding | `docs/app-links.md` |
| touch opening download folders or revealing downloaded files | `docs/file-actions.md` |
| touch download categories or table subtitles | `docs/download-location-labels.md` |
| investigate idle/tray memory or tune runtime, pool, or UI resource usage | `docs/memory-optimization.md` |
| touch the Settings view, desktop configuration, or deferred settings | `docs/settings-deferred.md` |
| touch the local API server (`axum`) or browser extension pairing | `docs/api-and-integration.md` |
| read or touch the browser extension under `browser-extension/` | `docs/api-and-integration.md` |
| bump `gpui-kit` or touch GPUI versioning | `docs/gpui-upgrade.md` |
| check roadmap, milestone status, or overall plan | `plan.md` |
| assess architectural risks, known gaps, or security boundaries | `risk.md` |

---

## Layout

```text
Cargo.toml            Cargo workspace (members: crates/core, crates/ui)
crates/
  core/               UI-free: config, db, download, queue, monitor, integration models, platform
  ui/                 gpui-kit desktop UI: shell, views, bridge, tray
browser-extension/    wxt-based cross-browser extension (Chromium + Firefox)
docs/                 Decisions, architectural invariants, and deferred items
.agents/skills/       Agent skills, pinned in skills-lock.json
plan.md               Milestone tracking and overall roadmap
risk.md               Risk registry and gap analysis
```

---

## Rules for every task

- **Skills on demand**: Load skills (`rust-skills`, `rust-async-patterns`, `rust-best-practices`, `gpui-kit`, `gpui-kit-design-guides`) on demand as needed for the specific task. Do not preload every skill for small changes or targeted fixes to conserve context window.
- **Skills are reference, not repo rules. Every `AGENTS.md` file overrides every skill, always.** Where a skill disagrees with an `AGENTS.md` file, the `AGENTS.md` file wins and that part of the skill is ignored.
- **Design decisions belong in `docs/`**: A design choice made with the user — what was chosen, what was rejected, why, and what was deferred — goes in `docs/`, one file per area, with a row in the routing table above. `AGENTS.md` files say what to do; `docs/` says why.
- **Core stays UI-free**: `crates/core` must never depend on `gpui-kit`, `gpui`, or any windowing crate. It must build and pass tests completely headless (`cargo test -p raijin-core`).
- **Tokio ↔ GPUI Bridge**: GPUI owns the main thread. Never update an `Entity` directly from a Tokio worker. Forward data via channels through `crates/ui/src/bridge.rs` and apply inside `entity.update(cx, ...)` + `cx.notify()`.
- **Never hold locks across `.await`**: Never hold `Mutex` or `RwLock` guards across an `.await` boundary.
- **No unnecessary comments in source**: Clean, self-documenting code. Explanations belong in chat, commit messages, or markdown documentation. Do not leave commented-out code.
- **Commit convention**: Every commit follows `type(scope): subject` (e.g. `feat(engine): dynamic range split verification`).
- **Push back on bad design decisions**: If a request violates `gpui-kit-design-guides`, breaks async safety, or bypasses download validation invariants, say so before or alongside the change: explain the problem, suggest the better alternative, and wait for confirmation.

---

## Verification Rules

- **Targeted testing**: Run only tests directly related to the touched/relevant module or function (the full test can takes long time). Example:
  ```bash
  cargo test -p raijin-core <module/filter_name>
  ```
- **No thread limits**: Do not restrict test threads (avoid `--test-threads=1` unless a specific test explicitly requires it). Let cargo test run multi-threaded by default for fast execution.
- **checks & clippy**:
  - For `crates/core`:
    ```bash
    cargo check -p raijin-core
    cargo clippy --all-targets -p raijin-core -- -D warnings
    ```
  - For `crates/ui`:
    ```bash
    cargo check -p raijin-ui
    cargo clippy --all-targets -p raijin-ui -- -D warnings
    ```

For `crates/core`:
```bash
cargo test -p raijin-core
cargo clippy --all-targets -p raijin-core -- -D warnings
```

For `crates/ui`:
```bash
cargo check -p raijin-ui
cargo clippy --all-targets -p raijin-ui -- -D warnings
```

---

## Do Not Do

- Do not bind GPUI views directly to download engine jobs.
- Do not update `Entity` state from Tokio background workers.
- Do not put queue scheduling logic inside download workers.
- Do not write migration files manually; use `sqlx migrate add <name>`.
- Do not persist download state as JSON sidecars.
- Do not make window-system crates required for core tests.
- Do not introduce aria2-next as default engine.
- Do not silently discard validation errors or restart changed remote files.
- Do not hold locks across `.await`.
