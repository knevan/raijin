# Dynamic Splitting: Smarter Scheduling and Hardening

Status: **Agreed improvement direction; implementation and validation tracked below**.

Tracking convention: unchecked means not yet verified for this plan, not necessarily absent from the current engine. Check an item only after implementation or audit confirms it and relevant tests/measurements support it. Record the supporting commit, test, or benchmark reference with the item; update incrementally rather than marking an entire phase complete from design agreement alone.

This document records the discussion conclusions for improving native HTTP download performance, robustness, and reliability. Existing engine invariants remain authoritative in [download-engine.md](download-engine.md).

## 1. Goal and Decision

Make downloads feel fast across small and large files, high and low bandwidth, high latency, and unstable connections. Robustness and correctness are mandatory acceptance criteria, not optional trade-offs against speed.

Retain the current execution model and midpoint range splitting. Improve the decisions around it:

1. **Size-aware startup**: choose useful initial parallelism without delaying transfer for adaptive sampling.
2. **Measured connection adaptation**: evaluate whether additional workers actually improve aggregate throughput.
3. **ETA-aware candidate selection**: help likely stragglers when measurements are reliable, with largest-remaining fallback.
4. **Benefit-aware splitting**: avoid opening a child worker when setup overhead is likely to exceed the completion-time benefit.
5. **Correctness hardening**: verify boundary ownership, completion, persistence, cancellation, and recovery throughout these changes.

More connections are not inherently faster. No policy can guarantee a speedup on every server or network; improvements must demonstrate benefits without material regressions against simple baselines.

## 2. Current Baseline

The discussion inspected these implementation points:

- `crates/core/src/download/job.rs`: `run_ranged()`, `ConnectionController`, `split_pending_part()`, `request_inflight_split()`, `handle_part_worker_control()`, and `part_worker_inner()`.
- `crates/core/src/download/part.rs`: initial partitioning and `split_part_at()`.
- `crates/core/src/download/model.rs`: `DownloadConfig` defaults.
- `crates/core/src/db/repositories.rs`: transactional `PartRepository::set_many()`.

Current behavior:

- Initial ranges are partitioned according to connection target and file size.
- Vacant worker slots consume pending parts first; pending parts can be split when more work is needed.
- With no pending work, active workers can propose splitting their remaining range at its midpoint.
- Candidates are prioritized by remaining bytes, not predicted completion time.
- Dynamic splitting requires at least twice `min_part_size` remaining. Initial partitioning uses a ceiling-based count cap, so its minimum-size behavior is not identical.
- An active parent stops processing chunks while awaiting the split commit decision. Parent and child metadata are persisted in one SQLite transaction before the commit signal allows the split to proceed.
- The current protocol has a worker proposal and scheduler commit signal, not a separate post-application worker ACK. This distinction must be reconciled with the engine's acknowledgement invariant during hardening.
- The parent's existing HTTP response is retained; local writes are capped at its new boundary. Some data beyond that boundary may already be in flight or buffered.
- Adaptive tuning reacts to progress deltas and retries. It does not explicitly isolate the benefit of a connection-count change.
- Lowering the connection target does not immediately terminate active workers.
- Explicit `preferred_connections` selects fixed connection tuning; dynamic splitting can still fill that fixed target.

Defaults at discussion time: 8 initial connections, adaptive tuning enabled, bounds of 1–16 connections, 1-second sampling interval, 10% gain threshold, and 4 MiB `min_part_size`. These are baseline values, not validated optimal settings.

## 3. Scheduling Improvements

### 3.1 Fast, Size-Aware Startup

Start transferring without waiting for an adaptation cycle. Use known file size to avoid excessive initial parallelism for small transfers while allowing larger transfers to start with moderate parallelism.

Audit startup request/setup overhead without weakening remote validation. Unknown-size and non-range resources retain the single-worker path required by the engine invariants.

Do not select new size thresholds or initial connection counts by intuition alone. Compare candidate settings against the current defaults and single/fixed-connection baselines.

### 3.2 Probe-and-Observe Connection Control

Use a bounded control cycle:

```text
measure baseline -> probe one additional worker -> allow settling -> evaluate gain
                                                                  |
                                   useful: retain <---------------+
                               not useful: stop expansion + cooldown
```

Required properties:

- Calculate throughput using actual elapsed time, not byte delta alone.
- Smooth samples, for example with EWMA, rather than reacting to individual fluctuations.
- Use a settling period after connection changes and hysteresis/cooldown to avoid oscillation.
- Evaluate actual active workers, not target count alone; target reduction currently drains gradually.
- Respect manual connection overrides, configured bounds, and speed limits.
- Account for local throttling and competing downloads before interpreting low gain as server/network behavior.
- Do not assume a throughput increase was caused by extra parallelism merely because it occurred in the same interval.

Exact sampling, probe, settling, and cooldown parameters remain benchmark-driven decisions. A failed probe must not introduce unsafe worker termination or discard progress.

### 3.3 ETA-Aware Candidate Selection

When a worker has enough reliable samples, estimate:

```text
remaining_time = remaining_bytes / smoothed_worker_throughput
```

Prefer helping a likely completion-time straggler rather than always choosing the largest remaining range. Keep largest-remaining selection as the fallback for new workers or insufficient/unreliable measurements.

Retain midpoint splitting initially. Changing both candidate selection and split ratio at once would make benefits harder to attribute. Worker throughput does not guarantee equivalent child throughput, especially on a shared bottleneck.

Treat stalled workers separately from slow workers; an effectively infinite ETA is not sufficient reason to repeatedly split a stalled range.

### 3.4 Benefit-Aware Split Eligibility

Keep the minimum-byte guard. Add a conservative benefit check when the necessary measurements are available:

```text
predicted completion without split
    > predicted completion with split + setup overhead + safety margin
```

This is a decision principle, not a finalized estimator. Consider remaining time, observed child startup latency, available throughput, and configured speed limits. Avoid late splits when the parent is likely to finish before the child becomes productive.

Use simple fallbacks when estimates are unreliable. Do not add a complex prediction model before measuring unnecessary split/request overhead.

### 3.5 Stall Detection and Recovery

Audit existing timeouts and retry paths before adding mechanisms. Distinguish:

- A slow worker that still makes progress.
- A worker with no progress while peers continue transferring.
- Download-wide network/server failure.
- Local speed-limit or disk waits.

Recovery should target the affected worker when appropriate, preserving committed range ownership and progress. A retry counter alone does not detect a connection that remains open but unproductive.

### 3.6 Smart / Performance Acceptance Checklist

- [ ] Initial parallelism is size-aware and starts transfer without waiting for adaptive sampling; small-file startup and large-file ramp-up are measured against the baseline.
- [ ] Throughput samples use actual elapsed time and smoothing rather than raw byte deltas alone.
- [ ] Bounded connection probes compare baseline and post-settling throughput to evaluate useful gain.
- [ ] Settling, hysteresis, and cooldown prevent expansion or oscillation driven by individual noisy samples.
- [ ] Adaptation evaluates actual active workers and handles gradual target reduction without unsafe termination or lost progress.
- [ ] Manual connection overrides and configured bounds remain authoritative; adaptation avoids futile expansion under local/global speed limits.
- [ ] Local throttling and competing downloads are considered when interpreting throughput changes.
- [ ] Candidate selection uses estimated remaining time when worker samples are reliable and falls back to largest remaining bytes otherwise.
- [ ] Midpoint splitting and the minimum-byte guard are retained while candidate selection is improved.
- [ ] Split eligibility uses a conservative benefit estimate, including child setup overhead, with a defined fallback for unreliable estimates.
- [ ] Late or unnecessary splits do not materially regress completion time or request/network overhead.
- [ ] Stall handling distinguishes slow progress, individual stalls, download-wide failures, and local limiter/disk waits; targeted recovery preserves range ownership.

## 4. Robustness and Reliability Acceptance Checklist

The following are requirements to verify, not claims that all are currently broken or already satisfied:

- [x] A ranged part is complete only when `current_byte == end_byte + 1`; EOF alone is insufficient. Verified by the completion guard and `cargo test -p raijin-core download::job::tests` (31 passed), including early EOF after a committed in-flight split.
- [ ] Repeated splits, retries, and resume preserve complete coverage without gaps or overlapping ownership.
- [ ] Download progress is not double-counted across parent/child transitions and retries.
- [ ] Persisted resume offsets never claim bytes that recovery cannot safely trust. Define and test file durability ordering relative to SQLite progress, including split commits.
- [ ] Proposal rejection, transaction failure, commit-channel failure, worker failure, and cancellation have explicit outcomes.
- [ ] Recovery is tested before commit, during commit, after commit, and before the child starts.
- [ ] Slow/blocked persistence does not create ambiguous range ownership or unacceptable cancellation behavior.
- [ ] Any timeout design distinguishes an uncommitted proposal from a committed split. A parent must not resume its old boundary merely because a wait expired after commit.
- [ ] The acknowledgement contract in the engine documentation matches the implemented protocol and its tests.
- [ ] Partial/truncated HTTP responses cannot produce false completion. Ranged clean-EOF regression coverage is implemented in the first increment; the broader response/fallback matrix remains to be audited.
- [ ] Range rejection and remote metadata changes retain validation errors and require the existing explicit recovery/reset behavior; no silent restart or progress loss.
- [ ] Manual connection overrides and local/global speed limits remain effective.
- [ ] Finalization occurs only after all required bytes are verified and the existing file synchronization/finalization requirements are met.

Particular audit areas identified during static inspection were EOF completion handling, file-versus-metadata durability, and commit waiting under cancellation/storage delay. These require focused verification; discussion alone does not establish reproducible corruption or failure.

## 5. Validation and Performance Acceptance

Measure user-visible speed, not peak throughput alone:

| Metric | Purpose |
|---|---|
| Time to first downloaded byte | Startup responsiveness |
| Total completion time | End-to-end download speed |
| Stall frequency and duration | Progress continuity |
| Tail completion time | Straggler impact; define one consistent measurement window |
| Aggregate useful throughput | Benefit of connection changes |
| Requests, splits, and timeouts | Scheduling/setup overhead |
| Network bytes versus useful file bytes | Redundant transfer overhead |

Compare single connection, representative fixed connection counts, the current engine, and the improved policy under identical conditions. Use repeated runs and compare distributions, not one favorable result.

Benchmark matrix:

- Small, medium, and large files, including sizes around policy thresholds.
- Low/high bandwidth and low/high latency independently.
- Jitter, transient errors, interrupted transfers, and worker stalls.
- Servers with and without per-connection throughput limits.
- Range support rejection/disappearance and unknown-size resources for correctness and fallback behavior.
- Local/global speed limits, competing downloads, and slow storage.

Use deterministic fault-injection tests for ownership and recovery; performance benchmarks do not replace correctness tests. Define concrete regression budgets before accepting tuning results. No thresholds, gains, or universal speedups have been established by this discussion.

### 5.1 Validation Acceptance Checklist

- [x] A reproducible baseline captures the current engine, single connection, and representative fixed connection counts before policy changes. Verified by `download::job::baseline::run_performance_baseline`: 27 release runs across three scenarios and three strategies; see first-increment results below.
- [ ] Measurements cover first-byte latency, total completion time, stalls, consistently defined tail time, useful throughput, requests/splits/timeouts, and redundant network bytes.
- [ ] Benchmarks cover small/medium/large files and sizes around policy thresholds.
- [ ] Bandwidth and latency are varied independently, including jitter and servers with/without per-connection throughput limits.
- [ ] Transient errors, interrupted transfers, worker stalls, range rejection/disappearance, and unknown-size fallback are exercised.
- [ ] Local/global speed limits, competing downloads, and slow storage are covered.
- [ ] Deterministic fault-injection tests verify split ownership and recovery separately from performance benchmarks.
- [ ] Repeated runs compare distributions under equivalent conditions rather than a single favorable result.
- [ ] Concrete regression budgets are defined before accepting tuning results; measured gains and regressions are recorded per increment.
- [ ] Tuned startup, sampling, settling/cooldown, ETA confidence, split-benefit, and stall parameters have supporting evidence rather than arbitrary defaults.

## 6. Delivery Order

1. **Establish correctness and measurement baselines**: verify the hardening checklist, add focused missing tests, and create representative benchmark scenarios.
2. **Improve `ConnectionController`**: elapsed-time throughput, smoothing, bounded probes, settling, and cooldown.
3. **Tune startup parallelism**: measure small-file startup costs and large-file ramp-up against existing defaults.
4. **Improve split decisions**: ETA-aware candidate selection with fallback, then conservative benefit-aware eligibility.
5. **Close measured stall/recovery gaps**: reuse existing timeout/retry mechanisms where possible and validate interactions with adaptation.

Each increment must retain correctness and be independently comparable to its baseline. This is a staged improvement plan, not approval for a scheduler rewrite.

### 6.1 First Increment: Completion Guard and Minimal Baseline

Approved scope:

- Reject a ranged worker's early EOF unless its current offset reaches its current exclusive end, including an end changed by an in-flight split. Preserve partial progress for the existing retry/error path.
- Add focused regression coverage for incomplete responses, retry/resume, and exhausted retries. Keep normal completion and split coverage.
- Establish a manual, repeatable loopback baseline for small files, large files under a shared bandwidth cap, and large files under a per-connection cap. Compare unchanged adaptive defaults with single and representative fixed connections.
- Measure first useful downloaded byte, completion time, HTTP requests, and sampled actual active workers; verify final file bytes. Do not gate CI on wall-clock performance thresholds.
- Audit HTTP metadata probes separately from connection adaptation. Do not change probe requests, connection policy, startup thresholds, or split heuristics in this increment.

Recovery scope is process interruption: pause, graceful shutdown, application crash, and forced termination while the OS remains running. Re-downloading recent uncheckpointed bytes is acceptable; skipping unwritten bytes is not. OS crash and power-loss durability guarantees are deferred. File-write ordering relative to SQLite checkpoints still needs verification; this scope decision does not establish that crash recovery is already safe.

#### Minimal Baseline Harness

The ignored test in `crates/core/src/download/job/baseline.rs` runs all scenarios sequentially, with three repetitions and rotated Adaptive/Single/Fixed-4 order. Production startup and sampling defaults remain unchanged. Each run uses a fresh test database, destination, and server bandwidth budget; these databases are test fixtures, not application persistence alternatives.

| Scenario | Payload | Server shaping |
|---|---|---|
| Small-file startup | 2 MiB | Uncapped loopback |
| Shared bottleneck | 40 MiB | 10 MiB/s shared across data requests |
| Per-connection bottleneck | 40 MiB | 4 MiB/s per data request |

```bash
cargo test --release -p raijin-core download::job::baseline -- --ignored --nocapture
```

On the inspected Windows/MSVC 14.44 environment, the default release dependency build failed with `aws-lc-sys` error C1056 while writing COFF timestamps. A session-local build workaround succeeded without changing repository configuration or dependencies:

```bash
CRATE_CC_NO_DEFAULTS=1 CFLAGS='-MD -O2' CARGO_BUILD_JOBS=4 cargo test --release -p raijin-core download::job::baseline -- --ignored --nocapture
```

This workaround omits cc-rs default native compiler flags (including `-Brepro`) and explicitly retains the dynamic CRT and optimization flags. It limits build parallelism only, not test threads. Use the same build environment for all compared revisions and record it with results; it is not a required application setting.

Output is CSV on stdout, not a persisted engine sidecar. Each row records a first-useful-byte upper bound, completion time, end-to-end useful throughput, probe/data request counts, sampled peak workers, and timestamped worker-count transitions. Final bytes are compared with the source payload on every run.

Timing begins at the headless job's `run()` call, after fixture/client setup, and includes engine metadata probing, file allocation, and transfer. Live progress is polled nominally every 5 ms: first-byte and worker observations include scheduler delay and can miss brief transitions. Worker counts include connecting workers, not just productive HTTP streams; the final zero is recorded from job completion because the single-stream path's live counter is not cleared inside the job. When a transfer finishes before progress is sampled, completion time is the first-byte upper bound, not a fabricated sub-5-ms observation. The Add dialog, queue admission, and UI rendering are outside this measurement.

The server bypasses shaping for metadata probes, uses 256 KiB data chunks and the existing token scheduler, and keeps connection tasks owned by a `JoinSet`. It models neither WAN latency nor TCP congestion/loss. The shaping rates are ceilings rather than guaranteed achieved throughput: OS timer granularity, scheduling, buffering, and disk speed can lower observed rates. Three repetitions reduce ordering bias but do not eliminate host load or cache effects; inspect individual results and repeat before accepting tuning. No timing assertion or regression budget is established by this harness. Debug runs validate the harness only; release runs are required for performance conclusions.

#### First-Increment Validation Results

Environment: Windows x86_64/MSVC 14.44, `rustc 1.101.0-nightly (12433c307 2026-10-24)`, base revision `eb407b2` plus this increment's working-tree changes. Release execution used the session-local native-build workaround above; adaptive/startup policy was unchanged.

- `cargo test -p raijin-core download::job::tests`: **31 passed**, including `part_worker_should_fail_with_committed_split_boundary_on_premature_eof`, retry success, progress accounting, exhaustion, persisted resume with pending splits, and existing normal/split/cancellation tests.
- `cargo check -p raijin-core`: **passed**.
- `cargo clippy --all-targets -p raijin-core -- -D warnings`: **passed**.
- `cargo fmt --all -- --check` and `git diff --check`: **passed**.
- Release baseline command above: **1 ignored-by-default test passed**, executing **27 verified downloads** in 80.32 seconds (excluding build).
- Active LSP probes were inconclusive because the primary language server did not answer; Cargo checks are the validation evidence, not an empty diagnostics cache.

Completion distributions below are milliseconds, three runs per row. First-byte values are medians of sampled upper bounds, not socket-level TTFB. Every run used one metadata probe. Data request counts and sampled peak workers were 1 for all small-file strategies; large-file Adaptive/Single/Fixed-4 used 8/1/4 respectively. No additional split/retry requests were observed in this matrix.

| Scenario | Strategy | Completion min / median / max (ms) | First-byte upper-bound median (ms) |
|---|---|---|---|
| Small | Adaptive | 30.83 / 35.56 / 39.06 | 27.76 |
| Small | Single | 17.37 / 24.98 / 39.94 | 11.74 |
| Small | Fixed-4 | 23.88 / 51.37 / 56.79 | 22.18 |
| Shared cap | Adaptive | 3996.10 / 4021.37 / 4043.67 | 28.55 |
| Shared cap | Single | 3990.41 / 3996.22 / 4030.42 | 5.28 |
| Shared cap | Fixed-4 | 4000.71 / 4022.98 / 4023.04 | 22.38 |
| Per-connection cap | Adaptive | 1264.63 / 1295.25 / 1316.44 | 23.98 |
| Per-connection cap | Single | 9951.36 / 9954.40 / 9961.69 | 5.08 |
| Per-connection cap | Fixed-4 | 2511.08 / 2521.26 / 2532.05 | 17.10 |

These are baselines, not before/after speedup claims. Shared-cap completion was similar across strategies, while the synthetic per-connection cap rewarded existing parallelism as expected. Small-file timings varied substantially despite all strategies using one data worker; these three samples do not justify new size thresholds or connection defaults. This matrix also does not isolate a connection expansion's benefit or meaningfully exercise a long adaptive ramp-up. Elapsed-time measurement and controlled connection probes remain separate future increments; no policy tuning is accepted from these results alone.

#### HTTP Probe Audit

Static inspection of `HttpDownloadJob::run_inner()`, `probe()`, `probe_http()`, and the Add dialog's `probe_download_info()` found:

- The Add dialog performs a best-effort metadata probe for size and naming. The engine independently probes when starting a job; dialog metadata is not a trusted replacement for current remote validation.
- Each `run_inner()` attempt performs an HTTP metadata probe before starting body workers. Resume validation runs when the incomplete file has nonzero length; a job-level retry re-enters this setup path, whereas ranged part retries remain inside `run_ranged()`.
- `probe_http()` first requests `Range: bytes=0-255`. A valid `206` supplies range metadata. Selected fallback statuses trigger a second, non-range request; the production client captures at most 4096 useful body bytes for metadata inspection.
- Transfer requests are separate from the probe: one body request for the single-stream path or one request per ranged worker, plus retries and split children. Probe latency and worker setup must not be attributed to connection-controller sampling.
- `ConnectionController::observe()` currently uses byte deltas without actual elapsed-time normalization. This is a confirmed measurement limitation, not measured proof that adaptation dominates user-visible latency.

No probe request has been removed or reused in this increment. Potential request reuse requires separate evidence and must preserve range rejection and remote-change validation.

#### Remaining Recovery Audit Items

This increment retains the existing ranged retry policy: exhaustion pauses the download with a `TooManyRetries` failure rather than silently finalizing it. Persisted part retry counts are retained on a later resume. Whether explicit resume should renew the retry budget, and whether exhaustion should emit a failed rather than paused manager event, are separate policy decisions; changing outer job retries at the same time could multiply attempts.

Checkpoint ordering under forced termination, cancellation during a delayed split commit, and a separate post-application worker ACK remain unverified. Passing EOF and split tests does not establish the entire process-interruption recovery checklist.

## 7. Alternatives and Open Parameters

Not selected for this improvement plan:

- **Fixed-size chunk queue rewrite**: avoids in-flight boundary changes but adds request/metadata churn and is unnecessary to improve the existing model.
- **Speculative duplicate/hedged ranges**: can reduce some stalled tails but consumes extra bandwidth and complicates write ownership, particularly on constrained connections.
- **Complex predictive or learning-based schedulers**: insufficient evidence to justify their tuning and maintenance cost.
- **New per-server durable tuning profiles or user-facing settings**: not required for the agreed first implementation.

Deferred until measurement: initial targets, size thresholds, EWMA parameters, probe intervals, settling/cooldown duration, ETA confidence criteria, split-benefit margin, and stall thresholds. Preserve SQLite as the sole durable state store; this plan does not introduce sidecars or change queue/UI boundaries.
