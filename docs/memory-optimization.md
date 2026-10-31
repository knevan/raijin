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
| Repeated main-window open/close, no downloads (latest manual observation) | ~20-25 MB; screenshot: Memory 23.90 MiB, Commit 114.18 MiB, total working set 84.17 MiB |

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

- The app-level `spawn_tray_sync` in `main.rs` owns activity updates on every 1s tick, regardless of visibility. `MainView` no longer writes tray activity. Fresh windowless startup (`--minimized` or `--startup`) and Settings/Add-only windows do not need a live `MainView` to refresh the tooltip and menu flags.
- The `MainView` polling loop checks view liveness even when table polling is suspended, and exits on the next tick after teardown. This closes the detached-task accumulation path; a hidden but live view still performs a lightweight liveness check every 500ms.
- Tray summaries may lag by one tick. No window hide/destroy semantics were changed.

### 3. Tokio worker count (Implemented: 4 workers)

**Scope:** runtime construction in `crates/ui/src/main.rs`.

- Set explicit `.worker_threads(4)` on `tokio::runtime::Builder::new_multi_thread()`.
- Async workers have a fixed count; idle workers park rather than being destroyed. Four workers reduce worker/thread resources on hosts where Tokio's available-CPU default is higher, but can increase them on smaller hosts. Tokio's blocking pool, SQLx workers, and GPUI threads are separate.
- The user reports no noticeable performance regression in manual testing with four workers and four SQLite connections. This is not a controlled throughput benchmark or a multi-gigabit saturation guarantee. GPUI owns a separate main thread, but engine scheduling latency can still affect perceived responsiveness.

### 4. SQLite connections and caches (Implemented: 4 conns, 30s idle, memory pragmas)

**Scope:** `crates/core/src/db/mod.rs`; see [SQLite Database & Migrations](database.md).

- Pool limits: `max_connections(4)` and `idle_timeout(Duration::from_secs(30))` reduce potential connection resources and reclaim idle connections sooner. `min_connections(0)` is already SQLx's default, not lazy initialization: `connect_with` still opens a connection initially. Fewer connections may increase acquisition waits; reopening after idle incurs connection/cache warm-up costs.
- Connection pragmas:
  - `synchronous(SqliteSynchronous::Normal)`: an explicit durability/performance decision accepted by the user. WAL maintains consistency, but recent committed transactions can be lost after power loss or an OS crash. This affects all state in the shared database, not only progress. It is not a memory optimization.
  - `busy_timeout(Duration::from_secs(5))`: makes the existing SQLx default explicit; waits for locks but does not guarantee the absence of `SQLITE_BUSY` errors.
  - `cache_size = -1024`: suggests a page-cache target of approximately 1 MiB per connection, not a hard total memory cap. Smaller caches may increase page reads on larger histories.
  - `temp_store` is left at SQLite's default; forcing temporary sorts into RAM was rejected for this memory-focused change.
- This pool policy targets the file-backed desktop database. An in-memory database disappears if all its connections are closed; long-lived in-memory workloads need a separate lifetime policy.

### 5. Snapshot and presentation duplication (Implemented: Arc sharing)

**Scope:** `crates/core/src/monitor/manager.rs`, `monitor/state.rs`, and `crates/ui/src/main_view.rs` / `downloads.rs`.

- Switched `DownloadMonitorHandle::state()` and monitor watch channel to publish immutable `Arc<MonitorState>`.
- Read operations (`handle.state()`) clone an `Arc` handle rather than deeply copying nested `BTreeMap` structures every 500 ms.
- `MainView` diffing checks pointer identity (`Arc::ptr_eq`) before value equality, bypassing deep equality comparisons when the snapshot is identical.
- Publication still deep-clones the projection on every event; presentation rows remain separate. Readers holding older snapshots retain their allocations until the last handle drops.

## Deferred or rejected shortcuts

- No periodic `EmptyWorkingSet`, forced paging, or equivalent working-set trimming to make Task Manager look smaller. It does not remove live allocations and can hurt restore latency.
- No production allocator replacement, forced cache flushing, native-window rewrite, or process split without profiling evidence and a separate trade-off decision. Hotpath's allocation-tracking wrapper is restricted to opt-in profiling builds.
- No assumed RAM savings from executable stripping or `opt-level = "z"`. The deploy profile already uses Thin LTO, one codegen unit, and abort-on-panic; binary size and resident memory are different measurements.
- No disabling integration, tray functionality, persistence, or download safety just to meet the target.

## Opt-in Hotpath profiling (local and CI)

Hotpath 0.30.2 is a regular dependency with no profiling features enabled by default. Its macros are no-ops in normal builds. `raijin-ui/hotpath` forwards timing instrumentation to core; `hotpath-alloc` additionally enables the tracking allocator in the executable. No profiling feature belongs in `default`.

Starting coverage is intentionally small: monitor event projection and publication, UI snapshot application, and tray summary aggregation. Allocation attribution covers Rust allocator traffic, not every native SQLite/graphics allocation or the entire process footprint. Instrumented timings and memory include profiler overhead; measure actual idle footprint and throughput in a separate uninstrumented deploy run.

### Local GUI profiling

Quit an existing instance before launching. From a writable working directory, run:

```powershell
$env:HOTPATH_METRICS_SERVER_OFF = "true"
$env:HOTPATH_OUTPUT_PATH = Join-Path $PWD "hotpath-ui.json"
cargo run --profile deploy -p raijin-ui --features hotpath-alloc -- --minimized
```

Open/close the main window, Settings, and Add Download, then use explicit Quit. On normal return from `main`, Hotpath writes a JSON report; the default is `hotpath-ui.json` in the working directory. The file path is necessary because the Windows GUI subsystem has no console. `HOTPATH_OUTPUT_PATH` overrides it; `HOTPATH_OUTPUT_FORMAT=table` selects a readable report. Reports are ignored by git at the repository root. A killed process, abort/panic, or early `process::exit` is not guaranteed to flush a report. Do not use forced timed shutdown for a real download session.

Static reports require no CLI installation. For an optional live dashboard, install `cargo install hotpath --features=tui`, run `hotpath console`, and launch the instrumented application in a second terminal with `HOTPATH_METRICS_SERVER_OFF` unset. This enables Hotpath's localhost metrics server; keep it off for static-only profiling. No cloud service or report upload is required.

### Repeatable headless workload

```powershell
$env:HOTPATH_METRICS_SERVER_OFF = "true"
$env:HOTPATH_OUTPUT_PATH = Join-Path $PWD "hotpath-monitor.json"
cargo run --locked --profile deploy -p raijin-core --example profile_monitor --features hotpath-alloc
```

The workload seeds 1,000 completed items and 16 active items, applies 500 progress events with subscriber synchronization, and reads shared snapshots 20,000 times. Assertions check projection results, snapshot sharing, and older-snapshot immutability. It uses no database, network, or desktop resources. A 30s timeout bounds the workload. Use `--features hotpath` for timing without allocation tracking.

`.github/workflows/profiling.yml` provides a manually triggered Windows/Linux run of this workload and stores JSON reports as GitHub Actions artifacts. It does not upload to Hotpath Cloud or enforce regression thresholds. It remains separate from the automated Cloud benchmark. This checks core projection/publication costs, not GUI lifecycle, real download throughput, or absence of leaks. Do not compare timing/RSS across operating systems as equivalent measurements.

For normal tests, leave profiling off. To check instrumentation builds, use `cargo check -p raijin-ui --features hotpath-alloc`; ordinary `cargo test` does not initialize a profiling session. Run the dedicated example to obtain a report without global-guard conflicts between parallel tests.

### Hotpath Cloud regression CI

`.github/workflows/hotpath-benchmark.yml` runs `profile_monitor` on pushes to `main` and pull requests, including forks. It builds once with `--profile deploy` and opt-in `hotpath-alloc,hotpath-cloud,hotpath-callgrind`, then merges native timing/allocation metrics with Callgrind instruction counts. Ubuntu 26.04, Valgrind 3.26.0, and Rust nightly-2026-10-25 are pinned for comparability. Bump the toolchain in a separate PR; its merge establishes a new baseline. The deploy profile matches the application build settings (Thin LTO, one codegen unit, abort-on-panic, symbols retained). Re-establish the default-branch baseline after switching from release to deploy; differences across profiles are not application regressions.

The Cloud workflow shares job environment through a YAML anchor while keeping push/PR permissions separate. Actions use official version tags (v7 for checkout and upload-artifact), and checkout does not persist credentials. PR concurrency cancels superseded runs; push concurrency does not cancel a running baseline upload. The existing manual artifact-only workflow also uses deploy, versioned actions, Rust nightly-2026-10-25, and checkout without persisted credentials. No matrix, extra command runner, or new manual Cloud trigger is needed for this single benchmark.

`hotpath/policy.toml` uses the official default policy with the recommended initial 3% instruction threshold and `fail_check = false`. Timing is shown but not judged. No absolute memory budgets or required merge checks are configured. Re-run the first default-branch jobs to confirm instruction-count variation stays below the threshold; the Windows local environment cannot validate Callgrind. This benchmark measures monitor allocation traffic and CPU work, not overall GUI/tray resident memory or leak freedom.

Push jobs upload using GitHub OIDC. PR jobs have only `contents: read` and store `hotpath-report/report.json`; `.github/workflows/hotpath-relay.yml` uploads it through the official `hotpath-rs/hotpath-relay@v1.0.0` action, without checking out PR code. The relay runs only from the default branch and matches the benchmark workflow name exactly; renaming that workflow requires updating the relay. The relay itself is not a PR check. Review PR changes to `hotpath/*.toml` as CI configuration, since each PR supplies its own policy.

Local verification: the policy validation API returned `valid: true`; core feature-off/on checks and all-target Clippy with the Cloud/Callgrind features passed. Two native release runs on Windows with Rust 1.101.0-nightly embedded the expected benchmark/policy and matched every function allocation total (`publish`: 218,457,000 B across 500 calls; snapshot reads: 0 B). Rust 1.99.0 feature-off check passed, but its Windows release run failed in `aws-lc-sys` with MSVC C1056 before executing the workload. The pinned Linux build, merged reports, uploads, and PR comments remain unverified until CI runs. The initial mutable action references have since been replaced by SHA pins. Security analyzers may still flag the intentional `workflow_run` trigger; the privileged relay is SHA-pinned and never executes PR code. The release results above are historical validation, not verification of the deploy profile. After switching profiles, YAML/anchor parsing, command and shell-syntax assertions, SHA pins, concurrency, PR permissions, and relay mapping checks passed. The local deploy workload attempt failed before execution in `aws-lc-sys` with MSVC C1056 (object timestamp update), so no deploy report or repeatability result is claimed. The local linter's unknown `ubuntu-26.04` label is a false positive: that x64 image is listed in the official GitHub runner-images README.

To activate, install the Hotpath GitHub App for `knevan/raijin` through <https://hotpath.rs/app>, then merge these files to `main`. Its first push supplies the baseline; older PR branches must incorporate the policy. Reports for this public repository are public by default. No upload is triggered during local verification.

## Acceptance and next action

Start with controlled deploy measurements and cold-versus-warm VMMap comparison. Then test one narrowly scoped candidate at a time; keep only changes whose benefit exceeds baseline variation and whose throughput/latency trade-offs are acceptable.

Report cold tray, warm tray after main/Settings/Add Download use, main-window settled memory, peaks, and repeated-cycle trends. Approximately 20 MB warm idle tray remains the aspiration, not a promised outcome. A useful first milestone is a lower, stable warm-tray footprint without repeated growth or feature regressions.

The headless Hotpath workload has been executed locally in both dev and release builds with allocation tracking and JSON output. The release report records 500 calls each to monitor projection/publication and zero allocations in the 20,000 shared snapshot reads. Publication still accounts for most allocation traffic because it deep-clones history; allocation traffic is not retained process memory. No controlled before/after deploy memory benchmark, live GUI profiling session, or GitHub Actions run has been verified.
