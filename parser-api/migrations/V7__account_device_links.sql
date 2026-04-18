CREATE TABLE account_device_links (
    owner_token TEXT NOT NULL,
    account_id INTEGER NOT NULL,
    linked_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    PRIMARY KEY(owner_token, account_id),
    FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_account_device_links_owner_seen
    ON account_device_links(owner_token, last_seen_at DESC);