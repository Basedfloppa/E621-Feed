use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rocket::{
    Build, Rocket,
    fairing::{Fairing, Info, Kind},
};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

mod accounts;
mod cooccurrence;
mod cooccurrence_backfill;
mod digest;
mod feed;
mod posts;
mod profiles;
mod sessions;
mod tags;

pub use accounts::*;
pub use cooccurrence::*;
pub use cooccurrence_backfill::*;
pub use digest::*;
pub use feed::*;
pub use posts::*;
pub use profiles::*;
pub use sessions::*;
pub use tags::*;

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

fn write_conn() -> &'static Mutex<rusqlite::Connection> {
    WRITE_CONN.get_or_init(|| {
        let path = crate::models::cfg().db_path.clone();
        let conn = rusqlite::Connection::open(&path).expect("open writer connection");
        // Set busy_timeout BEFORE the batch below: the `journal_mode = WAL`
        // switch needs an exclusive lock, and a concurrently-starting worker
        // (e.g. tag_relation_import on a fresh DB) may hold a shared lock.
        // With the default busy_timeout of 0 the switch fails immediately
        // with SQLITE_BUSY and this `.expect` panics — observed as
        // "init writer connection: database is locked" at startup.
        conn.busy_timeout(std::time::Duration::from_secs(60))
            .expect("set writer busy_timeout");
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous  = NORMAL;
            PRAGMA busy_timeout = 60000;
            PRAGMA temp_store   = MEMORY;
            ",
        )
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
}
