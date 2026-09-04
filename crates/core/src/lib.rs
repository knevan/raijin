#![deny(
    clippy::panic,
    clippy::panicking_unwrap,
    clippy::redundant_clone,
    clippy::implicit_clone,
    clippy::perf,
    clippy::large_types_passed_by_value,
    clippy::large_futures,
    clippy::trivially_copy_pass_by_ref,
    clippy::clone_on_ref_ptr,
)]
// Tests may panic on violated assumptions; the deny above stays for production code.
#![cfg_attr(test, allow(clippy::panic))]

pub mod config;
pub mod db;
pub mod download;
pub mod integration;
pub mod monitor;
pub mod platform;
pub mod queue;

/// Initializes process-wide tracing subscriber for headless and UI binaries.
///
/// Safe to call once at startup; subsequent calls are ignored.
pub fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}
