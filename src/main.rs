#![cfg_attr(
    all(windows, feature = "desktop", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(feature = "desktop")]
fn main() {
    raijin::desktop::run();
}

#[cfg(not(feature = "desktop"))]
fn main() {
    eprintln!("raijin built without desktop feature");
}
