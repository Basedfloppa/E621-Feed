//! Wire shape of a watchlist entry (`/api/account/<id>/subscriptions`).

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct Subscription {
    pub tag_id: i64,
    pub tag_name: String,
    pub group_type: String,
    pub created_at: String,
    pub last_seen_post_id: i64,
    /// Non-deleted posts carrying the tag with an id above the mark.
    pub new_count: i64,
}
