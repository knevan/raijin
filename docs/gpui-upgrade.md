# gpui-kit Upgrade Discussion

Status: **done** (landed). Sections 1-5 are the analysis that was written
before the upgrade; §6 records what actually changed when it shipped.

Scope: upgrading the UI stack from the pinned `gpui-kit` 0.6.1 to 0.8.0, and
whether `gpui-tray` needs a fork to allow it.

## 1. Pins after the upgrade

```toml
gpui-kit  = { version = "0.8", features = ["component", "assets"] }
gpui-tray = { version = "0.3", default-features = false, features = ["gpui-kit", "menu-state"] }
```

Resolved: `gpui-kit` 0.8.0, `gpui-base`/`gpui-component`/`gpui-kit-assets`
0.8.0, `gpui-pre` 0.3.9, `gpui-tray` 0.3.0. Exactly one `gpui-pre` in the
lockfile, verified with `cargo tree -i gpui-pre`.

`gpui-tray` 0.3.0 is functionally identical to 0.2.0 for the `gpui-kit`
backend: the whole 0.2.0 -> 0.3.0 diff is an `gpui-ce` dependency bump plus a
test-only change in `icon.rs`. Every breaking change arrived in 0.2.0.

`gpui-tray` pulls `zbus` 6 on Linux while `gpui-pre-linux` still uses `zbus` 5,
so `cargo tree --target all` shows both. The constraint is
`cfg(target_os = "linux")`, so Windows and macOS builds are untouched.

## 2. Fork question: not needed

An earlier assumption was that `gpui-tray` pins `gpui-pre` exactly and would
force two `gpui-pre` versions alongside `gpui-kit` 0.8. **That was wrong.**
`gpui-tray` declares `gpui-pre` as `version = "0.3.4"`, which in Cargo is a
caret requirement (`>=0.3.4, <0.4.0`), not an exact match. Only `gpui-kit`
uses an exact pin (`=0.3.9`).

Verified with a throwaway crate, not by reading manifests:

```text
$ cargo generate-lockfile   # gpui-kit 0.8 + gpui-tray 0.3.0
gpui-pre  → 0.3.9     (single version)
gpui-kit  → 0.8.0
gpui-tray → 0.3.0

$ cargo check
Finished `dev` profile

# This compiles against one shared App type:
gpui_tray::Tray::builder()
    .menu(|cx: &mut App| ...)   // App from gpui_kit
    .build(cx)
```

So `cargo tree` still shows a single `gpui` version, which is what
`AGENTS.md` requires.

Why a fork would still be the wrong answer even if compatibility broke:

- Nothing needs changing upstream today, so a fork would maintain a copy for a
  problem that does not exist.
- `gpui-tray` carries three native backends (`objc2` on macOS, `windows` 0.62
  on Windows, `zbus` on Linux). A fork means owning all three.
- A fork blocks future upstream fixes; every bug fix becomes a manual
  cherry-pick.
- `AGENTS.md` requires pinning to an explicit upstream version/commit, which a
  fork replaces with a private maintenance burden.

If upstream ever genuinely blocks us, prefer a minimal `[patch.crates-io]`
fork that only relaxes the version requirement over a full fork.

## 3. What actually changes in 0.7 / 0.8

From source diffs (`gpui-kit` 0.6.1 → 0.7.3 → 0.8.0), limited to what this
codebase touches:

| Change | Impact on Raijin |
| --- | --- |
| `gpui_kit::open_window(options, cx, build)` added; the build closure returns the *view* and Kit wraps `Root` itself | `main.rs:430` and `add_download.rs:722` construct `Root::new` directly. Adopting `open_window` naively would double-wrap. Choose: adopt the helper, or keep the manual wrap. |
| `Root` re-exported from `gpui_base::Root` through `component::Root` | Imports keep working unchanged. |
| `platform` module is feature-gated; `application()` gated to non-mobile | No desktop impact. |
| New `gpui-fast` feature (GPUI Fast 0.1.7 replaces `gpui-pre`) | Optional, not used. |
| `test.rs`: `press()` now sends key-down **and** key-up; Enter/Tab run key bindings | No impact; the project has no GPUI widget tests. |
| `open_window` docs: in async context call it inside `cx.update` | The existing spawn pattern in `main.rs:158` already does this. |

Confirmed **unchanged** in the new stack (`gpui-pre` 0.3.9):

- `Application::with_quit_mode(QuitMode)` still exists (`gpui-pre-0.3.9/src/app.rs:228`).
- `QuitMode` still has `Default` / `LastWindowClosed` / `Explicit` with identical
  documentation, so `QuitMode::Explicit` keeps meaning "quit only via
  `App::quit`" (`app.rs:458`). The tray-mode rationale in `main.rs` still holds.
- `Root::new(view, window, cx)` signature is unchanged
  (`gpui-base-0.8.0/src/root.rs:170`).

Net: the upgrade is expected to be mechanical, not architectural.

## 4. Unverified

- No full `cargo check -p raijin-ui` against 0.8. The probe crate above proves
  the dependency graph resolves and that `App` is shared, but does **not**
  cover `gpui-component` 0.8 widget APIs (`Button`, `TableDelegate`,
  `TitleBar`, `Input`, `Select`). Those may carry their own breaking changes.
- No runtime check. Window startup, tray lifecycle, and the close-veto path
  are behavior-sensitive (`window.on_window_should_close`, deferred
  `remove_window`, `on_window_closed` observers) and were not exercised.
- Cross-platform build unverified; only Windows was probed. (Resolved when the
  upgrade landed: `cargo check`, `cargo clippy -D warnings` and 59 UI tests
  passed on Windows.)

## 6. What actually changed when it landed

Three code changes, everything else was the dependency bump:

| Change | Why |
| --- | --- |
| `Root::new` / `Root::render_dialog_layer|sheet|notification_layer` removed from all four views | In 0.8 `Root` is `gpui_base::Root`; the dialog/sheet/notification layers are mounted by the `RootPlugin` registered in `gpui_component::init`. The `.children(...)` calls were deleted, not renamed. |
| `build_main_root` -> `build_main_view`, now returns `Entity<MainView>` | Nothing wraps `Root` in application code any more, so the name no longer described the result. |
| `tray_icon()` calls `.build()` on `Icon::from_rgba` | `gpui-tray` 0.2.0 turned `Icon::from_rgba`/`from_gpui` into builders that validate at `.build()`. |

Decisions taken, recorded so a future upgrade does not relitigate them:

- **Correction: removing the root wrapper was not safe.** Component plugins
  are registered per application but instantiated per `base::Root`, not for
  every bare view passed to `cx.open_window`. `WindowExt` requires that root
  and otherwise panics when opening a dialog or notification. Settings now
  uses `gpui_kit::open_window` and accesses its content through `Root::view()`
  when reusing the window. Other window entry points still need a separate
  audit; global plugin registration alone does not prove overlay support.
- **`menu-state` is enabled.** Without it `gpui-tray` hardcodes
  `enabled: true` in `compile_menu`, so every `MenuItem::disabled(...)` in
  `main.rs` was silently discarded. The `has_window` row also needed a
  rebuild signal: `TraySession::mark_shown`/`mark_closed` set `menu_dirty` and
  the existing 1 s sync loop consumes it, so the menu rebuilds on window
  transitions without polling `App::windows()` every tick.
- **`on_activate_with` (Wayland activation token) is deferred to Q3.**
  `gpui-pre` 0.3.9 exposes no API that consumes an external activation token,
  so wiring it now would drop the token on the floor.

## 5. Recommended procedure when it is scheduled

1. Do it as a standalone change. Never combined with a feature landing: a
   mixed diff makes a compile failure impossible to attribute.
2. Branch, then `cargo update -p gpui-kit --precise 0.8.0` and
   `cargo update -p gpui-tray --precise 0.3.0`.
3. `cargo tree -i gpui-pre` and confirm exactly one version before fixing any
   code. If two appear, stop and re-read the manifests rather than patching.
4. `cargo check -p raijin-ui`, then inventory errors by category
   (`open_window`/`Root`, component widgets, action macros).
5. Decide `open_window` adoption once, up front, and apply it consistently to
   both window entry points.
6. Re-verify the platform-sensitive paths by hand:
   - `--minimized` starts windowless and stays alive.
   - Close button hides to tray (not quit) with `close_to_tray`.
   - Tray "Show" rebuilds the window from monitor + DB.
   - Tray "Quit" removes the icon and exits.
   - Add Download window opens standalone via `--add <url>`.
7. Run the full gate before merging:
   `cargo test -p raijin-core`, `cargo test -p raijin-ui`,
   `cargo clippy --all-targets -p raijin-core -- -D warnings`,
   `cargo clippy --all-targets -p raijin-ui -- -D warnings`.
8. Commit `Cargo.lock` in the same commit as the version bump.

## 6. Why deferred now

Not because of a dependency blocker (there is none), but because the localhost
integration server just landed and is not yet verified end-to-end against the
browser extension. Upgrading the UI stack at this point would make any failure
ambiguous between the new wiring and the new toolkit. Revisit after the
extension flow is confirmed working.
