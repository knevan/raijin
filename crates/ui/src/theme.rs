use gpui_kit::component::{Colorize as _, Theme};
use gpui_kit::*;

// Sleeve-inspired product theme tokens (`.agents/design/sleeve/DESIGN.md`).
// Raw color values stay centralized here; views consume semantic `cx.theme()` roles.
const CANVAS: &str = "#FFFFFF";
const PAPER: &str = "#F9FAFB";
const INK: &str = "#000000";
const GRAPHITE: &str = "#333333";
const SLATE: &str = "#374151";
const SILVER_MIST: &str = "#E5E7EB";
const SMOKE: &str = "#999999";
const OCEAN: &str = "#0E95EE";
const SUCCESS: &str = "#16A34A";
const WARNING: &str = "#D97706";
const DANGER: &str = "#DC2626";
const ROW_CARD_BG: &str = "#FFFFFF";
const ROW_CARD_HOVER_BG: &str = "#F9FAFB";
const ROW_CARD_SELECTED_BG: &str = "#EFF6FF";

/// Parses a product-theme hex color. Panics only for hardcoded constants, which
/// are covered by unit tests below, so a failure always means a typo here.
fn hx(hex: &'static str) -> Hsla {
    Hsla::parse_hex(hex).expect("product theme hex color must parse")
}

/// Card background for table rows in the hybrid card-table design.
/// Rows float on the white canvas; borders and hover states define their edges.
pub fn row_card_bg() -> Hsla {
    hx(ROW_CARD_BG)
}

/// Card background for a selected table row in the hybrid card-table design.
/// A light blue wash keeps selection visible without turning every row chromatic.
pub fn row_card_selected_bg() -> Hsla {
    hx(ROW_CARD_SELECTED_BG)
}

/// Applies the Raijin product palette to the global theme.
///
/// Must be called on the GPUI thread after `gpui_kit::init`, on top of the
/// light preset. Mutating the global theme directly requires
/// [`Theme::sync_base`] so Base-owned scrollbars and resize handles pick up
/// the new projection.
pub fn apply_product_theme(cx: &mut App) {
    // `ThemeColor` is `Copy`: patch a snapshot, then write it back together
    // with the re-projected token surface (avoids simultaneous borrows).
    let mut colors = Theme::global_mut(cx).colors;
    colors.background = hx(CANVAS);
    colors.foreground = hx(INK);
    colors.border = hx(SILVER_MIST);
    colors.input = hx(CANVAS);
    colors.primary = hx(INK);
    colors.primary_foreground = hx(CANVAS);
    colors.primary_hover = hx(GRAPHITE);
    colors.primary_active = hx(SLATE);
    colors.accent = hx(OCEAN);
    colors.accent_foreground = hx(CANVAS);
    colors.muted = hx(PAPER);
    colors.muted_foreground = hx(SMOKE);
    colors.secondary = hx(SLATE);
    colors.secondary_foreground = hx(INK);
    colors.sidebar = hx(PAPER);
    colors.sidebar_foreground = hx(GRAPHITE);
    colors.sidebar_border = hx(SILVER_MIST);
    colors.sidebar_accent = hx(CANVAS);
    colors.sidebar_accent_foreground = hx(INK);
    colors.success = hx(SUCCESS);
    colors.info = hx(OCEAN);
    colors.warning = hx(WARNING);
    colors.danger = hx(DANGER);
    colors.table = gpui::transparent_black();
    colors.table_head = hx(PAPER);
    colors.table_head_foreground = hx(SLATE);
    colors.table_hover = hx(ROW_CARD_HOVER_BG);
    colors.table_active = gpui::transparent_black();
    colors.table_active_border = gpui::transparent_black();
    // Floating card rows own their border; the table's row separator stays out
    // of the way so it does not double the hairline on light surfaces.
    colors.table_row_border = hx(SILVER_MIST).opacity(0.0);
    colors.button_primary = hx(INK);
    colors.button_primary_foreground = hx(CANVAS);
    colors.button_primary_hover = hx(GRAPHITE);
    colors.button_primary_active = hx(SLATE);
    colors.progress_bar = hx(OCEAN);

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
        CANVAS, DANGER, GRAPHITE, INK, OCEAN, PAPER, ROW_CARD_BG, ROW_CARD_HOVER_BG,
        ROW_CARD_SELECTED_BG, SILVER_MIST, SLATE, SMOKE, SUCCESS, WARNING, hx,
    };

    #[test]
    fn product_theme_hex_colors_should_parse() {
        for hex in [
            CANVAS,
            PAPER,
            INK,
            GRAPHITE,
            SLATE,
            SILVER_MIST,
            SMOKE,
            OCEAN,
            SUCCESS,
            WARNING,
            DANGER,
            ROW_CARD_BG,
            ROW_CARD_HOVER_BG,
            ROW_CARD_SELECTED_BG,
        ] {
            let color = hx(hex);
            assert!(color.a > 0.99, "{hex} must be fully opaque");
        }
    }
}
