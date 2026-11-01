-- Watchlist: a tag an account asked to be told about, plus the high-water mark
-- of what it has already seen.
--
-- `last_seen_post_id` is seeded with the newest post that already carries the
-- tag at subscribe time: a fresh subscription must not report the whole catalog
-- as "new". Post ids are the clock because they are monotonic under e621's
-- assignment and the feed already walks them; `created_at` would make a
-- backfilled post look new forever.
CREATE TABLE account_subscriptions (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    last_seen_post_id INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account_id, tag_id)
) STRICT;

-- Listing a user's watchlist is the only hot read, and it is always scoped.
CREATE INDEX idx_subs_account ON account_subscriptions(account_id);
