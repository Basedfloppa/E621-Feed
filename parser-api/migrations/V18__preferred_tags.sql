-- Positive preferences: tags the user wants to see more of (soft boost).
-- weight ∈ [0.1, 10.0]; blacklist takes priority over preferred_tags.
CREATE TABLE IF NOT EXISTS account_preferred_tags (
    account_id INTEGER NOT NULL,
    tag_name   TEXT NOT NULL,
    group_type TEXT NOT NULL,
    weight     REAL NOT NULL DEFAULT 1.0,
    PRIMARY KEY (account_id, tag_name, group_type),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;
