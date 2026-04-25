-- Canonicalise tag names to lowercase. Pairs with the ingest-time
-- normalisation in `save_posts_tags_batch_inner` (db.rs); together they
-- guarantee the in-memory relation graph (which lowercases on load) and the
-- stored tables agree on tag identity.
--
-- The migration is a no-op on databases that have always been ingested with
-- lowercase tags (the default, which matches the e621 API). On databases
-- with historical mixed-case rows it canonicalises them, ABORTing on
-- collisions so an operator can intervene rather than silently merging
-- counts incorrectly.
--
-- Collision recovery (manual): a collision means both "Foo" and "foo" exist
-- as separate rows in `tags`. The merge needs to redirect tags_posts
-- references and sum any per-account aggregates. This wasn't observed on
-- any deployment we know of, so the merge logic is deferred to a future
-- migration if it becomes necessary.

-- 1. tags.name. UNIQUE (name, group_type) means a collision on lower(name)
--    aborts the whole transaction.
UPDATE tags
   SET name = lower(name)
 WHERE name <> lower(name);

-- 2. account_tag_counts.tag_name. PK includes (account_id, tag_name,
--    group_type) so collisions abort cleanly.
UPDATE account_tag_counts
   SET tag_name = lower(tag_name)
 WHERE tag_name <> lower(tag_name);

-- 3. account_tag_feedback.tag_name. Same PK shape.
UPDATE account_tag_feedback
   SET tag_name = lower(tag_name)
 WHERE tag_name <> lower(tag_name);

-- 4. account_tag_cooccurrence.{tag1_name, tag2_name}. Two columns; do
--    them in one update statement so the canonicalisation is atomic
--    relative to the row's PK constraint.
UPDATE account_tag_cooccurrence
   SET tag1_name = lower(tag1_name),
       tag2_name = lower(tag2_name)
 WHERE tag1_name <> lower(tag1_name)
    OR tag2_name <> lower(tag2_name);
