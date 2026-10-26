# SQLite Database & Migrations

Status: **Active durable store**.

---

## 1. Core Principles

- **SQLite via SQLx**: SQLite is the sole durable source of truth for downloads, parts, settings, and queue states. No ad-hoc JSON sidecar files for download state.
- **WAL Mode**: WAL (Write-Ahead Logging) mode is enabled unconditionally during database initialization.
- **Durability policy**: `synchronous = NORMAL` is an accepted performance trade-off: the database remains consistent in WAL mode, but recent committed transactions across downloads, settings, and queues may be lost after power loss or an OS crash. See [Memory Optimization Investigation](memory-optimization.md) for pool/cache tuning and measurement limits.
- **Atomic Transactions**: Multi-table state updates (e.g. updating download item status alongside part allocation) must execute within a `sqlx::Transaction`.
- **Batched Commits**: Avoid per-chunk database writes. Progress is persisted on time intervals and lifecycle transitions (pause, complete, error).

---

## 2. Migrations Workflow

> [!IMPORTANT]
> **Always use this tool to add a migration:**
> ```bash
> sqlx migrate add <migration_filename>
> ```

### Development Environment Policy:
- Nothing is in production yet. Breaking schema changes during development are completely acceptable.
- In development, you may edit existing migration files directly if needed, and reset the database with:
  ```bash
  sqlx database reset
  ```
- Do not create superfluous migration files for minor trial changes while iterating on local development.

---

## 3. Data Modeling Rules

- Use typed Rust newtypes/enums at domain boundaries; do not pass raw stringly-typed values into business logic.
- `queues.schedule_json` stays as a `TEXT` column (no migration needed); serialize/deserialize via the typed `QueueSchedule` model.
- Store byte counts and speeds as typed 64-bit unsigned integers (`u64`).

