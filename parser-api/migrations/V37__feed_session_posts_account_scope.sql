-- Scope the intra-scroll dedup set by account, not just by session id.
--
-- `feed_sessions` has had a composite PRIMARY KEY `(session_id, account_id)`
-- since V22, and both session functions check that the session belongs to the
-- account they were handed. `feed_session_posts` did not: V22 had to drop its
-- foreign key (`session_id` stopped being unique), and rebuilt it as
-- `PRIMARY KEY (session_id, post_id)` with no account column. Reads and writes
-- reach it through a join on `feed_sessions`, which is correct only while one
-- session id belongs to exactly one account.
--
-- It does not. The browser mints `feed-<ts>-<rand>` per tab inside a
-- `use_state`, and switching the account select does not remount the page, so
-- account A and account B can share one id. Then:
--
--   * B's request reads A's shown posts — the join pins `fs.account_id = B`
--     but not `fp.account_id`, because there is no such column — and the first
--     page of B silently loses posts that A happened to be shown;
--   * `clear_feed_interactions` for A deletes by `session_id IN (…)`, taking
--     B's scroll set with it.
--
-- Carrying the account in the key makes the row mean what every caller already
-- assumes: "this account was shown this post in this scroll".
--
-- Rows are backfilled by joining `feed_sessions`; rows whose session no longer
-- exists are dropped, since nothing can ever address them again (the previous
-- code kept them as orphans and swept them on a timer).
--
-- A row written before this migration has no account of its own, so an id
-- shared by two accounts cannot be attributed to either. The join copies such
-- a row to **every** account that owned the id. That is the safer direction:
-- the set is read only for the session that is live right now and is deleted
-- after `FEED_SESSION_TTL_MIN`, so the cost is one page of over-filtering in a
-- scroll that is already in progress, whereas the other choices (pick one
-- account, or drop the row) would let the just-rendered page come back. A
-- session id is one tab, so in practice this affects at most the handful of
-- sessions alive at upgrade time.
--
-- The composite foreign key replaces the join as the integrity check: deleting
-- a session now deletes exactly its account's rows. V22 could not express that
-- and had to leave the child table unconstrained, which is how the orphan
-- class of bugs started.
CREATE TABLE feed_session_posts_new (
    account_id  INTEGER NOT NULL,
    session_id  TEXT NOT NULL,
    post_id     INTEGER NOT NULL,
    position    INTEGER NOT NULL,
    shown_at    TEXT NOT NULL,
    PRIMARY KEY (account_id, session_id, post_id),
    FOREIGN KEY (session_id, account_id) REFERENCES feed_sessions(session_id, account_id) ON DELETE CASCADE,
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

INSERT OR IGNORE INTO feed_session_posts_new (account_id, session_id, post_id, position, shown_at)
    SELECT fs.account_id, fp.session_id, fp.post_id, fp.position, fp.shown_at
    FROM feed_session_posts fp
    INNER JOIN feed_sessions fs ON fs.session_id = fp.session_id;

DROP TABLE feed_session_posts;
ALTER TABLE feed_session_posts_new RENAME TO feed_session_posts;

-- `PRIMARY KEY (account_id, session_id, post_id)` serves every read (both
-- columns are equality predicates) and the cascade probe, so the old
-- `(session_id)` index — already dropped by V32 — must not come back.
