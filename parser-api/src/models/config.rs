use crate::utils::Priors;
use anyhow::Context;
use arc_swap::ArcSwap;
use rocket::serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};
use std::{fs, thread};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub admin_user: String,
    pub admin_api: String,
    pub tag_blacklist: Vec<String>,
    /// Default per-account blacklist applied at DB write when the client
    /// omits or empties the `blacklist` field. e621-style search syntax;
    /// promoted from a hardcoded list in `db::accounts::set_account`
    /// pre-v5.3 so operators can edit it without a rebuild.
    #[serde(default = "default_default_account_blacklist")]
    pub default_account_blacklist: Vec<String>,
    pub posts_domain: String,
    pub posts_limit: i32,
    pub rps_delay_ms: u64,
    pub max_retries: u64,

    #[serde(default = "default_user_agent")]
    pub user_agent: String,

    /// In-memory TTL cache over outbound GET requests to e621. Hit rate
    /// is highest on the recommendations path, where two devices on the
    /// same account or two accounts with the same default blacklist
    /// otherwise turn into duplicate `posts.json` round-trips. `0`
    /// disables the cache entirely.
    #[serde(default = "default_e621_cache_ttl_secs")]
    pub e621_cache_ttl_secs: u64,
    /// Hard cap on cache entries. Past this, the oldest 10% are evicted
    /// in one pass — keeps memory bounded even under a key-spraying
    /// adversary or a config typo (e.g. enormous TTL).
    #[serde(default = "default_e621_cache_max_entries")]
    pub e621_cache_max_entries: usize,

    pub priors: Priors,

    #[serde(default)]
    pub buckets: HashMap<String, BucketOverride>,

    /// Runtime knobs for the recommendations endpoint, prefetcher, cleanup,
    /// and IDF/tag-relation rebuild workers. All fields have defaults — the
    /// section is optional in `config.toml`.
    #[serde(default)]
    pub runtime: RuntimeConfig,

    /// Calibration / seed binary knobs. Skip in production deployments.
    #[serde(default)]
    pub backtest: BacktestConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeConfig {
    /// Cap on local-pool candidates merged into a `/recommendations` page.
    #[serde(default = "default_local_candidate_limit")]
    pub local_candidate_limit: i64,
    /// How far back the seen-post dedup looks when filtering already-shown
    /// posts out of the candidate pool.
    #[serde(default = "default_dedup_lookback_days")]
    pub dedup_lookback_days: i64,
    /// Parallel pages fetched during /process favourites import.
    #[serde(default = "default_process_fetch_concurrency")]
    pub process_fetch_concurrency: usize,

    /// Cooldown after an IDF rebuild — burst of dirty marks coalesces into
    /// one rebuild.
    #[serde(default = "default_rebuild_cooldown_secs")]
    pub idf_rebuild_cooldown_secs: u64,
    /// After this many incremental `bump_idf` calls, schedule a corrective
    /// full rebuild to flush accumulated drift.
    #[serde(default = "default_idf_bump_drift_threshold")]
    pub idf_bump_drift_threshold: i64,
    /// Cooldown after a tag-relation rebuild (analogous to idf_rebuild_cooldown).
    #[serde(default = "default_rebuild_cooldown_secs")]
    pub tag_relation_rebuild_cooldown_secs: u64,

    /// Background catalog prefetcher: how often to wake.
    #[serde(default = "default_prefetch_interval_secs")]
    pub prefetch_interval_secs: u64,
    /// Catalog cleanup worker: how often to wake.
    #[serde(default = "default_cleanup_interval_secs")]
    pub cleanup_interval_secs: u64,
    /// Catalog cleanup retention. Posts not in any user's favs and not
    /// touched within this many days are pruned.
    #[serde(default = "default_catalog_retention_days")]
    pub catalog_retention_days: i64,
    /// Prefetcher only targets accounts that interacted with the feed
    /// within this window.
    #[serde(default = "default_prefetch_active_window_days")]
    pub prefetch_active_window_days: i64,
    /// Circuit-breaker: after this many consecutive e621 fetch failures
    /// the prefetcher pauses for `prefetch_breaker_pause_secs` instead of
    /// hammering an already-failing upstream. Without this a single
    /// Cloudflare 403/520 against the admin account turns into an N-tag,
    /// every-`prefetch_interval_secs` retry storm that accelerates any
    /// rate-limit/ban already in progress.
    #[serde(default = "default_prefetch_breaker_threshold")]
    pub prefetch_breaker_threshold: u32,
    /// How long the prefetcher sleeps once the circuit breaker is open
    /// before resetting the counter and resuming normal cadence.
    #[serde(default = "default_prefetch_breaker_pause_secs")]
    pub prefetch_breaker_pause_secs: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            local_candidate_limit: default_local_candidate_limit(),
            dedup_lookback_days: default_dedup_lookback_days(),
            process_fetch_concurrency: default_process_fetch_concurrency(),
            idf_rebuild_cooldown_secs: default_rebuild_cooldown_secs(),
            idf_bump_drift_threshold: default_idf_bump_drift_threshold(),
            tag_relation_rebuild_cooldown_secs: default_rebuild_cooldown_secs(),
            prefetch_interval_secs: default_prefetch_interval_secs(),
            cleanup_interval_secs: default_cleanup_interval_secs(),
            catalog_retention_days: default_catalog_retention_days(),
            prefetch_active_window_days: default_prefetch_active_window_days(),
            prefetch_breaker_threshold: default_prefetch_breaker_threshold(),
            prefetch_breaker_pause_secs: default_prefetch_breaker_pause_secs(),
        }
    }
}

fn default_user_agent() -> String {
    "E621AccountParser/0.1 (+https://github.com/zorolin/E621-Account-Parser)".to_string()
}

fn default_default_account_blacklist() -> Vec<String> {
    vec![
        "gore".into(),
        "scat".into(),
        "watersports".into(),
        "young -rating:s".into(),
        "loli".into(),
        "shota".into(),
    ]
}

fn default_e621_cache_ttl_secs() -> u64 {
    600
}
fn default_e621_cache_max_entries() -> usize {
    5000
}

fn default_local_candidate_limit() -> i64 {
    400
}
fn default_dedup_lookback_days() -> i64 {
    14
}
fn default_process_fetch_concurrency() -> usize {
    4
}
fn default_rebuild_cooldown_secs() -> u64 {
    15
}
fn default_idf_bump_drift_threshold() -> i64 {
    200
}
fn default_prefetch_interval_secs() -> u64 {
    180
}
fn default_cleanup_interval_secs() -> u64 {
    21_600
}
fn default_catalog_retention_days() -> i64 {
    90
}
fn default_prefetch_active_window_days() -> i64 {
    14
}
fn default_prefetch_breaker_threshold() -> u32 {
    3
}
fn default_prefetch_breaker_pause_secs() -> u64 {
    600
}

#[derive(Debug, Clone, Deserialize)]
pub struct BacktestConfig {
    /// Minimum favourites an account must have to enter the calibration set.
    /// Lower = more accounts evaluated (better statistical power), but each
    /// account's synthetic profile + holdout become noisier. Accounts with
    /// fewer than `min_favs / 2` train posts after the split are also
    /// rejected — so dropping below ~50 stops being useful.
    #[serde(default = "default_min_favs")]
    pub min_favs: usize,

    /// Fraction of each user's favourites held out as the test set (the
    /// newest 20% by post id, by default). Higher = more test items per
    /// account → less noisy per-account NDCG, but the synthetic profile
    /// built from the train half is correspondingly sparser. 0.20 mirrors
    /// the standard ML train/test split.
    #[serde(default = "default_test_fraction")]
    pub test_fraction: f32,

    /// Sampled random negatives per held-out positive. Higher = stricter
    /// retrieval task (closer to production where most catalog posts aren't
    /// favourites). Dominates cached dataset RAM:
    ///   `max_accounts × ~256 · negative_ratio × ~5 KB/post`
    /// At defaults that's ~2 GB. If it exceeds free RAM the box swap-
    /// thrashes and per-eval cost grows ~10×.
    #[serde(default = "default_negative_ratio")]
    pub negative_ratio: usize,

    /// Cutoff `k` for NDCG@k. Smaller k weights only the top of the feed
    /// (matches where users actually look); larger k evaluates a wider
    /// slice. 20 reflects roughly one screen of an infinite-scroll page.
    #[serde(default = "default_top_k_ndcg")]
    pub top_k_ndcg: usize,

    /// Cutoff `k` for Recall@k. Larger than the NDCG cutoff on purpose:
    /// recall measures coverage, not ranking, so we care whether a held-out
    /// positive lands anywhere in the first ~50 results, not at the very
    /// top. Useful as a sanity check that the model isn't ignoring a slice
    /// of the user's taste.
    #[serde(default = "default_top_k_recall")]
    pub top_k_recall: usize,

    /// Cap on accounts evaluated per run. Confidence intervals tighten as
    /// `sqrt(N)`, so 150 vs 50 buys ~1.7× tighter bounds. Cost grows
    /// linearly in both prep time (one-time hydration into the cache) and
    /// per-eval scoring. Calibrate picks the top accounts by favourite
    /// count, so lowering this prefers the deepest profiles.
    #[serde(default = "default_max_accounts")]
    pub max_accounts: usize,

    /// Cap on favourites pages the seed binary fetches per user
    /// (160 posts/page on e621). Lower = faster seed runs, smaller catalog
    /// growth. Higher = richer per-user profiles but seed wall-clock grows
    /// linearly. 8 pages = 1280 favs sits well above `min_favs` and bounds
    /// each user's import to a few seconds of fetch + DB work.
    #[serde(default = "default_max_pages_per_user")]
    pub max_pages_per_user: i32,

    /// `owner_token` written into the device-link table for accounts seeded
    /// by the calibration binary. These accounts have no real device
    /// linked, so they never appear in any user's `/recommendations`
    /// request — they coexist with production accounts safely. Change only
    /// when running multiple isolated seed campaigns against the same DB
    /// (e.g. `"calibration-seed-v2"` for a second sample).
    #[serde(default = "default_seed_owner_token")]
    pub seed_owner_token: String,

    /// Threads in the rayon pool used by `calibrate`'s per-account
    /// parallel scoring. `0` = auto = `nproc / 2`, matching the
    /// `.cargo/config.toml` build-jobs cap so a multi-hour grid leaves
    /// half the box free for other work. Set higher (up to `nproc`) to
    /// finish faster at the cost of saturating the machine.
    #[serde(default = "default_calibrate_threads")]
    pub calibrate_threads: usize,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            min_favs: default_min_favs(),
            test_fraction: default_test_fraction(),
            negative_ratio: default_negative_ratio(),
            top_k_ndcg: default_top_k_ndcg(),
            top_k_recall: default_top_k_recall(),
            max_accounts: default_max_accounts(),
            max_pages_per_user: default_max_pages_per_user(),
            seed_owner_token: default_seed_owner_token(),
            calibrate_threads: default_calibrate_threads(),
        }
    }
}

fn default_min_favs() -> usize {
    100
}
fn default_test_fraction() -> f32 {
    0.20
}
fn default_negative_ratio() -> usize {
    10
}
fn default_top_k_ndcg() -> usize {
    20
}
fn default_top_k_recall() -> usize {
    50
}
fn default_max_accounts() -> usize {
    150
}
fn default_max_pages_per_user() -> i32 {
    8
}
fn default_seed_owner_token() -> String {
    "calibration-seed".to_string()
}
fn default_calibrate_threads() -> usize {
    0
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BucketOverride {
    pub mix_sim: Option<f32>,
    pub mix_quality: Option<f32>,
    pub mix_recency: Option<f32>,
    pub mix_rating: Option<f32>,
    pub mix_media: Option<f32>,
    pub mix_popularity: Option<f32>,
    pub mix_interaction: Option<f32>,
    pub mix_tag_relation: Option<f32>,
}

impl BucketOverride {
    pub fn apply_to(&self, p: &mut Priors) {
        if let Some(v) = self.mix_sim {
            p.mix_sim = v;
        }
        if let Some(v) = self.mix_quality {
            p.mix_quality = v;
        }
        if let Some(v) = self.mix_recency {
            p.mix_recency = v;
        }
        if let Some(v) = self.mix_rating {
            p.mix_rating = v;
        }
        if let Some(v) = self.mix_media {
            p.mix_media = v;
        }
        if let Some(v) = self.mix_popularity {
            p.mix_popularity = v;
        }
        if let Some(v) = self.mix_interaction {
            p.mix_interaction = v;
        }
        if let Some(v) = self.mix_tag_relation {
            p.mix_tag_relation = v;
        }
    }
}

impl Config {
    pub fn pick_bucket(&self, account_id: i32, explicit: Option<&str>) -> (Option<String>, Priors) {
        let mut priors = self.priors.clone();
        if self.buckets.is_empty() {
            return (None, priors);
        }
        let chosen_name = explicit
            .filter(|name| self.buckets.contains_key(*name))
            .map(str::to_owned)
            .or_else(|| {
                let mut keys: Vec<&String> = self.buckets.keys().collect();
                keys.sort();
                if keys.is_empty() {
                    None
                } else {
                    let h = (account_id as i64).unsigned_abs() as usize;
                    Some(keys[h % keys.len()].clone())
                }
            });
        if let Some(name) = &chosen_name {
            if let Some(ovr) = self.buckets.get(name) {
                ovr.apply_to(&mut priors);
            }
        }
        (chosen_name, priors)
    }
}

pub struct ConfigWatcher {
    pub stop: Arc<AtomicBool>,
    pub handle: Option<JoinHandle<()>>,
}
impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub fn load_config(p: &Path) -> anyhow::Result<Config> {
    let s = fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    toml::from_str(&s).context("parsing config.toml")
}

pub fn default_path() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from("config.toml"))
}

pub fn start_config_watcher(path: PathBuf) -> anyhow::Result<ConfigWatcher> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let _ = path; // path is captured by the closure below

    let handle = thread::spawn(move || {
        let mut last_mtime: Option<SystemTime> = file_mtime(&path).ok();

        while !stop_flag.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_secs(2));
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            if let Ok(mtime) = file_mtime(&path) {
                if last_mtime.is_none_or(|old| old < mtime) {
                    thread::sleep(Duration::from_millis(120));

                    match reload_from(&path) {
                        Ok(_) => {
                            last_mtime = Some(mtime);
                            info!("[config] reloaded {}", path.display());
                        }
                        Err(e) => {
                            error!("[config] reload failed: {e:#}");
                        }
                    }
                }
            }
        }
    });

    Ok(ConfigWatcher {
        stop,
        handle: Some(handle),
    })
}

pub fn file_mtime(p: &Path) -> std::io::Result<SystemTime> {
    fs::metadata(p)?.modified()
}

pub static CONFIG: LazyLock<ArcSwap<Config>> = LazyLock::new(|| {
    let p = default_path().expect("config path");
    let cfg = load_config(&p).expect("initial config");
    ArcSwap::from_pointee(cfg)
});

pub fn cfg() -> Arc<Config> {
    CONFIG.load_full()
}

pub fn reload_from(p: &Path) -> anyhow::Result<()> {
    let new = load_config(p)?;
    let arc = Arc::new(new);
    CONFIG.store(arc.clone());
    debug!("[config] current value:\n{arc:#?}");
    Ok(())
}
