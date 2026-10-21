# Raijin — native download manager (Rust port)

Workspace layout (`AGENTS.md` authoritative, `plan.md` milestones):

```text
Cargo.toml            # workspace
crates/
  core/               # UI-free: config, db, download, queue model, monitor, integration stubs, platform
  ui/                 # GPUI shell + views (depends on core)
```

## Commands

```powershell
# M0-M2 headless core (canonical; plan.md `core` = `raijin-core`)
cargo test -p raijin-core
cargo clippy --all-targets -p raijin-core -- -D warnings

# UI shell (plan.md `ui` = `raijin-ui`)
cargo check -p raijin-ui
cargo run -p raijin-ui   # opens empty shell (M0 acceptance, needs display)

# SQLite migrations (created only via `sqlx migrate add -r`)
$env:DATABASE_URL = "sqlite:C:/path/to/raijin.sqlite"
sqlx migrate run --source crates/core/migrations
sqlx migrate revert --source crates/core/migrations
```

## Environment overrides

- `RAIJIN_DATA_DIR`, `RAIJIN_DOWNLOAD_DIR`, `RAIJIN_DOWNLOAD_ROOT`, `RAIJIN_DATABASE_URL`.
- Logging via `RUST_LOG` (`raijin_core::init_tracing()`).

## Milestone scope

- M0 done: workspace, toolchain/fmt, tracing, platform dirs, empty GPUI shell.
- M1 done: SQLite schema + WAL + repositories + domain newtypes/enums + duplicate-name strategy.
- M2 done: native HTTP engine (manager actor, probe/validation, single + ranged jobs,
  dynamic + in-flight split, adaptive connections, retry, speed limiter).
- M3 ON HOLD per `AGENTS.md`: no queue manager/scheduler or queue UI.
- M4 done: monitor projection (`watch` feed, speed meter, ETA), layered
  global + per-download limiter, disk/unwritable checks persisted as
  `Disk`/`NoSpace` errors, `reset` recovery for validation errors.
- M6 tray: native icon integrated via `gpui-tray 0.1.3` (`features = ["gpui-kit"]`),
  keeping `Cargo.lock` on a single `gpui-pre` runtime. Run
  `cargo run -p raijin-ui -- --minimized` to start windowless in the tray;
  left-click shows, right-click opens the menu.
