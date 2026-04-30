use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use crate::models::cfg;

/// After a rebuild completes, the worker thread lingers for this long picking
/// up any further `mark_idf_dirty()` calls. Without it, a busy /recommendations
/// stream re-spawns the rebuild thread on every request — same work, more
/// thread churn and more redundant table scans.
const REBUILD_COOLDOWN: Duration = Duration::from_secs(15);

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

    pub fn from_db() -> rusqlite::Result<Self> {
        let df = crate::db::get_tags_df()?;
        let n_posts = crate::db::post_count();
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
static IDF_REBUILDING: AtomicBool = AtomicBool::new(false);

pub fn mark_idf_dirty() {
    IDF_DIRTY.store(true, Ordering::Release);
    spawn_rebuild_if_needed();
}

fn spawn_rebuild_if_needed() {
    if IDF_REBUILDING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::Builder::new()
        .name("idf-rebuild".to_string())
        .spawn(|| {
            loop {
                let mut last_rebuild_failed = false;
                'work: loop {
                    if !IDF_DIRTY.swap(false, Ordering::AcqRel) {
                        break 'work;
                    }
                    match IdfIndex::from_db() {
                        Ok(new) => IDF_CACHE.store(Arc::new(new)),
                        Err(e) => {
                            eprintln!("[idf] rebuild failed: {e}");
                            IDF_DIRTY.store(true, Ordering::Release);
                            last_rebuild_failed = true;
                            break 'work;
                        }
                    }
                }

                if last_rebuild_failed {
                    // Don't busy-spin on persistent failure; bow out and let
                    // the next mark_idf_dirty re-arm a fresh worker.
                    break;
                }

                // Cooldown: stay alive briefly so a burst of mark_idf_dirty()
                // calls (e.g. infinite-scroll fetches) coalesces into a single
                // follow-up rebuild instead of re-spawning the thread.
                let deadline = Instant::now() + REBUILD_COOLDOWN;
                while Instant::now() < deadline {
                    if IDF_DIRTY.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }

                if !IDF_DIRTY.load(Ordering::Acquire) {
                    break;
                }
            }

            IDF_REBUILDING.store(false, Ordering::Release);
            // A mark_idf_dirty() that landed between our last DIRTY check and
            // the REBUILDING release would otherwise hang; re-spawn if so.
            if IDF_DIRTY.load(Ordering::Acquire) {
                spawn_rebuild_if_needed();
            }
        })
        .expect("spawn idf-rebuild thread");
}

pub fn current_idf() -> Arc<IdfIndex> {
    if IDF_DIRTY.load(Ordering::Acquire) {
        spawn_rebuild_if_needed();
    }
    IDF_CACHE.load_full()
}
