use rusqlite::params;
use std::collections::HashMap;

use crate::models::{TagCount, TagRelationEdge, TagRelationGraphPayload, TagRelationNode};

use super::open_db;

/// SQLite default SQLITE_MAX_VARIABLE_NUMBER is 999. Each pair is 2 params,
/// plus headroom — cap at 200 pairs (400 params) per statement.
const COOC_PAIRS_PER_STATEMENT: usize = 200;

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
    super::with_write_tx(|tx| {
        tx.execute(
            "DELETE FROM account_tag_cooccurrence WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear account tag cooccurrence: {e}"))?;

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
        let mut stmt = conn.prepare(
            "SELECT id, name, group_type, COALESCE(df, 0) FROM tags",
        )?;
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
                    let c = count.max(0).min(u32::MAX as i64) as u32;
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
            ",
        )
        .map_err(|e| format!("Failed to prepare relation graph query: {e}"))?;

    let rows = stmt
        .query_map(params![account_id, pair_min], |row| {
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
    let conn = open_db()?;
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
            ",
        )
        .map_err(|e| format!("Failed to prepare account tag cooc query: {e}"))?;

    let rows = stmt
        .query_map(params![account_id], |row| {
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
