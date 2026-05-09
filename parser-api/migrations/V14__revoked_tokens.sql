-- Persistent denylist of revoked owner tokens. `auth.rs` keeps a hot
-- in-memory HashSet mirror for O(1) per-request lookup; this table is
-- the source of truth, reloaded on startup and after each prune cycle.
-- `revoked_at` is unix-seconds; the pruner drops rows older than
-- `OWNER_TOKEN_MAX_AGE_DAYS + buffer`.
CREATE TABLE IF NOT EXISTS revoked_tokens (
    token       TEXT PRIMARY KEY,
    revoked_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_revoked_tokens_revoked_at
    ON revoked_tokens(revoked_at);
