use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Same coalescing as IDF: hold the rebuild worker alive briefly so a burst of
/// dirty marks doesn't re-spawn the thread for every page. Read fresh from
/// `runtime.tag_relation_rebuild_cooldown_secs` per iteration.
fn rebuild_cooldown() -> Duration {
    Duration::from_secs(
        crate::models::cfg()
            .runtime
            .tag_relation_rebuild_cooldown_secs
            .max(1),
    )
}

pub type GroupKey = u8;
pub type TagId = u32;

const GROUP_COUNT: usize = 7;

/// PMI co-occurrence graph keyed by interned u32 tag-ids. The hot path
/// (`tag_relation_fit`) resolves each post's tags once into ids, then walks
/// `T*(T-1)/2` pairs without any string allocation.
///
/// `tag_to_id` is a per-group array of `HashMap<String, TagId>` (rather than a
/// single `HashMap<(GroupKey, String), TagId>`) so lookups can pass a borrowed
/// `&str` directly — no transient `(u8, String)` tuple per call.
///
/// `pairs` has two shapes. While the graph is being built it sits in a
/// `HashMap<(TagId, TagId), i64>` for O(1) accumulation. After
/// [`Self::freeze`] is called (calibrate calls it once per per-account
/// graph after `CachedPostFeatures` are resolved), pairs are compacted
/// into a sorted `Vec<(TagId, TagId, i64)>` — 16 B/pair vs ~32-48 B in
/// the HashMap, which cut per-account memory ~3× and let 1000-account
/// runs fit in 15 GB again. The query method handles both shapes.
#[derive(Debug, Clone, Default)]
pub struct TagRelationGraph {
    tag_to_id: [HashMap<String, TagId>; GROUP_COUNT],
    pairs: PairStorage,
    marginals: Vec<i64>,
    n_posts: i64,
}

#[derive(Debug, Clone)]
enum PairStorage {
    /// Build-time / mutable form. All inserts go here. i64 retained
    /// because the global graph accumulates from multi-million-row SQL
    /// scans where intermediate sums could exceed u32.
    Hot(HashMap<(TagId, TagId), i64>),
    /// Query-only form. Sorted by `(a, b)`; lookup is binary search.
    /// Counts narrowed to `u32` (12 B/entry vs 16 B with `i64`); the
    /// final per-pair PMI math casts back to `f32` anyway, and even a
    /// 6 M-post catalog can't push a single pair's cooc past `u32::MAX`.
    /// Created by [`TagRelationGraph::freeze`] / `freeze_with_query_set`;
    /// inserts after freeze panic.
    Frozen(Vec<(TagId, TagId, u32)>),
}

impl Default for PairStorage {
    fn default() -> Self {
        PairStorage::Hot(HashMap::new())
    }
}

impl PairStorage {
    fn len(&self) -> usize {
        match self {
            PairStorage::Hot(m) => m.len(),
            PairStorage::Frozen(v) => v.len(),
        }
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn get(&self, key: (TagId, TagId)) -> i64 {
        match self {
            PairStorage::Hot(m) => *m.get(&key).unwrap_or(&0),
            PairStorage::Frozen(v) => v
                .binary_search_by(|&(a, b, _)| (a, b).cmp(&key))
                .map(|i| v[i].2 as i64)
                .unwrap_or(0),
        }
    }
    fn entry_add(&mut self, key: (TagId, TagId), delta: i64) {
        match self {
            PairStorage::Hot(m) => {
                *m.entry(key).or_insert(0) += delta;
            }
            PairStorage::Frozen(_) => {
                panic!("insert on frozen TagRelationGraph; freeze() is one-way");
            }
        }
    }
}

impl TagRelationGraph {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_posts(n_posts: i64) -> Self {
        Self {
            tag_to_id: Default::default(),
            pairs: PairStorage::default(),
            marginals: Vec::new(),
            n_posts: n_posts.max(0),
        }
    }

    /// Intern a (group, tag) pair, returning a stable id for the lifetime of
    /// this graph. Allocates the key string only on first sight; lookup-only
    /// callers (`tag_id`) hit the map without allocating at all.
    fn intern(&mut self, g: GroupKey, t: &str) -> TagId {
        let bucket = &mut self.tag_to_id[g as usize];
        if let Some(&id) = bucket.get(t) {
            return id;
        }
        let id = self.marginals.len() as TagId;
        bucket.insert(t.to_owned(), id);
        self.marginals.push(0);
        id
    }

    /// Look up the id for a (group, tag) without inserting. Zero-alloc:
    /// borrows directly into the per-group HashMap.
    pub fn tag_id(&self, g: GroupKey, t: &str) -> Option<TagId> {
        if t.is_empty() {
            return None;
        }
        self.tag_to_id.get(g as usize)?.get(t).copied()
    }

    pub fn insert_pair(&mut self, g1: GroupKey, t1: &str, g2: GroupKey, t2: &str, count: i64) {
        if count <= 0 || t1.is_empty() || t2.is_empty() {
            return;
        }
        let a = self.intern(g1, t1);
        let b = self.intern(g2, t2);
        if a == b {
            return;
        }
        let key = if a < b { (a, b) } else { (b, a) };
        self.pairs.entry_add(key, count);
    }

    /// Insert a pair using pre-resolved `TagId`s — caller has already
    /// interned both endpoints (typical when loading from a JOIN-free
    /// cooccurrence scan against SQLite's `tag_id`s). Cuts ~400M string
    /// alloc + lowercase ops on a multi-million-pair `load_global_tag_relation`.
    pub fn insert_pair_by_id(&mut self, a: TagId, b: TagId, count: i64) {
        if count <= 0 || a == b {
            return;
        }
        let key = if a < b { (a, b) } else { (b, a) };
        self.pairs.entry_add(key, count);
    }

    pub fn set_marginal(&mut self, g: GroupKey, t: &str, count: i64) {
        if t.is_empty() {
            return;
        }
        let id = self.intern(g, t);
        self.marginals[id as usize] = count.max(0);
    }

    #[inline]
    pub fn cooc_by_id(&self, a: TagId, b: TagId) -> i64 {
        if a == b {
            return self.marginal_by_id(a);
        }
        let key = if a < b { (a, b) } else { (b, a) };
        self.pairs.get(key)
    }

    #[inline]
    pub fn marginal_by_id(&self, id: TagId) -> i64 {
        self.marginals.get(id as usize).copied().unwrap_or(0)
    }

    #[inline]
    pub fn n_posts(&self) -> i64 {
        self.n_posts
    }

    /// Total interned (group, tag) keys. Diagnostic only.
    #[inline]
    pub fn n_tags(&self) -> usize {
        self.marginals.len()
    }

    /// Total stored co-occurrence pairs. Diagnostic only.
    #[inline]
    pub fn n_pairs(&self) -> usize {
        self.pairs.len()
    }

    /// True if the graph holds nothing — used by the idle-eviction path so
    /// we don't churn-evict an already-empty cache every tick.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty() && self.marginals.is_empty() && self.n_posts == 0
    }

    /// Compact mutable build state into query-only form. After this call:
    ///  * pairs occupy `Vec<(TagId, TagId, u32)>` (12 B/entry) instead
    ///    of `HashMap<(TagId, TagId), i64>` (~32-48 B/entry incl.
    ///    bucket overhead) — ~3× memory drop on large per-account graphs;
    ///  * pairs with `count < min_cooc` are **dropped**. Production's
    ///    default `tag_relation_min_cooc = 2` already filters singleton
    ///    pairs at scoring time, so passing `min_cooc = 2` here is
    ///    a no-op for the prod pipeline and prunes the long tail of
    ///    one-off cooccurrences (~30-50% of pair count on typical
    ///    user graphs). Pass `min_cooc = 1` to keep everything;
    ///  * `tag_to_id` is cleared because the typical post-build call
    ///    pattern goes through `marginal_by_id` / `cooc_by_id` (callers
    ///    pre-resolved every `TagId` they need before freeze).
    ///
    /// Inserts after freeze panic. The global tag-relation graph in
    /// production is **not** frozen — its prod scoring path needs
    /// `tag_id(g, &str)` per request.
    pub fn freeze(&mut self, min_cooc: i64) {
        self.freeze_inner(min_cooc, None);
    }

    /// Stricter variant of [`Self::freeze`]: only retain pairs whose
    /// **both** endpoints are in `queryable_tids`. The calibrate harness
    /// passes the union of `user_tid`s on (test ∪ neg) cached features
    /// — pairs with at least one endpoint outside that set will never
    /// be queried by `tag_relation_fit_cached` (it walks pairs of tags
    /// from the *current* post being scored), so they're dead weight.
    /// Cuts another 50–80% off per-account pair counts on typical
    /// fixtures where train tags far outnumber (test ∪ neg) tags.
    pub fn freeze_with_query_set(
        &mut self,
        queryable_tids: &std::collections::HashSet<TagId>,
        min_cooc: i64,
    ) {
        self.freeze_inner(min_cooc, Some(queryable_tids));
    }

    /// Bypass the Hot HashMap entirely: take ownership of a `Vec` of
    /// pre-resolved `(TagId, TagId, count)` triples and store them
    /// directly as `PairStorage::Frozen`. Pairs may arrive in any
    /// order; this method canonicalises endpoints, sorts, and
    /// in-place coalesces adjacent duplicates without re-allocating.
    ///
    /// Used by `db::load_global_tag_relation` to skip the multi-GB
    /// HashMap allocation peak when loading a multi-million-pair
    /// catalog graph — prod doesn't need string-keyed lookups against
    /// the global graph after dataset prep, only id-keyed cooc/marginal
    /// queries.
    pub fn set_pairs_frozen_vec(&mut self, mut v: Vec<(TagId, TagId, u32)>) {
        v.retain(|&(a, b, _)| a != b);
        for entry in v.iter_mut() {
            if entry.0 > entry.1 {
                let (a, b, c) = (entry.1, entry.0, entry.2);
                entry.0 = a;
                entry.1 = b;
                entry.2 = c;
            }
        }
        v.sort_by_key(|&(a, b, _)| (a, b));
        // In-place coalesce of adjacent duplicate pairs (sum counts).
        let mut write = 0usize;
        for read in 0..v.len() {
            if write > 0 && v[write - 1].0 == v[read].0 && v[write - 1].1 == v[read].1 {
                v[write - 1].2 = v[write - 1].2.saturating_add(v[read].2);
            } else {
                v[write] = v[read];
                write += 1;
            }
        }
        v.truncate(write);
        v.shrink_to_fit();
        self.pairs = PairStorage::Frozen(v);
    }

    fn freeze_inner(
        &mut self,
        min_cooc: i64,
        queryable: Option<&std::collections::HashSet<TagId>>,
    ) {
        let min_cooc = min_cooc.max(1) as u32;
        // Pull both shapes through one filtering pass — lets calibrate
        // call freeze a second time after the global graph was already
        // loaded directly into Frozen (e.g. to apply the cross-account
        // queryable filter).
        let existing = std::mem::take(&mut self.pairs);
        let mut filtered: Vec<(TagId, TagId, u32)> = match existing {
            PairStorage::Hot(map) => map
                .into_iter()
                .filter_map(|((a, b), c)| {
                    if c < min_cooc as i64 {
                        return None;
                    }
                    if let Some(q) = queryable {
                        if !q.contains(&a) || !q.contains(&b) {
                            return None;
                        }
                    }
                    Some((a, b, c.max(0).min(u32::MAX as i64) as u32))
                })
                .collect(),
            PairStorage::Frozen(v) => v
                .into_iter()
                .filter(|&(a, b, c)| {
                    if c < min_cooc {
                        return false;
                    }
                    if let Some(q) = queryable {
                        return q.contains(&a) && q.contains(&b);
                    }
                    true
                })
                .collect(),
        };
        filtered.sort_by_key(|&(a, b, _)| (a, b));
        filtered.shrink_to_fit();
        self.pairs = PairStorage::Frozen(filtered);

        for bucket in &mut self.tag_to_id {
            bucket.clear();
            bucket.shrink_to_fit();
        }
        self.marginals.shrink_to_fit();
    }

    /// Build a per-account graph from a slice of posts (e.g. the train
    /// half of a calibrate split). Tag pairs across all groups except
    /// `meta` are interned, with marginals = per-tag occurrence counts
    /// and pair counts = how many train posts hold both tags.
    ///
    /// Used by the calibrate harness to give the personal tag-relation
    /// channel an actual gradient (the production path builds these
    /// graphs from the user's full favourite history; under the
    /// synthetic split, the train-half is the analogue).
    pub fn from_train_posts(posts: &[crate::models::Post]) -> Self {
        let mut g = Self::with_posts(posts.len() as i64);
        // Same group set tag_relation_fit operates on (no `meta`).
        let groups: [(GroupKey, fn(&crate::models::Post) -> &Vec<String>); 6] = [
            (0, |p| &p.tags.artist),
            (1, |p| &p.tags.character),
            (2, |p| &p.tags.copyright),
            (3, |p| &p.tags.species),
            (4, |p| &p.tags.general),
            (5, |p| &p.tags.lore),
        ];
        // Per-post: gather (group, lc) tuples, intern, bump marginals,
        // then walk pairs upper-triangular and bump cooccurrences.
        let mut scratch: Vec<(GroupKey, TagId)> = Vec::with_capacity(64);
        for post in posts {
            scratch.clear();
            for (gk, getter) in groups.iter() {
                for raw in getter(post) {
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let lc: String = if trimmed.bytes().any(|b| b.is_ascii_uppercase()) {
                        trimmed.to_ascii_lowercase()
                    } else {
                        trimmed.to_owned()
                    };
                    let id = g.intern(*gk, &lc);
                    if let Some(slot) = g.marginals.get_mut(id as usize) {
                        *slot += 1;
                    }
                    scratch.push((*gk, id));
                }
            }
            for i in 0..scratch.len() {
                let (_, ai) = scratch[i];
                for j in (i + 1)..scratch.len() {
                    let (_, bj) = scratch[j];
                    g.insert_pair_by_id(ai, bj, 1);
                }
            }
        }
        g
    }
}

static GLOBAL_CACHE: LazyLock<ArcSwap<TagRelationGraph>> =
    LazyLock::new(|| ArcSwap::from_pointee(TagRelationGraph::empty()));
static GLOBAL_DIRTY: AtomicBool = AtomicBool::new(true);
static GLOBAL_REBUILDING: AtomicBool = AtomicBool::new(false);

/// Two separate access timers so background dirty-marks (cleanup, process)
/// don't prevent idle-eviction of the user-facing cache.
///
/// `LAST_USER_ACCESS` — updated by request-serving code (`current_global_relation`).
/// Used by `evict_if_idle` to decide whether to evict.
///
/// `LAST_SYSTEM_ACCESS` — updated by background workers (`mark_global_relation_dirty`).
/// Not consulted for idle-eviction.
static LAST_USER_ACCESS: LazyLock<Mutex<Instant>> =
    LazyLock::new(|| Mutex::new(Instant::now()));
static LAST_SYSTEM_ACCESS: LazyLock<Mutex<Instant>> =
    LazyLock::new(|| Mutex::new(Instant::now()));

fn touch_user_access() {
    let mut g = LAST_USER_ACCESS.lock().unwrap_or_else(|p| p.into_inner());
    *g = Instant::now();
}

fn touch_system_access() {
    let mut g = LAST_SYSTEM_ACCESS.lock().unwrap_or_else(|p| p.into_inner());
    *g = Instant::now();
}

pub fn mark_global_relation_dirty() {
    touch_system_access();
    GLOBAL_DIRTY.store(true, Ordering::Release);
    spawn_rebuild_if_needed();
}

fn spawn_rebuild_if_needed() {
    if GLOBAL_REBUILDING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::Builder::new()
        .name("tag-relation-rebuild".to_string())
        .spawn(|| {
            loop {
                let mut last_rebuild_failed = false;
                'work: loop {
                    if !GLOBAL_DIRTY.swap(false, Ordering::AcqRel) {
                        break 'work;
                    }
                    match crate::db::load_global_tag_relation() {
                        Ok(new) => GLOBAL_CACHE.store(Arc::new(new)),
                        Err(e) => {
                            error!("[tag-relation] rebuild failed: {e}");
                            GLOBAL_DIRTY.store(true, Ordering::Release);
                            last_rebuild_failed = true;
                            break 'work;
                        }
                    }
                }

                if last_rebuild_failed {
                    break;
                }

                let deadline = Instant::now() + rebuild_cooldown();
                while Instant::now() < deadline {
                    if GLOBAL_DIRTY.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }

                if !GLOBAL_DIRTY.load(Ordering::Acquire) {
                    break;
                }
            }

            GLOBAL_REBUILDING.store(false, Ordering::Release);
            if GLOBAL_DIRTY.load(Ordering::Acquire) {
                spawn_rebuild_if_needed();
            }
        })
        .expect("spawn tag-relation-rebuild thread");
}

pub fn current_global_relation() -> Arc<TagRelationGraph> {
    touch_user_access();
    if GLOBAL_DIRTY.load(Ordering::Acquire) {
        spawn_rebuild_if_needed();
    }
    GLOBAL_CACHE.load_full()
}

/// Drop the loaded co-occurrence graph if no caller has touched it for at
/// least `idle_secs` seconds. Returns `(prev_n_pairs, prev_n_tags)` if
/// eviction happened, or `(0, 0)` otherwise.
///
/// After eviction, `GLOBAL_DIRTY` is set so the next
/// `current_global_relation()` call schedules a fresh rebuild — same code
/// path as cold startup. The first post-eviction request therefore sees
/// an empty graph (zero co-occurrence weights) until the rebuild thread
/// completes; this matches existing startup behaviour and avoids
/// blocking the request thread on the multi-second pair scan.
///
/// `idle_secs == 0` disables eviction.
pub fn evict_if_idle(idle_secs: u64) -> (usize, usize) {
    if idle_secs == 0 {
        return (0, 0);
    }
    let elapsed = {
        let g = LAST_USER_ACCESS.lock().unwrap_or_else(|p| p.into_inner());
        g.elapsed()
    };
    if elapsed.as_secs() < idle_secs {
        return (0, 0);
    }
    let current = GLOBAL_CACHE.load_full();
    if current.is_empty() {
        return (0, 0);
    }
    let prev_pairs = current.n_pairs();
    let prev_tags = current.n_tags();
    drop(current);
    GLOBAL_CACHE.store(Arc::new(TagRelationGraph::empty()));
    GLOBAL_DIRTY.store(true, Ordering::Release);
    {
        let mut g = LAST_USER_ACCESS.lock().unwrap_or_else(|p| p.into_inner());
        *g = Instant::now();
    }
    (prev_pairs, prev_tags)
}
