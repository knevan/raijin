// Adapted from domenkozar/gpui-tray (Apache-2.0).

mod backend;
mod error;
mod icon;
mod menu;
mod tray;

pub use error::{Error, Result};
pub use icon::Icon;
pub use tray::Tray;
