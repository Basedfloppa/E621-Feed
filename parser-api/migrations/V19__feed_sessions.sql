-- Session-based feed continuation: cursor + dedup set for each session.
-- Each session tracks which posts were shown so continuation requests
-- can skip them. TTL is managed by the server (pruner).

CREATE TABLE IF NOT EXISTS feed_sessions (
    session_id      TEXT PRIMARY KEY,
    account_id      INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    last_accessed_at TEXT NOT NULL,
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

CREATE TABLE IF NOT EXISTS feed_session_posts (
    session_id  TEXT NOT NULL,
    post_id     INTEGER NOT NULL,
    position    INTEGER NOT NULL,
    shown_at    TEXT NOT NULL,
    PRIMARY KEY (session_id, post_id),
    FOREIGN KEY (session_id) REFERENCES feed_sessions(session_id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_feed_session_posts_session ON feed_session_posts(session_id);
CREATE INDEX IF NOT EXISTS idx_feed_sessions_account ON feed_sessions(account_id);
CREATE INDEX IF NOT EXISTS idx_feed_sessions_accessed ON feed_sessions(last_accessed_at);
