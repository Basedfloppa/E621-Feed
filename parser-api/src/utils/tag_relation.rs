use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

pub type GroupKey = u8;

pub const GROUP_ARTIST: GroupKey = 0;
pub const GROUP_CHARACTER: GroupKey = 1;
pub const GROUP_COPYRIGHT: GroupKey = 2;
pub const GROUP_SPECIES: GroupKey = 3;
pub const GROUP_GENERAL: GroupKey = 4;
pub const GROUP_LORE: GroupKey = 5;
pub const GROUP_META: GroupKey = 6;

pub fn group_key_from_str(s: &str) -> Option<GroupKey> {
    Some(match s {
        "artist" => GROUP_ARTIST,
        "character" => GROUP_CHARACTER,
        "copyright" => GROUP_COPYRIGHT,
        "species" => GROUP_SPECIES,
        "general" => GROUP_GENERAL,
        "lore" => GROUP_LORE,
        "meta" => GROUP_META,
        _ => return None,
    })
}

#[derive(Debug, Clone, Default)]
pub struct TagRelationGraph {
    pairs: HashMap<(GroupKey, String, GroupKey, String), i64>,
    marginals: HashMap<(GroupKey, String), i64>,
    n_posts: i64,
}

impl TagRelationGraph {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_posts(n_posts: i64) -> Self {
        Self {
            pairs: HashMap::new(),
            marginals: HashMap::new(),
            n_posts: n_posts.max(0),
        }
    }

    #[inline]
    fn canonical<'a>(
        g1: GroupKey,
        t1: &'a str,
        g2: GroupKey,
        t2: &'a str,
    ) -> ((GroupKey, &'a str), (GroupKey, &'a str)) {
        if (g1, t1) <= (g2, t2) {
            ((g1, t1), (g2, t2))
        } else {
            ((g2, t2), (g1, t1))
        }
    }

    pub fn insert_pair(&mut self, g1: GroupKey, t1: &str, g2: GroupKey, t2: &str, count: i64) {
        if count <= 0 || t1.is_empty() || t2.is_empty() {
            return;
        }
        let ((a_g, a_t), (b_g, b_t)) = Self::canonical(g1, t1, g2, t2);
        if a_g == b_g && a_t == b_t {
            return;
        }
        let key = (a_g, a_t.to_owned(), b_g, b_t.to_owned());
        *self.pairs.entry(key).or_insert(0) += count;
    }

    pub fn set_marginal(&mut self, g: GroupKey, t: &str, count: i64) {
        if t.is_empty() {
            return;
        }
        self.marginals.insert((g, t.to_owned()), count.max(0));
    }

    #[inline]
    pub fn cooc(&self, g1: GroupKey, t1: &str, g2: GroupKey, t2: &str) -> i64 {
        if t1.is_empty() || t2.is_empty() {
            return 0;
        }
        if g1 == g2 && t1 == t2 {
            return self.marginal(g1, t1);
        }
        let ((a_g, a_t), (b_g, b_t)) = Self::canonical(g1, t1, g2, t2);
        let key_owned = (a_g, a_t.to_owned(), b_g, b_t.to_owned());
        *self.pairs.get(&key_owned).unwrap_or(&0)
    }

    #[inline]
    pub fn marginal(&self, g: GroupKey, t: &str) -> i64 {
        if t.is_empty() {
            return 0;
        }
        let key = (g, t.to_owned());
        *self.marginals.get(&key).unwrap_or(&0)
    }

    #[inline]
    pub fn n_posts(&self) -> i64 {
        self.n_posts
    }
}

static GLOBAL_CACHE: LazyLock<ArcSwap<TagRelationGraph>> =
    LazyLock::new(|| ArcSwap::from_pointee(TagRelationGraph::empty()));
static GLOBAL_DIRTY: AtomicBool = AtomicBool::new(true);
static GLOBAL_REBUILDING: AtomicBool = AtomicBool::new(false);

pub fn mark_global_relation_dirty() {
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
                GLOBAL_DIRTY.store(false, Ordering::Release);
                match crate::db::load_global_tag_relation() {
                    Ok(new) => GLOBAL_CACHE.store(Arc::new(new)),
                    Err(e) => {
                        eprintln!("[tag-relation] rebuild failed: {e}");
                        GLOBAL_DIRTY.store(true, Ordering::Release);
                        break;
                    }
                }
                if !GLOBAL_DIRTY.load(Ordering::Acquire) {
                    break;
                }
            }
            GLOBAL_REBUILDING.store(false, Ordering::Release);
        })
        .expect("spawn tag-relation-rebuild thread");
}

/// Returns the current tag-relation cache. Never blocks; rebuilds happen in
/// the background and the call returns the previous snapshot in the meantime.
pub fn current_global_relation() -> Arc<TagRelationGraph> {
    if GLOBAL_DIRTY.load(Ordering::Acquire) {
        spawn_rebuild_if_needed();
    }
    GLOBAL_CACHE.load_full()
}
