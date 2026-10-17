use rusqlite::params;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use crate::models::{Post, TagCount};

use super::open_db;

/// TTL cache for `get_tag_counts`. Tag counts only change during `/process`
/// (profile refresh), which calls `clear_tag_counts_cache()`. Between
/// refreshes they're static, so a short TTL (30s) saves redundant PK lookups
/// during infinite-scroll pagination.
struct CachedTagCounts {
    counts: Vec<TagCount>,
    inserted_at: std::time::Instant,
}

static TAG_COUNTS_CACHE: LazyLock<Mutex<HashMap<i32, CachedTagCounts>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Cache TTL for tag counts. 30 seconds is long enough to dedup
/// infinite-scroll pages for the same account, short enough that stale
/// data after a `/process` refresh resolves within half a minute.
const TAG_COUNTS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Hard cap on cached tag-count snapshots. Each distinct account_id ever
/// queried previously stayed cached forever (TTL only gated reads), so an
/// attacker who could mint many accounts could grow this map without bound.
/// Expired/stale entries are evicted on insert and the map is capped below.
const TAG_COUNTS_CACHE_MAX_ENTRIES: usize = 2048;

/// Clear the tag counts cache for a specific account.
/// Called after `/process` updates an account's tag counts.
pub fn clear_tag_counts_cache(account_id: i32) {
    if let Ok(mut cache) = TAG_COUNTS_CACHE.lock() {
        cache.remove(&account_id);
    }
}

/// Clear the entire tag counts cache. Used when bulk operations may have
/// invalidated multiple accounts at once.
pub fn clear_all_tag_counts_caches() {
    if let Ok(mut cache) = TAG_COUNTS_CACHE.lock() {
        cache.clear();
    }
}

/// Replace catalog tag relations with the authoritative upstream tag set.
///
/// Hydration repairs historical imports, so retaining removed tags would leave
/// stale scorer/category data. It deliberately does not update account
/// co-occurrence: catalog hydration is not an account preference event.
pub fn replace_posts_tags_batch(posts: &[Post]) -> Result<(), String> {
    if posts.is_empty() {
        return Ok(());
    }
    // One transaction for the delete AND the re-insert. Splitting them meant a
    // failure in between left the posts tagless (with `tags.df` stale), and a
    // concurrent reader could observe a post with zero tags.
    let effects = super::with_write_tx(|tx| {
        let mut delete = tx
            .prepare_cached("DELETE FROM tags_posts WHERE post_id = ?1")
            .map_err(|e| format!("prep replace post tags: {e}"))?;
        for post in posts {
            delete
                .execute(params![post.id])
                .map_err(|e| format!("clear tags for post {}: {e}", post.id))?;
        }
        drop(delete);
        save_posts_tags_batch_in_tx(tx, posts, &HashSet::new(), false, None)
    })?;
    effects.apply();
    Ok(())
}

pub fn save_posts_tags_batch(
    posts: &[Post],
    blacklist: &HashSet<String>,
    track_cooccurrence: bool,
    account_id: Option<i32>,
) -> Result<(), String> {
    if posts.is_empty() {
        return Ok(());
    }
    let effects = super::with_write_tx(|tx| {
        save_posts_tags_batch_in_tx(tx, posts, blacklist, track_cooccurrence, account_id)
    })?;
    effects.apply();
    Ok(())
}

/// Upsert posts (with their `accounts_post` link) **and** their tag postings in
/// a single transaction.
///
/// Prefer this over `save_posts` followed by [`save_posts_tags_batch`]: the pair
/// needed two write transactions per page, so every commit also paid the WAL
/// sync and index maintenance twice, and the post was briefly readable with no
/// tags at all. Both upserts are one unit of work — a post row without its tag
/// postings is not a usable post.
///
/// `link_account_id` is the account the posts are linked to; `cooc_account_id`
/// selects the account-level co-occurrence update (seed imports pass `None`
/// because calibration reads no account co-occurrence).
pub fn save_posts_with_tags(
    posts: &[Post],
    link_account_id: i32,
    blacklist: &HashSet<String>,
    track_cooccurrence: bool,
    cooc_account_id: Option<i32>,
) -> Result<(), String> {
    if posts.is_empty() {
        return Ok(());
    }
    let result = super::with_write_tx(|tx| {
        super::posts::save_posts_in_tx(tx, posts, link_account_id)?;
        save_posts_tags_batch_in_tx(tx, posts, blacklist, track_cooccurrence, cooc_account_id)
    });
    // Mirrors `save_posts`: the posts themselves changed, so owned-post and
    // candidate hydration for this account is stale regardless of the outcome.
    super::hydrate_cache::clear_owned_and_candidates(i64::from(link_account_id));
    result.map(TagBatchEffects::apply)
}

/// Catalog-only counterpart of [`save_posts_with_tags`] for callers that must
/// not link the posts to an account.
pub fn upsert_catalog_posts_with_tags(posts: &[Post]) -> Result<(), String> {
    if posts.is_empty() {
        return Ok(());
    }
    let effects = super::with_write_tx(|tx| {
        super::posts::upsert_catalog_posts_in_tx(tx, posts)?;
        save_posts_tags_batch_in_tx(tx, posts, &HashSet::new(), false, None)
    })?;
    effects.apply();
    Ok(())
}

/// In-memory follow-ups of a tag batch, handed back to the caller so they run
/// only **after** its transaction commits.
///
/// The SQL half recomputes `tags.df` inside the transaction, but the in-memory
/// IDF index and the relation-graph dirty flag are process state with no
/// rollback: bumping them for a batch whose COMMIT then fails (busy timeout,
/// deferred constraint) leaves the index claiming postings the database never
/// accepted. Applying them from the caller, past the `with_write_tx` result,
/// is what keeps those two in step.
#[derive(Default)]
pub(super) struct TagBatchEffects {
    df_delta_by_name: HashMap<String, i64>,
    cooc_dirty: bool,
}

impl TagBatchEffects {
    pub(super) fn apply(self) {
        if !self.df_delta_by_name.is_empty() {
            // n_posts_delta = 0: catalog growth shifts every IDF by a tiny
            // fraction, and the periodic full rebuild reconciles the absolute
            // level.
            crate::utils::bump_idf(self.df_delta_by_name, 0);
        }
        if self.cooc_dirty {
            crate::utils::mark_global_relation_dirty();
        }
    }
}

/// Body of [`save_posts_tags_batch`], taking an existing connection/transaction
/// so callers can combine it with other statements atomically
/// (see [`replace_posts_tags_batch`] and [`save_posts_with_tags`]).
///
/// Returns the side effects for the caller to [`TagBatchEffects::apply`] once
/// the transaction has committed.
pub(super) fn save_posts_tags_batch_in_tx(
    tx: &rusqlite::Connection,
    posts: &[Post],
    blacklist: &HashSet<String>,
    track_cooccurrence: bool,
    account_id: Option<i32>,
) -> Result<TagBatchEffects, String> {
    if posts.is_empty() {
        return Ok(TagBatchEffects::default());
    }

    let mut cooc_dirty = false;
    let mut touched_tag_ids: HashSet<i64> = HashSet::new();
    // tag_id → number of (tag_id, post_id) rows actually inserted in this
    // batch. `INSERT OR IGNORE` returns 0 on duplicate, so summing
    // rows_affected gives us "new postings only" — that's the per-tag df
    // delta we publish to the in-memory IDF index after commit.
    let mut df_delta_by_id: HashMap<i64, i64> = HashMap::new();
    // Per-batch cache: ~5–10K tag references but only a few hundred distinct
    // (name, group) pairs in a 320-post batch. Caching collapses repeats to
    // a HashMap hit and avoids the upsert+RETURNING round-trip.
    let mut tag_id_cache: HashMap<(String, &'static str), i64> = HashMap::new();
    {
        // The `DO UPDATE SET name = name` no-op forces RETURNING to fire on
        // the conflict path, so we always get a row back.
        let mut upsert_tag = tx
            .prepare_cached(
                "INSERT INTO tags (name, group_type) VALUES (?1, ?2)
                 ON CONFLICT(name, group_type) DO UPDATE SET name = name
                 RETURNING id",
            )
            .map_err(|e| format!("prep upsert tag: {e}"))?;
        let mut link = tx
            .prepare_cached("INSERT OR IGNORE INTO tags_posts(tag_id, post_id) VALUES (?1, ?2)")
            .map_err(|e| format!("prep link: {e}"))?;
        let mut post_has_tags = tx
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM tags_posts WHERE post_id = ?1)")
            .map_err(|e| format!("prep post_has_tags: {e}"))?;

        for post in posts {
            let pid = post.id;
            let had_tags: bool = if track_cooccurrence {
                post_has_tags
                    .query_row(params![pid], |r| r.get(0))
                    .map_err(|e| format!("post_has_tags lookup: {e}"))?
            } else {
                true
            };

            let mut post_tag_ids: Vec<i64> = Vec::new();

            // Meta is stored so the interaction channel can read it; the
            // other scorers filter meta out via group_wts[Meta] = 0.
            for (group, tags) in [
                ("artist", &post.tags.artist),
                ("character", &post.tags.character),
                ("copyright", &post.tags.copyright),
                ("general", &post.tags.general),
                ("lore", &post.tags.lore),
                ("species", &post.tags.species),
                ("meta", &post.tags.meta),
            ] {
                for tag in tags {
                    if tag.is_empty() {
                        continue;
                    }
                    let tag_lc = if tag.bytes().any(|b| b.is_ascii_uppercase()) {
                        tag.to_ascii_lowercase()
                    } else {
                        tag.clone()
                    };
                    if blacklist.contains(&tag_lc) {
                        continue;
                    }

                    let cache_key = (tag_lc, group);
                    let tag_id = if let Some(&id) = tag_id_cache.get(&cache_key) {
                        id
                    } else {
                        let id: i64 = upsert_tag
                            .query_row(params![&cache_key.0, group], |r| r.get(0))
                            .map_err(|e| format!("upsert tag {}:{group}: {e}", cache_key.0))?;
                        tag_id_cache.insert(cache_key, id);
                        id
                    };

                    let inserted = link
                        .execute(params![tag_id, pid])
                        .map_err(|e| format!("link tag_id={tag_id} post_id={pid}: {e}"))?;

                    if inserted > 0 {
                        *df_delta_by_id.entry(tag_id).or_insert(0) += inserted as i64;
                    }
                    touched_tag_ids.insert(tag_id);
                    post_tag_ids.push(tag_id);
                }
            }

            let has_multi_tags = post_tag_ids.len() >= 2;
            // Sort+dedup once per post — unconditional so the account-cooc
            // branch below can never see duplicate tag_ids (which would
            // produce phantom self-pairs in `account_tag_cooccurrence`).
            // The previous "skip if cooc_dirty" optimisation conflated a
            // batch-level flag with a per-post invariant: once any earlier
            // post had triggered the global branch, this branch would skip
            // the sort+dedup on every subsequent post regardless of whether
            // those posts had been sorted.
            if has_multi_tags {
                post_tag_ids.sort_unstable();
                post_tag_ids.dedup();
            }

            if track_cooccurrence && !had_tags && has_multi_tags {
                super::cooccurrence::upsert_cooccurrence_pairs(tx, &post_tag_ids)?;
                cooc_dirty = true;
            }
            // Class G: incremental account-level cooccurrence update.
            // Since we cleared account_tag_cooccurrence in drop_account_posts,
            // every post's tag pairs are fresh; ON CONFLICT handles dedup.
            // The table is keyed by tag ids, so nothing has to be translated
            // back to names on the write path.
            if let Some(account_id) = account_id
                && track_cooccurrence
                && has_multi_tags
            {
                super::cooccurrence::upsert_account_cooccurrence_pairs(
                    tx,
                    account_id,
                    &post_tag_ids,
                )?;
            }
        }

        if !touched_tag_ids.is_empty() {
            recompute_df_for_tags(tx, &touched_tag_ids)?;
        }
    }

    // Translate the tag-id deltas into tag names here, while `tag_id_cache`
    // is still in hand; the caller owns applying them once the commit lands.
    let df_delta_by_name: HashMap<String, i64> = if df_delta_by_id.is_empty() {
        HashMap::new()
    } else {
        let mut id_to_name: HashMap<i64, String> = HashMap::with_capacity(tag_id_cache.len());
        for ((name, _group), id) in &tag_id_cache {
            id_to_name.entry(*id).or_insert_with(|| name.clone());
        }
        df_delta_by_id
            .iter()
            .filter_map(|(id, delta)| id_to_name.get(id).map(|n| (n.clone(), *delta)))
            .collect()
    };

    Ok(TagBatchEffects {
        df_delta_by_name,
        cooc_dirty,
    })
}

pub(super) fn recompute_df_for_tags(
    tx: &rusqlite::Connection,
    tag_ids: &HashSet<i64>,
) -> Result<(), String> {
    if tag_ids.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = tag_ids.iter().copied().collect();
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "UPDATE tags
                SET df = (SELECT COUNT(*) FROM tags_posts WHERE tag_id = tags.id)
              WHERE id IN ({placeholders})"
        );
        let mut stmt = tx.prepare(&sql).map_err(|e| format!("prep df: {e}"))?;
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        stmt.execute(rusqlite::params_from_iter(params_vec))
            .map_err(|e| format!("exec df: {e}"))?;
    }
    Ok(())
}

pub fn set_tag_counts(account_id: i32) -> Result<(), String> {
    let connection = open_db()?;

    let counts: Vec<TagCount> = {
        let mut stmt = connection
            .prepare(
                r"
        SELECT t.name, t.group_type, COUNT(*) as count
        FROM tags t
        INNER JOIN tags_posts tp ON t.id = tp.tag_id
        INNER JOIN accounts_post ap ON tp.post_id = ap.post_id
        WHERE ap.account_id = ?
        GROUP BY t.name, t.group_type
        ORDER BY count DESC
        ",
            )
            .map_err(|e| format!("Failed to construct query: {e}"))?;

        stmt.query_map([account_id], |row| {
            Ok(TagCount {
                name: row.get(0)?,
                group_type: row.get(1)?,
                count: row.get(2)?,
            })
        })
        .map_err(|e| format!("Failed to get accounts: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate accounts: {e}"))?
    };

    drop(connection);

    super::with_write_tx(|tx| {
        tx.execute(
            "DELETE FROM account_tag_counts WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to delete account_tag_counts: {e}"))?;

        let mut insert_calc = tx
            .prepare_cached(
                "
        INSERT INTO account_tag_counts (account_id, tag_name, group_type, count)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(account_id, tag_name, group_type) DO UPDATE SET
        count = excluded.count;
        ",
            )
            .map_err(|e| format!("Failed to prepare transaction: {e}"))?;

        for entry in counts {
            insert_calc
                .execute(params![
                    account_id,
                    entry.name,
                    entry.group_type,
                    entry.count
                ])
                .map_err(|e| format!("Failed to execute transaction: {e}"))?;
        }
        Ok(())
    })
}

pub fn get_tag_counts(account_id: i32) -> Result<Vec<TagCount>, String> {
    // Check TTL cache first (dedups repeated lookups for the same account
    // within a 30-second window, e.g. infinite-scroll pagination).
    if let Ok(cache) = TAG_COUNTS_CACHE.lock()
        && let Some(entry) = cache.get(&account_id)
        && entry.inserted_at.elapsed() < TAG_COUNTS_CACHE_TTL
    {
        return Ok(entry.counts.clone());
    }

    let conn = open_db()?;

    let mut stmt = conn
        .prepare("SELECT * FROM account_tag_counts WHERE account_id = ?")
        .map_err(|e| format!("Failed to construct query: {e}"))?;

    let counts = stmt
        .query_map([account_id], |row| {
            Ok(TagCount {
                name: row.get(1)?,
                group_type: row.get(2)?,
                count: row.get(3)?,
            })
        })
        .map_err(|e| format!("Failed to get accounts: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate accounts: {e}"))?;

    // Store in cache before returning.
    if let Ok(mut cache) = TAG_COUNTS_CACHE.lock() {
        // Reclaim stale entries so per-account snapshots don't accumulate
        // forever; then enforce a hard size cap as a backstop.
        cache.retain(|_, e| e.inserted_at.elapsed() < TAG_COUNTS_CACHE_TTL);
        cache.insert(
            account_id,
            CachedTagCounts {
                counts: counts.clone(),
                inserted_at: std::time::Instant::now(),
            },
        );
        if cache.len() > TAG_COUNTS_CACHE_MAX_ENTRIES {
            let mut keys: Vec<(std::time::Instant, i32)> =
                cache.iter().map(|(k, v)| (v.inserted_at, *k)).collect();
            keys.sort_by_key(|(t, _)| *t);
            let excess = cache.len() - TAG_COUNTS_CACHE_MAX_ENTRIES;
            for (_, k) in keys.into_iter().take(excess) {
                cache.remove(&k);
            }
        }
    }

    Ok(counts)
}

pub fn get_tags_df() -> rusqlite::Result<HashMap<String, i64>> {
    let conn = open_db().unwrap();
    let mut stmt = conn.prepare("SELECT name, df FROM tags")?;

    let mut map = HashMap::new();
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;

    for pair in rows {
        let (name, df) = pair?;
        map.insert(name, df);
    }

    Ok(map)
}

// ── Tag aliases & implications persistence ────────────────────────────

use crate::models::{TagAlias, TagImplication};

/// Statuses the `tag_aliases` / `tag_implications` schema declares.
const DECLARED_RELATION_STATUSES: [&str; 7] = [
    "active",
    "deleted",
    "processing",
    "queued",
    "retired",
    "error",
    "pending",
];

/// Fold an upstream `status` onto the declared set.
///
/// e621 keeps a free-form message in this column for every relation whose
/// validation failed (`"error: Validation failed: …"`), so what arrives is not
/// the seven-value enum the migration declares and the `CHECK` constraint
/// rejects it. Anything unrecognised is folded into `error`: never aborting the
/// batch it arrived in, and never reading as `active` — the only status any
/// reader filters on. The second element is the raw text when it was not
/// declared, so the caller can report what upstream actually sent.
fn canonical_relation_status(raw: &str) -> (&'static str, Option<&str>) {
    for declared in DECLARED_RELATION_STATUSES {
        if declared == raw {
            return (declared, None);
        }
    }
    ("error", Some(raw))
}

/// Resolve a tag-relation table name against a closed whitelist.
///
/// `save_tag_relations` and `count_tag_relation` splice the table name
/// straight into SQL text, so the parameter must never carry caller-controlled
/// data. Every current caller passes a literal; this turns that from a
/// convention into a checked invariant, mirroring the table whitelist in
/// `db::posts::delete_by_account_in_batches`.
fn resolve_tag_relation_table(table: &str) -> Result<&'static str, String> {
    match table {
        "tag_aliases" => Ok("tag_aliases"),
        "tag_implications" => Ok("tag_implications"),
        other => Err(format!(
            "unknown tag relation table '{other}' \
             (expected 'tag_aliases' or 'tag_implications')"
        )),
    }
}

/// Bulk upsert tag relation entries (aliases or implications) with
/// `INSERT OR REPLACE`, so existing rows are overwritten with fresh
/// status/dates. Both tables share the same schema:
/// (`antecedent_name`, `consequent_name`, status, `created_at`, `updated_at`).
///
/// `table` is validated against [`resolve_tag_relation_table`] before it is
/// interpolated into SQL.
pub fn save_tag_relations(table: &str, entries: &[impl TagRelationEntry]) -> Result<(), String> {
    if entries.is_empty() {
        return Ok(());
    }
    let table = resolve_tag_relation_table(table)?;
    // Every entry shares one transaction, so a single value the CHECK
    // constraint rejects rolls back the whole page — that is how one upstream
    // row froze the entire import. Resolve the statuses up front and report
    // anything that was not the declared shape.
    let mut folded_messages: usize = 0;
    let mut unrecognised: BTreeMap<&str, usize> = BTreeMap::new();
    let statuses: Vec<&'static str> = entries
        .iter()
        .map(|entry| {
            let (status, raw) = canonical_relation_status(entry.status());
            if let Some(raw) = raw {
                // `error: <message>` is e621's documented shape for a relation
                // whose validation failed; every other value is news.
                if raw.starts_with("error") {
                    folded_messages += 1;
                } else {
                    *unrecognised.entry(raw).or_insert(0) += 1;
                }
            }
            status
        })
        .collect();
    if folded_messages > 0 {
        debug!("[{table}] folded {folded_messages} 'error: …' status(es) to 'error'");
    }
    if !unrecognised.is_empty() {
        warn!(
            "[{table}] unrecognised upstream status value(s) folded to 'error': {unrecognised:?}"
        );
    }

    let sql = format!(
        "INSERT OR REPLACE INTO {table}
         (antecedent_name, consequent_name, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)"
    );
    super::with_write_tx(|tx| {
        let mut stmt = tx
            .prepare_cached(&sql)
            .map_err(|e| format!("prep {table} upsert: {e}"))?;
        for (entry, status) in entries.iter().zip(&statuses) {
            stmt.execute(params![
                entry.antecedent_name().to_ascii_lowercase(),
                entry.consequent_name().to_ascii_lowercase(),
                *status,
                entry.created_at().unwrap_or(""),
                entry.updated_at().unwrap_or(""),
            ])
            .map_err(|e| {
                format!(
                    "insert {table} {} -> {}: {e}",
                    entry.antecedent_name(),
                    entry.consequent_name()
                )
            })?;
        }
        Ok(())
    })
}

// Helper trait to unify TagAlias and TagImplication for save_tag_relations
pub trait TagRelationEntry {
    fn antecedent_name(&self) -> &str;
    fn consequent_name(&self) -> &str;
    fn status(&self) -> &str;
    fn created_at(&self) -> Option<&str>;
    fn updated_at(&self) -> Option<&str>;
}

impl TagRelationEntry for TagAlias {
    fn antecedent_name(&self) -> &str {
        &self.antecedent_name
    }
    fn consequent_name(&self) -> &str {
        &self.consequent_name
    }
    fn status(&self) -> &str {
        &self.status
    }
    fn created_at(&self) -> Option<&str> {
        self.created_at.as_deref()
    }
    fn updated_at(&self) -> Option<&str> {
        self.updated_at.as_deref()
    }
}

impl TagRelationEntry for TagImplication {
    fn antecedent_name(&self) -> &str {
        &self.antecedent_name
    }
    fn consequent_name(&self) -> &str {
        &self.consequent_name
    }
    fn status(&self) -> &str {
        &self.status
    }
    fn created_at(&self) -> Option<&str> {
        self.created_at.as_deref()
    }
    fn updated_at(&self) -> Option<&str> {
        self.updated_at.as_deref()
    }
}

// Convenience wrappers
pub fn save_tag_aliases(aliases: &[TagAlias]) -> Result<(), String> {
    save_tag_relations("tag_aliases", aliases)
}

pub fn save_tag_implications(implications: &[TagImplication]) -> Result<(), String> {
    save_tag_relations("tag_implications", implications)
}

/// Bulk-ensure tag (name, group) rows exist, from the db_exports `tags.csv`.
/// The catalog `tags` table has no `post_count` column (only `df`, which the
/// post-ingest path recomputes), so this just guarantees the taxonomy rows
/// are present with the right group before any posts reference them.
pub fn upsert_catalog_tags(rows: &[(String, String)]) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    super::with_write_tx(|tx| {
        let mut stmt = tx
            .prepare_cached("INSERT OR IGNORE INTO tags (name, group_type) VALUES (?1, ?2)")
            .map_err(|e| format!("prep tags upsert: {e}"))?;
        for (name, group) in rows {
            stmt.execute(params![name, group])
                .map_err(|e| format!("upsert tag {name}:{group}: {e}"))?;
        }
        Ok(())
    })
}

/// Resolve a tag through the alias chain to find its canonical name.
/// E.g. "canyne" -> "canine" (following the `consequent_name` chain).
pub fn get_alias_consequent(tag: &str) -> Result<Option<String>, String> {
    let tag = tag.to_ascii_lowercase();
    let conn = open_db()?;
    // Follow the chain: if alias has status 'active', return consequent_name.
    let mut stmt = conn
        .prepare(
            "SELECT consequent_name FROM tag_aliases
             WHERE antecedent_name = ?1 AND status = 'active'
             LIMIT 1",
        )
        .map_err(|e| format!("prep get_alias_consequent: {e}"))?;
    let result: Option<String> = stmt.query_row(params![tag], |row| row.get(0)).ok();
    Ok(result)
}

/// Get all active tags that imply this tag (reverse implications).
/// Returns antecedents where `consequent_name` equals the given tag.
pub fn get_implied_by(tag: &str) -> Result<Vec<String>, String> {
    let tag = tag.to_ascii_lowercase();
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT antecedent_name FROM tag_implications
             WHERE consequent_name = ?1 AND status = 'active'
             ORDER BY antecedent_name",
        )
        .map_err(|e| format!("prep get_implied_by: {e}"))?;
    let results = stmt
        .query_map(params![tag], |row| row.get::<_, String>(0))
        .map_err(|e| format!("query get_implied_by: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect get_implied_by: {e}"))?;
    Ok(results)
}

/// Get all active implications for a tag (what this tag implies).
pub fn get_implications(tag: &str) -> Result<Vec<String>, String> {
    let tag = tag.to_ascii_lowercase();
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT consequent_name FROM tag_implications
             WHERE antecedent_name = ?1 AND status = 'active'
             ORDER BY consequent_name",
        )
        .map_err(|e| format!("prep get_implications: {e}"))?;
    let results = stmt
        .query_map(params![tag], |row| row.get::<_, String>(0))
        .map_err(|e| format!("query get_implications: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect get_implications: {e}"))?;
    Ok(results)
}

/// Batch fetch implications for multiple tags in one query.
/// Returns a map of tag → list of tags it implies.
pub fn get_implications_batch(tags: &[String]) -> Result<HashMap<String, Vec<String>>, String> {
    if tags.is_empty() {
        return Ok(HashMap::new());
    }
    let conn = open_db()?;
    let placeholders: Vec<String> = tags
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect();
    let sql = format!(
        "SELECT antecedent_name, consequent_name FROM tag_implications
         WHERE antecedent_name IN ({}) AND status = 'active'
         ORDER BY antecedent_name, consequent_name",
        placeholders.join(", ")
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prep get_implications_batch: {e}"))?;
    let params_refs: Vec<&dyn rusqlite::ToSql> =
        tags.iter().map(|t| t as &dyn rusqlite::ToSql).collect();
    let rows = stmt
        .query_map(params_refs.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("query get_implications_batch: {e}"))?;

    let mut result: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (ante, cons) = row.map_err(|e| format!("row get_implications_batch: {e}"))?;
        result.entry(ante).or_default().push(cons);
    }
    Ok(result)
}

/// Get all active aliases that point TO a tag (reverse lookup).
/// Returns the list of antecedents (synonyms) for the given canonical tag.
pub fn get_aliases_for(tag: &str) -> Result<Vec<String>, String> {
    let tag = tag.to_ascii_lowercase();
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT antecedent_name FROM tag_aliases
             WHERE consequent_name = ?1 AND status = 'active'
             ORDER BY antecedent_name",
        )
        .map_err(|e| format!("prep get_aliases_for: {e}"))?;
    let results = stmt
        .query_map(params![tag], |row| row.get::<_, String>(0))
        .map_err(|e| format!("query get_aliases_for: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect get_aliases_for: {e}"))?;
    Ok(results)
}

// ── In-memory caches for tag_aliases & tag_implications ─────────────
// These tables are static between import cycles (~24h). Caching them in
// memory avoids N separate SQL queries for every batch resolution.

static ALIASES_CACHE: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static IMPLICATIONS_CACHE: LazyLock<Mutex<HashMap<String, Vec<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Whether [`load_tag_relation_caches`] has run since the last clear.
///
/// This MUST be a separate flag rather than `!map.is_empty()`: an empty
/// `tag_aliases` table is a legitimate state (import disabled via
/// `tag_alias_import_interval_secs = 0`, or nothing imported yet), and with
/// emptiness as the signal every tag-relation/taste-profile request re-scanned
/// both tables.
static TAG_RELATION_CACHES_LOADED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Check whether the cache has been loaded.
fn is_cache_loaded() -> bool {
    TAG_RELATION_CACHES_LOADED.load(std::sync::atomic::Ordering::Acquire)
}

/// Load or refresh the in-memory caches from `SQLite`.
pub fn load_tag_relation_caches() -> Result<(), String> {
    let conn = open_db()?;

    // Load aliases
    let mut aliases = HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT antecedent_name, consequent_name FROM tag_aliases WHERE status = 'active'",
            )
            .map_err(|e| format!("prep load aliases cache: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| format!("query load aliases: {e}"))?;
        for row in rows {
            let (ante, cons) = row.map_err(|e| format!("row alias: {e}"))?;
            aliases.insert(ante, cons);
        }
    }

    // Load implications
    let mut implications: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT antecedent_name, consequent_name FROM tag_implications WHERE status = 'active' ORDER BY antecedent_name, consequent_name",
            )
            .map_err(|e| format!("prep load implications cache: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| format!("query load implications: {e}"))?;
        for row in rows {
            let (ante, cons) = row.map_err(|e| format!("row impl: {e}"))?;
            implications.entry(ante).or_default().push(cons);
        }
    }

    if let Ok(mut a) = ALIASES_CACHE.lock() {
        *a = aliases;
    }
    if let Ok(mut i) = IMPLICATIONS_CACHE.lock() {
        *i = implications;
    }
    // Publish "loaded" only after both maps are replaced, so a concurrent
    // reader either sees the old state or the new one — never a half-loaded
    // pair with the flag already set.
    TAG_RELATION_CACHES_LOADED.store(true, std::sync::atomic::Ordering::Release);
    info!(
        "[tag_relation_cache] loaded: {} aliases, {} implicators",
        { ALIASES_CACHE.lock().map_or(0, |c| c.len()) },
        { IMPLICATIONS_CACHE.lock().map_or(0, |c| c.len()) }
    );
    Ok(())
}

/// Clear the in-memory caches (called by the import worker after updating
/// the tables, so the next request lazily reloads).
pub fn clear_tag_relation_caches() {
    if let Ok(mut a) = ALIASES_CACHE.lock() {
        a.clear();
    }
    if let Ok(mut i) = IMPLICATIONS_CACHE.lock() {
        i.clear();
    }
    TAG_RELATION_CACHES_LOADED.store(false, std::sync::atomic::Ordering::Release);
    info!("[tag_relation_cache] cleared");
}

/// Resolve alias from cache (lazy-loads if needed).
pub fn get_alias_consequent_cached(tag: &str) -> Result<Option<String>, String> {
    let tag = tag.to_ascii_lowercase();
    if !is_cache_loaded() {
        load_tag_relation_caches()?;
    }
    if let Ok(cache) = ALIASES_CACHE.lock() {
        return Ok(cache.get(&tag).cloned());
    }
    Ok(None)
}

/// Batch fetch implications from cache (lazy-loads if needed).
pub fn get_implications_batch_cached(
    tags: &[String],
) -> Result<HashMap<String, Vec<String>>, String> {
    if tags.is_empty() {
        return Ok(HashMap::new());
    }
    if !is_cache_loaded() {
        load_tag_relation_caches()?;
    }
    if let Ok(cache) = IMPLICATIONS_CACHE.lock() {
        let mut result = HashMap::with_capacity(tags.len());
        for tag in tags {
            if let Some(impls) = cache.get(tag)
                && !impls.is_empty()
            {
                result.insert(tag.clone(), impls.clone());
            }
        }
        return Ok(result);
    }
    Ok(HashMap::new())
}

// ── Background import worker ──────────────────────────────────────────

use std::time::Duration;

/// Count rows in a tag relation table. `table` is validated against
/// [`resolve_tag_relation_table`] before it is interpolated into SQL.
pub fn count_tag_relation(table: &str) -> Result<i64, String> {
    let table = resolve_tag_relation_table(table)?;
    let conn = open_db()?;
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .map_err(|e| format!("count {table}: {e}"))
}

/// Spawn the background tag-relation import worker.
pub fn spawn_tag_relation_importer() {
    let interval_secs = crate::models::cfg().runtime.tag_alias_import_interval_secs;
    if interval_secs == 0 {
        info!("[tag_relation_import] disabled (interval=0)");
        return;
    }

    tokio::spawn(async move {
        info!("[tag_relation_import] starting (interval={interval_secs}s)");

        if let Err(e) = run_full_import().await {
            error!("[tag_relation_import] initial import failed: {e}");
        }

        loop {
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
            if let Err(e) = run_full_import().await {
                error!("[tag_relation_import] import failed: {e}");
            }
        }
    });
}

async fn run_full_import() -> Result<(), String> {
    use crate::api;

    // ── tag_aliases ──────────────────────────────────────────────
    info!("[tag_relation_import] starting tag_aliases sync…");
    let alias_count = count_tag_relation("tag_aliases").unwrap_or(0);
    if alias_count < 1000 {
        info!(
            "[tag_relation_import] tag_aliases: {} rows (partial/fresh), starting full sync",
            alias_count
        );
        let mut page = 1;
        loop {
            let (entries, _changed) =
                api::fetch_tag_relations::<crate::models::TagAlias>("tag_aliases.json", page)
                    .await?;
            if entries.is_empty() {
                break;
            }
            save_tag_aliases(&entries)?;
            info!(
                "[tag_relation_import] tag_aliases page {page}: {} entries",
                entries.len()
            );
            page += 1;
            if page > 500 {
                break;
            }
        }
        info!("[tag_relation_import] tag_aliases full import complete");
        if let Err(e) = load_tag_relation_caches() {
            warn!("[tag_relation_import] cache preload failed: {e}");
        }
    } else {
        let (entries, changed) =
            api::fetch_tag_relations::<crate::models::TagAlias>("tag_aliases.json", 1).await?;
        if changed {
            if !entries.is_empty() {
                save_tag_aliases(&entries)?;
                clear_tag_relation_caches();
            }
            info!(
                "[tag_relation_import] tag_aliases incremental check: {} entries synced",
                entries.len()
            );
        } else {
            info!("[tag_relation_import] tag_aliases unchanged (304); nothing to sync");
        }
    }

    // ── tag_implications ─────────────────────────────────────────
    info!("[tag_relation_import] starting tag_implications sync…");
    let impl_count = count_tag_relation("tag_implications").unwrap_or(0);
    if impl_count < 10000 {
        info!(
            "[tag_relation_import] tag_implications: {} rows (partial/fresh), starting full sync",
            impl_count
        );
        let mut page = 1;
        loop {
            let (entries, _changed) = api::fetch_tag_relations::<crate::models::TagImplication>(
                "tag_implications.json",
                page,
            )
            .await?;
            if entries.is_empty() {
                break;
            }
            save_tag_implications(&entries)?;
            info!(
                "[tag_relation_import] tag_implications page {page}: {} entries",
                entries.len()
            );
            page += 1;
            if page > 500 {
                break;
            }
        }
        info!("[tag_relation_import] tag_implications full import complete");
        if let Err(e) = load_tag_relation_caches() {
            warn!("[tag_relation_import] cache preload failed: {e}");
        }
    } else {
        let (entries, changed) =
            api::fetch_tag_relations::<crate::models::TagImplication>("tag_implications.json", 1)
                .await?;
        if changed {
            if !entries.is_empty() {
                save_tag_implications(&entries)?;
                clear_tag_relation_caches();
            }
            info!(
                "[tag_relation_import] tag_implications incremental check: {} entries synced",
                entries.len()
            );
        } else {
            info!("[tag_relation_import] tag_implications unchanged (304); nothing to sync");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DECLARED_RELATION_STATUSES, canonical_relation_status, resolve_tag_relation_table,
    };

    /// The exact table the migration creates, so the CHECK constraint under
    /// test is the one production has.
    fn relation_table() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tag_implications (
                 antecedent_name TEXT NOT NULL,
                 consequent_name TEXT NOT NULL,
                 status TEXT NOT NULL CHECK (status IN ('active','deleted','processing','queued','retired','error','pending')),
                 created_at TEXT,
                 updated_at TEXT,
                 PRIMARY KEY(antecedent_name, consequent_name)
             ) STRICT;",
        )
        .unwrap();
        conn
    }

    fn insert(conn: &rusqlite::Connection, status: &str) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT OR REPLACE INTO tag_implications
             (antecedent_name, consequent_name, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["dango_(food)", "food", status, "", ""],
        )
    }

    #[test]
    fn the_value_e621_actually_sends_is_the_one_the_schema_rejects() {
        // Production shape (implication id 75979): e621 writes the validation
        // failure into `status`, so the declared enum is not what arrives.
        // Pin both halves — the raw value must fail the constraint, otherwise
        // this test would pass even if the fold were a no-op.
        let raw = "error: Validation failed: dango_(food) already implies food through another implication";
        let conn = relation_table();
        assert!(
            insert(&conn, raw).is_err(),
            "the raw upstream status must be rejected for this test to mean anything"
        );
        let (folded, report) = canonical_relation_status(raw);
        assert_eq!(folded, "error");
        assert_eq!(report, Some(raw));
        insert(&conn, folded).expect("the folded status must satisfy the constraint");
    }

    #[test]
    fn declared_statuses_pass_through_unchanged() {
        for declared in DECLARED_RELATION_STATUSES {
            assert_eq!(canonical_relation_status(declared), (declared, None));
            insert(&relation_table(), declared)
                .expect("declared status must satisfy the constraint");
        }
    }

    #[test]
    fn unknown_statuses_are_folded_instead_of_aborting_the_batch() {
        // A status e621 adds later must never read as `active` and must never
        // roll back the page it arrived in. Case is significant on purpose:
        // readers compare `status = 'active'` exactly.
        for raw in ["approved", "ACTIVE", ""] {
            let (folded, report) = canonical_relation_status(raw);
            assert_eq!(folded, "error", "{raw:?} must not become an active status");
            assert_eq!(report, Some(raw));
            insert(&relation_table(), folded).expect("folded status must satisfy the constraint");
        }
    }

    /// Only the two known relation tables may reach the SQL text. Anything
    /// else must be refused before it is interpolated.
    #[test]
    fn tag_relation_table_whitelist_rejects_anything_else() {
        assert_eq!(
            resolve_tag_relation_table("tag_aliases").unwrap(),
            "tag_aliases"
        );
        assert_eq!(
            resolve_tag_relation_table("tag_implications").unwrap(),
            "tag_implications"
        );
        for bad in [
            "tags",
            "tag_aliases; DROP TABLE tags",
            "tag_aliases JOIN tags",
            "",
        ] {
            assert!(
                resolve_tag_relation_table(bad).is_err(),
                "{bad:?} must not be accepted as a relation table"
            );
        }
    }
}
