-- Re-create tag_aliases, tag_implications, and tag_relation_probe tables.
-- These were dropped in V2__remove_relations.sql (initially removed because
-- no code was populating them). Now the tag_relation_import worker and
-- Taste Profile component need them for alias/implication resolution and
-- generic-tag filtering.

CREATE TABLE IF NOT EXISTS tag_aliases (
    antecedent_name TEXT PRIMARY KEY,
    consequent_name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active','deleted','processing','queued','retired','error','pending')),
    created_at TEXT,
    updated_at TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS tag_implications (
    antecedent_name TEXT NOT NULL,
    consequent_name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active','deleted','processing','queued','retired','error','pending')),
    created_at TEXT,
    updated_at TEXT,
    PRIMARY KEY(antecedent_name, consequent_name)
) STRICT;

CREATE TABLE IF NOT EXISTS tag_relation_probe (
    tag TEXT PRIMARY KEY,
    aliases_last_checked TIMESTAMP,
    aliases_count INTEGER NOT NULL DEFAULT 0,
    implications_last_checked TIMESTAMP,
    implications_count INTEGER NOT NULL DEFAULT 0
);

-- Indexes for efficient lookups
CREATE INDEX IF NOT EXISTS idx_tag_aliases_consequent ON tag_aliases(consequent_name);
CREATE INDEX IF NOT EXISTS idx_tag_imps_ante ON tag_implications(antecedent_name);
