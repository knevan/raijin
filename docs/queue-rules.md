# Queue & Scheduler Rules

Status: **ON HOLD** (Deliberately deferred).

---

## 1. Why Queue is On Hold

Queue management and scheduling (Milestone 3 in `plan.md`) is deliberately held until the following milestones are completely stable and hardened:
1. Native HTTP Download Engine (M2)
2. Monitor & Projection layer (M4)
3. Main Desktop UI (M5)
4. Packaging & distribution (M8)
5. Browser integration extension (M7)

### Current Operating Rules:
- Downloads run directly through the engine actor on an immediate execution basis without queue scheduling.
- **Do NOT implement Queue UI** (e.g. queue pickers, queue management views, queue reordering tabs) until M3 is officially unheld.
- Queue logic must not live inside download workers or jobs.

---

## 2. Unholding Plan

When Milestone 3 is unheld:
1. Port the existing queue implementation from `rnd/raijin` (`src/queue/`) as a baseline reference.
2. Establish the typed `QueueSchedule` boundary per `plan.md` Section 6.
3. Keep `queues.schedule_json` as a `TEXT` column in SQLite without requiring schema migrations (strict validation on write, tolerant decoding on read).
4. Run the queue scheduler on a dedicated fake-clock test harness to verify max-concurrent limits and scheduled start/stop triggers.

