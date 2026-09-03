//! Persisted queue model.
//!
//! Queue manager and scheduler live in M3, currently ON HOLD per `AGENTS.md`.
//! This module exposes only the durable types needed by `db` repositories.

pub mod model;

pub use model::{Queue, QueueItem};
