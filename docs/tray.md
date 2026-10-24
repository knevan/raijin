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

---

## 2. Tray Abstraction

- **Backend Trait**:
  - UI code interacts with the tray through the platform abstraction trait (`platform::tray`).
  - Underlying backend: `domenkozar/gpui-tray` (with `Tryanks/gpui-tray` as fallback).
  - **Never use `Yamrc/gpui-tray`** (archived and unmaintained macOS stub).
- **Startup Mode**:
  - When Raijin starts with `--startup` and `close_to_tray = true`, launch directly in windowless tray mode without flashing the main window.

