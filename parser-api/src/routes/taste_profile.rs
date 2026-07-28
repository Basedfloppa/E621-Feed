//! Single endpoint for the frontend's "Your Taste Profile" card.
//! Computes Taste Themes v3 (Label Propagation community detection) server-side.

use std::collections::{HashMap, HashSet};

use rocket::serde::json::Json;
use rocket_okapi::openapi;

use e621_account_parser_api::{
    auth::OwnerToken,
    db::{
        get_account_by_id, get_implications_batch_cached, get_tag_counts, load_tag_relation_caches,
    },
    db_blocking,
    errors::ApiError,
    models::*,
    ratelimit::{self},
    utils::taste_themes,
};

// ── Route ───────────────────────────────────────────────────────────

#[openapi(tag = "Users")]
#[get("/account/<account_id>/taste-profile?<top>&<min_cooc>")]
pub(crate) async fn get_taste_profile(
    account_id: i32,
    top: Option<i32>,
    min_cooc: Option<i64>,
    owner: OwnerToken,
) -> Result<Json<TasteProfileResponse>, ApiError> {
    crate::validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;

    let top_val = top.unwrap_or(250).clamp(5, 1000) as usize;
    let min_cooc_val = min_cooc.unwrap_or(3).max(1);
    let owner_token = owner_token.clone();

    if let Err(e) = load_tag_relation_caches() {
        error!("Failed to load tag relation caches: {e}");
    }

    let result = db_blocking(move || {
        get_account_by_id(&owner_token, account_id)
            .map_err(|e| format!("account access: {e}"))?;

        let tag_counts = get_tag_counts(account_id)
            .map_err(|e| format!("get_tag_counts: {e}"))?;

        let candidate_tags: Vec<String> = tag_counts
            .iter()
            .filter(|t| matches!(t.group_type.as_str(), "species" | "general" | "lore"))
            .map(|t| t.name.to_ascii_lowercase())
            .collect();

        // Catalog post count for global PMI normalisation
        let n_catalog_posts: i64 = {
            let conn = e621_account_parser_api::db::open_db_for_calibration().unwrap();
            conn.query_row("SELECT COUNT(*) FROM posts", [], |row| row.get(0))
                .unwrap_or(1)
        };

        // User post count for user PMI normalisation
        let n_user_posts: i64 = {
            let conn = e621_account_parser_api::db::open_db_for_calibration().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM accounts_post WHERE account_id = ?1",
                rusqlite::params![account_id],
                |row| row.get(0),
            )
            .unwrap_or(1)
        };

        // Build user PMI graph from account_tag_cooccurrence
        let user_graph = {
            let mut g = e621_account_parser_api::utils::TagRelationGraph::with_posts(n_user_posts);
            use rusqlite::params;
            if let Ok(conn) = e621_account_parser_api::db::open_db_for_calibration() {
                // Marginals for general+lore+species
                for tc in &tag_counts {
                    if matches!(tc.group_type.as_str(), "general" | "lore" | "species") {
                        let gk = group_key(&tc.group_type);
                        g.set_marginal(gk, &tc.name.to_ascii_lowercase(), tc.count);
                    }
                }
                // Co-occurrences
                let allowed: HashSet<String> = tag_counts.iter()
                    .filter(|t| matches!(t.group_type.as_str(), "general" | "lore" | "species"))
                    .map(|t| t.name.to_ascii_lowercase())
                    .collect();

                if let Ok(mut stmt) = conn.prepare(
                    "SELECT atc.tag1_name, atc.tag1_group, atc.tag2_name, atc.tag2_group, atc.cooc_count
                     FROM account_tag_cooccurrence atc
                     WHERE atc.account_id = ?1 AND atc.cooc_count >= ?2"
                )
                    && let Ok(rows) = stmt.query_map(params![account_id, min_cooc_val], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    }) {
                        for (t1, g1, t2, g2, cooc) in rows.flatten() {
                            let t1l = t1.to_ascii_lowercase();
                            let t2l = t2.to_ascii_lowercase();
                            if allowed.contains(&t1l) && allowed.contains(&t2l) {
                                let gk1 = group_key(&g1);
                                let gk2 = group_key(&g2);
                                g.insert_pair(gk1, &t1l, gk2, &t2l, cooc);
                            }
                        }
                    }
            }
            g
        };

        // Build global co-occurrence map for global PMI
        let tag_id_df_map: HashMap<String, (i64, i64)> = {
            use rusqlite::params;
            let conn = e621_account_parser_api::db::open_db_for_calibration().unwrap();
            let mut m = HashMap::new();
            for tc in &tag_counts {
                if let Ok(mut stmt) = conn.prepare("SELECT id, df FROM tags WHERE name = ?1")
                    && let Ok(row) = stmt.query_row(params![tc.name.to_ascii_lowercase()], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1).unwrap_or(0)))
                    }) {
                        m.insert(tc.name.to_ascii_lowercase(), row);
                    }
            }
            m
        };
        let mut global_cooc_map: HashMap<String, (i64, i64, i64)> = HashMap::new();
        {
            let conn = e621_account_parser_api::db::open_db_for_calibration().unwrap();
            let min_g = min_cooc_val.max(1);
            // The profile only uses the selected top-N tags. Restricting both
            // endpoints avoids scanning and materialising unrelated catalog pairs.
            let mut global_names: Vec<(String, i64)> = tag_counts
                .iter()
                .filter(|t| matches!(t.group_type.as_str(), "species" | "general" | "lore"))
                .map(|t| (t.name.to_ascii_lowercase(), t.count))
                .collect();
            global_names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let mut global_names: Vec<String> = global_names.into_iter().map(|(name, _)| name).collect();
            global_names.dedup();
            global_names.truncate(top_val);
            let placeholders = std::iter::repeat_n("?", global_names.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT t1.name, t2.name, tc.cooc_count
                 FROM tag_cooccurrence tc
                 JOIN tags t1 ON t1.id = tc.tag1_id
                 JOIN tags t2 ON t2.id = tc.tag2_id
                 WHERE tc.cooc_count >= ?
                   AND t1.name IN ({placeholders})
                   AND t2.name IN ({placeholders})"
            );
            let mut query_params = vec![rusqlite::types::Value::Integer(min_g)];
            query_params.extend(global_names.iter().cloned().map(rusqlite::types::Value::Text));
            query_params.extend(global_names.iter().cloned().map(rusqlite::types::Value::Text));
            if let Ok(mut stmt) = conn.prepare(&sql)
                && let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(query_params), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                }) {
                    for (t1, t2, cooc) in rows.flatten() {
                        if cooc < min_g { continue; }
                        let t1l = t1.to_ascii_lowercase();
                        let t2l = t2.to_ascii_lowercase();
                        if let (Some(&(_, df1)), Some(&(_, df2))) = (
                            tag_id_df_map.get(&t1l),
                            tag_id_df_map.get(&t2l),
                        ) {
                            let key = taste_themes::canonical_pair_key(&t1l, &t2l);
                            global_cooc_map.insert(key, (cooc, df1, df2));
                        }
                    }
                }
        }

        let tag_df_map: HashMap<String, i64> = tag_id_df_map
            .iter()
            .map(|(name, (_, df))| (name.clone(), *df))
            .collect();

        // Resolve aliases and implications for taste themes
        let aliases = get_alias_consequent_batch_inline(&candidate_tags).unwrap_or_default();
        let implications = get_implications_batch_cached(&candidate_tags).unwrap_or_default();

        // Compute taste themes v3
        let themes = taste_themes::compute_taste_themes(
            &tag_counts,
            &user_graph,
            n_user_posts,
            &global_cooc_map,
            n_catalog_posts,
            &implications,
            &aliases,
            &tag_df_map,
            top_val,
            min_cooc_val,
        );

        Ok(TasteProfileResponse {
            themes,
        })
    })
    .await?;

    Ok(Json(result))
}

fn get_alias_consequent_batch_inline(tags: &[String]) -> Result<HashMap<String, String>, String> {
    use e621_account_parser_api::db::get_alias_consequent_cached;
    let mut result = HashMap::with_capacity(tags.len());
    for tag in tags {
        let mut current = tag.clone();
        let mut seen = HashSet::new();
        while let Some(next) = get_alias_consequent_cached(&current)? {
            if !seen.insert(current.clone()) || next == current {
                break;
            }
            result.insert(current.clone(), next.clone());
            current = next;
        }
    }
    Ok(result)
}

fn group_key(group_type: &str) -> u8 {
    match group_type {
        "artist" => 0,
        "character" => 1,
        "copyright" => 2,
        "species" => 3,
        "general" => 4,
        "lore" => 5,
        _ => 6,
    }
}
