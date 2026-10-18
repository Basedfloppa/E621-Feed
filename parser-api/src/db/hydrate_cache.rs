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
//! - `seen` (recent + long-term) is *merged into* on `record_feed_interaction`
//!   — a new sighting only adds ids — and *cleared* on
//!   `remove_feed_interaction` / `clear_feed_interactions`, where a removal
//!   cannot be expressed as an addition;
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

fn put_set(
    map: &Mutex<HashMap<i64, SetEntry>>,
    key: i64,
    set: HashSet<i64>,
    ttl: Duration,
) -> Arc<HashSet<i64>> {
    let shared = Arc::new(set);
    if let Ok(mut guard) = map.lock() {
        // Prune against *this* map's TTL. The previous shared
        // `RECENT_SEEN_TTL.max(OWNED_TTL)` bound kept `recent_seen` (20 s) and
        // `long_term_seen` (30 s) entries resident for a full 60 s — reads
        // still honoured their own TTL, so this was only wasted memory, but
        // the constant named the wrong window.
        guard.retain(|_, e| e.inserted_at.elapsed() < ttl);
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

/// Add ids to a warm set entry, leaving its age alone.
///
/// A no-op when the entry is absent or expired: the next read rebuilds it from
/// the table, which already contains the new rows.
fn merge_into(map: &Mutex<HashMap<i64, SetEntry>>, key: i64, ids: &[i64], ttl: Duration) {
    let Ok(mut guard) = map.lock() else {
        return;
    };
    let Some(entry) = guard.get_mut(&key) else {
        return;
    };
    if entry.inserted_at.elapsed() >= ttl {
        return;
    }
    Arc::make_mut(&mut entry.set).extend(ids.iter().copied());
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
    put_set(&RECENT_SEEN_CACHE, account_id, set, RECENT_SEEN_TTL)
}

/// Fold newly recorded exposures into the cached `recent_seen` set, if the
/// account has one.
///
/// The interaction batch that proves a card was on screen runs while the user
/// scrolls, so invalidating the set there would empty it every few seconds and
/// the cache would never help — that is why this adds to the live set instead
/// of calling `clear_seen`. Removal (undo, cleared history) still goes through
/// [`clear_seen`], because a deletion cannot be expressed as an addition.
///
/// The ids being added are by definition the newest, so they fall inside every
/// window this cache was built for.
///
/// `inserted_at` is deliberately NOT refreshed: adding ids must not extend the
/// entry's lifetime past another writer's invalidation, and the TTL is short
/// enough that the next miss picks such a change up.
pub fn merge_recent_seen(account_id: i64, ids: &[i64]) {
    merge_into(&RECENT_SEEN_CACHE, account_id, ids, RECENT_SEEN_TTL);
}

// --- long-term seen --------------------------------------------------------

pub const LONG_TERM_SEEN_TTL: Duration = Duration::from_secs(30);
static LONG_TERM_SEEN_CACHE: LazyLock<Mutex<HashMap<i64, SetEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn long_term_seen(account_id: i64) -> Option<Arc<HashSet<i64>>> {
    get_set(&LONG_TERM_SEEN_CACHE, account_id, LONG_TERM_SEEN_TTL)
}
pub fn store_long_term_seen(account_id: i64, set: HashSet<i64>) -> Arc<HashSet<i64>> {
    put_set(&LONG_TERM_SEEN_CACHE, account_id, set, LONG_TERM_SEEN_TTL)
}

/// Long-window counterpart of [`merge_recent_seen`].
///
/// Both windows are fed by the same evidence (a confirmed sighting), so both
/// need to learn about it in the same write; leaving this one to expire would
/// let a just-seen post come back from the local candidate pool for up to
/// `LONG_TERM_SEEN_TTL`.
pub fn merge_long_term_seen(account_id: i64, ids: &[i64]) {
    merge_into(&LONG_TERM_SEEN_CACHE, account_id, ids, LONG_TERM_SEEN_TTL);
}

// --- owned ----------------------------------------------------------------

pub const OWNED_TTL: Duration = Duration::from_secs(60);
static OWNED_CACHE: LazyLock<Mutex<HashMap<i64, SetEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn owned(account_id: i64) -> Option<Arc<HashSet<i64>>> {
    get_set(&OWNED_CACHE, account_id, OWNED_TTL)
}
pub fn store_owned(account_id: i64, set: HashSet<i64>) -> Arc<HashSet<i64>> {
    put_set(&OWNED_CACHE, account_id, set, OWNED_TTL)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The interaction batch merges into the live set instead of clearing it,
    /// so the dedup read stays cheap while the user scrolls. If the merge
    /// dropped the previously cached ids (or failed to add the new ones) it
    /// would silently re-serve posts — the exact bug V36 exists to close.
    #[test]
    fn merge_recent_seen_extends_a_warm_entry() {
        let account = 990_001;
        store_recent_seen(account, HashSet::from([1i64, 2]));
        merge_recent_seen(account, &[2, 3]);

        let cached = recent_seen(account).expect("entry must still be warm");
        assert_eq!(*cached, HashSet::from([1i64, 2, 3]));
        clear_seen(account);
    }

    /// The long window reads the same evidence, so a confirmed sighting has to
    /// land in both caches in the same write. Missing this one would let a
    /// just-seen post return from the local candidate pool for a whole TTL.
    #[test]
    fn merge_long_term_seen_extends_a_warm_entry() {
        let account = 990_003;
        store_long_term_seen(account, HashSet::from([1i64]));
        merge_long_term_seen(account, &[4, 5]);

        let cached = long_term_seen(account).expect("entry must still be warm");
        assert_eq!(*cached, HashSet::from([1i64, 4, 5]));
        clear_seen(account);
    }

    /// A merge must not fabricate an entry: the table is the source of truth,
    /// and a synthesised set would hide a missing row for a full TTL.
    #[test]
    fn merge_recent_seen_is_a_noop_without_an_entry() {
        let account = 990_002;
        clear_seen(account);
        merge_recent_seen(account, &[5]);
        assert!(
            recent_seen(account).is_none(),
            "merge must not create a cache entry"
        );
    }
}
