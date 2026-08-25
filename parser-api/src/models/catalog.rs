//! Local-catalog + media queue response types (docs/offline-catalog.md).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Status summary for the background media worker / download queue.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MediaQueueStatus {
    pub paused: bool,
    pub pending: i64,
    pub stored: i64,
    pub bytes: i64,
}

/// One queued item (a saved post still awaiting its original download).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MediaQueueItem {
    pub post_id: i64,
    pub file_url: String,
}

/// Generic "action succeeded" response for catalog-manage mutations.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActionOk {
    pub ok: bool,
}
