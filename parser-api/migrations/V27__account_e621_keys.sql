-- Per-account e621 API key + direct-sync tracking (encrypted at rest).
--
-- ACCOUNT-scoped: an e621 account has ONE canonical API key (the owner's),
-- stored on the `accounts` row so it is available to every linked device for
-- direct sync. Ownership is enforced at ACCESS time via `account_device_links`
-- (a token must be linked to the account to view/manage the key), but the key
-- is a single shared account resource — sync therefore works from any linked
-- device. (The admin_user account syncs with the shared admin_api directly and
-- needs no stored key.)
--
-- e621_api_key_encrypted holds `base64(nonce || AES-256-GCM ciphertext)` from
-- `crypto::encrypt` (encryption key derived from
-- `config.e621_key_encryption_secret`) — never plaintext, never returned over
-- the API, and never included in profile export (the export is built from
-- explicit fields).
--
-- Timestamps:
--   e621_api_key_added_at      when the key was set/rotated
--   e621_api_key_verified_at   last time the key was verified against e621
--   last_direct_synced_at      last successful direct sync (account-wide)
ALTER TABLE accounts ADD COLUMN e621_api_key_encrypted TEXT;
ALTER TABLE accounts ADD COLUMN e621_api_key_added_at TEXT;
ALTER TABLE accounts ADD COLUMN e621_api_key_verified_at TEXT;
ALTER TABLE accounts ADD COLUMN last_direct_synced_at TEXT;
