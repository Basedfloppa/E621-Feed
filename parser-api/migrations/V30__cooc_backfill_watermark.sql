-- Resumable global tag-cooccurrence backfill (see db/cooccurrence_backfill.rs).
--
-- The V9 backfill always restarted from `post_id = -1` and decided whether to
-- run at all from `COUNT(*) FROM tag_cooccurrence > 0`. Individually those are
-- reasonable; together they are unsafe. The build takes hours on a seeded
-- database, and an interrupted one (Ctrl-C, OOM, reboot) leaves the table
-- NON-empty, so the next run reads "already built", skips the pass, and keeps a
-- graph holding only the pairs of the lowest post ids. `calibrate` then scores
-- every `tag_relation_*` knob against a truncated graph and reports them inert
-- with nothing in the logs to explain it.
--
-- `last_post_id` is that watermark: every post at or below it is fully counted.
-- The backfill advances it inside the SAME transaction as the pair upserts it
-- accounts for, so across a crash the watermark can neither run ahead (losing a
-- batch) nor lag behind (double-counting one).
--
-- Databases arriving here with a populated `tag_cooccurrence` are seeded at the
-- newest post id, i.e. treated as FINISHED. That is exactly the pre-V30 reading
-- of a non-empty table, and pushing every existing install through a
-- multi-hour rebuild on upgrade would be a worse bug than the one being fixed.
-- The cost is that a graph already truncated by pre-V30 code stays truncated;
-- to rebuild one deliberately:
--
--     DELETE FROM tag_cooccurrence;
--     UPDATE tag_cooccurrence_backfill SET last_post_id = -1;

CREATE TABLE IF NOT EXISTS tag_cooccurrence_backfill (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    last_post_id INTEGER NOT NULL DEFAULT -1
) STRICT;

INSERT OR IGNORE INTO tag_cooccurrence_backfill (id, last_post_id)
SELECT 1,
       CASE WHEN EXISTS(SELECT 1 FROM tag_cooccurrence)
            THEN COALESCE((SELECT MAX(post_id) FROM tags_posts), -1)
            ELSE -1
       END;
