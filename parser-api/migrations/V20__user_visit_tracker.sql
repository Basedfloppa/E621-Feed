-- Tracks user visit frequency for adaptive daily digest precompute.
-- Only active users (visit_streak >= 2 or avg_gap <= 3 days) get the
-- full personalised digest; infrequent users get a cheap generic fallback.
CREATE TABLE IF NOT EXISTS user_visit_tracker (
    account_id        INTEGER PRIMARY KEY,
    last_visit_date   TEXT    NOT NULL,          -- YYYY-MM-DD
    visit_streak      INTEGER NOT NULL DEFAULT 0,
    avg_visit_gap_days REAL   NOT NULL DEFAULT 7.0,
    total_visits_30d  INTEGER NOT NULL DEFAULT 0,
    last_digest_date  TEXT,                      -- YYYY-MM-DD, NULL = never
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_visit_tracker_streak
    ON user_visit_tracker(visit_streak);
CREATE INDEX IF NOT EXISTS idx_visit_tracker_gap
    ON user_visit_tracker(avg_visit_gap_days);
