CREATE TABLE tag_cooccurrence (
    tag1_id INTEGER NOT NULL,
    tag2_id INTEGER NOT NULL,
    cooc_count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (tag1_id, tag2_id),
    FOREIGN KEY (tag1_id) REFERENCES tags(id) ON DELETE CASCADE,
    FOREIGN KEY (tag2_id) REFERENCES tags(id) ON DELETE CASCADE,
    CHECK (tag1_id < tag2_id)
) STRICT;

CREATE INDEX idx_tag_cooc_tag2 ON tag_cooccurrence(tag2_id);

CREATE TABLE account_tag_cooccurrence (
    account_id INTEGER NOT NULL,
    tag1_name  TEXT NOT NULL,
    tag1_group TEXT NOT NULL,
    tag2_name  TEXT NOT NULL,
    tag2_group TEXT NOT NULL,
    cooc_count INTEGER NOT NULL,
    PRIMARY KEY (account_id, tag1_name, tag1_group, tag2_name, tag2_group),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_atc_a_first  ON account_tag_cooccurrence(account_id, tag1_name, tag1_group);
CREATE INDEX idx_atc_a_second ON account_tag_cooccurrence(account_id, tag2_name, tag2_group);

-- Backfill the global pair graph from existing tags_posts data.
INSERT INTO tag_cooccurrence (tag1_id, tag2_id, cooc_count)
SELECT tp1.tag_id, tp2.tag_id, COUNT(*)
FROM tags_posts tp1
INNER JOIN tags_posts tp2
    ON tp1.post_id = tp2.post_id
   AND tp1.tag_id < tp2.tag_id
GROUP BY tp1.tag_id, tp2.tag_id;

-- Backfill per-account pair graphs over each account's favorite posts.
INSERT INTO account_tag_cooccurrence (
    account_id, tag1_name, tag1_group, tag2_name, tag2_group, cooc_count
)
SELECT
    ap.account_id,
    t1.name, t1.group_type,
    t2.name, t2.group_type,
    COUNT(*)
FROM accounts_post ap
INNER JOIN tags_posts tp1 ON tp1.post_id = ap.post_id
INNER JOIN tags_posts tp2
    ON tp2.post_id = ap.post_id
   AND tp1.tag_id < tp2.tag_id
INNER JOIN tags t1 ON t1.id = tp1.tag_id
INNER JOIN tags t2 ON t2.id = tp2.tag_id
GROUP BY ap.account_id, t1.name, t1.group_type, t2.name, t2.group_type;
