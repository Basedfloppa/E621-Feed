-- Cardinality notes (read before scaling concerns kick in):
--   - `tag_cooccurrence` is the unordered cartesian product of tags that
--     ever appeared together on any single post, with `tag1_id < tag2_id`.
--   - Worst case: O(T²) where T is the unique tag count. In practice it's
--     much sparser because most tag pairs never co-occur.
--   - Empirical reference: a catalog of ~10k tags / ~250k posts has
--     ~1-2M pairs after the runtime backfill. Memory footprint at load
--     time is dominated by the in-memory HashMap in `TagRelationGraph`,
--     keyed by (group, name, group, name) — roughly 80-120 bytes per
--     entry on 64-bit, so ~150 MB for 1.5M pairs.
--   - The scorer's `tag_relation_min_cooc` (default 2) is applied at
--     load time to prune pairs below threshold (see `load_global_tag_relation`
--     in db.rs). Heavy-tailed distributions mean this typically eliminates
--     50-80% of rows.
--   - If you push past ~10M pairs, plan to either raise `tag_relation_min_cooc`
--     or move the in-memory graph to a streaming/on-demand model.
--
-- `account_tag_cooccurrence` is per-user and bounded by the user's tag set,
-- which is at most a few thousand even for power users — no cardinality
-- concerns there.

CREATE TABLE IF NOT EXISTS tag_cooccurrence (
    tag1_id INTEGER NOT NULL,
    tag2_id INTEGER NOT NULL,
    cooc_count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (tag1_id, tag2_id),
    FOREIGN KEY (tag1_id) REFERENCES tags(id) ON DELETE CASCADE,
    FOREIGN KEY (tag2_id) REFERENCES tags(id) ON DELETE CASCADE,
    CHECK (tag1_id < tag2_id)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_tag_cooc_tag2 ON tag_cooccurrence(tag2_id);

CREATE TABLE IF NOT EXISTS account_tag_cooccurrence (
    account_id INTEGER NOT NULL,
    tag1_name  TEXT NOT NULL,
    tag1_group TEXT NOT NULL,
    tag2_name  TEXT NOT NULL,
    tag2_group TEXT NOT NULL,
    cooc_count INTEGER NOT NULL,
    PRIMARY KEY (account_id, tag1_name, tag1_group, tag2_name, tag2_group),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_atc_a_first  ON account_tag_cooccurrence(account_id, tag1_name, tag1_group);
CREATE INDEX IF NOT EXISTS idx_atc_a_second ON account_tag_cooccurrence(account_id, tag2_name, tag2_group);

-- Backfill is performed at runtime in a background thread (see
-- `backfill_tag_cooccurrence_if_needed` in db.rs). Doing it here would hold a
-- single SQLite write lock for the entire join across `tags_posts × tags_posts`,
-- blocking the API from starting on databases with many posts.
