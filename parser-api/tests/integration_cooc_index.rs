//! The per-account co-occurrence aggregate (`db::feed::load_candidate_pool_input`)
//! scans up to 5.5M rows for an active account. It is only index-only, and
//! therefore not a table walk per row, as long as one covering index exists —
//! and the older `(account_id, cooc_count)` index must stay gone, because it is
//! a pure prefix of the covering one.

mod support;

use e621_account_parser_api::db;

#[test]
fn account_cooc_aggregate_uses_one_covering_index() {
    support::install_isolated_db_config();
    db::ensure_sqlite().expect("migrations");
    let conn = db::open_db_for_calibration().expect("open db");

    let prefix_index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'index' AND name = 'idx_atc_account_cooc'",
            [],
            |r| r.get(0),
        )
        .expect("count old index");
    assert_eq!(
        prefix_index, 0,
        "idx_atc_account_cooc is a prefix of idx_atc_account_cooc_cover and only adds write cost"
    );

    let mut stmt = conn
        .prepare(
            "EXPLAIN QUERY PLAN \
             SELECT tag1_id, cooc_count FROM account_tag_cooccurrence \
             WHERE account_id = ?1 AND cooc_count >= ?2",
        )
        .expect("prepare plan");
    let plan: Vec<String> = stmt
        .query_map([1i64, 2i64], |r| r.get::<_, String>(3))
        .expect("explain")
        .collect::<Result<_, _>>()
        .expect("collect plan");
    let plan = plan.join("\n");
    assert!(
        plan.contains("COVERING INDEX"),
        "the aggregate must not fetch table rows per pair; plan was:\n{plan}"
    );
}
