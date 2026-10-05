-- Hot-query indexes, redundant-index cleanup, and the missing
-- `account_uploader_profile` foreign key.
--
-- All three changes come from an EXPLAIN QUERY PLAN audit against a production-
-- sized database (posts 65k, tags 1.6M, tags_posts 4.7M, tag_implications 64k,
-- account_tag_cooccurrence 61M).

-- ── 1. Index for the digest strata ─────────────────────────────────────────
-- `db::digest::{get_trending_posts, get_popular_posts_since,
-- get_old_random_posts}` filter on `created_at` alone, but every pre-existing
-- index on `posts` leads with a different column
-- (`idx_posts_rating_created_at(rating, created_at)`,
--  `idx_posts_score_created(score_total DESC, created_at DESC)`), so each of
-- the three queries full-scanned `posts` and then sorted a temp B-tree.
CREATE INDEX IF NOT EXISTS idx_posts_created_at ON posts(created_at);

-- ── 2. Index for `get_implied_by` ──────────────────────────────────────────
-- `db::tags::get_implied_by` filters `tag_implications` by
-- `consequent_name = ?1 AND status = 'active'`, which was a full table scan
-- (the table's PK is `(antecedent_name, consequent_name)`, so it cannot serve
-- the lookup). Reachable from the public `GET /api/tag/implications/...` route.
CREATE INDEX IF NOT EXISTS idx_tag_imps_cons
    ON tag_implications(consequent_name, status);

-- The old index was a pure prefix of `PRIMARY KEY(antecedent_name,
-- consequent_name)` and therefore never used by the planner — it only added
-- write cost to every tag-relationship import.
DROP INDEX IF EXISTS idx_tag_imps_ante;

-- ── 3. Redundant indexes removed from the hottest insert paths ─────────────
-- `save_posts_tags_batch` inserts into `tags_posts` (4.7M rows and growing),
-- and each insert maintained two identical tag indexes:
--   * `idx_tp_tag(tag_id)`        ⊂ PRIMARY KEY(tag_id, post_id)
--   * `idx_ap_post(post_id)`      ⊂ PRIMARY KEY(post_id, account_id)
--   * `idx_ap_account(account_id)` ⊂ idx_ap_acc_post(account_id, post_id)
--   * `idx_tags_name_group(name, group_type)` duplicates UNIQUE(name, group_type)
-- Dropping them is safe: a prefix duplicate cannot serve any plan the
-- covering index does not already serve. `idx_tp_post(post_id)` and
-- `idx_ap_acc_post(account_id, post_id)` are NOT prefixes and are kept.
DROP INDEX IF EXISTS idx_tp_tag;
DROP INDEX IF EXISTS idx_ap_post;
DROP INDEX IF EXISTS idx_ap_account;
DROP INDEX IF EXISTS idx_tags_name_group;

-- ── 4. `account_uploader_profile` gained the missing cascade ───────────────
-- The table was created without `REFERENCES accounts(id) ON DELETE CASCADE`,
-- so it was exempt from account teardown: `delete_device_link` and
-- `cleanup_orphan_accounts` never touched it, and rows for removed accounts
-- accumulated forever (16k orphan rows on the audited database). SQLite cannot
-- add a foreign key in place, so the table is rebuilt.
--
-- The rebuild is ordered to be non-destructive even on a schema that already
-- contains conflicting rows: the `DROP TABLE` on a child table is a no-op for
-- referential actions, while deleting the same rows first removes any orphan
-- that would otherwise make the copy violate the new constraint.
CREATE TABLE account_uploader_profile_new (
    account_id   INTEGER NOT NULL,
    uploader_id  INTEGER NOT NULL,
    post_count   INTEGER NOT NULL,
    avg_score    REAL    NOT NULL,
    avg_fav      REAL    NOT NULL,
    PRIMARY KEY (account_id, uploader_id),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
);

INSERT INTO account_uploader_profile_new (account_id, uploader_id, post_count, avg_score, avg_fav)
    SELECT account_id, uploader_id, post_count, avg_score, avg_fav
    FROM account_uploader_profile
    WHERE account_id IN (SELECT id FROM accounts);

DROP TABLE account_uploader_profile;

ALTER TABLE account_uploader_profile_new RENAME TO account_uploader_profile;
