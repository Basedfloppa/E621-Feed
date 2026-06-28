-- Prefetch cooldown: track when each account was last prefetched so the
-- background worker avoids hammering the same tags on every tick.
ALTER TABLE accounts ADD COLUMN last_prefetched_at TEXT DEFAULT '';
