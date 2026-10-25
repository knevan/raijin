# Startup, Autostart, and Logging — Deferred Items

Scope: what shipped for Windows login behavior, and what is explicitly **not**
implemented yet. Complements `settings-deferred.md` (which covers the Settings
surface) and `risk.md` (which tracks known gaps per milestone).

## Shipped

- **GUI subsystem.** `crates/ui/src/main.rs` starts with
  `#![windows_subsystem = "windows"]`, so no console window is allocated when
  the OS launches Raijin from the Run key. Verified on the built exe:
  `subsystem = 2` (`IMAGE_SUBSYSTEM_WINDOWS_GUI`). Unconditional: a release
  build has the same subsystem, because `[profile.release]` does not touch
  linker flags.
- **`--startup` flag.** The autostart entry registers `--startup`, not
  `--minimized`. `--startup` means "launched by the OS"; the window decision
  then defers to `close_to_tray` via `tray::start_windowless`:
  - `close_to_tray = true` → windowless behind the tray.
  - `close_to_tray = false` → main window opens.
  - No native tray icon → window on every path (a windowless run would have
    no way back).
  `--minimized` keeps its old meaning (explicit hide) and still wins.
- **Logging.** `core::logging` writes `<app data>/log/AppLog.log` in debug
  builds (or when `RAIJIN_LOG_FILE` is set) and always appends panics plus
  fatal startup errors to `<app data>/log/crash.log`. Console tracing is kept
  only when stdout is a real terminal. The file sink is mandatory for the
  GUI subsystem: Windows discards writes to a null std handle **without**
  reporting an error, so a console-only app would go silent.
- **Build-output guard.** `autostart::is_build_output_path` rejects any
  executable whose path contains a `target` component. `apply_autostart`
  returns the error, and the existing Settings path reverts the row, so a
  `cargo run` session can no longer point the Run key at
  `target\debug\raijin.exe` (or replace a valid install entry).

## Deferred

### 1. Single instance — implemented on Windows only

Windows now claims an exclusive local named pipe before engine/database
bootstrap. Later launches forward navigation or the existing confirmation-only
`--add` command and exit; `--startup` and `--minimized` do not raise windows
on an existing instance. See `app-links.md` for lifecycle and security details.

macOS/Linux single-instance behavior remains deferred. The earlier loopback
lock proposal was not selected for Windows: a named pipe avoids a second
network listener and works while browser integration is disabled. No HTTP
forwarding endpoint or unauthenticated silent-download capability was added.

### 2. `minimize_to_tray` — blocked upstream by GPUI

**Deferred, and not merely unbuilt.** See `settings-deferred.md` §3: GPUI
exposes no minimize interception, `TraySession::minimize_requested()` has no
call site in `crates/ui`, and the General section therefore does not show a
switch for it. The OS minimize button always minimizes to the taskbar, even
with `close_to_tray` enabled. `close_to_tray` and `minimize_to_tray` are
independent by design: the former is the X button, the latter the minimize
button.

### 3. Build-output detection is a path heuristic

**Limitation.** `is_build_output_path` matches a `target` path component. It
misclassifies a legitimate portable install placed in a folder named `target`,
and it misses a custom `--target-dir`. The real fix is an explicit "installed"
marker owned by packaging (M8); the heuristic exists to stop
`target\debug\raijin.exe` from becoming the login entry.

## Manual verification

Run once per OS before shipping:

1. Toggle `Launch at startup` on, confirm
   `reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Run` contains
   `Raijin` with `--startup`.
2. Log out and back in with `close_to_tray` **on**: no console window, tray
   icon only.
3. Repeat with `close_to_tray` **off**: main window opens, still no console.
4. Toggle the switch from a `cargo run` session: the switch must revert and
   show the build-output error instead of writing the Run key.
