use freya::prelude::*;

pub(crate) const RAIJIN_BACKGROUND: Color = Color::from_rgb(27, 28, 32);
pub(crate) const SURFACE: Color = Color::from_rgb(31, 32, 36);
pub(crate) const SURFACE_ELEVATED: Color = Color::from_rgb(38, 39, 43);
pub(crate) const BORDER: Color = Color::from_rgb(48, 50, 56);
pub(crate) const ACCENT: Color = Color::from_rgb(90, 178, 255);
pub(crate) const TEXT_PRIMARY: Color = Color::from_rgb(226, 228, 232);
pub(crate) const TEXT_MUTED: Color = Color::from_rgb(176, 178, 184);
pub(crate) const TEXT_SUBTLE: Color = Color::from_rgb(146, 148, 156);
pub(crate) const HANDLE: Color = Color::from_rgb(36, 37, 42);
pub(crate) const HANDLE_HOVER: Color = Color::from_rgb(66, 68, 76);

pub(crate) fn raijin_theme() -> Theme {
    let mut theme = dark_theme();
    theme.name = "raijin-dark";
    theme.colors = ColorsSheet {
        primary: ACCENT,
        background: RAIJIN_BACKGROUND,
        surface_primary: SURFACE,
        surface_secondary: SURFACE_ELEVATED,
        border: BORDER,
        border_focus: ACCENT,
        text_primary: TEXT_PRIMARY,
        text_secondary: TEXT_MUTED,
        active: Color::from_rgb(45, 47, 53),
        ..DARK_COLORS
    };
    theme.set(
        "resizable_handle",
        ResizableHandleThemePreference {
            background: Preference::Specific(HANDLE),
            hover_background: Preference::Specific(HANDLE_HOVER),
            corner_radius: CornerRadius::new_all(0.).into(),
        },
    );
    theme
}
