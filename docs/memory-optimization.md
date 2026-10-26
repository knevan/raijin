# Memory Optimization Investigation

Status: **Active optimization; Tokio 4-worker limit, SQLite tuning, Arc monitor sharing, and tray polling lifecycle fix implemented.**

## Goal and observed behavior

The primary target is approximately **20 MB in idle tray mode after normal UI use**, with no active downloads. A similar main-window footprint is a secondary, best-effort goal. Neither target is a measured feasibility claim or a hard cap during active multi-connection downloads.

User-reported Windows Task Manager **Processes > Memory** observations, with one completed history item:

| Scenario | Approximate memory |
| --- | --- |
| Fresh process launched directly to tray | ~25 MB |
| Initial UI opening peak | ~40-50 MB |
| Main window after settling | ~33 MB |
| Tray after UI use, short wait | ~30 MB |
| Tray extended idle (~20-30 mins) | ~2.98 MB private working set (Commit: 85.6 MB, Working set total: 62.5 MB) via Windows automatic working set trimming |

Earlier tray estimates were 30 MB and 40 MB depending on the interaction sequence. These are informal observations, not a controlled benchmark. The launch command provided was `cargo run -p raijin-ui -- --minimized`, which defaults to the dev profile. The profile of the latest observations has not been confirmed; do not label them deploy measurements.

A stable elevated footprint does not establish a leak. Possible causes include reusable framework caches, live resources, and freed heap retained by the allocator. Continued growth across repeated cycles requires a different investigation from a stable plateau. With only one item, history duplication is a lower-priority explanation for this baseline, not a proven non-factor.

## Measurement protocol before tuning

Use the same deploy executable, database, integration setting, window dimensions, display scaling, and workload throughout a comparison. Do not reset the database or remove history to improve the result.

```powershell
cargo build --profile deploy -p raijin-ui
.\target\deploy\raijin.exe --minimized
```

Quit any existing instance first; single-instance forwarding would otherwise leave the old executable running. If a custom target directory is configured, use its actual deploy output path. Record executable path and version/commit.

1. Cold-start directly to tray, without opening any window.
2. Open the main window, record the transient peak, then close to tray.
3. Repeat main-window open/close ten times, recording the settled tray value each time.
4. Repeat separately for Settings and Add Download, then for a mixed sequence.
5. Use a consistent settle interval (for example, 60 seconds); record peaks separately from settled samples. Repeat the baseline run to establish normal variation.
6. Record Task Manager Memory, private working set, Private Bytes/commit, thread count, and handle count. `Get-Process WorkingSet64` is total working set, not the same metric as the Processes tab's Memory column. Private Bytes is not resident RAM. Record GPU memory separately if relevant; do not add it to the Task Manager figure as though it were the same metric.
7. Capture VMMap snapshots at cold tray and settled warm tray. Inspect heap, private data, stacks, mapped/image memory, and their committed versus resident sizes. Use allocation tracing if heap ownership remains unclear; VMMap alone does not identify the owning Rust object or prove a graphics-resource leak.

## Prioritized candidates

### 1. UI resource lifetime and retained allocations

**Scope:** `crates/ui/src/main.rs`, `main_view.rs`, `settings.rs`, and `add_download.rs`; GPUI platform resources if profiling points upstream.

- Determine which window-owned entities, subscriptions, tasks, and native resources actually drop after close. Investigate retained allocations with evidence rather than assuming every warm cache is a leak.
- Settings stores an `AnyWindowHandle` and reuses a live window; a failed update causes reopening. A stored window handle is not by itself evidence that it strongly retains the view. Verify ownership/drop behavior before changing it.
- Check GPUI font/glyph, icon/image, and graphics caches only if measurements implicate them. Do not assume window removal destroys application-global caches or releases every driver allocation.
- The current Windows `hide_window_to_tray` path programmatically removes windows and rebuilds on show; macOS uses application hide. Therefore, "destroy the Windows window" is not a new optimization candidate.
- **Constraint:** current Windows behavior conflicts with the hide/suspend requirements in `crates/ui/AGENTS.md` and `docs/tray.md`. This note records the discrepancy; it does not authorize lifecycle changes or amend those requirements. Resolve the policy/upstream capability question with the user before changing lifecycle semantics.

### 2. Background polling ownership and tray summary (Implemented)

**Scope:** `crates/ui/src/main_view.rs`, `main.rs`, and `crates/core/src/monitor/manager.rs`.

- Lifted hidden tray activity updates into `spawn_tray_sync` in `main.rs` (1s timer), ensuring the native tray icon stays accurate even under fresh windowless startup (`--minimized` or `--startup`) without depending on a live `MainView`.
- Corrected the `MainView` polling loop so that it detects view/window teardown (`weak.update(cx, ...).is_err()`) and terminates immediately via `break` instead of looping forever via `continue`. Prevents background task accumulation across show/hide cycles.

### 3. Tokio worker count (Implemented: 4 workers)

**Scope:** runtime construction in `crates/ui/src/main.rs`.

- Set explicit `.worker_threads(4)` on `tokio::runtime::Builder::new_multi_thread()`.
- Tokio async I/O utilizes non-blocking completion ports (IOCP on Windows) where 4 threads comfortably saturate multi-gigabit bandwidth while eliminating redundant OS thread stacks (previously 12+ threads on typical 12-core host CPUs).
- UI responsiveness is unaffected because GPUI runs on the main OS thread with its own independent executor.

### 4. SQLite connections and caches (Implemented: 4 conns, 30s idle, memory pragmas)

**Scope:** `crates/core/src/db/mod.rs`; see [SQLite Database & Migrations](database.md).

- Pool limits: lowered `max_connections(4)`, added `min_connections(0)` lazy connection, and `idle_timeout(Duration::from_secs(30))` to reclaim connections when idle in tray.
- Connection pragmas:
  - `synchronous(SqliteSynchronous::Normal)`: safe in WAL mode while eliminating redundant double-fsync delays.
  - `busy_timeout(Duration::from_secs(5))`: prevents transient `SQLITE_BUSY` contention across concurrent workers.
  - `cache_size = -1024`: caps page cache memory at ~1 MB per connection.
  - `temp_store = MEMORY`: handles temporary sort/filter operations in RAM without temporary disk file churn.

### 5. Snapshot and presentation duplication (Implemented: Arc sharing)

**Scope:** `crates/core/src/monitor/manager.rs`, `monitor/state.rs`, and `crates/ui/src/main_view.rs` / `downloads.rs`.

- Switched `DownloadMonitorHandle::state()` and monitor watch channel to publish immutable `Arc<MonitorState>`.
- Read operations (`handle.state()`) clone only the lightweight 8-byte pointer instead of performing deep copies of nested `BTreeMap` structures every 500 ms.
- `MainView` diffing now checks pointer identity (`Arc::ptr_eq`) before value equality, bypassing deep equality comparisons when the snapshot is identical.

## Deferred or rejected shortcuts

- No periodic `EmptyWorkingSet`, forced paging, or equivalent working-set trimming to make Task Manager look smaller. It does not remove live allocations and can hurt restore latency.
- No allocator replacement, forced cache flushing, native-window rewrite, or process split without profiling evidence and a separate trade-off decision.
- No assumed RAM savings from executable stripping or `opt-level = "z"`. The deploy profile already uses Thin LTO, one codegen unit, and abort-on-panic; binary size and resident memory are different measurements.
- No disabling integration, tray functionality, persistence, or download safety just to meet the target.

## Acceptance and next action

Start with controlled deploy measurements and cold-versus-warm VMMap comparison. Then test one narrowly scoped candidate at a time; keep only changes whose benefit exceeds baseline variation and whose throughput/latency trade-offs are acceptable.

Report cold tray, warm tray after main/Settings/Add Download use, main-window settled memory, peaks, and repeated-cycle trends. Approximately 20 MB warm idle tray remains the aspiration, not a promised outcome. A useful first milestone is a lower, stable warm-tray footprint without repeated growth or feature regressions.

This note is based on user observations and source inspection. No deploy benchmark, allocation trace, memory reduction, or leak fix has been verified as part of documenting this backlog.
