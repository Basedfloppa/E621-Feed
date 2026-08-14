//! Short-TTL per-account caches for the expensive `db_hydrate` reads.
//!
//! `get_recently_seen_post_ids`, `get_long_term_seen_post_ids`,
//! `get_owned_post_ids` and `collect_local_candidate_ids` each re-scan their
//! table on every call. During infinite-scroll pagination the same account
//! fires `/recommendations?page=N` a few seconds apart with an identical
//! candidate pool, so these per-account id-sets get recomputed needlessly
//! (the dominant remaining `db_hydrate` cost on large catalogs — see TODO
//! §2.2b). They are served from a short-TTL cache here (mirroring
//! `TAG_COUNTS_CACHE` in `db/tags.rs`), with explicit invalidation on the
//! write paths that actually mutate the underlying data:
//! - `seen` (recent + long-term) is cleared on `record_feed_interaction` /
//!   `remove_feed_interaction`;
//! - `owned` + `candidates` are cleared on the `/process` rebuild
//!   (`save_posts` / `drop_account_posts`).
//!
//! A cache hit returns an `Arc` to the stored set; callers clone it into the
//! `HashSet`/`Vec` they need (much cheaper than the table re-scan they replace).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

const CACHE_MAX_ENTRIES: usize = 1024;

struct SetEntry {
    set: Arc<HashSet<i64>>,
    inserted_at: Instant,
}
struct IdsEntry {
    ids: Arc<Vec<i64>>,
    inserted_at: Instant,
}

// --- generic helpers -------------------------------------------------------

fn get_set(
    map: &Mutex<HashMap<i64, SetEntry>>,
    key: i64,
    ttl: Duration,
) -> Option<Arc<HashSet<i64>>> {
    let guard = map.lock().ok()?;
    let entry = guard.get(&key)?;
    (entry.inserted_at.elapsed() < ttl).then(|| entry.set.clone())
}

fn put_set(map: &Mutex<HashMap<i64, SetEntry>>, key: i64, set: HashSet<i64>) -> Arc<HashSet<i64>> {
    let shared = Arc::new(set);
    if let Ok(mut guard) = map.lock() {
        guard.retain(|_, e| e.inserted_at.elapsed() < RECENT_SEEN_TTL.max(OWNED_TTL));
        guard.insert(
            key,
            SetEntry {
                set: shared.clone(),
                inserted_at: Instant::now(),
            },
        );
        evict_oldest_set(&mut guard);
    }
    shared
}

fn clear_set(map: &Mutex<HashMap<i64, SetEntry>>, key: i64) {
    if let Ok(mut guard) = map.lock() {
        guard.remove(&key);
    }
}

fn evict_oldest_set(guard: &mut HashMap<i64, SetEntry>) {
    if guard.len() <= CACHE_MAX_ENTRIES {
        return;
    }
    let mut keys: Vec<(i64, Instant)> = guard.iter().map(|(k, v)| (*k, v.inserted_at)).collect();
    keys.sort_by_key(|(_, t)| *t);
    let excess = guard.len() - CACHE_MAX_ENTRIES;
    for (k, _) in keys.into_iter().take(excess) {
        guard.remove(&k);
    }
}

fn get_ids(map: &Mutex<HashMap<i64, IdsEntry>>, key: i64, ttl: Duration) -> Option<Arc<Vec<i64>>> {
    let guard = map.lock().ok()?;
    let entry = guard.get(&key)?;
    (entry.inserted_at.elapsed() < ttl).then(|| entry.ids.clone())
}

fn put_ids(map: &Mutex<HashMap<i64, IdsEntry>>, key: i64, ids: Vec<i64>) -> Arc<Vec<i64>> {
    let shared = Arc::new(ids);
    if let Ok(mut guard) = map.lock() {
        guard.retain(|_, e| e.inserted_at.elapsed() < CANDIDATE_TTL);
        guard.insert(
            key,
            IdsEntry {
                ids: shared.clone(),
                inserted_at: Instant::now(),
            },
        );
        if guard.len() > CACHE_MAX_ENTRIES {
            let mut keys: Vec<(i64, Instant)> =
                guard.iter().map(|(k, v)| (*k, v.inserted_at)).collect();
            keys.sort_by_key(|(_, t)| *t);
            let excess = guard.len() - CACHE_MAX_ENTRIES;
            for (k, _) in keys.into_iter().take(excess) {
                guard.remove(&k);
            }
        }
    }
    shared
}

fn clear_ids(map: &Mutex<HashMap<i64, IdsEntry>>, key: i64) {
    if let Ok(mut guard) = map.lock() {
        guard.remove(&key);
    }
}

// --- recent seen -----------------------------------------------------------

pub const RECENT_SEEN_TTL: Duration = Duration::from_secs(20);
static RECENT_SEEN_CACHE: LazyLock<Mutex<HashMap<i64, SetEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn recent_seen(account_id: i64) -> Option<Arc<HashSet<i64>>> {
    get_set(&RECENT_SEEN_CACHE, account_id, RECENT_SEEN_TTL)
}
pub fn store_recent_seen(account_id: i64, set: HashSet<i64>) -> Arc<HashSet<i64>> {
    put_set(&RECENT_SEEN_CACHE, account_id, set)
}

// --- long-term seen --------------------------------------------------------

pub const LONG_TERM_SEEN_TTL: Duration = Duration::from_secs(30);
static LONG_TERM_SEEN_CACHE: LazyLock<Mutex<HashMap<i64, SetEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn long_term_seen(account_id: i64) -> Option<Arc<HashSet<i64>>> {
    get_set(&LONG_TERM_SEEN_CACHE, account_id, LONG_TERM_SEEN_TTL)
}
pub fn store_long_term_seen(account_id: i64, set: HashSet<i64>) -> Arc<HashSet<i64>> {
    put_set(&LONG_TERM_SEEN_CACHE, account_id, set)
}

// --- owned ----------------------------------------------------------------

pub const OWNED_TTL: Duration = Duration::from_secs(60);
static OWNED_CACHE: LazyLock<Mutex<HashMap<i64, SetEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn owned(account_id: i64) -> Option<Arc<HashSet<i64>>> {
    get_set(&OWNED_CACHE, account_id, OWNED_TTL)
}
pub fn store_owned(account_id: i64, set: HashSet<i64>) -> Arc<HashSet<i64>> {
    put_set(&OWNED_CACHE, account_id, set)
}

// --- local candidates ------------------------------------------------------

pub const CANDIDATE_TTL: Duration = Duration::from_secs(20);
static CANDIDATE_CACHE: LazyLock<Mutex<HashMap<i64, IdsEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn candidates(account_id: i64) -> Option<Arc<Vec<i64>>> {
    get_ids(&CANDIDATE_CACHE, account_id, CANDIDATE_TTL)
}
pub fn store_candidates(account_id: i64, ids: Vec<i64>) -> Arc<Vec<i64>> {
    put_ids(&CANDIDATE_CACHE, account_id, ids)
}

// --- invalidation ----------------------------------------------------------

/// Clear all `seen`-derived caches for an account. Called when a feed
/// interaction is recorded/removed so the dedup set is not stale within the
/// TTL window.
pub fn clear_seen(account_id: i64) {
    clear_set(&RECENT_SEEN_CACHE, account_id);
    clear_set(&LONG_TERM_SEEN_CACHE, account_id);
}

/// Clear `owned` + `candidates` for an account. Called when `/process`
/// rebuilds the account's favourites / top tags.
pub fn clear_owned_and_candidates(account_id: i64) {
    clear_set(&OWNED_CACHE, account_id);
    clear_ids(&CANDIDATE_CACHE, account_id);
}
