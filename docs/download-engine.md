# Native HTTP Download Engine

Status: **Active core engine (M2 complete, hardening ongoing)**.

This document records the architectural invariants and design decisions for Raijin's native HTTP multi-part download engine.

---

## 1. Engine Invariants

- **Native HTTP First**: The native HTTP engine is the default and primary implementation target. Aria2 is strictly a potential future adapter behind an engine trait; it is not used in MVP.
- **Separate Durable State**: Download item state and part state must be persisted separately in SQLite (see `docs/database.md`).
- **Incomplete Destination & Atomic Rename**:
  - In-flight downloads write to an incomplete destination file (e.g. `.incomplete` or part files).
  - On full verification and completion, the file is atomically renamed to the target filename.
  - Destination collisions use `name(1).exe`, `name(2).exe`, and so on, without a space and before the extension. Extensionless names use `name(1)`. Both initial and final destination selection follow this convention; existing files are not renamed. Initial selection also avoids occupied incomplete destinations.
- **Batched Persistence**:
  - Do NOT persist progress on every incoming chunk.
  - Persist progress on a periodic timer interval (e.g. 500ms - 1s) and immediately upon pause, error, or completion.
- **No Direct UI Binding**: The engine operates purely in background Tokio tasks. UI reads only projected state via `monitor` (see `crates/ui/AGENTS.md`).

---

## 2. Resume & Validation Rules

When resuming an interrupted or paused download:

1. **Remote Validation**:
   - Validate `Content-Length`, `ETag`, `Last-Modified`, and range support (`Accept-Ranges: bytes`).
   - If the remote file has changed (`ETag` mismatch or length mismatch), **pause with a validation error**. Do NOT silently discard progress or silently restart from zero.
2. **Range Support Disappearance**:
   - If a ranged download previously split into parts encounters a server that suddenly refuses `Range` requests (or drops `Accept-Ranges`), pause and require user intervention/reset.
3. **Non-Resumable / Unknown Size**:
   - If `Content-Length` is missing or the server does not support byte ranges, use a single blind/non-resumable worker without range partitioning.

---

## 3. Dynamic Range Splitting & In-Flight Worker Acks

- **No Overlap, No Gap**:
  - The byte space `[0, total_bytes)` must be partitioned without gaps or overlaps.
  - Unit tests in `crates/core` must continuously verify no-gap and no-overlap invariants across dynamic splits.
- **In-Flight Splitting**:
  - Dynamic splitting of active parts requires transactional commit of the new part boundaries and worker acknowledgement before the new range starts downloading.
- **Connection Tuning**:
  - Adaptive connections dynamically scale based on bandwidth and server response, but respect explicit manual `preferred_connections` overrides configured by the user.

