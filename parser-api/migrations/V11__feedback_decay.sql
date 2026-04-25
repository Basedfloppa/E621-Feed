-- Track when decay was last applied to a feedback row, separately from the
-- last interaction timestamp. Decay is anchored on this column so we can
-- compute elapsed-since-decay independently of new interactions arriving.
ALTER TABLE account_tag_feedback
    ADD COLUMN last_decayed_at TEXT NOT NULL DEFAULT '';

UPDATE account_tag_feedback
SET last_decayed_at = last_interaction_at
WHERE last_decayed_at = '';
