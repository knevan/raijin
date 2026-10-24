# Settings & Browser Integration — Deferred Work

Status: **partially deferred** (Settings v1 shipped, listed items are not scheduled).

This records what Settings v1 deliberately left out and what must be revisited
before those items can be built honestly. Each entry names the blocking
evidence in the codebase, so a future pass starts from facts instead of
re-deriving them.

Scope: the Settings window (`crates/ui/src/settings.rs`), the browser
integration server (`crates/core/src/integration/`), and the desktop settings
document (`crates/core/src/config/mod.rs`).

Shipped in v1:

- Two-pane shell: `Sidebar` nav + section pane (resizable, `720×460`, min
  `560×360`).
- **Browser Integration**: server status (port from `address.port()`), token
  masked with reveal toggle, copy, regenerate behind an `AlertDialog`.
- **General**: `Close to tray`, `Launch at startup` — both with live
  write-through.
- Token persistence model: one token per install, read back from
  `integration.api_token`. No per-run entry, no pairing endpoint.

---

## 1. Integration on/off toggle — blocked by server lifecycle

**Deferred.** The Settings window shows integration status read-only
(`Running on port N` / `Stopped`).

Why it cannot be a switch yet:

- The listener is bound once during boot in `bootstrap_engine`
  (`crates/ui/src/main.rs`, `start_integration`) and `serve()` hands the
  `TcpListener` to a spawned task with no handle kept. There is nothing to
  stop, and nothing to rebind.
- `RAIJIN_INTEGRATION=1` exists only because the persisted toggle has no UI
  (`integration_enabled`, `crates/ui/src/main.rs`). It is a development
  escape hatch, not a feature.

What a real toggle needs:

1. Keep the `JoinHandle` (or a cancellation token) for the server task so it
   can shut down deterministically.
2. Rebind on enable. On Windows a rebind right after shutdown can fail
   (`TIME_WAIT`); the UI must surface that instead of showing "Enabled".
3. Decide what the extension sees during the gap. It already distinguishes
   "server down" from "token rejected" (`browser-extension/utils/backend.ts`),
   so a brief network error is the honest signal — do not paper over it.

Also drop `RAIJIN_INTEGRATION` once the toggle ships; leaving both makes
"why is this on?" undebuggable.

## 2. Editable server port — same blocker as §1

**Deferred.** Port is read-only and comes from `address.port()`, never from a
hardcoded `DEFAULT_PORT`, so it stays correct the moment rebinding lands.

Blocked on: the same server lifecycle work as §1. Changing the port is a
rebind, not a settings write. Additionally the extension stores its own port
(`browser-extension/utils/config.ts`, `local:port`), so a port change requires
a second user action — the UI must say so, or pairing silently breaks.

## 3. `minimize_to_tray` — blocked upstream by GPUI

**Deferred, and not merely unbuilt.** The field exists
(`crates/core/src/config/mod.rs`) but its own doc comment records the
constraint: *"Reserved for a future OS-minimize hook: GPUI exposes no minimize
interception"*.

`TrayBehavior` has three resolution paths (`on_minimize`, `on_close`) but the
UI can only honour two. Showing a switch for it would be a control that does
nothing, so General lists only `Close to tray`.

Revisit when GPUI exposes a minimize hook. Until then the OS minimize button
always minimizes to the taskbar.

## 4. Dead settings fields — need engine wiring first

**Deferred.** These fields exist in `DesktopSettings`, persist correctly, and
are read by nobody outside `crates/core/src/config/mod.rs`:

| Field | State |
| --- | --- |
| `thread_count` | unused |
| `max_concurrent_downloads` | unused (queue is on hold) |
| `max_download_retries` | unused |
| `dynamic_part_creation` | unused |
| `global_speed_limit_bps` | unused |

For contrast, the fields that *are* wired: `default_download_folder`,
`category_root`, and `use_category_by_default` reach `EngineHandles`;
`close_to_tray` reaches `TraySession::set_behavior`; `launch_at_startup`
reaches the autostart manager.

`DownloadManagerOptions` currently carries only `command_buffer` and
`event_buffer`; the engine reads `DownloadConfig::default()` directly. So a
slider for thread count would change a number that nothing consumes.

Revisit only after `DownloadConfig` is threaded into the manager. Building the
form first guarantees a UI that lies.

## 5. Appearance / theme section — needs a persisted field

**Deferred.** `Theme::change(ThemeMode::Dark, ...)` is hardcoded at startup
(`crates/ui/src/main.rs`) and `DesktopSettings` has no theme field.

Not a small addition: it needs a new settings field, a migration-safe default,
and applying the theme before the first window paints to avoid a flash. Listed
here so it is not mistaken for a one-line section.

## 6. Edge cases implemented but untested in-app

**Deferred verification.** These are implemented, compile clean, and pass unit
tests, but have not been exercised in a running app:

- `AlertDialog` (Regenerate confirmation) rendered from a Settings window whose
  root is a plain `Root::new`. `settings.rs` mounts `render_dialog_layer`,
  `render_sheet_layer`, and `render_notification_layer`, matching `main_view.rs`,
  but the Add Download window also uses `Root::new` and does not mount those
  layers — so the pattern is proven only for the main window.
- `Sidebar` inside `h_flex` with `flex_1().min_h_0()` on the parent: the nav is
  a `list()`-backed sidebar, which sizes from the viewport. Resizing the
  Settings window narrower than the nav must not collapse it.
- `Clipboard` component click while the token `Input` is `readonly(true)`.
- Auto-reveal after Regenerate, and that closing/reopening the window returns
  to masked (reveal is intentionally not persisted).

## 7. Auto-pair — explicitly deferred, with a trigger

**Deferred by decision.** There is no `POST /pair` route and no
`Origin`-based trust. The trigger to revisit: users who cannot complete a
manual token paste, or a store-distributed extension build where the extension
id is stable.

When it is revisited, the shape is: `POST /pair` unauthenticated + an
Allow/Deny dialog in the app requiring a real user gesture. Never auto-grant,
and never accept a token in a URL query — `browser-extension/extension.md`
rules that out.

## 8. Multi-profile / multi-browser pairing

**No work planned; documented behaviour.** One token per install, pasted once
per browser profile. Two profiles mean two pastes, same as ABDM. The extension
already stores the token in `storage.local` (not `sync`), so it survives
browser restarts and does not leave the machine.

Revisit only if real users report the N-paste cost as friction.

---

## Verified before writing this note

```
cargo test -p raijin-core    → 134 passed
cargo test -p raijin-ui      → 53 passed
cargo clippy --all-targets -p raijin-core -- -D warnings  → clean
cargo clippy --all-targets -p raijin-ui   -- -D warnings  → clean
```

Item §6 is the only list entry whose risk is *not* covered by tests: it needs a
manual smoke run of the Settings window.
