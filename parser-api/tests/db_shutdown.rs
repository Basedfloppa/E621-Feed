//! Clean-shutdown behaviour of the database layer.
//!
//! `docker compose down` removes the container together with the SQLite
//! journal: the database is a *file* bind mount, so `-wal`/`-shm` live in the
//! container's writable layer. A shutdown that only stopped the process would
//! therefore roll back every frame committed since the last checkpoint — up to
//! `cache_validate_interval_secs` (300 s) of writes, silently — and a kill that
//! landed mid-checkpoint left a database with no WAL to recover from. This test
//! keeps both halves from coming back.
//!
//! Own test binary → own process → own temporary database: `db::shutdown` is
//! irreversible by design, so it cannot share a binary with another test.

mod support;

use e621_account_parser_api::db;
use e621_account_parser_api::models::cfg;

#[test]
fn shutdown_folds_the_wal_into_the_database_and_stops_writers() {
    support::install_isolated_db_config();
    db::ensure_sqlite().expect("DB migrations failed");

    // Open the pool first, the way any served request does. This is load
    // bearing: with the writer as the only connection, `Connection::close`
    // would itself checkpoint and delete the WAL, and the test would pass even
    // if `shutdown` skipped its checkpoint. In production readers hold the
    // database open, so only the explicit checkpoint can empty the WAL.
    db::open_db_for_calibration()
        .expect("open a pooled connection")
        .query_row("SELECT 1", [], |_| Ok(()))
        .expect("read through the pool");

    // A committed row that deliberately lives only in the WAL — the state every
    // write from the last few minutes is in when someone runs `compose down`.
    db::with_write_tx(|tx| {
        tx.execute_batch("CREATE TABLE shutdown_probe(value TEXT NOT NULL);")
            .map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO shutdown_probe(value) VALUES ('kept')", [])
            .map_err(|e| e.to_string())?;
        Ok(())
    })
    .expect("write the probe row");

    let wal = format!("{}-wal", cfg().db_path);
    let before = std::fs::metadata(&wal).map_or(0, |m| m.len());
    assert!(
        before > 0,
        "the probe write must be in the WAL, otherwise this test proves nothing"
    );

    db::shutdown().expect("clean shutdown");

    // The checkpoint folded the frames into the main file and truncated the
    // journal. `compose down` discards the container layer anyway; what matters
    // is that after this point the journal holds nothing to discard.
    let after = std::fs::metadata(&wal).map_or(0, |m| m.len());
    assert_eq!(after, 0, "the shutdown must truncate the WAL");

    // With an empty journal the row can only come from the database file.
    let conn = db::open_db_for_calibration().expect("reopen the database for reading");
    let value: String = conn
        .query_row("SELECT value FROM shutdown_probe", [], |row| row.get(0))
        .expect("the probe row must survive the shutdown");
    drop(conn);
    assert_eq!(value, "kept");

    // Writers are disarmed, not merely idle: a background worker that wakes up
    // after the checkpoint gets an error instead of committing a fresh frame.
    let err = db::with_write_tx(|_| Ok(())).expect_err("writes must be refused after shutdown");
    assert!(
        err.contains("database is shut down"),
        "unexpected error: {err}"
    );

    // Idempotent: a second close neither panics nor resurrects the writer.
    db::shutdown().expect("shutdown is idempotent");
}
