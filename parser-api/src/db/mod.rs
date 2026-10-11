use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rocket::{
    Build, Rocket,
    fairing::{Fairing, Info, Kind},
};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

#[cfg(unix)]
use std::fs::{File, OpenOptions, TryLockError};

mod accounts;
mod catalog;
mod cooccurrence;
mod cooccurrence_backfill;
mod digest;
mod feed;
mod hydrate_cache;
mod media;
mod pools;
mod posts;
mod profiles;
mod sessions;
mod tags;

pub use accounts::*;
pub use catalog::*;
pub use cooccurrence::*;
pub use cooccurrence_backfill::*;
pub use digest::*;
pub use feed::*;
pub use hydrate_cache::*;
pub use media::*;
pub use pools::*;
pub use posts::*;
pub use profiles::*;
pub use sessions::*;
pub use tags::*;

/// Maximum number of values bound into a single `IN (…)` clause.
///
/// SQLite accepts up to 32766 bound parameters, but several call sites build
/// the placeholder list from whatever the caller hands them (`DELETE … WHERE
/// id IN (…)` over prune results, media evictions, catalog deletes). Rather
/// than leave that as an undocumented "keep your list short" contract, the
/// helpers chunk at this size: any list length is accepted, each statement
/// stays small, and the whole operation still runs inside one write
/// transaction so it remains atomic.
pub(crate) const IN_CLAUSE_CHUNK: usize = 1000;

mod embedded {
    use refinery::embed_migrations;
    embed_migrations!("migrations");
}

pub struct DbInit;

#[rocket::async_trait]
impl Fairing for DbInit {
    fn info(&self) -> Info {
        Info {
            name: "SQLite DB Initializer",
            kind: Kind::Ignite,
        }
    }

    async fn on_ignite(&self, rocket: Rocket<Build>) -> rocket::fairing::Result {
        match ensure_sqlite() {
            Ok(()) => {
                println!("SQLite DB Initialized");
                spawn_tag_cooccurrence_backfill_if_needed();
                // Hot-cache the revocation denylist before any request can
                // hit the auth guard — otherwise the first few requests
                // race with the load and could silently bypass revocation.
                if let Err(e) = crate::auth::reload_revocation_set() {
                    println!("Failed to load revocation denylist: {e}");
                    return Err(rocket);
                }
                Ok(rocket)
            }
            Err(e) => {
                println!("Database initialization failed: {e}");
                Err(rocket)
            }
        }
    }
}

pub type DbPool = Pool<SqliteConnectionManager>;
pub type DbConn = r2d2::PooledConnection<SqliteConnectionManager>;

static POOL: OnceLock<DbPool> = OnceLock::new();

fn pool() -> &'static DbPool {
    POOL.get_or_init(|| {
        let path = crate::models::cfg().db_path.clone();
        let manager = SqliteConnectionManager::file(&path).with_init(|conn| {
            conn.execute_batch(
                "
                PRAGMA foreign_keys = ON;
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous  = NORMAL;
                PRAGMA busy_timeout = 60000;
                PRAGMA temp_store   = MEMORY;
                PRAGMA cache_size   = -65536;
                PRAGMA mmap_size    = 268435456;
                ",
            )
        });
        Pool::builder()
            .max_size(16)
            .connection_timeout(Duration::from_mins(2))
            .build(manager)
            .expect("build sqlite pool")
    })
}

/// Dedicated single writer connection guarded by a `Mutex`. `SQLite` WAL only
/// permits one writer at a time anyway; serialising at the application
/// level (instead of letting many writers compete via `busy_timeout`) gives
/// FIFO ordering, frees the pool for readers, and avoids the cascading
/// "database is locked" / "timed out waiting for connection" failures we
/// were seeing under concurrent prefetch + cooc-backfill load.
static WRITE_CONN: OnceLock<Mutex<rusqlite::Connection>> = OnceLock::new();

/// Static handle that keeps the process-wide DB writer flock alive for the
/// lifetime of the process. Cross-process only — within one process every
/// writer already shares the `WRITE_CONN` mutex.
#[cfg(unix)]
static DB_WRITER_LOCK: OnceLock<File> = OnceLock::new();

/// Take an exclusive advisory flock on the DB file. **Two OS processes must
/// never write the same WAL-mode SQLite file at once** — e.g. `catalog_seed`
/// bulk-importing tags while the server's `tag_relation_importer` writes the
/// same `tag_implications`/`tag_aliases` tables. That race corrupts the file
/// and shows up as "database disk image is malformed" on exactly those tables.
/// flock is automatically released by the kernel on process exit, so a crashed
/// or finished process frees the lock without manual cleanup.
#[cfg(unix)]
fn acquire_db_writer_lock(path: &str) -> Result<(), String> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true) // create a missing DB file, like Connection::open below
        .truncate(false) // NEVER truncate an existing DB — we only want the lock
        .open(path)
        .map_err(|e| format!("open db for writer lock: {e}"))?;
    match f.try_lock() {
        Ok(()) => {
            // Keep the handle alive for the whole process.
            let _ = DB_WRITER_LOCK.set(f);
            Ok(())
        }
        Err(TryLockError::WouldBlock) => {
            Err("database is already being written by another E621 process \
             (catalog_seed or a running server). Stop that process before starting \
             another writer — two writers on one SQLite WAL database corrupt it."
                .to_string())
        }
        Err(e) => Err(format!("db writer lock failed: {e}")),
    }
}

fn write_conn() -> &'static Mutex<rusqlite::Connection> {
    WRITE_CONN.get_or_init(|| {
        let path = crate::models::cfg().db_path.clone();
        // Cross-process writer guard: abort this process rather than risk a
        // second concurrent writer corrupting the WAL database.
        #[cfg(unix)]
        acquire_db_writer_lock(&path).expect("acquire db writer lock");
        let conn = rusqlite::Connection::open(&path).expect("open writer connection");
        // Set busy_timeout BEFORE the batch below: the `journal_mode = WAL`
        // switch needs an exclusive lock, and a concurrently-starting worker
        // (e.g. tag_relation_import on a fresh DB) may hold a shared lock.
        // With the default busy_timeout of 0 the switch fails immediately
        // with SQLITE_BUSY and this `.expect` panics — observed as
        // "init writer connection: database is locked" at startup.
        conn.busy_timeout(std::time::Duration::from_secs(60))
            .expect("set writer busy_timeout");
        // `SQLite`'s default page cache is 2 MiB, orders of magnitude below the
        // working set of the tag-cooccurrence build — its three btrees reach
        // ~4 GiB, so at the default every upsert faults a page and the build
        // crawls at ~65k pairs/s. This connection does every bulk write
        // (`seed`, `calibrate graph`, `/process`), so it takes the large cache;
        // the pool above stays small because its connections are readers.
        // `E621_DB_CACHE_MB` overrides the size on memory-constrained hosts.
        let cache_kib = std::env::var("E621_DB_CACHE_MB")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|mb| *mb >= 16)
            .unwrap_or(8192)
            * 1024;
        conn.execute_batch(&format!(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous  = NORMAL;
            PRAGMA busy_timeout = 60000;
            PRAGMA temp_store   = MEMORY;
            PRAGMA cache_size   = -{cache_kib};
            PRAGMA mmap_size    = 8589934592;
            ",
        ))
        .expect("init writer connection");
        Mutex::new(conn)
    })
}

/// Run a closure inside an `IMMEDIATE` write transaction on the dedicated
/// writer connection. Wraps acquire-mutex / begin / commit so callers can
/// focus on SQL. The mutex is held for the full duration of the closure +
/// commit; keep the closure body lean (no network I/O, no long sleeps).
pub fn with_write_tx<R, F>(f: F) -> Result<R, String>
where
    F: FnOnce(&rusqlite::Transaction) -> Result<R, String>,
{
    let mut guard = write_conn()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tx = guard
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| format!("Failed to begin write transaction: {e}"))?;
    let result = f(&tx)?;
    tx.commit()
        .map_err(|e| format!("Failed to commit write transaction: {e}"))?;
    Ok(result)
}

/// Run WAL checkpoint with truncate to keep the WAL file from growing
/// unbounded. Safe to call frequently — `SQLite` no-ops when there's nothing
/// to checkpoint. Uses the dedicated writer connection.
pub fn wal_checkpoint() -> Result<(), String> {
    let guard = write_conn()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .map_err(|e| format!("WAL checkpoint failed: {e}"))
}

/// Run migrations on the dedicated writer connection (so they share locking
/// semantics with all other writers). The pool is not yet initialised when
/// this runs, so we open the writer ourselves.
pub fn ensure_sqlite() -> Result<(), String> {
    let mut guard = write_conn()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Guard against pointing a stale/older build at an already-migrated
    // database BEFORE refinery runs. With `abort_missing` (default) refinery
    // otherwise just aborts with a terse "migration V21 is missing from the
    // filesystem" whenever the shared `database.db` was migrated by a newer
    // build — i.e. its applied history contains a version this binary doesn't
    // embed. Diagnose that up front so the operator gets an actionable message
    // ("this binary is older than the schema; rebuild it") instead of an
    // opaque startup crash.
    ensure_embedded_migrations_cover_db(&guard)?;
    embedded::migrations::runner()
        .run(&mut *guard)
        .map_err(|e| format!("Failed to run migrations: {e}"))?;
    Ok(())
}

/// Reject starting against a database whose applied schema history this
/// binary's embedded migrations cannot account for, with an actionable message.
///
/// Returns `Ok(())` when the DB has no history yet, its history is empty, or
/// every applied version is embedded in this binary (i.e. schema ≤ binary).
fn ensure_embedded_migrations_cover_db(conn: &rusqlite::Connection) -> Result<(), String> {
    let runner = embedded::migrations::runner();
    let embedded: Vec<i64> = runner
        .get_migrations()
        .iter()
        .map(|m| m.version() as i64)
        .collect();
    let embedded_min = embedded.iter().copied().min().unwrap_or(0);
    let embedded_max = embedded.iter().copied().max().unwrap_or(0);

    // Applied history — absent or empty on a brand-new database.
    let applied: Vec<i64> =
        match conn.prepare("SELECT version FROM refinery_schema_history ORDER BY version") {
            Ok(mut stmt) => match stmt.query_map([], |row| row.get::<_, i64>(0)) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        };
    if applied.is_empty() {
        return Ok(());
    }
    let db_max = *applied.iter().max().unwrap();

    // Versions applied in the DB but not embedded in this build — the exact
    // situation refinery reports as "missing from the filesystem".
    let ahead: Vec<i64> = applied
        .iter()
        .copied()
        .filter(|v| !embedded.contains(v))
        .collect();
    if !ahead.is_empty() {
        let ahead_name = ahead
            .iter()
            .map(|v| format!("V{v}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "database schema is newer than this binary: the shared database has \
             applied migration(s) {ahead_name} that this build does not embed \
             (this binary embeds V{embedded_min}..V{embedded_max}; the database \
             is at V{db_max}). A newer build migrated this database. Rebuild this \
             binary / image from the current source (`cargo build` / `docker build`) \
             so it embeds the full migration set, then restart. Never delete or \
             renumber an already-applied migration by hand."
        ));
    }
    Ok(())
}

/// Verify that a pooled `SQLite` connection can execute a trivial query.
pub fn check_database_health() -> Result<(), String> {
    let conn = open_db()?;
    conn.query_row("SELECT 1", [], |_| Ok(()))
        .map_err(|e| format!("SQLite health check failed: {e}"))
}

pub(super) fn open_db() -> Result<DbConn, String> {
    pool()
        .get()
        .map_err(|e| format!("Failed to acquire sqlite connection: {e}"))
}

pub(crate) fn open_db_for_prefetch() -> Result<DbConn, String> {
    open_db()
}

pub fn open_db_for_calibration() -> Result<DbConn, String> {
    open_db()
}

pub(super) fn parse_db_datetime(raw: &str) -> Result<DateTime<Utc>, String> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| {
            chrono::DateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S%.f %Z")
                .map(|dt| dt.with_timezone(&Utc))
        })
        .or_else(|_| {
            chrono::DateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S %Z")
                .map(|dt| dt.with_timezone(&Utc))
        })
        .map_err(|e| format!("Failed to parse datetime '{raw}': {e}"))
}

#[cfg(test)]
mod tests {
    use super::ensure_embedded_migrations_cover_db;

    fn mem() -> rusqlite::Connection {
        rusqlite::Connection::open_in_memory().unwrap()
    }

    #[test]
    fn guard_is_ok_when_no_history_or_history_covered() {
        // No schema-history table yet (brand-new DB) -> treat as fresh.
        let conn = mem();
        assert!(ensure_embedded_migrations_cover_db(&conn).is_ok());

        // History exists and every applied version is embedded in this binary.
        let conn = mem();
        conn.execute_batch(
            "CREATE TABLE refinery_schema_history(version int4 PRIMARY KEY,
                 name VARCHAR(255), applied_on VARCHAR(255), checksum VARCHAR(255));
             INSERT INTO refinery_schema_history(version,name,applied_on,checksum)
                 VALUES (25, 'backfill_timestamp', '', '');",
        )
        .unwrap();
        assert!(ensure_embedded_migrations_cover_db(&conn).is_ok());
    }

    #[test]
    fn guard_rejects_db_ahead_of_this_binary() {
        let conn = mem();
        conn.execute_batch(
            "CREATE TABLE refinery_schema_history(version int4 PRIMARY KEY,
                 name VARCHAR(255), applied_on VARCHAR(255), checksum VARCHAR(255));
             INSERT INTO refinery_schema_history(version,name,applied_on,checksum)
                 VALUES (99, 'future_migration', '', '');",
        )
        .unwrap();
        let err = ensure_embedded_migrations_cover_db(&conn).unwrap_err();
        assert!(
            err.contains("V99"),
            "message should name the missing migration: {err}"
        );
        assert!(err.contains("database schema is newer"), "{err}");
    }

    /// V31 — rewrite a lone `-animated` line to `require:animated`.
    ///
    /// Run from the embedded migration text rather than a re-implementation, so
    /// the artifact refinery applies is the artifact under test. No deployment we
    /// can inspect has a `-animated` row left, so this test and the one-shot file
    /// are the only places the rewrite is exercised.
    #[test]
    fn migration_v31_rewrites_only_a_lone_dashed_static_rule() {
        const SQL: &str = include_str!("../../migrations/V31__require_animated_blacklist_rule.sql");

        let cases: &[(&str, &str)] = &[
            // The old "Block static images" entry, alone on its line, in each
            // position it can occupy.
            ("-animated", "require:animated"),
            ("gore\n-animated", "gore\nrequire:animated"),
            ("-animated\ngore", "require:animated\ngore"),
            ("gore\n-animated\nscat", "gore\nrequire:animated\nscat"),
            // Structure around it survives: trailing newline, blank run.
            ("-animated\n", "require:animated\n"),
            ("gore\n\n-animated", "gore\n\nrequire:animated"),
            // Neither case nor surrounding whitespace hides the rule. The
            // converted line comes out canonical, so its padding is dropped.
            ("-ANIMATED", "require:animated"),
            ("-Animated", "require:animated"),
            ("  -animated  ", "require:animated"),
            ("\t-animated\t", "require:animated"),
            // CRLF: the rewritten line loses its CR, the separator stays `\n`.
            ("-animated\r\ngore", "require:animated\ngore"),
            // NOT rewritten — the dashed text is not a rule on its own, so it was
            // not written by the button and inverting it would corrupt a rule the
            // user typed deliberately.
            ("foo-animated", "foo-animated"),
            ("-animated_gif", "-animated_gif"),
            ("gore -animated", "gore -animated"),
            ("-animated -animated", "-animated -animated"),
            ("gore\t-animated\tscat", "gore\t-animated\tscat"),
            // A comment is not a rule; the line after it still is.
            ("# -animated", "# -animated"),
            ("# -animated\n-animated", "# -animated\nrequire:animated"),
            // Already-migrated and unrelated text is left alone.
            ("require:animated", "require:animated"),
            ("gore", "gore"),
        ];

        let conn = mem();
        conn.execute_batch(
            "CREATE TABLE accounts(id INTEGER PRIMARY KEY, blacklisted_tags TEXT NOT NULL DEFAULT '');
             CREATE TABLE account_device_links(id INTEGER PRIMARY KEY, blacklisted_tags TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        for (i, (input, _)) in cases.iter().enumerate() {
            let id = i as i64 + 1;
            for table in ["accounts", "account_device_links"] {
                conn.execute(
                    &format!("INSERT INTO {table}(id, blacklisted_tags) VALUES (?1, ?2)"),
                    rusqlite::params![id, input],
                )
                .unwrap();
            }
        }

        let read = |table: &str, id: i64| -> String {
            conn.query_row(
                &format!("SELECT blacklisted_tags FROM {table} WHERE id = ?1"),
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap()
        };

        // Applying twice proves the rewrite is a fixed point: the migrated text
        // no longer matches the `LIKE '%-animated%'` screen, and a surviving
        // non-rule (`gore -animated`) re-serialises to itself.
        for pass in 1..=2 {
            conn.execute_batch(SQL).unwrap();
            for (i, (input, expected)) in cases.iter().enumerate() {
                let id = i as i64 + 1;
                for table in ["accounts", "account_device_links"] {
                    let got = read(table, id);
                    assert_eq!(
                        got, *expected,
                        "pass {pass}, {table}, case {input:?}: expected {expected:?}, got {got:?}"
                    );
                }
            }
        }
    }

    /// Every `migrations/*.sql` must be picked up by `embed_migrations!`.
    ///
    /// refinery skips a filename that is not `V<n>__<name>.sql` without a word, so
    /// a typo there produces a migration that simply never runs — the failure is
    /// invisible until someone wonders why a column is missing. Compare the two
    /// sets instead of trusting the directory.
    #[test]
    fn every_migration_file_on_disk_is_embedded() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut on_disk: Vec<i64> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".sql"))
            .map(|name| {
                let rest = name
                    .strip_prefix('V')
                    .unwrap_or_else(|| panic!("migration filename must start with V: {name}"));
                let (version, _) = rest.split_once("__").unwrap_or_else(|| {
                    panic!("migration filename must be V<n>__<name>.sql: {name}")
                });
                version
                    .parse::<i64>()
                    .unwrap_or_else(|_| panic!("migration version must be numeric: {name}"))
            })
            .collect();
        on_disk.sort_unstable();

        let mut embedded_versions: Vec<i64> = super::embedded::migrations::runner()
            .get_migrations()
            .iter()
            .map(|migration| migration.version() as i64)
            .collect();
        embedded_versions.sort_unstable();

        assert_eq!(
            on_disk, embedded_versions,
            "migrations/ and the embedded set disagree (both sorted by version)"
        );
    }
}
