-- Durable record of the posts a user actually saw, per account.
--
-- Dedup used to rest on two layers, and neither one survived a page reload:
--
--   * `feed_session_posts` is keyed by `session_id`, and the browser mints
--     that id inside a `use_state` hook (`feed-<ts>-<rand>`), so a reload
--     starts a brand new one. `prune_expired_sessions` then deletes the old
--     rows after `FEED_SESSION_TTL_MIN` (30 minutes).
--   * `feed_interactions` is durable, but the `qualified_impression` that
--     proves a card was on screen sits in an 800 ms debounce buffer in the
--     browser, so a reload loses whatever was in flight.
--
-- A redeploy is always a reload, so both layers emptied at the same moment.
-- The candidate pool is deterministic (fixed IDF anchors, cached first page),
-- so the same posts came back on the very next request and at the top of the
-- list — the "good posts never leave the feed" report.
--
-- This table closes that hole without guessing: it is written from the
-- client's own confirmation (`/interaction/batch`), in the same transaction as
-- the interaction row, and keyed by nothing the browser can forget. The feed
-- response path deliberately does NOT write here — a response means "the
-- server selected this post", not "a human saw it". Recording the selection
-- instead of the sighting is wrong in the other direction: the browser renders
-- a window of the list, so posts the user never reached would be hidden for
-- weeks, which is not what "don't show me what I've seen" means.
--
-- `shown_count` counts confirmed sightings, so a post that keeps coming back
-- can be told apart from one that was glanced at once.
--
-- No FOREIGN KEY on `post_id`: catalog persistence is fire-and-forget, so a
-- live post can be interacted with before its `posts` row exists. The cascade
-- on `accounts` stays — deleting an account must not leave its exposure
-- history behind.
CREATE TABLE feed_shown_posts (
    account_id INTEGER NOT NULL,
    post_id INTEGER NOT NULL,
    first_shown_at TEXT NOT NULL,
    last_shown_at TEXT NOT NULL,
    shown_count INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (account_id, post_id),
    FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

-- Pruning is by age alone (`last_shown_at < cutoff`), across all accounts, so
-- it needs its own index: the primary key leads on `account_id` and cannot
-- serve that scan.
CREATE INDEX idx_feed_shown_posts_last ON feed_shown_posts(last_shown_at);
