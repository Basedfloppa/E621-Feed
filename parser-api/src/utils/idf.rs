use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use crate::models::cfg;

#[derive(Debug, Clone)]
pub struct IdfIndex {
    idf: HashMap<String, f32>,
}

impl IdfIndex {
    pub fn empty() -> Self {
        Self { idf: HashMap::new() }
    }

    pub fn from_df(df: &HashMap<String, i64>, n_posts: i64) -> Self {
        let cfg = cfg();
        let mut idf = HashMap::with_capacity(df.len());
        let n = n_posts.max(1) as f32;

        for (tag, &df_raw) in df {
            let dfv = df_raw.max(0) as f32;
            let dfp = dfv + cfg.df_floor;
            let val = (1.0 + ((n - dfp + 0.5) / (dfp + 0.5)).max(0.0))
                .ln()
                .min(cfg.idf_max)
                .max(0.0);
            idf.insert(tag.to_lowercase(), val);
        }

        Self { idf }
    }

    pub fn from_db(
        get_df: impl Fn() -> rusqlite::Result<HashMap<String, i64>>,
        get_post_count: impl Fn() -> i64,
    ) -> rusqlite::Result<Self> {
        let df = get_df()?;
        let n_posts = get_post_count();
        Ok(Self::from_df(&df, n_posts))
    }

    #[inline]
    pub fn idf_raw(&self, tag: &str) -> f32 {
        if tag.bytes().any(|b| b.is_ascii_uppercase()) {
            *self.idf.get(&tag.to_ascii_lowercase()).unwrap_or(&1.0)
        } else {
            *self.idf.get(tag).unwrap_or(&1.0)
        }
    }

    #[inline]
    pub fn idf_tempered(&self, tag: &str, lambda: f32, alpha: f32) -> f32 {
        let raw = self.idf_raw(tag);
        let blended = 1.0 + lambda.clamp(0.0, 1.0) * (raw - 1.0);
        blended.powf(alpha.clamp(0.0, 1.0))
    }
}

static IDF_CACHE: LazyLock<ArcSwap<IdfIndex>> =
    LazyLock::new(|| ArcSwap::from_pointee(IdfIndex::empty()));
static IDF_DIRTY: AtomicBool = AtomicBool::new(true);
static IDF_REBUILD_LOCK: Mutex<()> = Mutex::new(());

pub fn mark_idf_dirty() {
    IDF_DIRTY.store(true, Ordering::Release);
}

pub fn current_idf() -> rusqlite::Result<Arc<IdfIndex>> {
    if IDF_DIRTY.load(Ordering::Acquire) {
        let _guard = IDF_REBUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if IDF_DIRTY.swap(false, Ordering::AcqRel) {
            match IdfIndex::from_db(crate::db::get_tags_df, crate::db::post_count) {
                Ok(new) => IDF_CACHE.store(Arc::new(new)),
                Err(e) => {
                    IDF_DIRTY.store(true, Ordering::Release);
                    return Err(e);
                }
            }
        }
    }
    Ok(IDF_CACHE.load_full())
}
