use std::num::NonZeroU16;

use serde::{Deserialize, Serialize};

use crate::download::{DownloadId, QueueId};

/// Persisted download queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    /// Stable persisted identifier.
    pub id: QueueId,
    /// Human-readable queue name.
    pub name: String,
    /// Maximum active downloads allowed for this queue.
    pub max_concurrent: NonZeroU16,
    /// Whether the queue should stop itself after all items finish.
    pub stop_on_empty: bool,
    /// Serialized schedule configuration, if any.
    pub schedule_json: Option<String>,
    /// Creation timestamp as Unix milliseconds.
    pub created_at: i64,
    /// Last update timestamp as Unix milliseconds.
    pub updated_at: i64,
}

/// Persisted queue membership and ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    /// Parent queue identifier.
    pub queue_id: QueueId,
    /// Queued download identifier.
    pub download_id: DownloadId,
    /// Zero-based position inside the queue.
    pub position: u32,
}
