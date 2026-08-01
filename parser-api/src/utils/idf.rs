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
    #[must_use]
    pub fn empty() -> Self {
        Self {
            df: HashMap::new(),
            n_posts: 0,
        }
    }

    /// Robertson-Sparck-Jones-style smoothed IDF. `df_floor`, `idf_max`,
    /// `rsj_smoothing` are all priors-supplied so calibrate can probe them.
    #[inline]
    fn compute_idf(df_raw: i64, n_posts: i64, df_floor: f32, idf_max: f32, rsj: f32) -> f32 {
        let n = n_posts.max(1) as f32;
        let dfv = df_raw.max(0) as f32;
        let dfp = dfv + df_floor;
        let rsj = rsj.max(1e-3);
        (1.0 + ((n - dfp + rsj) / (dfp + rsj)).max(0.0))
            .ln()
            .min(idf_max)
            .max(0.0)
    }

    #[must_use]
    pub fn from_df(df: &HashMap<String, i64>, n_posts: i64) -> Self {
        let mut df_lc: HashMap<String, i64> = HashMap::with_capacity(df.len());
        for (tag, &df_raw) in df {
            let lc = tag.to_lowercase();
            df_lc.insert(lc, df_raw);
        }
        Self { df: df_lc, n_posts }
    }

    pub fn from_db() -> rusqlite::Result<Self> {
        let df = crate::db::get_tags_df()?;
        let n_posts = crate::db::post_count();
        Ok(Self::from_df(&df, n_posts))
    }

    #[must_use]
    pub fn n_posts(&self) -> i64 {
        self.n_posts
    }

    /// True if the index holds nothing — used by the idle-eviction path so
    /// we don't churn-evict an already-empty cache every tick.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.df.is_empty() && self.n_posts == 0
    }

    /// Number of distinct (lowercased) tag names in the index. Diagnostic only.
    #[must_use]
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
    #[must_use]
    pub fn df_for(&self, tag: &str) -> i64 {
        self.lookup_df(tag)
    }

    /// Apply the IDF transform from a pre-resolved raw DF count. Mirrors
    /// `idf_tempered` exactly but skips the `lookup_df` `HashMap` probe.
    #[inline]
    #[must_use]
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
    #[must_use]
    pub fn idf_raw(&self, tag: &str, df_floor: f32, idf_max: f32, rsj: f32) -> f32 {
        Self::compute_idf(self.lookup_df(tag), self.n_posts, df_floor, idf_max, rsj)
    }

    #[inline]
    #[must_use]
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

/// Two separate access timers so background work (prefetch, `save_posts_tags_batch`)
/// doesn't prevent idle-eviction of the user-facing cache.
///
/// `LAST_USER_ACCESS` — updated only by request-serving code (`current_idf`).
/// The cache-pruner uses this to decide whether to evict.
///
/// `LAST_SYSTEM_ACCESS` — updated by background workers (`bump_idf`,
/// `mark_idf_dirty`). Not consulted for idle-eviction.
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

pub fn mark_idf_dirty() {
    touch_system_access();
    IDF_DIRTY.store(true, Ordering::Release);
    spawn_rebuild_if_needed();
}

pub fn bump_idf(df_delta: HashMap<String, i64>, n_posts_delta: i64) {
    if df_delta.is_empty() && n_posts_delta == 0 {
        return;
    }
    touch_system_access();
    let _guard = BUMP_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    touch_user_access();
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
    // Snapshot user-access under the lock, but drop the guard before
    // touching ArcSwap to keep the critical section short.
    let elapsed = {
        let g = LAST_USER_ACCESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    // Bump LAST_USER_ACCESS so the next cache-pruner tick (every 30s on a
    // tight cadence) doesn't see "still idle, evict again" against the
    // empty cache and log spurious evictions.
    {
        let mut g = LAST_USER_ACCESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = Instant::now();
    }
    (prev_tags, prev_posts)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── compute_idf math ───────────────────────────────────────────────

    #[test]
    fn compute_idf_zero_df_gives_max_idf() {
        // df=0 → the rare-tag "bonus": n / 0 transforms to (n - 0 + rsj) / (0 + rsj)
        // which is large (but capped at idf_max).
        let idf = IdfIndex::compute_idf(0, 10_000, 0.4, 100.0, 0.35);
        assert!(idf > 0.0);
        assert!(idf <= 100.0, "idf must not exceed idf_max: {idf}");
        // For df=0, n=10000, floor=0.4, rsj=0.35:
        // dfp = 0.4, n-dfp+rsj = 10000-0.4+0.35 = 9999.95, dfp+rsj = 0.75
        // 1 + (9999.95/0.75) ≈ 13334.27, ln(13334.27) ≈ 9.50, min(100) ≈ 9.50
        // That's a reasonable upper bound.
        assert!(
            (idf - 9.5).abs() < 0.1,
            "df=0, n=10000 → idf ≈ 9.5, got {idf}"
        );
    }

    #[test]
    fn compute_idf_high_df_gives_low_idf() {
        // df ≈ n → almost no IDF bonus.
        let idf = IdfIndex::compute_idf(95_000, 100_000, 0.4, 100.0, 0.35);
        // dfp = 95000 + 0.4 = 95000.4, n - dfp + rsj = 100000 - 95000.4 + 0.35 = 4999.95
        // dfp + rsj = 95000.4 + 0.35 = 95000.75
        // 1 + 4999.95/95000.75 ≈ 1.0526
        // ln(1.0526) ≈ 0.0513
        assert!(idf < 1.0, "common tags get low IDF, got {idf}");
        assert!(idf > 0.0);
    }

    #[test]
    fn compute_idf_n_posts_floor() {
        // n_posts = 0 or negative → clamped to 1
        let idf = IdfIndex::compute_idf(0, 0, 0.4, 100.0, 0.35);
        assert!(idf > 0.0);
        let idf_neg = IdfIndex::compute_idf(0, -10, 0.4, 100.0, 0.35);
        assert!((idf - idf_neg).abs() < 1e-6, "negative n_posts => clamped");
    }

    #[test]
    fn compute_idf_clamps_to_idf_max() {
        // Extreme case: n very large, df very small → should saturate at idf_max.
        // We can also set idf_max to a tiny value to force clamping.
        let idf = IdfIndex::compute_idf(1, 1_000_000, 0.4, 2.0, 0.35);
        assert!(idf <= 2.0, "idf must be capped at idf_max=2.0, got {idf}");
    }

    #[test]
    fn compute_idf_negative_df_clamps_to_zero() {
        let idf = IdfIndex::compute_idf(-5, 1000, 0.4, 100.0, 0.35);
        assert!(idf > 0.0);
        // df clamped to 0 internally, so same as df=0
        let zero = IdfIndex::compute_idf(0, 1000, 0.4, 100.0, 0.35);
        assert!((idf - zero).abs() < 0.01);
    }

    #[test]
    fn compute_idf_minimum_rsj_smoothing() {
        // rsj = 0 → internally clamped to 0.001
        let idf = IdfIndex::compute_idf(0, 1000, 0.4, 100.0, 0.0);
        assert!(idf > 0.0);
        let ref_idf = IdfIndex::compute_idf(0, 1000, 0.4, 100.0, 0.001);
        assert!((idf - ref_idf).abs() < 0.01);
    }

    // ── from_df / lookup_df ────────────────────────────────────────────

    #[test]
    fn from_df_lowercases_tags() {
        let mut df = HashMap::new();
        df.insert("Fluffy".into(), 10i64);
        df.insert("Wolf".into(), 5i64);
        let idx = IdfIndex::from_df(&df, 100);
        assert_eq!(idx.df_for("fluffy"), 10);
        assert_eq!(idx.df_for("FLUFFY"), 10);
        assert_eq!(idx.df_for("Fluffy"), 10);
        assert_eq!(idx.df_for("wolf"), 5);
    }

    #[test]
    fn from_df_empty() {
        let df = HashMap::new();
        let idx = IdfIndex::from_df(&df, 0);
        assert!(idx.is_empty());
        assert_eq!(idx.n_tags(), 0);
        assert_eq!(idx.n_posts(), 0);
    }

    #[test]
    fn lookup_df_unknown_tag_returns_zero() {
        let df = HashMap::new();
        let idx = IdfIndex::from_df(&df, 100);
        assert_eq!(idx.df_for("nonexistent_tag"), 0);
    }

    // ── idf_tempered ───────────────────────────────────────────────────

    #[test]
    fn idf_tempered_applies_lambda_and_alpha() {
        let mut df = HashMap::new();
        df.insert("common".into(), 800i64);
        let idx = IdfIndex::from_df(&df, 1000);

        // lambda=0: blended = 1.0 + 0*(raw-1) = 1.0
        let zero_lambda = idx.idf_tempered("common", 0.4, 100.0, 0.35, 0.0, 1.0);
        assert!(
            (zero_lambda - 1.0).abs() < 1e-6,
            "lambda=0 should give 1.0, got {zero_lambda}"
        );

        // alpha=0: result^0 = 1.0
        let zero_alpha = idx.idf_tempered("common", 0.4, 100.0, 0.35, 1.0, 0.0);
        assert!(
            (zero_alpha - 1.0).abs() < 1e-6,
            "alpha=0 should give 1.0, got {zero_alpha}"
        );

        // lambda=1, alpha=1: should give raw IDF
        let full = idx.idf_tempered("common", 0.4, 100.0, 0.35, 1.0, 1.0);
        let raw = idx.idf_raw("common", 0.4, 100.0, 0.35);
        assert!(
            (full - raw).abs() < 1e-6,
            "lambda=1, alpha=1 should equal raw IDF, got {full} vs {raw}"
        );
    }

    #[test]
    fn idf_tempered_from_df_matches_idf_tempered() {
        let mut df = HashMap::new();
        df.insert("tag".into(), 50i64);
        let idx = IdfIndex::from_df(&df, 1000);

        let df_raw = idx.df_for("tag");
        let from_df = idx.idf_tempered_from_df(df_raw, 0.4, 100.0, 0.35, 0.8, 0.9);
        let direct = idx.idf_tempered("tag", 0.4, 100.0, 0.35, 0.8, 0.9);
        assert!(
            (from_df - direct).abs() < 1e-6,
            "idf_tempered_from_df must match idf_tempered: {from_df} vs {direct}"
        );
    }

    // ── bump ───────────────────────────────────────────────────────────

    #[test]
    fn bump_increments_df_and_n_posts() {
        let mut idx = IdfIndex::empty();
        assert!(idx.is_empty());

        let mut delta = HashMap::new();
        delta.insert("fluffy".into(), 3i64);
        delta.insert("wolf".into(), 1i64);
        idx.bump(&delta, 5);

        assert_eq!(idx.n_posts(), 5);
        assert_eq!(idx.df_for("fluffy"), 3);
        assert_eq!(idx.df_for("wolf"), 1);
        assert_eq!(idx.df_for("unknown"), 0);
    }

    #[test]
    fn bump_negative_delta_decrements() {
        let mut idx = IdfIndex::empty();
        let mut d1 = HashMap::new();
        d1.insert("a".into(), 10i64);
        idx.bump(&d1, 10);
        assert_eq!(idx.df_for("a"), 10);

        let mut d2 = HashMap::new();
        d2.insert("a".into(), -3i64);
        idx.bump(&d2, -2);
        assert_eq!(idx.df_for("a"), 7);
        assert_eq!(idx.n_posts(), 8);
    }

    #[test]
    fn bump_negative_below_zero_clamps_to_zero() {
        let mut idx = IdfIndex::empty();
        let mut d = HashMap::new();
        d.insert("a".into(), 5i64);
        idx.bump(&d, 5);
        let mut d2 = HashMap::new();
        d2.insert("a".into(), -10i64);
        idx.bump(&d2, -10);
        assert_eq!(idx.df_for("a"), 0, "df must not go below 0");
        assert_eq!(idx.n_posts(), 0, "n_posts must not go below 0");
    }

    #[test]
    fn bump_empty_delta_is_noop() {
        let mut idx = IdfIndex::empty();
        let delta = HashMap::new();
        idx.bump(&delta, 0);
        assert!(idx.is_empty());

        // Non-zero n_posts_delta with empty df_delta still bumps n_posts
        idx.bump(&delta, 5);
        assert_eq!(idx.n_posts(), 5);
    }

    #[test]
    fn bump_handles_case_variants() {
        let mut idx = IdfIndex::empty();
        // Insert "Fluffy" (with capital F)
        let mut d1 = HashMap::new();
        d1.insert("Fluffy".into(), 5i64);
        idx.bump(&d1, 1);

        // Lookup with different case
        assert_eq!(idx.df_for("fluffy"), 5, "case-insensitive lookup");
        assert_eq!(idx.df_for("FLUFFY"), 5);

        // Bump lowercase key should still go to the same entry
        let mut d2 = HashMap::new();
        d2.insert("fluffy".into(), 3i64);
        idx.bump(&d2, 0);
        assert_eq!(idx.df_for("fluffy"), 8, "case-insensitive bump");
    }

    // ── IdfIndex misc ──────────────────────────────────────────────────

    #[test]
    fn empty_index_properties() {
        let idx = IdfIndex::empty();
        assert!(idx.is_empty());
        assert_eq!(idx.n_tags(), 0);
        assert_eq!(idx.n_posts(), 0);
    }

    #[test]
    fn idf_tempered_unknown_tag_produces_max_idf() {
        let idx = IdfIndex::empty();
        // Unknown tag → df=0 → compute_idf with n_posts=0 → n clamped to 1
        let idf = idx.idf_tempered("anything", 0.4, 100.0, 0.35, 1.0, 1.0);
        assert!(idf > 0.0, "unknown tag gets positive IDF, got {idf}");
        // With n=1, df=0: dfp=0.4, n-dfp+rsj = 1-0.4+0.35 = 0.95, dfp+rsj = 0.75
        // 1 + 0.95/0.75 = 2.267, ln(2.267) = 0.818
        assert!(
            (idf - 0.82).abs() < 0.05,
            "empty index, unknown tag → idf ≈ 0.82, got {idf}"
        );
    }
}
