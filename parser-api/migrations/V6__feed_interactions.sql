CREATE TABLE feed_interactions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL,
    post_id INTEGER NOT NULL,
    event_type TEXT NOT NULL CHECK (event_type IN ('qualified_impression', 'open', 'hide')),
    position INTEGER NOT NULL DEFAULT 0,
    session_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(account_id, post_id, event_type, session_id),
    FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE,
    FOREIGN KEY(post_id) REFERENCES posts(id) ON DELETE CASCADE
) STRICT;

CREATE TABLE account_tag_feedback (
    account_id INTEGER NOT NULL,
    tag_name TEXT NOT NULL,
    group_type TEXT NOT NULL,
    impression_count INTEGER NOT NULL DEFAULT 0,
    positive_count INTEGER NOT NULL DEFAULT 0,
    negative_count INTEGER NOT NULL DEFAULT 0,
    last_interaction_at TEXT NOT NULL,
    PRIMARY KEY(account_id, tag_name, group_type),
    FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_feed_interactions_account_created
    ON feed_interactions(account_id, created_at DESC);
CREATE INDEX idx_feed_interactions_account_post
    ON feed_interactions(account_id, post_id);
CREATE INDEX idx_feed_interactions_account_event
    ON feed_interactions(account_id, event_type);
CREATE INDEX idx_account_tag_feedback_account_group
    ON account_tag_feedback(account_id, group_type);