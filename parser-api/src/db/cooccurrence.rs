use rusqlite::{Connection, params};
use std::collections::HashMap;

use crate::models::{TagCount, TagRelationEdge, TagRelationGraphPayload, TagRelationNode};

use super::open_db;

/// `SQLite` default `SQLITE_MAX_VARIABLE_NUMBER` is 999. Each pair is 2 params,
/// plus headroom — cap at 200 pairs (400 params) per statement.
const COOC_PAIRS_PER_STATEMENT: usize = 200;

/// Incremental upsert into `account_tag_cooccurrence` for the given
/// account and tag pairs. Called from `save_posts_tags_batch` to avoid
/// the full DELETE + INSERT SELECT rebuild in `set_account_tag_cooccurrence`.
/// Needs the reverse map from `tag_id → (name, group)`.
/// Each pair increments `cooc_count` by 1.
const COOC_ACCOUNT_PAIRS_PER_STMT: usize = 100;

pub(super) fn upsert_account_cooccurrence_pairs(
    tx: &rusqlite::Transaction,
    account_id: i32,
    tag_ids: &[i64],
    tag_id_to_meta: &std::collections::HashMap<i64, (String, String)>,
) -> Result<(), String> {
    if tag_ids.len() < 2 {
        return Ok(());
    }
    let mut pairs: Vec<(i64, i64)> = Vec::with_capacity(tag_ids.len() * (tag_ids.len() - 1) / 2);
    for i in 0..tag_ids.len() {
        let a = tag_ids[i];
        for &b in &tag_ids[i + 1..] {
            if a < b {
                pairs.push((a, b));
            } else {
                pairs.push((b, a));
            }
        }
    }

    for chunk in pairs.chunks(COOC_ACCOUNT_PAIRS_PER_STMT) {
        let mut sql = String::from(
            "INSERT INTO account_tag_cooccurrence (account_id, tag1_name, tag1_group, tag2_name, tag2_group, cooc_count) VALUES ",
        );
        // Use ?1 for account_id (shared), then 4 sequential ? per pair.
        let mut param_idx = 2usize;
        for (i, _) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            // (?1, ?P, ?P+1, ?P+2, ?P+3, 1) where P = param_idx
            sql.push_str(&format!(
                "(?1, ?{p}, ?{p1}, ?{p2}, ?{p3}, 1)",
                p = param_idx,
                p1 = param_idx + 1,
                p2 = param_idx + 2,
                p3 = param_idx + 3
            ));
            param_idx += 4;
        }
        sql.push_str(
            " ON CONFLICT(account_id, tag1_name, tag1_group, tag2_name, tag2_group) DO UPDATE SET cooc_count = cooc_count + 1"
        );

        let mut stmt = tx
            .prepare(&sql)
            .map_err(|e| format!("prepare account cooc batch: {e}"))?;
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(chunk.len() * 4 + 1);
        params_vec.push(Box::new(account_id));
        for (a, b) in chunk {
            let (na, ga) = tag_id_to_meta
                .get(a)
                .ok_or_else(|| format!("missing tag_id {a} in account cooc meta map"))?;
            let (nb, gb) = tag_id_to_meta
                .get(b)
                .ok_or_else(|| format!("missing tag_id {b} in account cooc meta map"))?;
            // Canonical ordering by (name, group) strings
            if (na, ga) <= (nb, gb) {
                params_vec.push(Box::new(na.clone()));
                params_vec.push(Box::new(ga.clone()));
                params_vec.push(Box::new(nb.clone()));
                params_vec.push(Box::new(gb.clone()));
            } else {
                params_vec.push(Box::new(nb.clone()));
                params_vec.push(Box::new(gb.clone()));
                params_vec.push(Box::new(na.clone()));
                params_vec.push(Box::new(ga.clone()));
            }
        }
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(std::convert::AsRef::as_ref).collect();
        stmt.execute(rusqlite::params_from_iter(params_refs))
            .map_err(|e| format!("exec account cooc batch: {e}"))?;
    }
    Ok(())
}

pub(super) fn upsert_cooccurrence_pairs(
    tx: &rusqlite::Transaction,
    tag_ids: &[i64],
) -> Result<(), String> {
    if tag_ids.len() < 2 {
        return Ok(());
    }
    let n = tag_ids.len();
    let mut pairs: Vec<(i64, i64)> = Vec::with_capacity(n * (n - 1) / 2);
    for i in 0..n {
        let a = tag_ids[i];
        for &b in &tag_ids[i + 1..] {
            // Canonical ordering enforced by CHECK (tag1_id < tag2_id).
            // tag_ids is sorted+deduped before this call, so a < b is implied.
            if a < b {
                pairs.push((a, b));
            } else {
                pairs.push((b, a));
            }
        }
    }

    for chunk in pairs.chunks(COOC_PAIRS_PER_STATEMENT) {
        let mut sql =
            String::from("INSERT INTO tag_cooccurrence (tag1_id, tag2_id, cooc_count) VALUES ");
        for i in 0..chunk.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str("(?,?,1)");
        }
        sql.push_str(" ON CONFLICT(tag1_id, tag2_id) DO UPDATE SET cooc_count = cooc_count + 1");

        let mut stmt = tx
            .prepare(&sql)
            .map_err(|e| format!("prepare cooc batch: {e}"))?;
        let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 2);
        for (a, b) in chunk {
            params_vec.push(a);
            params_vec.push(b);
        }
        stmt.execute(rusqlite::params_from_iter(params_vec))
            .map_err(|e| format!("exec cooc batch: {e}"))?;
    }
    Ok(())
}

pub fn set_account_tag_cooccurrence(account_id: i32) -> Result<(), String> {
    // The existing per-account row count can reach millions; folding the
    // DELETE into the same tx as the INSERT-SELECT held the writer mutex
    // for the entire scan. Wipe first with the shared batched helper
    // (releases the writer between chunks), then do the rebuild in its
    // own short tx.
    let batch_size = crate::models::cfg().runtime.drop_cooc_batch_size.max(1_000);
    super::drop_account_cooccurrence_batched(account_id, batch_size, |_, _| {})?;

    super::with_write_tx(|tx| {
        tx.execute(
            "
            INSERT INTO account_tag_cooccurrence (
                account_id, tag1_name, tag1_group, tag2_name, tag2_group, cooc_count
            )
            SELECT
                ?1,
                t1.name, t1.group_type,
                t2.name, t2.group_type,
                COUNT(*)
            FROM accounts_post ap
            INNER JOIN tags_posts tp1 ON tp1.post_id = ap.post_id
            INNER JOIN tags_posts tp2
                ON tp2.post_id = ap.post_id
               AND tp1.tag_id < tp2.tag_id
            INNER JOIN tags t1 ON t1.id = tp1.tag_id
            INNER JOIN tags t2 ON t2.id = tp2.tag_id
            WHERE ap.account_id = ?1
            GROUP BY t1.name, t1.group_type, t2.name, t2.group_type
            ",
            params![account_id],
        )
        .map_err(|e| format!("Failed to populate account tag cooccurrence: {e}"))?;
        Ok(())
    })
}

/// Two-pass loader: tags first (sets marginals + builds an SQLite-id →
/// local-`TagId` map), then a JOIN-free scan of `tag_cooccurrence`. The
/// old single-query approach paid for `tag_cooccurrence × tags × tags`
/// (200M+ index lookups + 400M short-string allocations on a 2M-post
/// catalog). This version turns the cooc pass into 3-int rows resolved
/// against an in-memory map → 5-10× faster on prod-scale data.
pub fn load_global_tag_relation() -> rusqlite::Result<crate::utils::TagRelationGraph> {
    let t0 = std::time::Instant::now();
    let conn = open_db().expect("open_db failed");
    let n_posts: i64 =
        conn.query_row("SELECT COUNT(*) FROM posts", [], |row| row.get::<_, i64>(0))?;

    let mut graph = crate::utils::TagRelationGraph::with_posts(n_posts);
    let min_cooc = crate::models::cfg().priors.tag_relation_min_cooc.max(1);

    // ---- Pass 1: tags table → marginals + sqlite_id → TagId map -----------
    let mut sqlite_to_local: HashMap<i64, crate::utils::TagId> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, name, group_type, COALESCE(df, 0) FROM tags")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        for r in rows {
            let (sid, name, group, df) = r?;
            let Some(g) = crate::utils::Group::from_str(&group) else {
                continue;
            };
            let lc = name.to_lowercase();
            // set_marginal interns the (group, lowercased_name) key once
            // and stores df. The local TagId is then recovered via tag_id().
            graph.set_marginal(g as u8, &lc, df);
            if let Some(local_id) = graph.tag_id(g as u8, &lc) {
                sqlite_to_local.insert(sid, local_id);
            }
        }
    }
    let pass1_secs = t0.elapsed().as_secs_f32();

    // ---- Pass 2: tag_cooccurrence raw 3-int scan, no JOIN -----------------
    // Build pairs directly into a Vec and hand them to TagRelationGraph
    // as a frozen sorted slice — the alternative (insert_pair_by_id into
    // a Hot HashMap, then maybe freeze later) holds ~2× the memory peak
    // for the same data because each `HashMap` slot is ~32 B vs ~12 B
    // for a `(u32,u32,u32)` tuple. On a multi-million-pair catalog
    // graph that's the difference between fitting and OOM-killing the
    // calibrate prep step.
    let t_cooc = std::time::Instant::now();
    let mut cooc_rows = 0u64;
    let mut cooc_skipped = 0u64;
    let mut staged: Vec<(crate::utils::TagId, crate::utils::TagId, u32)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT tag1_id, tag2_id, cooc_count FROM tag_cooccurrence WHERE cooc_count >= ?1",
        )?;
        let rows = stmt.query_map(params![min_cooc], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for r in rows {
            let (sid1, sid2, count) = r?;
            cooc_rows += 1;
            match (sqlite_to_local.get(&sid1), sqlite_to_local.get(&sid2)) {
                (Some(&a), Some(&b)) if a != b => {
                    let c = count.max(0).min(i64::from(u32::MAX)) as u32;
                    staged.push((a, b, c));
                }
                _ => cooc_skipped += 1,
            }
        }
    }
    graph.set_pairs_frozen_vec(staged);
    let pass2_secs = t_cooc.elapsed().as_secs_f32();

    info!(
        "[tag-relation] loaded n_posts={} tags={} pairs={} (skipped {}/{} dangling refs); \
         pass1 {:.1}s + pass2 {:.1}s = {:.1}s total",
        n_posts,
        graph.n_tags(),
        graph.n_pairs(),
        cooc_skipped,
        cooc_rows,
        pass1_secs,
        pass2_secs,
        t0.elapsed().as_secs_f32()
    );
    Ok(graph)
}

pub fn get_account_tag_relation_graph(
    account_id: i32,
    top: usize,
    min_user_cooc: i64,
) -> Result<TagRelationGraphPayload, String> {
    let conn = open_db()?;

    let catalog_post_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM posts", [], |row| row.get(0))
        .map_err(|e| format!("Failed to count posts: {e}"))?;
    let account_post_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM accounts_post WHERE account_id = ?1",
            params![account_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to count account posts: {e}"))?;

    let nodes: Vec<TagRelationNode> = {
        let mut stmt = conn
            .prepare(
                "
                SELECT tag_name, group_type, count
                FROM account_tag_counts
                WHERE account_id = ?1
                ORDER BY count DESC, tag_name ASC
                LIMIT ?2
                ",
            )
            .map_err(|e| format!("Failed to prepare top-tags query: {e}"))?;
        stmt.query_map(params![account_id, top as i64], |row| {
            Ok(TagRelationNode {
                name: row.get::<_, String>(0)?,
                group_type: row.get::<_, String>(1)?,
                count: row.get::<_, i64>(2)?,
            })
        })
        .map_err(|e| format!("Failed to fetch top tags: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate top tags: {e}"))?
    };

    if nodes.is_empty() {
        return Ok(TagRelationGraphPayload {
            nodes,
            edges: Vec::new(),
            catalog_post_count,
            account_post_count,
            scoring: Default::default(),
        });
    }

    let mut node_index: HashMap<(String, String), usize> = HashMap::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        node_index.insert((n.name.clone(), n.group_type.clone()), i);
    }

    let pair_min = min_user_cooc.max(1);

    // Bound the edges scan. The route only needs edges among the top-N nodes
    // (≤ `top*(top-1)/2` ≈ 31k for top=250), but a huge account can hold
    // millions of co-occurrence rows and the query previously materialized
    // ALL of them before the top-N filter — a single-request CPU/heap DoS.
    // Order by the strongest co-occurrence and cap how many rows are scanned.
    let edge_limit: i64 = (top as i64).saturating_mul(500).clamp(5000, 250_000);

    let mut stmt = conn
        .prepare(
            "
            SELECT
                atc.tag1_name, atc.tag1_group,
                atc.tag2_name, atc.tag2_group,
                atc.cooc_count,
                COALESCE(c.cooc_count, 0)               AS global_cooc,
                COALESCE(t1.df, 0)                      AS df1,
                COALESCE(t2.df, 0)                      AS df2
            FROM account_tag_cooccurrence atc
            LEFT JOIN tags t1
                   ON t1.name = atc.tag1_name AND t1.group_type = atc.tag1_group
            LEFT JOIN tags t2
                   ON t2.name = atc.tag2_name AND t2.group_type = atc.tag2_group
            LEFT JOIN tag_cooccurrence c
                   ON c.tag1_id = t1.id AND c.tag2_id = t2.id
            WHERE atc.account_id = ?1
              AND atc.cooc_count >= ?2
            ORDER BY atc.cooc_count DESC
            LIMIT ?3
            ",
        )
        .map_err(|e| format!("Failed to prepare relation graph query: {e}"))?;

    let rows = stmt
        .query_map(params![account_id, pair_min, edge_limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .map_err(|e| format!("Failed to fetch relation rows: {e}"))?;

    let n_global = catalog_post_count.max(1) as f32;
    let mut edges: Vec<TagRelationEdge> = Vec::new();

    for row in rows {
        let (t1, g1, t2, g2, user_cooc, global_cooc, df1, df2) =
            row.map_err(|e| format!("Failed to read relation row: {e}"))?;
        let (Some(&i), Some(&j)) = (node_index.get(&(t1, g1)), node_index.get(&(t2, g2))) else {
            continue;
        };
        if i == j {
            continue;
        }
        let lift = if global_cooc > 0 && df1 > 0 && df2 > 0 {
            (global_cooc as f32 * n_global) / (df1 as f32 * df2 as f32)
        } else {
            0.0
        };
        edges.push(TagRelationEdge {
            source: i.min(j),
            target: i.max(j),
            user_cooc,
            global_cooc,
            global_lift: lift,
        });
    }

    edges.sort_by(|a, b| {
        b.user_cooc
            .cmp(&a.user_cooc)
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.target.cmp(&b.target))
    });

    Ok(TagRelationGraphPayload {
        nodes,
        edges,
        catalog_post_count,
        account_post_count,
        scoring: Default::default(),
    })
}

pub fn load_account_tag_relation(
    account_id: i32,
    user_tag_counts: &[TagCount],
) -> Result<crate::utils::TagRelationGraph, String> {
    let edge_limit = crate::models::cfg().runtime.user_relation_edge_limit.max(1) as i64;
    let conn = open_db()?;
    load_account_tag_relation_conn(&conn, account_id, user_tag_counts, edge_limit)
}

/// Worker behind [`load_account_tag_relation`] with an explicit edge cap.
/// Only the strongest `edge_limit` account co-occurrence pairs (by
/// `cooc_count`) feed the MMR user-relation graph; weaker pairs are pruned at
/// the SQL level. This mirrors `get_account_tag_relation_graph`'s `edge_limit`
/// (ORDER BY cooc_count DESC LIMIT n) and bounds both the scan and the
/// per-row `insert_pair` graph build, which previously materialized the entire
/// account co-occurrence table (hundreds of thousands to millions of rows for
/// active accounts — the dominant remaining `db_hydrate` cost on large
/// accounts). A `&Connection` is threaded through so the cap is unit-testable
/// against an in-memory database.
pub fn load_account_tag_relation_conn(
    conn: &Connection,
    account_id: i32,
    user_tag_counts: &[TagCount],
    edge_limit: i64,
) -> Result<crate::utils::TagRelationGraph, String> {
    let edge_limit = edge_limit.max(1);
    let total_posts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM accounts_post WHERE account_id = ?1",
            params![account_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to count account posts: {e}"))?;

    let mut graph = crate::utils::TagRelationGraph::with_posts(total_posts);

    for tc in user_tag_counts {
        if let Some(g) = crate::utils::Group::from_str(&tc.group_type) {
            graph.set_marginal(g as u8, &tc.name.to_lowercase(), tc.count);
        }
    }

    let mut stmt = conn
        .prepare(
            "
            SELECT tag1_name, tag1_group, tag2_name, tag2_group, cooc_count
            FROM account_tag_cooccurrence
            WHERE account_id = ?1
            ORDER BY cooc_count DESC
            LIMIT ?2
            ",
        )
        .map_err(|e| format!("Failed to prepare account tag cooc query: {e}"))?;

    let rows = stmt
        .query_map(params![account_id, edge_limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|e| format!("Failed to execute account tag cooc query: {e}"))?;

    for r in rows {
        let (t1, g1, t2, g2, c) = r.map_err(|e| format!("Failed to read account cooc row: {e}"))?;
        if let (Some(gk1), Some(gk2)) = (
            crate::utils::Group::from_str(&g1).map(|g| g as u8),
            crate::utils::Group::from_str(&g2).map(|g| g as u8),
        ) {
            graph.insert_pair(gk1, &t1.to_lowercase(), gk2, &t2.to_lowercase(), c);
        }
    }

    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts_post (account_id INTEGER NOT NULL, post_id INTEGER NOT NULL);
             CREATE TABLE account_tag_cooccurrence (
                account_id INTEGER NOT NULL,
                tag1_name TEXT NOT NULL, tag1_group TEXT NOT NULL,
                tag2_name TEXT NOT NULL, tag2_group TEXT NOT NULL,
                cooc_count INTEGER NOT NULL
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn relation_edge_limit_caps_loaded_pairs_and_keeps_strongest() {
        let conn = mem_conn();
        for i in 0..20i64 {
            conn.execute(
                "INSERT INTO accounts_post (account_id, post_id) VALUES (1, ?1)",
                [i + 1],
            )
            .unwrap();
            // pair (artist_{i}, artist_{i+1}) — cooc_count grows with i, so the
            // top-5 pairs are i = 18..=14 (descending by cooc_count).
            conn.execute(
                "INSERT INTO account_tag_cooccurrence (account_id, tag1_name, tag1_group, tag2_name, tag2_group, cooc_count)
                 VALUES (1, ?1, 'artist', ?2, 'artist', ?3)",
                rusqlite::params![
                    format!("a{i}"),
                    format!("b{i}"),
                    100 + i * 10
                ],
            )
            .unwrap();
        }
        let tags: Vec<TagCount> = Vec::new();

        let g = load_account_tag_relation_conn(&conn, 1, &tags, 5).unwrap();
        // Only the top `edge_limit` pairs are materialized.
        assert_eq!(g.n_pairs(), 5, "graph must be capped to edge_limit");
        // The strongest pair (i=19, cooc 290) must be present.
        let (a, b) = (
            g.tag_id(crate::utils::Group::Artist as u8, "a19").unwrap(),
            g.tag_id(crate::utils::Group::Artist as u8, "b19").unwrap(),
        );
        assert!(g.cooc_by_id(a, b) > 0, "strongest pair retained");
        // The weakest pair (i=0, cooc 100) must have been pruned.
        let a0 = g.tag_id(crate::utils::Group::Artist as u8, "a0");
        assert!(a0.is_none(), "weakest pair pruned");
    }

    #[test]
    fn relation_with_small_account_loads_all_pairs() {
        let conn = mem_conn();
        conn.execute(
            "INSERT INTO accounts_post (account_id, post_id) VALUES (1, 1)",
            [],
        )
        .unwrap();
        for i in 0..3i64 {
            conn.execute(
                "INSERT INTO account_tag_cooccurrence (account_id, tag1_name, tag1_group, tag2_name, tag2_group, cooc_count)
                 VALUES (1, ?1, 'artist', ?2, 'artist', ?3)",
                rusqlite::params![format!("x{i}"), format!("y{i}"), 5],
            )
            .unwrap();
        }
        let g = load_account_tag_relation_conn(&conn, 1, &[], 100).unwrap();
        // Fewer rows than the limit: everything still loads (no pruning).
        assert_eq!(g.n_pairs(), 3);
    }
}
