ALTER TABLE posts ADD COLUMN updated_at TEXT;
ALTER TABLE posts ADD COLUMN file_ext TEXT;
ALTER TABLE posts ADD COLUMN file_width INTEGER;
ALTER TABLE posts ADD COLUMN file_height INTEGER;
ALTER TABLE posts ADD COLUMN file_size INTEGER;
ALTER TABLE posts ADD COLUMN is_animated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE posts ADD COLUMN duration REAL;
ALTER TABLE posts ADD COLUMN comment_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE posts ADD COLUMN has_notes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE posts ADD COLUMN is_deleted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE posts ADD COLUMN has_children INTEGER NOT NULL DEFAULT 0;

CREATE TABLE account_rating_profile (
    account_id INTEGER NOT NULL,
    rating TEXT NOT NULL CHECK (rating IN ('s','q','e')),
    count INTEGER NOT NULL,
    PRIMARY KEY(account_id, rating),
    FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

CREATE TABLE account_media_profile (
    account_id INTEGER NOT NULL,
    media_type TEXT NOT NULL CHECK (media_type IN ('image','animated','video')),
    count INTEGER NOT NULL,
    PRIMARY KEY(account_id, media_type),
    FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

CREATE TABLE account_quality_profile (
    account_id INTEGER PRIMARY KEY,
    avg_score_total REAL NOT NULL DEFAULT 0,
    avg_fav_count REAL NOT NULL DEFAULT 0,
    avg_comment_count REAL NOT NULL DEFAULT 0,
    avg_duration REAL NOT NULL DEFAULT 0,
    FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

CREATE TABLE account_recency_profile (
    account_id INTEGER PRIMARY KEY,
    avg_age_days REAL NOT NULL DEFAULT 0,
    avg_abs_dev_days REAL NOT NULL DEFAULT 0,
    FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_posts_rating_created_at ON posts(rating, created_at);
CREATE INDEX idx_posts_file_ext ON posts(file_ext);