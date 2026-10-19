-- Make the per-account co-occurrence aggregate index-only.
--
-- `db::feed::load_candidate_pool_input` folds both members of every pair with a
-- `UNION ALL` and groups by partner. On an account holding 2.7M pairs that is
-- ~5.5M rows, and with `(account_id, cooc_count)` as the only index every one
-- of them needed a table fetch for `tag1_id`/`tag2_id`. Measured on the
-- production catalog (2026-10-19, account 18044): the aggregate cost 0.77 s
-- when it still joined `tags` per row and 0.56 s after switching to an id-only
-- aggregate. This index removes the remaining table lookups; the temp b-tree
-- for `GROUP BY` stays, because `cooc_count >= 2` admits nearly every row.
--
-- The old index is a strict prefix of this one, so it cannot serve any plan the
-- new index does not. Keeping both would only add write cost to the
-- incremental per-pair upserts (`upsert_account_cooccurrence_pairs`), which are
-- on the `/process` hot path.
CREATE INDEX idx_atc_account_cooc_cover
    ON account_tag_cooccurrence(account_id, cooc_count, tag1_id, tag2_id);

DROP INDEX IF EXISTS idx_atc_account_cooc;
