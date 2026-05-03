use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::models::cfg;

/// Coalesce a burst of dirty marks into one rebuild. Duration read fresh from
/// `runtime.idf_rebuild_cooldown_secs` so config reloads take effect on the
/// next worker iteration.
fn rebuild_cooldown() -> Duration {
    Duration::from_secs(cfg().runtime.idf_rebuild_cooldown_secs.max(1))
}

#[derive(Debug, Clone)]
pub struct IdfIndex {
    df: HashMap<String, i64>,
    idf: HashMap<String, f32>,
    n_posts: i64,
}

impl IdfIndex {
    pub fn empty() -> Self {
        Self {
            df: HashMap::new(),
            idf: HashMap::new(),
            n_posts: 0,
        }
    }

    fn compute_idf(df_raw: i64, n_posts: i64) -> f32 {
        let cfg = cfg();
        let n = n_posts.max(1) as f32;
        let dfv = df_raw.max(0) as f32;
        let dfp = dfv + cfg.df_floor;
        (1.0 + ((n - dfp + 0.5) / (dfp + 0.5)).max(0.0))
            .ln()
            .min(cfg.idf_max)
            .max(0.0)
    }

    pub fn from_df(df: &HashMap<String, i64>, n_posts: i64) -> Self {
        let mut idf = HashMap::with_capacity(df.len());
        let mut df_lc: HashMap<String, i64> = HashMap::with_capacity(df.len());
        for (tag, &df_raw) in df {
            let lc = tag.to_lowercase();
            idf.insert(lc.clone(), Self::compute_idf(df_raw, n_posts));
            df_lc.insert(lc, df_raw);
        }
        Self {
            df: df_lc,
            idf,
            n_posts,
        }
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

    pub fn bump(&mut self, df_delta: &HashMap<String, i64>, n_posts_delta: i64) {
        if df_delta.is_empty() && n_posts_delta == 0 {
            return;
        }
        self.n_posts = (self.n_posts + n_posts_delta).max(0);

        if n_posts_delta != 0 {
            for (tag, df_raw) in &self.df {
                self.idf
                    .insert(tag.clone(), Self::compute_idf(*df_raw, self.n_posts));
            }
        }

        for (tag, &delta) in df_delta {
            if delta == 0 {
                continue;
            }
            let lc = if tag.bytes().any(|b| b.is_ascii_uppercase()) {
                tag.to_ascii_lowercase()
            } else {
                tag.clone()
            };
            let new_df = {
                let entry = self.df.entry(lc.clone()).or_insert(0);
                *entry = (*entry + delta).max(0);
                *entry
            };
            self.idf.insert(lc, Self::compute_idf(new_df, self.n_posts));
        }
    }
}

static IDF_CACHE: LazyLock<ArcSwap<IdfIndex>> =
    LazyLock::new(|| ArcSwap::from_pointee(IdfIndex::empty()));
static IDF_DIRTY: AtomicBool = AtomicBool::new(true);
static IDF_REBUILDING: AtomicBool = AtomicBool::new(false);
static BUMP_LOCK: Mutex<()> = Mutex::new(());
static BUMP_DRIFT_COUNT: AtomicI64 = AtomicI64::new(0);

pub fn mark_idf_dirty() {
    IDF_DIRTY.store(true, Ordering::Release);
    spawn_rebuild_if_needed();
}

pub fn bump_idf(df_delta: HashMap<String, i64>, n_posts_delta: i64) {
    if df_delta.is_empty() && n_posts_delta == 0 {
        return;
    }
    let _guard = BUMP_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let current = IDF_CACHE.load_full();
    let mut next = (*current).clone();
    next.bump(&df_delta, n_posts_delta);
    IDF_CACHE.store(Arc::new(next));

    let n = BUMP_DRIFT_COUNT.fetch_add(1, Ordering::AcqRel) + 1;
    if n >= cfg().runtime.idf_bump_drift_threshold.max(1) {
        BUMP_DRIFT_COUNT.store(0, Ordering::Release);
        mark_idf_dirty();
    }
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
                        Ok(new) => {
                            IDF_CACHE.store(Arc::new(new));
                            BUMP_DRIFT_COUNT.store(0, Ordering::Release);
                        }
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
                let deadline = Instant::now() + rebuild_cooldown();
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
