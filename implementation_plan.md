# Implementation Plan: Sleeve-Inspired Raijin Design Refresh

## Proposed Changes

### Design direction

- Move Raijin from the current dark, saturated sketch palette toward the newly added `Sleeve` reference: a light, quiet desktop UI with white canvas, subtle paper surfaces, hairline borders, generous rounding, and restrained typography.
- Adapt the reference for a production download manager rather than copying a marketing page:
  - Keep Raijin data-dense and desktop-native.
  - Preserve `DataTable`, persistent sidebar navigation, toolbar commands, keyboard/focus behavior, tray lifecycle, and monitor-driven state.
  - Use the Sleeve visual language mainly for color, surface treatment, radii, button hierarchy, and spacing rhythm.
- Keep semantic state visible and accessible: downloading, finished, paused, error, selected, checked, disabled, loading, and focused states must remain distinct without relying on color alone.

### Theme and token updates

Target file: `crates/ui/src/theme.rs`

- Replace the current dark sketch constants with Sleeve-aligned semantic tokens:
  - `background`: pure white canvas.
  - `foreground`: black / near-black text.
  - `muted_foreground`: graphite/smoke neutral.
  - `border`: silver mist hairline.
  - `sidebar`: subtle paper white instead of dark panel.
  - `muted`: subtle paper surface.
  - `primary`: black for the single strongest commit action.
  - `primary_foreground`: white.
  - `accent` / `info`: Ocean Drift blue for selection accents, progress, focus-adjacent emphasis, and compact status punctuation only.
  - `success`, `warning`, `danger`: semantic states only, not decoration.
- Update row-card helper colors so table rows look like white floating cards defined by hairline borders, not dark blocks.
- Keep color literals centralized in `theme.rs`; application views should continue consuming `cx.theme()`.
- Preserve `Theme::sync_base(cx)` after theme mutation.
- Rename comments from “sketch variables” to the Sleeve/reference-driven product theme to avoid implying `sketch/raijin.pen` is still the visual source of truth.

### Application theme mode

Target file: `crates/ui/src/main.rs`

- Change startup theme from `ThemeMode::Dark` to `ThemeMode::Light` before applying the product theme.
- Keep `gpui_kit::init(cx)` and theme application order unchanged.

### Main window shell

Target file: `crates/ui/src/main_view.rs`

- Title bar:
  - Make it read as a light floating desktop toolbar rather than a dark band.
  - Keep the brand mark, but change the mark treatment from accent-filled to black/white or neutral with a restrained blue accent only if needed.
  - Keep the search field in the title bar, but align its treatment with Sleeve pills: rounded, white/subtle surface, hairline border.
  - Change placeholder copy from `Search downloads...` to `Search downloads…`.
- Sidebar:
  - Use paper-white/sidebar surface with hairline separation from content.
  - Keep the visible `New Download` command, but make it the single strongest app-level command only if it remains the main action. Use black primary styling or an outline/default treatment depending on final hierarchy.
  - Replace saturated selected-state fills with a subtle selected item surface plus stronger foreground. Avoid one-sided selection markers.
  - Keep queue filtering behavior as-is; do not add queue UI beyond the existing section.
  - Adjust the storage card to read as a subtle nested card with a white/paper surface and hairline border.
- Content area:
  - Use white canvas with subtle spacing and cards instead of a dark body.
  - Keep toolbar command layout: title + item count + row actions + trailing icon commands.
  - Keep `Resume` / `Pause` as quiet pill/outline commands.
  - Make destructive/remove state visually distinct via danger semantics only when enabled and in destructive contexts.
  - Ensure icon-only actions retain tooltips and keyboard/action paths where available.
- Status bar:
  - Restyle as a quiet, light, hairline-separated bottom region.

### Downloads table

Target file: `crates/ui/src/downloads.rs`

- Keep `DataTable` and `TableDelegate`; do not replace with custom lists.
- Keep stable domain IDs, sorting, checkbox state, drag payloads, context menu, and row selection behavior.
- Restyle the hybrid card-table to match Sleeve:
  - Header as a white or paper rounded island with hairline border.
  - Rows as white floating cards with 1px silver border and slightly larger radius where feasible.
  - Selected rows use a subtle blue-tinted surface or border plus persistent stronger foreground; checked rows remain visually distinguishable.
  - Hover should be modest and not the only command affordance.
  - Progress remains visible with semantic progress color; paused/error states stay textually explicit.
- Replace direct radius/size literals where practical with theme radii or rem-based helpers. Keep documented pixel constants only for table column widths, virtual row heights, and other measured `DataTable` geometry.
- Review status colors so `Completed`, `Paused`, and `Error` use semantic colors without making ordinary rows overly colorful.
- Empty state should keep concise copy, but update `Use New Download to add your first link.` to use the exact visible command name and, if retained, the ellipsis convention only for commands that open another window.

### Add Download window

Target file: `crates/ui/src/add_download.rs`

- Restyle as a compact light utility window:
  - White/paper canvas.
  - Rounded input fields and pill-like/rounded command buttons.
  - Hairline borders and restrained section spacing.
- Preserve form derivation, probe behavior, validation, and submit behavior.
- Copy updates:
  - Window title remains `Add Download`.
  - Change `Use Category` to sentence case: `Use category`.
  - Change invalid URL sentence to include punctuation: `Enter a valid http(s) URL.`
  - Ensure any command that opens another surface uses the single ellipsis character where appropriate.
- Button hierarchy:
  - `Download` remains the default commit and should use the primary treatment.
  - `Add` and `Cancel` remain secondary/outline/default.
  - Icon buttons stay quiet and accessible with tooltips.

### Settings window

Target file: `crates/ui/src/settings.rs`

- Restyle the two-pane settings shell with the same light sidebar + white detail pane treatment.
- Keep persistent navigation and all async settings behavior unchanged.
- Copy updates:
  - `Browser Integration` should become sentence case in visible UI: `Browser integration`.
  - Keep alert copy specific: `Regenerate API token?` and `Regenerate` are already aligned with the design guide.
- Use cards/grouped sections only where they clarify settings groups; avoid decorative nested cards.

### Design-system cleanup

- Centralize any repeated app-specific design decisions in local helpers rather than scattering one-off `px(...)`, raw radii, or opacity choices through views.
- Keep GPUI Kit component usage semantic:
  - `Button` for commands.
  - `Sidebar`/`SidebarMenuItem` for navigation/filter rows.
  - `DataTable` for downloads.
  - `AlertDialog` for token regeneration confirmation.
- Do not introduce Freya, direct `gpui` dependencies in `ui`, or new UI crates.
- Do not mutate engine/core state from views; visual refresh only.

## Step-by-Step Plan

1. **Establish the visual token layer**
   - Update `crates/ui/src/theme.rs` to define the Sleeve-inspired light palette and row card helpers.
   - Keep all raw color literals confined to this theme file.
   - Update theme comments and tests so they validate the new constants.

2. **Switch the app to light mode at startup**
   - In `crates/ui/src/main.rs`, change the base GPUI component theme from dark to light before calling `apply_product_theme`.
   - Do not change lifecycle, tray, window, or engine bootstrap behavior.

3. **Restyle the main shell**
   - Update `MainView` title bar, sidebar, toolbar, table wrapper, storage card, and status bar styling to consume the new theme tokens.
   - Replace dark-panel assumptions with light surfaces and hairline borders.
   - Keep current state ownership and event wiring unchanged.

4. **Restyle the downloads table delegate**
   - Update header, row, checkbox frame, drag preview, empty state, and status/progress color usage.
   - Preserve `DataTable` delegate methods, stable row IDs, selection remapping, sort state, and context menu behavior.
   - Audit any direct `px(...)` usages and keep only measured geometry or documented physical boundaries.

5. **Restyle Add Download**
   - Update window background, title bar treatment, field rows, probe badge, icon buttons, and submit row to match the light system.
   - Apply copy casing and ellipsis/punctuation fixes.
   - Preserve all URL/file/category/folder derivation and async probe behavior.

6. **Restyle Settings**
   - Update sidebar/detail surfaces and section content treatment.
   - Apply sentence-case copy for visible section labels.
   - Preserve async token regeneration, desktop settings persistence, and alert dialog behavior.

7. **Manual UI pass and design review**
   - Review the main window, add window, settings window, tray-opened states, empty downloads list, active download row, completed row, paused/error row, checked multi-selection, selected row, and context menu.
   - Check that the Sleeve reference is translated into a desktop download manager rather than copied literally.
   - Verify hierarchy with color mentally removed: primary task, selected location, table content, and next action should still be clear.

8. **Polish and reduce duplication**
   - Extract small local helpers only after repeated styling patterns are clear.
   - Avoid broad abstractions or new component wrappers unless repetition becomes a maintenance issue.

## Verification Strategy

### Automated checks

- Run formatting/checking after implementation:
  - `cargo fmt --check`
  - `cargo check -p ui`
  - `cargo clippy --all-targets -p ui -- -D warnings`
- Run core checks to ensure UI-only changes did not affect shared workspace behavior:
  - `cargo test -p core`
  - `cargo clippy --all-targets -p core -- -D warnings`
- Run existing UI unit tests where feasible:
  - `cargo test -p ui`

### Manual visual and interaction verification

- Main window:
  - Fresh/empty state.
  - Several rows across statuses: downloading, finished, paused, error, queued.
  - Row hover, row selection, checkbox multi-select, select-all, sorting, column resizing, context menu, drag-to-queue preview.
  - Sidebar filter active state and queue active state.
  - Search field in the title bar.
  - Toolbar enabled/disabled states for no selection, finished selection, paused selection, active selection, and mixed checked selection.
  - Status bar totals and idle/active speed display.
- Add Download window:
  - Empty URL, invalid URL, valid URL, probe checking, probe success, probe failure, manual file/folder/category edits, paste button, reset folder, Add, Download, Cancel.
- Settings window:
  - Browser integration stopped/running state.
  - Token masking/copy/regenerate alert.
  - General toggles and error notification path if available.
- Tray lifecycle:
  - Start normally, start minimized, hide/show from tray, open Add Download from tray, open Settings from tray.

### Design checklist

- Use the GPUI Kit design review checklist:
  - Primary task and next action are clear.
  - Every action label matches its result and scope.
  - Strong color and primary buttons are scarce.
  - Peers share alignment spines and consistent spacing.
  - Standard components retain keyboard, focus, disabled, and overlay behavior.
  - Empty/loading/failure states remain useful.
  - Focus remains visible and overlays restore focus.
  - Light theme works at minimum window size and with larger base font.

### Regression risks to watch

- Light theme may reveal low-contrast status, border, disabled, or selected states.
- Direct pixel geometry in table rows may need adjustment if row cards become larger or more rounded.
- A black primary token can affect every primary component; verify `New Download`, `Download`, and alert buttons still communicate the correct hierarchy.
- Sidebar selected-state contrast must be sufficient without reverting to saturated fills.
- The app must remain data-dense enough for a download manager despite the more comfortable Sleeve reference.
