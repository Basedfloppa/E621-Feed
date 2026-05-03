-- Cleanup worker filters posts by `last_seen_at < cutoff`. Without an
-- index this devolves into a full table scan on a multi-GB `posts`
-- table, holding the writer lock for tens of seconds and starving
-- concurrent writers (prefetch, /process, feedback) past
-- `busy_timeout`. Partial index excludes the noisy "fresh" tail so the
-- cleanup-time scan is small even on a large catalog.
CREATE INDEX IF NOT EXISTS idx_posts_last_seen_at
    ON posts(last_seen_at);
