pub mod config;
pub mod db;
pub mod download;
pub mod integration;
pub mod monitor;
pub mod platform;
pub mod queue;

#[cfg(feature = "desktop")]
pub mod desktop;

#[cfg(feature = "desktop")]
pub mod ui;
