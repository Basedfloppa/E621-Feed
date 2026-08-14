-- Index to serve the top-N per-account co-occurrence query without a full
-- table sort.
--
-- `load_account_tag_relation` prunes to the strongest `user_relation_edge_limit`
-- pairs (ORDER BY cooc_count DESC LIMIT n — see TODO §2.2b). Without this
-- index, SQLite still scans and sorts the ENTIRE account co-occurrence table
-- before applying the LIMIT (hundreds of thousands to millions of rows for
-- active accounts — measured ~287 ms materializing a 320k-row account just to
-- cap at 250k). A leading `account_id, cooc_count` key lets the top-N query be
-- served by a bounded backward index traversal instead of a full sort, so cost
-- scales with the edge cap, not the account size.
CREATE INDEX IF NOT EXISTS idx_atc_account_cooc
    ON account_tag_cooccurrence(account_id, cooc_count);
