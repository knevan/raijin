// Adapted from domenkozar/gpui-tray (Apache-2.0).

use std::sync::Arc;

use crate::native_tray::{Error, Result};

/// A platform-neutral tray icon stored as non-premultiplied RGBA8 pixels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Icon {
    rgba: Arc<[u8]>,
    width: u32,
    height: u32,
}

impl Icon {
    /// Creates an icon from tightly packed, row-major RGBA8 pixels.
    pub fn from_rgba(rgba: impl Into<Arc<[u8]>>, width: u32, height: u32) -> Result<Self> {
        let rgba = rgba.into();
        let expected = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or_else(|| Error::InvalidIcon("dimensions overflow".into()))?;

        if width == 0 || height == 0 {
            return Err(Error::InvalidIcon("dimensions must be non-zero".into()));
        }
        if rgba.len() != expected {
            return Err(Error::InvalidIcon(format!(
                "expected {expected} RGBA bytes for {width}x{height}, got {}",
                rgba.len()
            )));
        }

        Ok(Self {
            rgba,
            width,
            height,
        })
    }

    pub(crate) fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub(crate) const fn width(&self) -> u32 {
        self.width
    }

    pub(crate) const fn height(&self) -> u32 {
        self.height
    }

    /// Byte access for shell-level tests (icon shape assertions).
    #[cfg(test)]
    pub(crate) fn test_bytes(&self) -> &[u8] {
        &self.rgba
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports: a `use super::*` glob would shadow the built-in
    // `#[test]` attribute with GPUI's `test` macro.
    use super::Icon;

    #[test]
    fn validates_pixel_length() {
        assert!(Icon::from_rgba(vec![0; 16], 2, 2).is_ok());
        assert!(Icon::from_rgba(vec![0; 15], 2, 2).is_err());
        assert!(Icon::from_rgba(Vec::<u8>::new(), 0, 0).is_err());
    }
}
