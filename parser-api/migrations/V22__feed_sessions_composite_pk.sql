-- Fix feed_sessions to support per-account session isolation.
-- The original V19 used session_id as the sole PRIMARY KEY, which means
-- two different accounts using the same session_id string would conflict
-- (INSERT ... ON CONFLICT(session_id) DO NOTHING silently discards the
-- second account's row). This breaks parallel test isolation and
-- multi-account deployments where different accounts share session_id
-- namespaces.
--
-- Migration strategy:
--   1. Drop feed_session_posts FK reference first (session_id won't be
--      unique after the schema change, so the FK would fail).
--   2. Rename feed_sessions → feed_sessions_old.
--   3. Create new feed_sessions with composite PK.
--   4. Migrate data.
--   5. Recreate feed_session_posts without the stale FK.
--   6. Clean up temp table.

-- Step 1: Recreate feed_session_posts without the FK (session_id is no
-- longer a unique/primary key in feed_sessions after this migration).
CREATE TABLE feed_session_posts_tmp (
    session_id  TEXT NOT NULL,
    post_id     INTEGER NOT NULL,
    position    INTEGER NOT NULL,
    shown_at    TEXT NOT NULL,
    PRIMARY KEY (session_id, post_id)
);

INSERT OR IGNORE INTO feed_session_posts_tmp (session_id, post_id, position, shown_at)
    SELECT session_id, post_id, position, shown_at FROM feed_session_posts;

DROP TABLE feed_session_posts;

-- Step 2: Rename old feed_sessions to feed_sessions_old
ALTER TABLE feed_sessions RENAME TO feed_sessions_old;

-- Step 3: Create new feed_sessions with composite PK
CREATE TABLE feed_sessions (
    session_id      TEXT NOT NULL,
    account_id      INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    last_accessed_at TEXT NOT NULL,
    PRIMARY KEY (session_id, account_id),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

-- Step 4: Migrate data
INSERT OR IGNORE INTO feed_sessions (session_id, account_id, created_at, last_accessed_at)
    SELECT session_id, account_id, created_at, last_accessed_at FROM feed_sessions_old;

-- Step 5: Recreate feed_session_posts (without FK — see Step 1)
ALTER TABLE feed_session_posts_tmp RENAME TO feed_session_posts;

-- Step 6: Drop old indexes then recreate (V19 may have created them already)
DROP INDEX IF EXISTS idx_feed_sessions_account;
DROP INDEX IF EXISTS idx_feed_sessions_accessed;
CREATE INDEX idx_feed_sessions_account ON feed_sessions(account_id);
CREATE INDEX idx_feed_sessions_accessed ON feed_sessions(last_accessed_at);

-- Step 7: Clean up temp table
DROP TABLE IF EXISTS feed_sessions_old;
