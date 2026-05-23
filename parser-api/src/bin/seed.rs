//! Bootstrap calibration data by importing the favourites of N e621 users
//! ranked by their favourite count.
//!
//! Strategy: e621's `/users.json` doesn't expose `favorite_count` as a sort
//! key, so we (a) widely sample candidates by `post_upload_count` (a strong
//! proxy for engagement), then (b) probe each candidate's `favorite_count`
//! via `/users/<id>.json`, and (c) keep the top `target_count` by that field.
//!
//! Usage:
//!   cargo run --release --bin seed -- 500
//!     # imports favourites of the 500 users with the most favourites among
//!     # the 1500 most active uploaders.
//!
//! All HTTP calls share the global rate gate in `api.rs`, so this won't
//! starve a concurrently-running server.

use std::collections::HashSet;
use std::env;

use e621_account_parser_api::{api, db, models};
use models::{Post, UserApiResponse};

fn load_existing_account_ids() -> anyhow::Result<HashSet<i32>> {
    let conn = db::open_db_for_calibration().map_err(|e| anyhow::anyhow!(e))?;
    let mut stmt = conn.prepare("SELECT DISTINCT account_id FROM accounts_post")?;
    let rows = stmt.query_map([], |r| r.get::<_, i32>(0))?;
    Ok(rows.collect::<Result<HashSet<_>, _>>()?)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let path = models::default_path()?;
    models::reload_from(&path)?;
    db::ensure_sqlite().map_err(|e| anyhow::anyhow!("migrate: {e}"))?;

    let target: usize = env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);
    // Probe ~3× the target so the top-by-favourite-count slice is reasonably
    // converged. More oversampling = better tail; less = faster discovery.
    let pool_size = (target * 3).max(target + 50);

    eprintln!("[seed] target = {target}, candidate pool = {pool_size}");

    // Resume support: skip users we've already imported. Computed once up
    // front and applied at selection time so repeated runs don't refetch
    // the same accounts.
    let already_imported = load_existing_account_ids()?;
    if !already_imported.is_empty() {
        eprintln!(
            "[seed] {} accounts already in DB — they'll be excluded from selection",
            already_imported.len()
        );
    }

    let candidates = discover_candidates(pool_size).await?;
    eprintln!("[seed] discovered {} candidate user ids", candidates.len());

    let probed = probe_favorite_counts(&candidates).await;
    eprintln!("[seed] probed {} users with favorite_count", probed.len());

    let mut sorted = probed;
    sorted.sort_by(|a, b| b.2.cmp(&a.2));
    sorted.retain(|(uid, _, _)| !already_imported.contains(uid));
    let chosen: Vec<(i32, String, i32)> = sorted.into_iter().take(target).collect();

    if chosen.is_empty() {
        anyhow::bail!("no users to import");
    }

    let max_fc = chosen.first().map(|x| x.2).unwrap_or(0);
    let min_fc = chosen.last().map(|x| x.2).unwrap_or(0);
    let median_fc = chosen.get(chosen.len() / 2).map(|x| x.2).unwrap_or(0);
    eprintln!(
        "[seed] selected top {} users by favorite_count (min={min_fc}, median={median_fc}, max={max_fc})",
        chosen.len()
    );

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let total = chosen.len();
    for (i, (uid, name, fc)) in chosen.iter().enumerate() {
        eprintln!(
            "[seed] [{:>3}/{}] importing {name} (id={uid}, fc={fc})",
            i + 1,
            total
        );
        match import_user(*uid, name).await {
            Ok(n) => {
                eprintln!("[seed]   ok ({n} favorites stored)");
                imported += 1;
            }
            Err(e) => {
                eprintln!("[seed]   skipped: {e}");
                skipped += 1;
            }
        }
    }

    eprintln!("[seed] done. imported={imported}, skipped={skipped}");
    Ok(())
}

/// Pages through `/users.json?search[order]=post_upload_count` until we have
/// `want` non-banned member-or-above accounts. Most active uploaders are
/// also the most active favouriters, so this is a high-recall first pass for
/// the favourite-count probe that follows.
async fn discover_candidates(want: usize) -> anyhow::Result<Vec<i32>> {
    let mut out: Vec<i32> = Vec::with_capacity(want);
    let mut seen: HashSet<i32> = HashSet::with_capacity(want);
    let mut page = 1;
    while out.len() < want {
        let users = match api::search_users("post_upload_count", page).await {
            Ok(u) => u,
            Err(e) => {
                eprintln!("[seed]   discovery page {page} failed: {e}");
                break;
            }
        };
        if users.is_empty() {
            break;
        }
        let new_count = users.len();
        for u in users {
            if u.is_banned || u.level < 10 {
                continue;
            }
            if seen.insert(u.id) {
                out.push(u.id);
                if out.len() >= want {
                    break;
                }
            }
        }
        eprintln!(
            "[seed]   discovery page {page}: +{new_count} candidates (total {})",
            out.len()
        );
        page += 1;
    }
    Ok(out)
}

/// For each candidate id, fetch the full user record (includes
/// `favorite_count`) and attach it. Failed lookups are silently dropped —
/// they're typically deleted accounts, banned users, or transient 5xx.
async fn probe_favorite_counts(ids: &[i32]) -> Vec<(i32, String, i32)> {
    let mut out = Vec::with_capacity(ids.len());
    for (i, uid) in ids.iter().enumerate() {
        match api::get_user_by_id(*uid).await {
            Ok(UserApiResponse::FullUser(u)) => out.push((u.id, u.name, u.favorite_count)),
            Ok(UserApiResponse::FullCurrentUser(u)) => out.push((u.id, u.name, u.favorite_count)),
            Err(e) => {
                if i % 100 == 0 {
                    eprintln!("[seed]   probe {uid} failed: {e}");
                }
            }
        }
        if (i + 1) % 100 == 0 {
            eprintln!("[seed]   probed {}/{}", i + 1, ids.len());
        }
    }
    out
}

/// Imports `uid`'s favourites for offline calibration. Pared down vs the
/// production `run_process` flow:
///   * `track_cooccurrence = false` — calibrate uses an empty user-relation
///     graph, so the per-page pair upserts (the dominant write cost) are
///     pure waste here.
///   * `refresh_account_profiles` is skipped — calibrate.rs builds a
///     synthetic profile from the train half of each user's favourites, so
///     the on-disk profile rows aren't read.
///     Net effect: ~10× faster per-user import. Don't reuse this for prod
///     account import.
async fn import_user(uid: i32, name: &str) -> anyhow::Result<usize> {
    let cfg = models::cfg();
    let blacklist: HashSet<String> = cfg.tag_blacklist.iter().map(|s| s.to_lowercase()).collect();

    let bt = &cfg.backtest;
    let account = db::set_account(&bt.seed_owner_token, uid, name, "")
        .map_err(|e| anyhow::anyhow!("set_account: {e}"))?;

    let user = api::get_user_by_id(uid)
        .await
        .map_err(|e| anyhow::anyhow!("get user: {e}"))?;
    let favcount = match user {
        UserApiResponse::FullCurrentUser(u) => u.favorite_count,
        UserApiResponse::FullUser(u) => u.favorite_count,
    };
    if favcount <= 0 {
        return Err(anyhow::anyhow!("user has 0 favorites (private?)"));
    }

    let pages = ((favcount / cfg.posts_limit) + i32::from(favcount % cfg.posts_limit > 0))
        .min(bt.max_pages_per_user.max(1));

    db::drop_account_posts(uid).map_err(|e| anyhow::anyhow!("drop posts: {e}"))?;

    let mut total = 0usize;
    for page in 1..=pages {
        let raw = match api::get_favorites(&account, page).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                // Malformed body. Already logged inside get_favorites;
                // treat as end-of-stream.
                break;
            }
            Err(e) => {
                // Hard fetch failure after all retries — surface so the
                // seed tool reports concrete reason, not a silent stop.
                return Err(anyhow::anyhow!("get_favorites page {page}: {e}"));
            }
        };
        if raw.is_empty() {
            // Either rate-limit hit (already retried inside send_with_retry)
            // or the user's favourites went private mid-import. Stop early
            // rather than spinning on empty pages.
            break;
        }
        let posts: Vec<Post> = raw
            .into_iter()
            .map(|p| strip_blacklisted_tags(p, &blacklist))
            .collect();
        let n = posts.len();
        db::save_posts(&posts, uid).map_err(|e| anyhow::anyhow!("save_posts page {page}: {e}"))?;
        db::save_posts_tags_batch(&posts, &blacklist, false, None)
            .map_err(|e| anyhow::anyhow!("save_tags page {page}: {e}"))?;
        total += n;
    }

    Ok(total)
}

fn strip_blacklisted_tags(mut p: Post, blacklist: &HashSet<String>) -> Post {
    let filter = |v: &mut Vec<String>| {
        v.retain(|t| !blacklist.contains(&t.to_lowercase().trim().to_string()));
    };
    filter(&mut p.tags.artist);
    filter(&mut p.tags.character);
    filter(&mut p.tags.copyright);
    filter(&mut p.tags.general);
    filter(&mut p.tags.lore);
    filter(&mut p.tags.meta);
    filter(&mut p.tags.species);
    p
}
