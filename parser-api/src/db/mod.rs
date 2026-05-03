use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rocket::{
    Build, Rocket,
    fairing::{Fairing, Info, Kind},
};
use std::sync::OnceLock;

mod accounts;
mod cooccurrence;
mod feed;
mod posts;
mod profiles;
mod tags;

pub use accounts::*;
pub use cooccurrence::*;
pub use feed::*;
pub use posts::*;
pub use profiles::*;
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
        let manager = SqliteConnectionManager::file("database.db").with_init(|conn| {
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
            .max_size(8)
            .build(manager)
            .expect("build sqlite pool")
    })
}


/// Begin a writer transaction with `BEGIN IMMEDIATE` so concurrent writers
/// queue via `busy_timeout` instead of failing with `SQLITE_BUSY_SNAPSHOT`.
/// The default `BEGIN DEFERRED` tries to upgrade a read snapshot to a write
/// lock — when two connections both hold a snapshot, neither can win and
/// `busy_timeout` cannot retry safely (it would deadlock). Always use this
/// helper for any transaction that performs writes.
pub(super) fn begin_write_tx(conn: &mut DbConn) -> Result<rusqlite::Transaction<'_>, String> {
    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| format!("Failed to begin write transaction: {e}"))
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

pub fn ensure_sqlite() -> Result<(), String> {
    let mut conn = open_db().map_err(|e| e.to_string())?;

    embedded::migrations::runner()
        .run(&mut *conn)
        .map_err(|e| format!("Failed to run migrations: {e}"))?;

    Ok(())
}
