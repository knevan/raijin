# ui

Rules for `crates/ui`. The root `AGENTS.md` applies too.

`crates/ui` owns all GPUI Kit application code, views, component wrappers, window lifecycle management, and the runtime bridge between background Tokio workers and GPUI foreground state.

## Skills — On-Demand Reference

Load the following skills on demand when relevant:
- `gpui-kit`: when building components, table delegates, or window overlays.
- `gpui-kit-design-guides`: when setting up layouts, spacing, token colors, and visual hierarchy.
- `rust-skills`: reference for Rust patterns.

Use Context7 (`gpui-kit` docs) when inspecting the latest `gpui-kit` APIs or documentation.
**Skills are reference material, not repo rules.** Rules in `AGENTS.md` always win.

---

## Dependencies & Imports

- **Single Dependency**: `crates/ui` depends only on `gpui-kit` (e.g. `gpui-kit = "0.6"`). Never declare `gpui`, `gpui-base`, or `gpui-component` as separate dependencies in `Cargo.toml`.
- **Unified Imports**: Import all GPUI types via:
  ```rust
  use gpui_kit::{*, platform, base, component, assets};
  ```
- **Framework Initialization**: Call `gpui_kit::init(cx)` first inside the `run` closure, before any component or theme is initialized.

---

## Runtime Bridge (Tokio ↔ GPUI)

- **Main Thread Ownership**: GPUI owns the main thread. **Never update a GPUI `Entity` directly from a Tokio background worker.**
- **Forwarding Pattern**:
  ```text
  Tokio Worker  ──(mpsc/broadcast)──>  Bridge Receiver  ──(cx.spawn)──>  entity.update(cx, ...) + cx.notify()
  ```
- **Bridge Isolation**: The `bridge/` module is the **only** place in the UI crate that connects Tokio channels to GPUI entities.
- **State Projection**: UI views render solely from projected monitor state (`monitor`). Views send commands to `core` handles; they never reach into job or part internals.

---

## UI Components & Design System

- **Design Guides Compliance**: Follow `gpui-kit-design-guides` for all spacing, colors, typography, and density. Do not invent arbitrary styling or unapproved raw colors.
- **Main Download List**: Use `DataTable` with a dedicated `TableDelegate`, virtualized rows, sortable/resizable columns, and persisted column widths.
- **Variable-Height Lists**: Use `VirtualList` for details, queue sidebars, or logs.
- **Widget Boundary**: Wrap `gpui-component` widgets (via `gpui_kit::component`) in local UI wrappers so upstream component updates stay isolated behind one boundary.

---

## Window Lifecycle & Tray Management

- **Hide vs. Remove**: When `close_to_tray` is enabled, closing or minimizing the window must hide or suspend it, not destroy it. Reserve `remove_window()` for the explicit Quit action.
- **Tray Suspension**: In windowless tray mode, suspend high-frequency table subscriptions, animations, and periodic repaints to keep CPU and memory footprint minimal.
- **Tray Backend**: Tray operations use the `platform::tray` trait. The underlying crate is `domenkozar/gpui-tray` (fallback: `Tryanks/gpui-tray`). **Never use `Yamrc/gpui-tray`**.
- See `docs/tray.md` and `docs/startup-deferred.md`.

---

## Verification

Before marking UI work complete, verify:
```bash
cargo check -p raijin-ui
cargo clippy --all-targets -p raijin-ui -- -D warnings
```
(If touching UI unit tests, run only relevant tests without restricting threads).
