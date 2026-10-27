-- Remove the A/B experiment machinery, of which only these two columns remain.
--
-- The arms, the pinning routes, the console column and the per-arm metrics are
-- gone: with no way to assign an arm and no panel reading the split, a stored
-- bucket value is a second, silent source of truth that a later reader would
-- take for a working mechanism. Keeping the column "just in case" is how a
-- configuration surface outlives its feature.
--
-- Both tables are small (accounts: hundreds of rows, feed_interactions:
-- thousands), so the rewrite `DROP COLUMN` implies is cheap; neither column is
-- part of an index.
ALTER TABLE accounts DROP COLUMN experiment_bucket;
ALTER TABLE feed_interactions DROP COLUMN experiment_bucket;
