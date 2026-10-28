use gpui_kit::component::{Colorize as _, Theme};
use gpui_kit::*;

// Sketch variables (`sketch/raijin.pen` → `variables`).
const BG: &str = "#0C1626";
const PANEL: &str = "#12233A";
const PANEL2: &str = "#1A3252";
const BORDER: &str = "#274863";
const TEXT_PRIMARY: &str = "#EDF4FB";
const TEXT_SECONDARY: &str = "#9FB6CC";
const TEXT_MUTED: &str = "#5F7B95";
const ACCENT: &str = "#14D9B8";
const ACCENT2: &str = "#FFB224";
const GREEN: &str = "#2DD4A7";
const BLUE: &str = "#5AA9FF";
const CTA_INK: &str = "#06281F";
const ROW_CARD_BG: &str = "#181D24";
const ROW_CARD_HOVER_BG: &str = "#16253B";
const ROW_CARD_SELECTED_BG: &str = "#1A3252";

/// Parses a sketch hex color. Panics only for hardcoded constants, which are
/// covered by unit tests below, so a failure always means a typo in this file.
fn hx(hex: &'static str) -> Hsla {
    Hsla::parse_hex(hex).expect("sketch hex color must parse")
}

/// Card background for table rows in the hybrid card-table design.
/// Slate-grey, distinct from the `#0C1626` window background and `#12233A` header.
pub fn row_card_bg() -> Hsla {
    hx(ROW_CARD_BG)
}

/// Card background for a selected table row in the hybrid card-table design.
/// Navy blue, matching the hover hue (`#1A3252`).
pub fn row_card_selected_bg() -> Hsla {
    hx(ROW_CARD_SELECTED_BG)
}

/// Applies the Raijin product palette to the global theme.
///
/// Must be called on the GPUI thread after `gpui_kit::init`, on top of the
/// dark preset. Mutating the global theme directly requires
/// [`Theme::sync_base`] so Base-owned scrollbars and resize handles pick up
/// the new projection.
pub fn apply_product_theme(cx: &mut App) {
    // `ThemeColor` is `Copy`: patch a snapshot, then write it back together
    // with the re-projected token surface (avoids simultaneous borrows).
    let mut colors = Theme::global_mut(cx).colors;
    colors.background = hx(BG);
    colors.foreground = hx(TEXT_PRIMARY);
    colors.border = hx(BORDER);
    colors.input = hx(BG);
    colors.primary = hx(ACCENT);
    colors.primary_foreground = hx(CTA_INK);
    colors.primary_hover = hx(ACCENT).lighten(0.08);
    colors.primary_active = hx(ACCENT).darken(0.08);
    colors.accent = hx(ROW_CARD_SELECTED_BG);
    colors.accent_foreground = hx(TEXT_PRIMARY);
    colors.muted = hx(PANEL2);
    colors.muted_foreground = hx(TEXT_MUTED);
    colors.secondary = hx(TEXT_SECONDARY);
    colors.secondary_foreground = hx(TEXT_PRIMARY);
    colors.sidebar = hx(PANEL);
    colors.sidebar_foreground = hx(TEXT_SECONDARY);
    colors.sidebar_border = hx(BORDER);
    colors.sidebar_accent = hx(ACCENT);
    colors.sidebar_accent_foreground = hx(CTA_INK);
    colors.success = hx(GREEN);
    colors.info = hx(BLUE);
    colors.warning = hx(ACCENT2);
    colors.popover = hx(PANEL);
    colors.popover_foreground = hx(TEXT_PRIMARY);
    colors.list = hx(PANEL);
    colors.list_active = hx(ROW_CARD_SELECTED_BG);
    colors.list_hover = hx(ROW_CARD_HOVER_BG);
    colors.table = gpui::transparent_black();
    colors.table_head = hx(PANEL);
    colors.table_head_foreground = hx(TEXT_SECONDARY);
    colors.table_hover = hx(ROW_CARD_HOVER_BG);
    colors.table_active = gpui::transparent_black();
    colors.table_active_border = gpui::transparent_black();
    // Sketch body rows have no separators. Header dividers use the separate
    // `border` token, so zeroing only the row token keeps them intact.
    colors.table_row_border = hx(BORDER).opacity(0.0);
    colors.button_primary = hx(ACCENT);
    colors.button_primary_foreground = hx(CTA_INK);
    colors.button_primary_hover = hx(ACCENT).lighten(0.08);
    colors.button_primary_active = hx(ACCENT).darken(0.08);
    colors.progress_bar = hx(ACCENT);

    let theme = Theme::global_mut(cx);
    theme.colors = colors;
    // Re-project the patched palette into the legacy token surface so
    // `tokens.sidebar/table/progress_bar` stay in sync automatically.
    theme.tokens = colors.into();

    Theme::sync_base(cx);
}

#[cfg(test)]
mod tests {
    // Explicit imports: a `use super::*` glob would shadow the built-in
    // `#[test]` attribute with GPUI's `test` macro.
    use super::{
        ACCENT, ACCENT2, BG, BLUE, BORDER, CTA_INK, GREEN, PANEL, PANEL2, ROW_CARD_BG,
        ROW_CARD_HOVER_BG, ROW_CARD_SELECTED_BG, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY, hx,
    };

    #[test]
    fn sketch_hex_colors_should_parse() {
        for hex in [
            BG,
            PANEL,
            PANEL2,
            BORDER,
            TEXT_PRIMARY,
            TEXT_SECONDARY,
            TEXT_MUTED,
            ACCENT,
            ACCENT2,
            GREEN,
            BLUE,
            CTA_INK,
            ROW_CARD_BG,
            ROW_CARD_HOVER_BG,
            ROW_CARD_SELECTED_BG,
        ] {
            let color = hx(hex);
            assert!(color.a > 0.99, "{hex} must be fully opaque");
        }
    }
}
