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
