-- Add last_backfilled_at column to accounts table for backfill worker.
ALTER TABLE accounts ADD COLUMN last_backfilled_at INTEGER;
CREATE INDEX IF NOT EXISTS idx_accounts_last_backfilled
    ON accounts(last_backfilled_at);
