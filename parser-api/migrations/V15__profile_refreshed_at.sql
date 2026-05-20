-- Track when the preference profile was last refreshed. Used by the
-- time-weighted interaction_fit channel to apply supplementary decay
-- between refresh cycles.
ALTER TABLE accounts
    ADD COLUMN profile_refreshed_at TEXT;
