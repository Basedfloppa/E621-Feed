-- Key `account_tag_cooccurrence` by tag ids instead of tag name/group strings.
--
-- V9 stored every pair as four TEXT columns (tag1_name, tag1_group,
-- tag2_name, tag2_group). That made this the widest structure in the database:
-- on a 106-account / 126M-row instance it held 25.2 GiB across the table and
-- its four indexes — 81% of the entire file. The primary-key autoindex alone
-- was 6.9 GiB, and the two name-keyed secondary indexes a further 10.2 GiB.
-- Neither name index was ever used as a lookup path: every reader selects by
-- `account_id` and can join `tags` for the names it displays. Keyed by ids the
-- primary key is three integers, the two name indexes are unnecessary, and the
-- same data is roughly 4-5x smaller.
--
-- The rows are NOT carried across. Every pair is derivable from
-- `accounts_post` x `tags_posts`, which is exactly what
-- `db::set_account_tag_cooccurrence` rebuilds, and a damaged b-tree cannot be
-- read in full anyway — so converting on the way through would have to guess.
-- A database that arrives here with rows loses a derived cache and gets it
-- back from the boot-time backfill (`db::cooccurrence_backfill`); nothing that
-- is not recomputable lives in this table.
--
-- The old table is removed through `writable_schema`, not `DROP TABLE`.
-- SQLite frees a dropped table's pages by walking its b-tree; when that tree
-- is damaged — which is how an instance arrives here, since a killed writer
-- can leave this cache inconsistent — the walk fails with "database disk image
-- is malformed" after several minutes and the migration aborts startup.
-- `DELETE FROM sqlite_master` never reads the b-tree, so it works on a damaged
-- table and a healthy one alike.
--
-- `PRAGMA writable_schema=RESET` is load-bearing, not decoration: editing
-- sqlite_master does not invalidate the connection's in-memory schema, so
-- without the RESET the CREATE TABLE below fails with "table ... already
-- exists". RESET re-reads the schema and clears the flag.
--
-- The removed table's pages are left unreferenced — `PRAGMA integrity_check`
-- reports them as "never used" — until a VACUUM rebuilds the file from
-- reachable objects. VACUUM cannot be run here: SQLite refuses it inside a
-- transaction and refinery wraps SQL migrations in one, so reclaiming that
-- space is a separate deliberate step:
--
--     sqlite3 database.db "VACUUM;"

PRAGMA writable_schema=ON;

DELETE FROM sqlite_master WHERE tbl_name = 'account_tag_cooccurrence';

PRAGMA writable_schema=RESET;

CREATE TABLE account_tag_cooccurrence (
    account_id INTEGER NOT NULL,
    tag1_id    INTEGER NOT NULL,
    tag2_id    INTEGER NOT NULL,
    cooc_count INTEGER NOT NULL,
    PRIMARY KEY (account_id, tag1_id, tag2_id),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE,
    FOREIGN KEY (tag1_id) REFERENCES tags(id) ON DELETE CASCADE,
    FOREIGN KEY (tag2_id) REFERENCES tags(id) ON DELETE CASCADE,
    CHECK (tag1_id < tag2_id)
) STRICT;

-- Serves the top-N-per-account reads (`load_account_tag_relation_conn`,
-- `get_account_tag_relation_graph`) and the partner aggregate in
-- `db/feed.rs`, all of which filter on `account_id` and order by
-- `cooc_count`. See V26 for the measurements behind this index.
CREATE INDEX IF NOT EXISTS idx_atc_account_cooc
    ON account_tag_cooccurrence(account_id, cooc_count);
