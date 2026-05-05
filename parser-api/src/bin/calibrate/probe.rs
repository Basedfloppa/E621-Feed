//! `calibrate probe` — descriptive stats on the local DB.

use rusqlite::params;

use e621_account_parser_api::db;

pub(crate) fn run_probe() -> anyhow::Result<()> {
    let conn = db::open_db_for_calibration().map_err(|e| anyhow::anyhow!(e))?;
    let posts: i64 = conn.query_row("SELECT COUNT(*) FROM posts", [], |r| r.get(0))?;
    let accounts: i64 = conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))?;
    let with_favs: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT account_id) FROM accounts_post",
        [],
        |r| r.get(0),
    )?;
    let total_favs: i64 = conn.query_row("SELECT COUNT(*) FROM accounts_post", [], |r| r.get(0))?;
    println!("posts: {posts}");
    println!("accounts: {accounts}");
    println!("accounts w/ favs: {with_favs}");
    println!("fav links: {total_favs}");

    println!("\nfav-count buckets:");
    for (label, lo, hi) in [
        ("<10", 0i64, 10i64),
        ("10-49", 10, 50),
        ("50-99", 50, 100),
        ("100-499", 100, 500),
        ("500+", 500, i64::MAX),
    ] {
        let c: i64 = conn.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT account_id, COUNT(*) c FROM accounts_post GROUP BY account_id
             ) WHERE c >= ?1 AND c < ?2",
            params![lo, hi],
            |r| r.get(0),
        )?;
        println!("  {label:>8}: {c} accounts");
    }

    println!("\ntop 20 by fav count:");
    let mut stmt = conn.prepare(
        "SELECT account_id, COUNT(*) FROM accounts_post
         GROUP BY account_id ORDER BY COUNT(*) DESC LIMIT 20",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i32>(0)?, r.get::<_, i64>(1)?)))?;
    for r in rows {
        let (id, c) = r?;
        println!("  account_id={id:>6}  favs={c}");
    }
    Ok(())
}
