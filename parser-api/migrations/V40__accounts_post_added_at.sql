-- When the favourite entered the local library.
--
-- e621 does not expose the favourite timestamp: `/favorites.json` returns the
-- *post's* `created_at`, and no `v1`/`v2` shape carries a favourite date (see
-- docs/calibration.md). The only honest clock available is the moment this
-- process first stored the row, so that is what `added_at` records. It is a
-- lower bound for accounts first synced after this migration and NULL for rows
-- written before it — readers must treat NULL as "older than any cutoff", not
-- as "unknown, include it".
--
-- The column is what makes an honest temporal holdout possible later: a
-- positive added after `T` was not in the profile at `T`, which is the one
-- property the current `time_causal` split can only approximate through the
-- post's own creation date.
ALTER TABLE accounts_post ADD COLUMN added_at TEXT;

-- The eval split asks "favourites added after T for this account", and the
-- digest asks the same question in reverse; both filter on (account, added_at).
CREATE INDEX idx_ap_account_added ON accounts_post(account_id, added_at);
