//! Local-catalog models (mirrors the backend `/catalog` endpoints).

use serde::{Deserialize, Serialize};

/// Status summary for the background media worker / download queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaQueueStatus {
    pub paused: bool,
    pub pending: i64,
    pub stored: i64,
    pub bytes: i64,
}
