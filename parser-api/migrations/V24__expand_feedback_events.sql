-- SQLite cannot alter a CHECK constraint in place. Rebuild the interaction
-- table to add explicit positive-feedback events while preserving all rows.
PRAGMA foreign_keys = OFF;

CREATE TABLE feed_interactions_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL,
    post_id INTEGER NOT NULL,
    event_type TEXT NOT NULL CHECK (event_type IN ('qualified_impression', 'open', 'like', 'strong_like', 'hide')),
    position INTEGER NOT NULL DEFAULT 0,
    session_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    experiment_bucket TEXT,
    UNIQUE(account_id, post_id, event_type, session_id),
    FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE,
    FOREIGN KEY(post_id) REFERENCES posts(id) ON DELETE CASCADE
) STRICT;

INSERT INTO feed_interactions_new
    (id, account_id, post_id, event_type, position, session_id, created_at, experiment_bucket)
SELECT id, account_id, post_id, event_type, position, session_id, created_at, experiment_bucket
FROM feed_interactions;

DROP TABLE feed_interactions;
ALTER TABLE feed_interactions_new RENAME TO feed_interactions;

CREATE INDEX idx_feed_interactions_account_created
    ON feed_interactions(account_id, created_at DESC);
CREATE INDEX idx_feed_interactions_account_post
    ON feed_interactions(account_id, post_id);
CREATE INDEX idx_feed_interactions_account_event
    ON feed_interactions(account_id, event_type);
CREATE INDEX idx_feed_interactions_account_event_post
    ON feed_interactions(account_id, event_type, post_id);

PRAGMA foreign_keys = ON;
