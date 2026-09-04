// Adapted from domenkozar/gpui-tray (Apache-2.0).

use crate::native_tray::Result;
use crate::native_tray::menu::MenuItemId;
use crate::native_tray::tray::TraySnapshot;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[derive(Clone, Copy, Debug)]
pub(crate) enum BackendEvent {
    MenuItemClicked {
        generation: u64,
        id: MenuItemId,
    },
    /// Primary activation: left click (Windows, Linux `activate`) or
    /// status-item click (macOS). Dispatches the builder's primary action.
    PrimaryAction,
}

pub(crate) struct PlatformTray {
    #[cfg(target_os = "linux")]
    inner: linux::LinuxTray,
    #[cfg(target_os = "macos")]
    inner: macos::MacTray,
    #[cfg(target_os = "windows")]
    inner: windows::WindowsTray,
}

impl PlatformTray {
    pub(crate) fn new(
        snapshot: &TraySnapshot,
        events: async_channel::Sender<BackendEvent>,
    ) -> Result<Self> {
        #[cfg(target_os = "linux")]
        return linux::LinuxTray::new(snapshot, events).map(|inner| Self { inner });
        #[cfg(target_os = "macos")]
        return macos::MacTray::new(snapshot, events).map(|inner| Self { inner });
        #[cfg(target_os = "windows")]
        return windows::WindowsTray::new(snapshot, events).map(|inner| Self { inner });
        #[allow(unreachable_code)]
        Err(crate::native_tray::Error::UnsupportedPlatform)
    }

    pub(crate) fn apply(&mut self, old: &TraySnapshot, new: &TraySnapshot) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        return self.inner.apply(old, new);
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = (old, new);
            Err(crate::native_tray::Error::UnsupportedPlatform)
        }
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        return self.inner.close();
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        Err(crate::native_tray::Error::UnsupportedPlatform)
    }
}
