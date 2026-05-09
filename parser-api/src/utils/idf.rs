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

/// Stores per-tag raw document frequency only. The IDF transform is applied
/// at lookup time using priors-supplied `df_floor` / `idf_max`, so calibrate
/// can probe those two knobs without rebuilding the index.
#[derive(Debug, Clone)]
pub struct IdfIndex {
    df: HashMap<String, i64>,
    n_posts: i64,
}

impl IdfIndex {
    pub fn empty() -> Self {
        Self {
            df: HashMap::new(),
            n_posts: 0,
        }
    }

    /// Robertson-Sparck-Jones-style smoothed IDF. `df_floor`, `idf_max`,
    /// `rsj_smoothing` are all priors-supplied so calibrate can probe them.
    #[inline]
    fn compute_idf(
        df_raw: i64,
        n_posts: i64,
        df_floor: f32,
        idf_max: f32,
        rsj: f32,
    ) -> f32 {
        let n = n_posts.max(1) as f32;
        let dfv = df_raw.max(0) as f32;
        let dfp = dfv + df_floor;
        let rsj = rsj.max(1e-3);
        (1.0 + ((n - dfp + rsj) / (dfp + rsj)).max(0.0))
            .ln()
            .min(idf_max)
            .max(0.0)
    }

    pub fn from_df(df: &HashMap<String, i64>, n_posts: i64) -> Self {
        let mut df_lc: HashMap<String, i64> = HashMap::with_capacity(df.len());
        for (tag, &df_raw) in df {
            let lc = tag.to_lowercase();
            df_lc.insert(lc, df_raw);
        }
        Self {
            df: df_lc,
            n_posts,
        }
    }

    pub fn from_db() -> rusqlite::Result<Self> {
        let df = crate::db::get_tags_df()?;
        let n_posts = crate::db::post_count();
        Ok(Self::from_df(&df, n_posts))
    }

    pub fn n_posts(&self) -> i64 {
        self.n_posts
    }

    /// True if the index holds nothing — used by the idle-eviction path so
    /// we don't churn-evict an already-empty cache every tick.
    pub fn is_empty(&self) -> bool {
        self.df.is_empty() && self.n_posts == 0
    }

    /// Number of distinct (lowercased) tag names in the index. Diagnostic only.
    pub fn n_tags(&self) -> usize {
        self.df.len()
    }

    #[inline]
    fn lookup_df(&self, tag: &str) -> i64 {
        let v = if tag.bytes().any(|b| b.is_ascii_uppercase()) {
            self.df.get(&tag.to_ascii_lowercase()).copied()
        } else {
            self.df.get(tag).copied()
        };
        // Unknown tag → df=0; compute_idf will produce the cap (treats as
        // maximally rare) — same behaviour as the old "missing → 1.0 raw".
        v.unwrap_or(0)
    }

    /// Raw document-frequency for a tag — used by calibrate to pre-resolve
    /// post features once at prep time so the hot scoring loop doesn't
    /// HashMap-lookup the same tag repeatedly across grid probes.
    #[inline]
    pub fn df_for(&self, tag: &str) -> i64 {
        self.lookup_df(tag)
    }

    /// Apply the IDF transform from a pre-resolved raw DF count. Mirrors
    /// `idf_tempered` exactly but skips the `lookup_df` HashMap probe.
    #[inline]
    pub fn idf_tempered_from_df(
        &self,
        df_raw: i64,
        df_floor: f32,
        idf_max: f32,
        rsj: f32,
        lambda: f32,
        alpha: f32,
    ) -> f32 {
        let raw = Self::compute_idf(df_raw, self.n_posts, df_floor, idf_max, rsj);
        let blended = 1.0 + lambda.clamp(0.0, 1.0) * (raw - 1.0);
        blended.powf(alpha.clamp(0.0, 1.0))
    }

    /// Raw IDF — debug/inspection only; scoring goes through `idf_tempered`.
    #[inline]
    pub fn idf_raw(&self, tag: &str, df_floor: f32, idf_max: f32, rsj: f32) -> f32 {
        Self::compute_idf(self.lookup_df(tag), self.n_posts, df_floor, idf_max, rsj)
    }

    #[inline]
    pub fn idf_tempered(
        &self,
        tag: &str,
        df_floor: f32,
        idf_max: f32,
        rsj: f32,
        lambda: f32,
        alpha: f32,
    ) -> f32 {
        let raw = self.idf_raw(tag, df_floor, idf_max, rsj);
        let blended = 1.0 + lambda.clamp(0.0, 1.0) * (raw - 1.0);
        blended.powf(alpha.clamp(0.0, 1.0))
    }

    pub fn bump(&mut self, df_delta: &HashMap<String, i64>, n_posts_delta: i64) {
        if df_delta.is_empty() && n_posts_delta == 0 {
            return;
        }
        self.n_posts = (self.n_posts + n_posts_delta).max(0);

        // No precomputed IDF cache to refresh — `idf_tempered` applies the
        // transform on-the-fly. Just keep df counters in sync.
        for (tag, &delta) in df_delta {
            if delta == 0 {
                continue;
            }
            let lc = if tag.bytes().any(|b| b.is_ascii_uppercase()) {
                tag.to_ascii_lowercase()
            } else {
                tag.clone()
            };
            let entry = self.df.entry(lc).or_insert(0);
            *entry = (*entry + delta).max(0);
        }
    }
}

static IDF_CACHE: LazyLock<ArcSwap<IdfIndex>> =
    LazyLock::new(|| ArcSwap::from_pointee(IdfIndex::empty()));
static IDF_DIRTY: AtomicBool = AtomicBool::new(true);
static IDF_REBUILDING: AtomicBool = AtomicBool::new(false);
static BUMP_LOCK: Mutex<()> = Mutex::new(());
static BUMP_DRIFT_COUNT: AtomicI64 = AtomicI64::new(0);

/// Wall-clock of the most recent interaction with this cache (read, bump,
/// or dirty-mark). The cache-pruner reads this on each tick and, if the
/// idle window has elapsed, swaps the index back to empty so the working
/// set returns to the OS. Initialised lazily to process-start.
static LAST_ACCESS: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

fn touch_access() {
    let mut g = LAST_ACCESS.lock().unwrap_or_else(|p| p.into_inner());
    *g = Instant::now();
}

pub fn mark_idf_dirty() {
    touch_access();
    IDF_DIRTY.store(true, Ordering::Release);
    spawn_rebuild_if_needed();
}

pub fn bump_idf(df_delta: HashMap<String, i64>, n_posts_delta: i64) {
    if df_delta.is_empty() && n_posts_delta == 0 {
        return;
    }
    touch_access();
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
                            error!("[idf] rebuild failed: {e}");
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
    touch_access();
    if IDF_DIRTY.load(Ordering::Acquire) {
        spawn_rebuild_if_needed();
    }
    IDF_CACHE.load_full()
}

/// Drop the loaded IDF index if no caller has touched it for at least
/// `idle_secs` seconds. Returns `(prev_n_tags, prev_n_posts)` if eviction
/// happened, or `(0, 0)` otherwise.
///
/// After eviction, `IDF_DIRTY` is set so the next `current_idf()` call
/// schedules a fresh rebuild — same code path as cold startup. The first
/// post-eviction request therefore sees an empty index (df=0 → max IDF
/// cap) until the rebuild thread completes; this matches existing
/// startup behaviour and avoids blocking the request thread on a
/// multi-second DB scan.
///
/// `idle_secs == 0` disables eviction.
pub fn evict_if_idle(idle_secs: u64) -> (usize, i64) {
    if idle_secs == 0 {
        return (0, 0);
    }
    // Snapshot last-access under the lock, but drop the guard before
    // touching ArcSwap to keep the critical section short.
    let elapsed = {
        let g = LAST_ACCESS.lock().unwrap_or_else(|p| p.into_inner());
        g.elapsed()
    };
    if elapsed.as_secs() < idle_secs {
        return (0, 0);
    }
    let current = IDF_CACHE.load_full();
    if current.is_empty() {
        return (0, 0);
    }
    let prev_tags = current.n_tags();
    let prev_posts = current.n_posts();
    drop(current);
    IDF_CACHE.store(Arc::new(IdfIndex::empty()));
    BUMP_DRIFT_COUNT.store(0, Ordering::Release);
    IDF_DIRTY.store(true, Ordering::Release);
    // Bump LAST_ACCESS so the next cache-pruner tick (every 30s on a
    // tight cadence) doesn't see "still idle, evict again" against the
    // empty cache and log spurious evictions.
    {
        let mut g = LAST_ACCESS.lock().unwrap_or_else(|p| p.into_inner());
        *g = Instant::now();
    }
    (prev_tags, prev_posts)
}
