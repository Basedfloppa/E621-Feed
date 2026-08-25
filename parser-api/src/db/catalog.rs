//! Local catalog search (docs/offline-catalog.md, Mode A).
//!
//! Pure reads over `accounts_post` + `tags_posts` — the owner's saved posts
//! and their tags. The catalog page searches the local DB directly
//! (`catalog_search_post_ids`), which is fast enough that maintaining
//! ready-made group buckets or per-post overrides is unnecessary.

use rusqlite::params;

use super::{open_db, with_write_tx};

/// Post ids in the owner's saved catalog matching ALL of the given tag terms
/// (case-insensitive AND), most recent first. Backs the catalog tag search box
/// — mirrors browse/search but restricted to locally-saved posts. The frontend
/// paginates by "empty page", so no total count is computed.
pub fn catalog_search_post_ids(
    account_id: i32,
    terms: &[String],
    limit: i64,
    offset: i64,
) -> Result<Vec<i64>, String> {
    let conn = open_db().map_err(|e| format!("catalog_search_post_ids open: {e}"))?;
    let predicate: String = (0..terms.len())
        .map(|_| {
            "EXISTS (SELECT 1 FROM tags_posts tp JOIN tags t ON t.id = tp.tag_id ".to_string()
                + "WHERE tp.post_id = ap.post_id AND t.name = ? COLLATE NOCASE)"
        })
        .collect::<Vec<_>>()
        .join("\n  AND ");

    let mut ids_sql =
        String::from("SELECT ap.post_id FROM accounts_post ap WHERE ap.account_id = ?1");
    if !terms.is_empty() {
        ids_sql.push_str(" AND ");
        ids_sql.push_str(&predicate);
    }
    ids_sql.push_str(" ORDER BY ap.post_id DESC LIMIT ? OFFSET ?");
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&account_id];
    params.extend(terms.iter().map(|t| t as &dyn rusqlite::ToSql));
    params.push(&limit);
    params.push(&offset);
    let mut stmt = conn
        .prepare(&ids_sql)
        .map_err(|e| format!("catalog search prepare: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |r| r.get::<_, i64>(0))
        .map_err(|e| format!("catalog search query: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("catalog search collect: {e}"))
}

/// Local tag autocomplete for the catalog search box: distinct tag names the
/// account's saved posts carry, prefix-matched (case-insensitive), ordered by
/// frequency then name. Sources suggestions from the **local** DB instead of
/// hitting the remote e621 tag resolver.
pub fn catalog_tag_suggest(
    account_id: i32,
    prefix: &str,
    limit: i64,
) -> Result<Vec<String>, String> {
    let conn = open_db().map_err(|e| format!("catalog_tag_suggest open: {e}"))?;
    if prefix.trim().is_empty() {
        return Ok(Vec::new());
    }
    // Escape LIKE wildcards so a user-entered `%`/`_` matches literally.
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let mut stmt = conn
        .prepare(
            "SELECT t.name, COUNT(*) AS cnt
             FROM tags t
             JOIN tags_posts tp ON tp.tag_id = t.id
             JOIN accounts_post ap ON ap.post_id = tp.post_id
             WHERE ap.account_id = ?1 AND t.name LIKE ?2 ESCAPE '\\'
             GROUP BY t.id, t.name
             ORDER BY cnt DESC, t.name
             LIMIT ?3",
        )
        .map_err(|e| format!("catalog_tag_suggest prepare: {e}"))?;
    let rows = stmt
        .query_map(params![account_id, format!("{}%", escaped), limit], |r| {
            r.get::<_, String>(0)
        })
        .map_err(|e| format!("catalog_tag_suggest query: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("catalog_tag_suggest collect: {e}"))
}

/// Remove a saved post from the owner's catalog (the `accounts_post`
/// association). Returns the number of rows deleted. Media-file cleanup is
/// handled separately by the route (via `media_store`).
pub fn delete_catalog_post(account_id: i32, post_id: i64) -> Result<usize, String> {
    with_write_tx(move |tx| {
        let n = tx
            .execute(
                "DELETE FROM accounts_post WHERE account_id = ?1 AND post_id = ?2",
                params![account_id, post_id],
            )
            .map_err(|e| format!("delete_catalog_post: {e}"))?;
        Ok(n)
    })
}

/// Whether `post_id` is still saved by **any** account. Used after deleting
/// one account's link to decide if the stored original (which is global, not
/// per-account) can be cascaded away: only when no owner remains.
pub fn post_still_saved(post_id: i64) -> Result<bool, String> {
    let conn = open_db().map_err(|e| format!("post_still_saved open: {e}"))?;
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM accounts_post WHERE post_id = ?1)",
        params![post_id],
        |r| r.get::<_, bool>(0),
    )
    .map_err(|e| format!("post_still_saved: {e}"))
}
