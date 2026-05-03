-- Composite index used by the "recent popular" candidate stream:
--   SELECT id FROM posts
--   WHERE rating IN (...) AND created_at > ?
--   ORDER BY score_total DESC LIMIT N
-- The existing idx_posts_rating_created_at supports the WHERE but not the
-- ORDER BY, forcing a sort. This index lets the planner walk in score order.
CREATE INDEX IF NOT EXISTS idx_posts_score_created
    ON posts(score_total DESC, created_at DESC);

-- Used by the "top-tag" candidate streams to find posts that carry a given
-- tag, ranked by quality. Joined from tags_posts(tag_id) → posts(id), where
-- the existing idx_tp_tag is already adequate for the join — but we also want
-- to filter newly discovered candidates by recency, which the post pk alone
-- doesn't give us cheaply.
CREATE INDEX IF NOT EXISTS idx_posts_id_score_created
    ON posts(id, score_total DESC, created_at DESC);

-- Cheap "have we already shown this to the user" lookup. The current
-- idx_feed_interactions_account_post is (account_id, post_id) — perfect for
-- "did they see THIS post" but not for "give me everything they saw" which is
-- what the candidate filter needs.
CREATE INDEX IF NOT EXISTS idx_feed_interactions_account_event_post
    ON feed_interactions(account_id, event_type, post_id);

-- A/B bucket assignment for online experiments. NULL = "not bucketed yet";
-- the recommendations endpoint hashes the account_id on the fly when this
-- column is NULL so we don't need a backfill on existing accounts.
ALTER TABLE accounts
    ADD COLUMN experiment_bucket TEXT;

-- Persist the bucket alongside each interaction so offline analysis can
-- compare CTR / hide-rate per arm without joining to a possibly-mutated
-- accounts.experiment_bucket.
ALTER TABLE feed_interactions
    ADD COLUMN experiment_bucket TEXT;

-- Posts in the local catalog need enough fields to be re-served as feed
-- candidates without a fresh e621 round-trip. score_total alone isn't enough
-- (the frontend renders ↑/↓ separately), and we have no way to render the
-- thumbnail without at least the preview URL.
ALTER TABLE posts ADD COLUMN preview_url TEXT;
ALTER TABLE posts ADD COLUMN sample_url  TEXT;
ALTER TABLE posts ADD COLUMN file_url    TEXT;
ALTER TABLE posts ADD COLUMN score_up    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE posts ADD COLUMN score_down  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE posts ADD COLUMN sample_width  INTEGER;
ALTER TABLE posts ADD COLUMN sample_height INTEGER;
ALTER TABLE posts ADD COLUMN preview_width  INTEGER;
ALTER TABLE posts ADD COLUMN preview_height INTEGER;
