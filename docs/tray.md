# Tray & Window Lifecycle

Status: **Active implementation (M6 complete, polish ongoing)**.

---

## 1. Window Lifecycle & Minimize Behavior

- **Hide vs. Destroy**:
  - Closing the window when `close_to_tray` is enabled **hides / suspends** the window.
  - Do NOT call `cx.remove_window()` on minimize/hide. Reserve window destruction only for the true exit/Quit action.
  - When the user restores the window, restore visibility and sync state from the `monitor` projection.
- **Resource Conservation**:
  - When the window is hidden/suspended in the tray, immediately suspend expensive table subscriptions, animations, and periodic view repaints.
  - Keep tray event loops, background engine workers, and essential IPC timers alive.
  - The app-level 1s tray sync owns activity summaries in every visibility state, including fresh windowless and Settings/Add-only startup. `MainView` does not update tray activity; its detached polling task exits on the next tick after view teardown.

---

## 2. Tray Abstraction

- **Backend Trait**:
  - UI code interacts with the tray through the platform abstraction trait (`platform::tray`).
  - Underlying backend: `domenkozar/gpui-tray` (with `Tryanks/gpui-tray` as fallback).
  - **Never use `Yamrc/gpui-tray`** (archived and unmaintained macOS stub).
  - Pinned at `gpui-tray` 0.3.0 with the `gpui-kit` and `menu-state` features.
    `menu-state` is required: without it the crate hardcodes `enabled: true`
    and every `MenuItem::disabled(...)` is silently dropped.
  - Menu rebuild signals: the crate refreshes the menu after each tray click,
    and `TraySession::mark_shown` / `mark_closed` set `menu_dirty`, which the
    1 s tray sync loop consumes. That covers window-set changes that no click
    produces (deferred `remove_window`, tray-driven reopen).
  - `on_activate_with` (Wayland activation token) is available in 0.3.0 but
    unused: `gpui-pre` 0.3.9 has no API that consumes an external activation
    token, so the token would be discarded. Deferred to Q3.
- **Startup Mode**:
  - When Raijin starts with `--startup` and `close_to_tray = true`, launch directly in windowless tray mode without flashing the main window.

