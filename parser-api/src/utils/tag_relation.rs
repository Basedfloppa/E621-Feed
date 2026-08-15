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
/// the `HashMap`, which cut per-account memory ~3× and let 1000-account
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
                .map_or(0, |i| i64::from(v[i].2)),
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
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    #[must_use]
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
    /// borrows directly into the per-group `HashMap`.
    #[must_use]
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
    /// cooccurrence scan against `SQLite`'s `tag_id`s). Cuts ~400M string
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
    #[must_use]
    pub fn cooc_by_id(&self, a: TagId, b: TagId) -> i64 {
        if a == b {
            return self.marginal_by_id(a);
        }
        let key = if a < b { (a, b) } else { (b, a) };
        self.pairs.get(key)
    }

    #[inline]
    #[must_use]
    pub fn marginal_by_id(&self, id: TagId) -> i64 {
        self.marginals.get(id as usize).copied().unwrap_or(0)
    }

    #[inline]
    #[must_use]
    pub fn n_posts(&self) -> i64 {
        self.n_posts
    }

    /// Total interned (group, tag) keys. Diagnostic only.
    #[inline]
    #[must_use]
    pub fn n_tags(&self) -> usize {
        self.marginals.len()
    }

    /// Total stored co-occurrence pairs. Diagnostic only.
    #[inline]
    #[must_use]
    pub fn n_pairs(&self) -> usize {
        self.pairs.len()
    }

    /// True if the graph holds nothing — used by the idle-eviction path so
    /// we don't churn-evict an already-empty cache every tick.
    #[inline]
    #[must_use]
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

    /// Bypass the Hot `HashMap` entirely: take ownership of a `Vec` of
    /// pre-resolved `(TagId, TagId, count)` triples and store them
    /// directly as `PairStorage::Frozen`. Pairs may arrive in any
    /// order; this method canonicalises endpoints, sorts, and
    /// in-place coalesces adjacent duplicates without re-allocating.
    ///
    /// Used by `db::load_global_tag_relation` to skip the multi-GB
    /// `HashMap` allocation peak when loading a multi-million-pair
    /// catalog graph — prod doesn't need string-keyed lookups against
    /// the global graph after dataset prep, only id-keyed cooc/marginal
    /// queries.
    pub fn set_pairs_frozen_vec(&mut self, mut v: Vec<(TagId, TagId, u32)>) {
        v.retain(|&(a, b, _)| a != b);
        for entry in &mut v {
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
                    if c < i64::from(min_cooc) {
                        return None;
                    }
                    if let Some(q) = queryable
                        && (!q.contains(&a) || !q.contains(&b))
                    {
                        return None;
                    }
                    Some((a, b, c.max(0).min(i64::from(u32::MAX)) as u32))
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
    #[must_use]
    pub fn from_train_posts(posts: &[crate::models::Post]) -> Self {
        let mut g = Self::with_posts(posts.len() as i64);
        // Same group set tag_relation_fit operates on (no `meta`).
        type TagGroupGetter = (GroupKey, fn(&crate::models::Post) -> &Vec<String>);
        let groups: [TagGroupGetter; 6] = [
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
            for (gk, getter) in &groups {
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
                for (_, bj) in scratch.iter().skip(i + 1) {
                    let bj = *bj;
                    g.insert_pair_by_id(ai, bj, 1);
                }
            }
        }
        g
    }

    /// Keep only the strongest `keep` pairs (by co-occurrence count, ties
    /// broken by pair key). Mirrors the production `load_account_tag_relation`
    /// SQL (`ORDER BY cooc_count DESC LIMIT n`) so calibration can measure the
    /// effect of `user_relation_edge_limit` on NDCG without hitting SQLite.
    /// Marginals (per-tag counts) are left untouched — same as production, where
    /// marginals come from the full tag-count profile and only the pair rows are
    /// capped. Returns the number of pairs retained. Panics if called on a
    /// frozen graph.
    pub fn truncate_to_top_pairs(&mut self, keep: usize) -> usize {
        if keep == 0 {
            return 0;
        }
        match &mut self.pairs {
            PairStorage::Hot(m) => {
                if m.len() <= keep {
                    return m.len();
                }
                let mut v: Vec<((TagId, TagId), i64)> = m.drain().collect();
                v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                v.truncate(keep);
                *m = v.into_iter().collect();
                m.len()
            }
            PairStorage::Frozen(_) => panic!("truncate_to_top_pairs on frozen graph"),
        }
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
static LAST_USER_ACCESS: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));
static LAST_SYSTEM_ACCESS: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

fn touch_user_access() {
    let mut g = LAST_USER_ACCESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *g = Instant::now();
}

fn touch_system_access() {
    let mut g = LAST_SYSTEM_ACCESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let g = LAST_USER_ACCESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut g = LAST_USER_ACCESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = Instant::now();
    }
    (prev_pairs, prev_tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction & empty state ─────────────────────────────────────

    #[test]
    fn empty_graph_defaults() {
        let g = TagRelationGraph::empty();
        assert!(g.is_empty());
        assert_eq!(g.n_tags(), 0);
        assert_eq!(g.n_pairs(), 0);
        assert_eq!(g.n_posts(), 0);
    }

    #[test]
    fn with_posts_sets_n_posts() {
        let g = TagRelationGraph::with_posts(42);
        assert_eq!(g.n_posts(), 42);
    }

    #[test]
    fn with_posts_negative_clamps_to_zero() {
        let g = TagRelationGraph::with_posts(-5);
        assert_eq!(g.n_posts(), 0);
    }

    // ── intern / tag_id ────────────────────────────────────────────────

    #[test]
    fn intern_assigns_incrementing_ids() {
        let mut g = TagRelationGraph::empty();
        let id_a = g.intern(0, "artist_a");
        let id_b = g.intern(0, "artist_b");
        assert_eq!(id_a, 0);
        assert_eq!(id_b, 1);
        // Re-interning same tag returns same id
        assert_eq!(g.intern(0, "artist_a"), id_a);
    }

    #[test]
    fn intern_separate_groups_have_separate_namespaces() {
        let mut g = TagRelationGraph::empty();
        let id1 = g.intern(0, "fluffy");
        let id2 = g.intern(4, "fluffy");
        // Different groups, so different ids
        // Intern uses marginals.len(), so they're sequential
        assert_ne!(id1, id2, "same tag in different groups gives different ids");
    }

    #[test]
    fn tag_id_returns_none_for_empty_tag() {
        let g = TagRelationGraph::empty();
        assert!(g.tag_id(0, "").is_none());
    }

    #[test]
    fn tag_id_returns_none_for_unknown() {
        let g = TagRelationGraph::empty();
        assert!(g.tag_id(0, "nonexistent").is_none());
    }

    #[test]
    fn tag_id_returns_some_for_interned() {
        let mut g = TagRelationGraph::empty();
        let id = g.intern(1, "char");
        assert_eq!(g.tag_id(1, "char"), Some(id));
    }

    // ── insert_pair ────────────────────────────────────────────────────

    #[test]
    fn insert_pair_stores_and_query() {
        let mut g = TagRelationGraph::empty();
        g.insert_pair(0, "artist_a", 4, "fluffy", 3);
        // Cross-group pair: artist(0) + general(4)
        let id_a = g.tag_id(0, "artist_a").unwrap();
        let id_f = g.tag_id(4, "fluffy").unwrap();
        assert_eq!(g.cooc_by_id(id_a, id_f), 3);
        // Order shouldn't matter
        assert_eq!(g.cooc_by_id(id_f, id_a), 3);
    }

    #[test]
    fn truncate_to_top_pairs_keeps_strongest_and_preserves_marginals() {
        let mut g = TagRelationGraph::with_posts(3);
        // 5 distinct pairs with increasing cooc counts.
        g.insert_pair(0, "a1", 4, "f1", 1);
        g.insert_pair(0, "a2", 4, "f2", 2);
        g.insert_pair(0, "a3", 4, "f3", 3);
        g.insert_pair(0, "a4", 4, "f4", 4);
        g.insert_pair(0, "a5", 4, "f5", 5);
        g.set_marginal(0, "a5", 9);
        assert_eq!(g.n_pairs(), 5);

        assert_eq!(g.truncate_to_top_pairs(2), 2);
        assert_eq!(g.n_pairs(), 2, "only the two strongest pairs survive");
        // Strongest pair (count 5) is retained.
        let a5 = g.tag_id(0, "a5").unwrap();
        let f5 = g.tag_id(4, "f5").unwrap();
        assert_eq!(g.cooc_by_id(a5, f5), 5);
        // Weakest pair (count 1) is pruned.
        assert!(
            g.tag_id(0, "a1").is_none()
                || g.cooc_by_id(
                    g.tag_id(0, "a1").unwrap_or(0),
                    g.tag_id(4, "f1").unwrap_or(0),
                ) == 0,
            "weakest pair pruned"
        );
        // Marginals are untouched by pair truncation.
        assert_eq!(g.marginal_by_id(a5), 9);
        // keep=0 empties; keep>=len is a no-op.
        assert_eq!(g.truncate_to_top_pairs(100), 2);
        g.truncate_to_top_pairs(1);
        assert_eq!(g.n_pairs(), 1);
    }

    #[test]
    fn insert_pair_zero_count_is_noop() {
        let mut g = TagRelationGraph::empty();
        g.insert_pair(0, "artist_a", 4, "fluffy", 0);
        assert!(g.is_empty());
    }

    #[test]
    fn insert_pair_empty_tag_is_noop() {
        let mut g = TagRelationGraph::empty();
        g.insert_pair(0, "artist_a", 4, "", 5);
        assert!(g.is_empty());
    }

    #[test]
    fn insert_pair_self_pair_is_noop() {
        let mut g = TagRelationGraph::empty();
        g.insert_pair(0, "tag", 0, "tag", 5);
        assert_eq!(g.n_pairs(), 0, "self-pair must be skipped");
    }

    #[test]
    fn insert_pair_canonical_ordering() {
        let mut g = TagRelationGraph::empty();
        // Insert with (b,a) where b < a should still store canonically as (a,b)
        let id_a = g.intern(0, "aaa");
        let id_b = g.intern(0, "bbb");
        g.insert_pair_by_id(id_b, id_a, 1);
        assert_eq!(g.cooc_by_id(id_a, id_b), 1, "canonical order query");
        assert_eq!(
            g.cooc_by_id(id_b, id_a),
            1,
            "reverse order query should match"
        );
    }

    // ── insert_pair_by_id ──────────────────────────────────────────────

    #[test]
    fn insert_pair_by_id_same_id_is_noop() {
        let mut g = TagRelationGraph::empty();
        let id = g.intern(0, "tag");
        g.insert_pair_by_id(id, id, 10);
        assert_eq!(g.n_pairs(), 0, "self-pair skipped");
    }

    #[test]
    fn insert_pair_by_id_accumulates() {
        let mut g = TagRelationGraph::empty();
        let a = g.intern(0, "a");
        let b = g.intern(4, "b");
        g.insert_pair_by_id(a, b, 2);
        g.insert_pair_by_id(a, b, 3);
        assert_eq!(g.cooc_by_id(a, b), 5, "insert_pair_by_id should accumulate");
    }

    // ── set_marginal / marginal_by_id ──────────────────────────────────

    #[test]
    fn set_marginal_stores_and_queries() {
        let mut g = TagRelationGraph::empty();
        let id = g.intern(0, "tag");
        g.set_marginal(0, "tag", 7);
        assert_eq!(g.marginal_by_id(id), 7);
    }

    #[test]
    fn set_marginal_negative_clamps_to_zero() {
        let mut g = TagRelationGraph::empty();
        g.set_marginal(0, "tag", -5);
        let id = g.tag_id(0, "tag").unwrap();
        assert_eq!(g.marginal_by_id(id), 0);
    }

    #[test]
    fn set_marginal_empty_tag_is_noop() {
        let mut g = TagRelationGraph::empty();
        g.set_marginal(0, "", 5);
        assert_eq!(g.n_tags(), 0);
    }

    #[test]
    fn marginal_by_id_unknown_returns_zero() {
        let g = TagRelationGraph::empty();
        assert_eq!(g.marginal_by_id(999), 0);
    }

    // ── cooc_by_id ─────────────────────────────────────────────────────

    #[test]
    fn cooc_by_id_self_returns_marginal() {
        let mut g = TagRelationGraph::empty();
        let id = g.intern(0, "tag");
        g.set_marginal(0, "tag", 10);
        assert_eq!(g.cooc_by_id(id, id), 10, "self-cooc = marginal");
    }

    #[test]
    fn cooc_by_id_unknown_pair_returns_zero() {
        let g = TagRelationGraph::empty();
        assert_eq!(g.cooc_by_id(0, 1), 0);
    }

    // ── freeze ─────────────────────────────────────────────────────────

    #[test]
    fn freeze_converts_hot_to_frozen() {
        let mut g = TagRelationGraph::empty();
        let a = g.intern(0, "a");
        let b = g.intern(4, "b");
        g.insert_pair_by_id(a, b, 5);

        assert!(matches!(g.pairs, PairStorage::Hot(_)));
        g.freeze(1);
        assert!(matches!(g.pairs, PairStorage::Frozen(_)));

        // Query still works
        assert_eq!(g.cooc_by_id(a, b), 5);
    }

    #[test]
    fn freeze_filters_below_min_cooc() {
        let mut g = TagRelationGraph::empty();
        let a = g.intern(0, "a");
        let b = g.intern(4, "b");
        let c = g.intern(4, "c");
        g.insert_pair_by_id(a, b, 2);
        g.insert_pair_by_id(a, c, 5);

        g.freeze(3); // min_cooc = 3
        assert_eq!(g.cooc_by_id(a, b), 0, "pair with count=2 dropped");
        assert_eq!(g.cooc_by_id(a, c), 5, "pair with count=5 kept");
    }

    #[test]
    fn freeze_clears_tag_to_id() {
        let mut g = TagRelationGraph::empty();
        g.intern(0, "a");
        g.intern(4, "b");
        g.insert_pair_by_id(0, 1, 1);
        g.freeze(1);
        // After freeze, tag_to_id is cleared
        assert!(g.tag_id(0, "a").is_none(), "tag_to_id cleared after freeze");
        // But marginals are preserved
        assert_eq!(g.marginal_by_id(0), 0, "marginals preserved");
    }

    // ── set_pairs_frozen_vec ───────────────────────────────────────────

    #[test]
    fn set_pairs_frozen_vec_canonicalises_and_dedups() {
        let mut g = TagRelationGraph::with_posts(100);
        // Supply unordered, non-canonical pairs with duplicates
        let raw = vec![
            (1u32, 0u32, 3u32), // non-canonical (1,0) → (0,1,3)
            (0u32, 1u32, 2u32), // duplicate (0,1,2) → coalesced with above → (0,1,5)
            (0u32, 2u32, 4u32),
        ];
        g.set_pairs_frozen_vec(raw);

        // Query
        assert_eq!(g.cooc_by_id(0, 1), 5, "pairs coalesced");
        assert_eq!(g.cooc_by_id(0, 2), 4);
        assert_eq!(g.cooc_by_id(1, 2), 0, "no pair between 1 and 2");
    }

    #[test]
    fn set_pairs_frozen_vec_removes_self_pairs() {
        let mut g = TagRelationGraph::with_posts(50);
        let raw = vec![(0u32, 0u32, 10u32), (1u32, 2u32, 5u32)];
        g.set_pairs_frozen_vec(raw);
        assert_eq!(g.n_pairs(), 1, "self-pair (0,0) must be removed");
    }

    // ── freeze_with_query_set ──────────────────────────────────────────

    #[test]
    fn freeze_with_query_set_filters_unrelated_pairs() {
        let mut g = TagRelationGraph::empty();
        let a = g.intern(0, "a"); // 0
        let b = g.intern(4, "b"); // 1
        let c = g.intern(4, "c"); // 2
        g.insert_pair_by_id(a, b, 3);
        g.insert_pair_by_id(a, c, 4);

        let mut queryable = std::collections::HashSet::new();
        queryable.insert(a); // a is in set
        queryable.insert(b); // b is in set
        // c is NOT in set

        g.freeze_with_query_set(&queryable, 1);
        assert_eq!(g.cooc_by_id(a, b), 3, "both endpoints queryable → kept");
        assert_eq!(g.cooc_by_id(a, c), 0, "c not queryable → dropped");
    }

    // ── from_train_posts ───────────────────────────────────────────────

    #[test]
    fn from_train_posts_builds_graph() {
        use crate::models::{Files, Flags, Has, Post, Rating, Relationships, Score, Stats, Tags};
        let post = Post {
            id: 1,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            change_seq: 0.0,
            files: Files::default(),
            uploader_id: 0,
            uploader_name: None,
            approver_id: None,
            stats: Stats {
                score: Score {
                    up: 1,
                    down: 0,
                    total: 1,
                },
                ..Default::default()
            },
            flags: Flags::default(),
            has: Has::default(),
            relationships: Relationships::default(),
            pools: vec![],
            rating: Rating::S,
            locked_tags: vec![],
            sources: vec![],
            description: None,
            tags: Tags {
                artist: vec!["artist_a".into()],
                general: vec!["fluffy".into(), "outdoor".into()],
                ..Tags::default()
            },
        };

        let g = TagRelationGraph::from_train_posts(&[post]);
        assert_eq!(g.n_posts(), 1);

        // Tags interned: artist_a(grp0), fluffy(grp4), outdoor(grp4)
        let artist_id = g.tag_id(0, "artist_a").expect("artist_a interned");
        let fluffy_id = g.tag_id(4, "fluffy").expect("fluffy interned");
        let outdoor_id = g.tag_id(4, "outdoor").expect("outdoor interned");

        // Marginals: each appears in 1 post
        assert_eq!(g.marginal_by_id(artist_id), 1);
        assert_eq!(g.marginal_by_id(fluffy_id), 1);
        assert_eq!(g.marginal_by_id(outdoor_id), 1);

        // Co-occurrences: artist × fluffy, artist × outdoor, fluffy × outdoor
        assert_eq!(g.cooc_by_id(artist_id, fluffy_id), 1);
        assert_eq!(g.cooc_by_id(artist_id, outdoor_id), 1);
        assert_eq!(g.cooc_by_id(fluffy_id, outdoor_id), 1);
    }

    #[test]
    fn from_train_posts_empty_slice() {
        let g = TagRelationGraph::from_train_posts(&[]);
        assert!(g.is_empty());
    }

    #[test]
    fn from_train_posts_lowercases_tags() {
        use crate::models::{Files, Flags, Has, Post, Rating, Relationships, Score, Stats, Tags};
        let post = Post {
            id: 1,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            change_seq: 0.0,
            files: Files::default(),
            uploader_id: 0,
            uploader_name: None,
            approver_id: None,
            stats: Stats {
                score: Score {
                    up: 1,
                    down: 0,
                    total: 1,
                },
                ..Default::default()
            },
            flags: Flags::default(),
            has: Has::default(),
            relationships: Relationships::default(),
            pools: vec![],
            rating: Rating::S,
            locked_tags: vec![],
            sources: vec![],
            description: None,
            tags: Tags {
                general: vec!["Fluffy".into()],
                ..Tags::default()
            },
        };
        let g = TagRelationGraph::from_train_posts(&[post]);
        // Tag should be lowercased to "fluffy"
        assert!(g.tag_id(4, "fluffy").is_some(), "tag should be lowercased");
        assert!(
            g.tag_id(4, "Fluffy").is_none(),
            "original case should not be stored"
        );
    }
}
