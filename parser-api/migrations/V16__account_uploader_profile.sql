-- Per-account uploader quality profile.
-- Tracks which uploaders the user tends to favourite and the average
-- quality of those uploaders' posts.
CREATE TABLE IF NOT EXISTS account_uploader_profile (
    account_id   INTEGER NOT NULL,
    uploader_id  INTEGER NOT NULL,
    post_count   INTEGER NOT NULL,
    avg_score    REAL    NOT NULL,
    avg_fav      REAL    NOT NULL,
    PRIMARY KEY (account_id, uploader_id)
);
