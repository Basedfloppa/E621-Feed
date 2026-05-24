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
            Ok(_) => {
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
            .connection_timeout(Duration::from_secs(120))
            .build(manager)
            .expect("build sqlite pool")
    })
}

/// Dedicated single writer connection guarded by a `Mutex`. SQLite WAL only
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
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tx = guard
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| format!("Failed to begin write transaction: {e}"))?;
    let result = f(&tx)?;
    tx.commit()
        .map_err(|e| format!("Failed to commit write transaction: {e}"))?;
    Ok(result)
}

/// Run WAL checkpoint with truncate to keep the WAL file from growing
/// unbounded. Safe to call frequently — SQLite no-ops when there's nothing
/// to checkpoint. Uses the dedicated writer connection.
pub fn wal_checkpoint() -> Result<(), String> {
    let guard = write_conn()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    embedded::migrations::runner()
        .run(&mut *guard)
        .map_err(|e| format!("Failed to run migrations: {e}"))?;
    Ok(())
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

