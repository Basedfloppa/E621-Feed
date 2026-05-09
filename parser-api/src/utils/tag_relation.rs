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
#[derive(Debug, Clone, Default)]
pub struct TagRelationGraph {
    tag_to_id: [HashMap<String, TagId>; GROUP_COUNT],
    pairs: HashMap<(TagId, TagId), i64>,
    marginals: Vec<i64>,
    n_posts: i64,
}

impl TagRelationGraph {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_posts(n_posts: i64) -> Self {
        Self {
            tag_to_id: Default::default(),
            pairs: HashMap::new(),
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
        *self.pairs.entry(key).or_insert(0) += count;
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
        *self.pairs.entry(key).or_insert(0) += count;
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
        *self.pairs.get(&key).unwrap_or(&0)
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
}

static GLOBAL_CACHE: LazyLock<ArcSwap<TagRelationGraph>> =
    LazyLock::new(|| ArcSwap::from_pointee(TagRelationGraph::empty()));
static GLOBAL_DIRTY: AtomicBool = AtomicBool::new(true);
static GLOBAL_REBUILDING: AtomicBool = AtomicBool::new(false);

/// Wall-clock of the most recent interaction with this cache (read or
/// dirty-mark). The cache-pruner reads this on each tick and, if the
/// idle window has elapsed, swaps the graph back to empty so its
/// O(N pairs) HashMap is freed back to the OS.
static LAST_ACCESS: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

fn touch_access() {
    let mut g = LAST_ACCESS.lock().unwrap_or_else(|p| p.into_inner());
    *g = Instant::now();
}

pub fn mark_global_relation_dirty() {
    touch_access();
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
    touch_access();
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
        let g = LAST_ACCESS.lock().unwrap_or_else(|p| p.into_inner());
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
        let mut g = LAST_ACCESS.lock().unwrap_or_else(|p| p.into_inner());
        *g = Instant::now();
    }
    (prev_pairs, prev_tags)
}
