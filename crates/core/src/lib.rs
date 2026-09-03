//! Raijin core: UI-free download engine, persistence, and platform helpers.
//!
//! Module boundaries follow `AGENTS.md`:
//! - `config`: desktop settings shape.
//! - `db`: SQLite bootstrap (WAL) and repositories.
//! - `download`: native HTTP engine (manager actor, jobs, probe, splitter).
//! - `queue`: persisted queue model only; manager/scheduler deferred to M3 (ON HOLD).
//! - `monitor`: UI-safe projected state, speed meter, event aggregation.
//! - `integration`: stub until M7; localhost API models only.
//! - `platform`: filesystem paths and `RAIJIN_*` overrides.

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
