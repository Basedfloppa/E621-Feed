use crate::utils::Priors;
use anyhow::Context;
use arc_swap::ArcSwap;
use rocket::serde::Deserialize;
use rocket::serde::json::serde_json;
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
    /// Hard ceiling per individual e621 HTTP attempt, in seconds.
    /// `reqwest`'s built-in `.timeout(30)` does not always fire when
    /// Cloudflare slow-streams the body (rare-byte trickle keeps the
    /// read timer happy), so we wrap each `.send()` in
    /// `tokio::time::timeout(attempt_timeout_secs)` as a non-negotiable
    /// stop. Set generously enough to allow a real slow response
    /// (~10s p99 for /favorites.json with many posts), tight enough
    /// that two failed retries can't burn five minutes. Default 45.
    #[serde(default = "default_attempt_timeout_secs")]
    pub attempt_timeout_secs: u64,

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
    /// Chunk size for the per-account cooccurrence wipe done at the start
    /// of `/process`. The full table holds one row per (tag1, tag2) pair
    /// per account; for active users that's commonly hundreds of
    /// thousands of rows, sometimes millions. A monolithic DELETE pins
    /// the writer mutex and starves every other write for the entire
    /// duration — we've measured 200+ seconds on a 2.6M-row account.
    /// Splitting into batches releases the mutex between chunks and
    /// surfaces visible progress in the logs. Default 50_000.
    #[serde(default = "default_drop_cooc_batch_size")]
    pub drop_cooc_batch_size: usize,

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
    /// touched within this many days are pruned. Belt-and-suspenders bound;
    /// the per-tick orphan prune (`orphan_retention_secs`) catches the
    /// short-term `/recommendations` browse churn far sooner.
    #[serde(default = "default_catalog_retention_days")]
    pub catalog_retention_days: i64,
    /// Aggressive orphan-candidate retention. Posts that aren't in any
    /// user's favs and haven't been re-touched within this many seconds
    /// are dropped on every `cache-pruner` tick. Browse-time posts
    /// pulled in by `/recommendations` go to disk immediately, bloating
    /// the catalog → IDF → tag-relation graph; this knob keeps them out
    /// of memory once the user moves on. Default 1 h.
    #[serde(default = "default_orphan_retention_secs")]
    pub orphan_retention_secs: u64,
    /// `/process` job state retention for Done/Failed entries. Pruned
    /// by `cache-pruner` on its tick; the frontend polls
    /// `/process/<id>/status` only briefly after kicking off a job, so
    /// keeping these for an hour was overkill. Default 10 min.
    #[serde(default = "default_jobs_finished_retain_secs")]
    pub jobs_finished_retain_secs: i64,
    /// Maximum lifetime for a Running `/process` job in seconds. Jobs
    /// stuck in Running state longer than this are evicted by the
    /// cache-pruner (guard against zombie jobs whose tokio task was
    /// cancelled/panicked). Default 24 h.
    #[serde(default = "default_jobs_running_timeout_secs")]
    pub jobs_running_timeout_secs: i64,
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

    /// Cadence for the unified cache-validator background worker. Walks
    /// every in-process cache (e621 outbound TTL cache, /process job
    /// state, ratelimit buckets) and drops entries whose validity
    /// window has expired. Runs in a single dedicated thread; cost is
    /// O(N) over each map. Values < 30s are clamped to 30 to avoid
    /// burning a thread; 0 disables the worker entirely.
    #[serde(default = "default_cache_validate_interval_secs")]
    pub cache_validate_interval_secs: u64,

    /// Idle-eviction window for the two heavy in-memory recommendation
    /// caches: `IDF_CACHE` (per-tag document frequency) and `GLOBAL_CACHE`
    /// (co-occurrence graph). On a cold box these can hold 500 MB–1 GB of
    /// HashMap state extracted from the SQLite catalog. The cache-pruner
    /// worker tracks each cache's last-touch timestamp; if no read or
    /// dirty-mark has happened within this many seconds, the loaded
    /// graph/index is swapped back to empty and the next access lazily
    /// rebuilds (same code path as cold startup → first post-eviction
    /// request runs against empty data while the async rebuild
    /// completes). Set to 0 to keep the caches resident forever.
    #[serde(default = "default_cache_idle_eviction_secs")]
    pub cache_idle_eviction_secs: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            local_candidate_limit: default_local_candidate_limit(),
            dedup_lookback_days: default_dedup_lookback_days(),
            process_fetch_concurrency: default_process_fetch_concurrency(),
            drop_cooc_batch_size: default_drop_cooc_batch_size(),
            idf_rebuild_cooldown_secs: default_rebuild_cooldown_secs(),
            idf_bump_drift_threshold: default_idf_bump_drift_threshold(),
            tag_relation_rebuild_cooldown_secs: default_rebuild_cooldown_secs(),
            prefetch_interval_secs: default_prefetch_interval_secs(),
            cleanup_interval_secs: default_cleanup_interval_secs(),
            catalog_retention_days: default_catalog_retention_days(),
            orphan_retention_secs: default_orphan_retention_secs(),
            jobs_finished_retain_secs: default_jobs_finished_retain_secs(),
            jobs_running_timeout_secs: default_jobs_running_timeout_secs(),
            prefetch_active_window_days: default_prefetch_active_window_days(),
            prefetch_breaker_threshold: default_prefetch_breaker_threshold(),
            prefetch_breaker_pause_secs: default_prefetch_breaker_pause_secs(),
            cache_validate_interval_secs: default_cache_validate_interval_secs(),
            cache_idle_eviction_secs: default_cache_idle_eviction_secs(),
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
fn default_attempt_timeout_secs() -> u64 {
    45
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
fn default_drop_cooc_batch_size() -> usize {
    50_000
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
fn default_orphan_retention_secs() -> u64 {
    3600
}
fn default_jobs_finished_retain_secs() -> i64 {
    600
}
fn default_jobs_running_timeout_secs() -> i64 {
    86400 // 24 h
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
fn default_cache_validate_interval_secs() -> u64 {
    300 // 5 min
}
fn default_cache_idle_eviction_secs() -> u64 {
    1800 // 30 min — idle-evict IDF + tag-relation graph
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
    /// Arbitrary `[priors.*]` overrides as a JSON object. Merged after the
    /// legacy mix fields so individual mix overrides take precedence. Every
    /// value must be a valid JSON type matching the target Priors field.
    /// Example in `config.toml`:
    /// ```toml
    /// [buckets.control]
    /// priors = { group_w_artist = 2.0, diversity_max_penalty = 0.3 }
    /// ```
    #[serde(default, deserialize_with = "deserialize_priors_json")]
    pub priors: Option<serde_json::Value>,
}

/// Deserialize `priors` from a TOML inline table or JSON object. TOML's
/// `{ key = value }` is valid JSON after key-quoting, and serde_json can
/// parse inline table values produced by `toml`.
fn deserialize_priors_json<'de, D>(d: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // toml::Value::Table can be deserialized to serde_json::Value via
    // the serde data model — no manual conversion needed.
    let opt: Option<serde_json::Value> = Option::deserialize(d)?;
    Ok(opt)
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
        // Apply generic priors overrides via JSON merge.
        if let Some(json) = &self.priors {
            if let Ok(overrides) = serde_json::from_value::<Priors>(json.clone()) {
                merge_priors(p, &overrides);
            }
        }
    }
}

/// Merge non-default values from `overrides` into `base`. The Priors default
/// for every numeric field is either 0.0 or a sentinel like f32::NAN; we
/// overwrite base fields when the override carries a non-default value.
/// This avoids requiring every field to be `Option<T>` in Priors itself.
fn merge_priors(base: &mut Priors, overrides: &Priors) {
    // --- mix weights ---
    if overrides.mix_sim != 0.0 { base.mix_sim = overrides.mix_sim; }
    if overrides.mix_quality != 0.0 { base.mix_quality = overrides.mix_quality; }
    if overrides.mix_recency != 0.0 { base.mix_recency = overrides.mix_recency; }
    if overrides.mix_rating != 0.0 { base.mix_rating = overrides.mix_rating; }
    if overrides.mix_media != 0.0 { base.mix_media = overrides.mix_media; }
    if overrides.mix_popularity != 0.0 { base.mix_popularity = overrides.mix_popularity; }
    if overrides.mix_interaction != 0.0 { base.mix_interaction = overrides.mix_interaction; }
    if overrides.mix_tag_relation != 0.08 { base.mix_tag_relation = overrides.mix_tag_relation; }
    if overrides.mix_uploader != 0.05 { base.mix_uploader = overrides.mix_uploader; }
    if overrides.mix_exclusivity != 0.0 { base.mix_exclusivity = overrides.mix_exclusivity; }
    if overrides.mix_novelty != 0.0 { base.mix_novelty = overrides.mix_novelty; }

    // --- IDF / freq ---
    if overrides.idf_lambda != 1.0 { base.idf_lambda = overrides.idf_lambda; }
    if overrides.idf_alpha != 1.0 { base.idf_alpha = overrides.idf_alpha; }
    if overrides.freq_alpha != 0.95 { base.freq_alpha = overrides.freq_alpha; }
    if overrides.df_floor != 0.4 { base.df_floor = overrides.df_floor; }
    if overrides.idf_max != 100.0 { base.idf_max = overrides.idf_max; }
    if overrides.bm25_k != 2.25 { base.bm25_k = overrides.bm25_k; }
    if overrides.idf_rsj_smoothing != 0.35 { base.idf_rsj_smoothing = overrides.idf_rsj_smoothing; }
    if overrides.one_sided_ratio_exp != 0.5 { base.one_sided_ratio_exp = overrides.one_sided_ratio_exp; }
    if !overrides.idf_lambda_meta.is_nan() { base.idf_lambda_meta = overrides.idf_lambda_meta; }

    // --- quality channel ---
    if overrides.quality_a != 0.5 { base.quality_a = overrides.quality_a; }
    if overrides.quality_b != 0.2 { base.quality_b = overrides.quality_b; }
    if overrides.quality_log_bias != -3.0 { base.quality_log_bias = overrides.quality_log_bias; }
    if overrides.quality_w_absolute != 0.55 { base.quality_w_absolute = overrides.quality_w_absolute; }
    if overrides.quality_w_relative_score != 0.3 { base.quality_w_relative_score = overrides.quality_w_relative_score; }
    if overrides.quality_w_relative_comments != 0.15 { base.quality_w_relative_comments = overrides.quality_w_relative_comments; }
    if overrides.quality_c != 0.3 { base.quality_c = overrides.quality_c; }

    // --- recency channel ---
    if overrides.recency_tau_days != 10.0 { base.recency_tau_days = overrides.recency_tau_days; }
    if overrides.recency_w_global != 0.4 { base.recency_w_global = overrides.recency_w_global; }
    if overrides.recency_w_personal != 0.6 { base.recency_w_personal = overrides.recency_w_personal; }
    if overrides.recency_personal_floor_frac != 1.0 { base.recency_personal_floor_frac = overrides.recency_personal_floor_frac; }
    if overrides.recency_log_personal != true { base.recency_log_personal = overrides.recency_log_personal; }
    if !overrides.recency_tau_hot.is_nan() { base.recency_tau_hot = overrides.recency_tau_hot; }
    if !overrides.recency_tau_recent.is_nan() { base.recency_tau_recent = overrides.recency_tau_recent; }
    if overrides.recency_split_age_hours != 24.0 { base.recency_split_age_hours = overrides.recency_split_age_hours; }
    if overrides.recency_split_age_days != 30.0 { base.recency_split_age_days = overrides.recency_split_age_days; }

    // --- popularity channel ---
    if overrides.popularity_w_fav != 0.8 { base.popularity_w_fav = overrides.popularity_w_fav; }
    if overrides.popularity_w_duration != 0.2 { base.popularity_w_duration = overrides.popularity_w_duration; }

    // --- tag relation channel ---
    if overrides.tag_relation_w_global != 0.4 { base.tag_relation_w_global = overrides.tag_relation_w_global; }
    if overrides.tag_relation_w_personal != 0.6 { base.tag_relation_w_personal = overrides.tag_relation_w_personal; }
    if overrides.tag_relation_pmi_scale != 3.5 { base.tag_relation_pmi_scale = overrides.tag_relation_pmi_scale; }
    if overrides.tag_relation_min_cooc != 2 { base.tag_relation_min_cooc = overrides.tag_relation_min_cooc; }
    if overrides.tag_relation_user_min_cooc != 1 { base.tag_relation_user_min_cooc = overrides.tag_relation_user_min_cooc; }
    if overrides.tag_relation_cooc_ref != 16.0 { base.tag_relation_cooc_ref = overrides.tag_relation_cooc_ref; }
    if overrides.tag_relation_user_cooc_ref != 5.0 { base.tag_relation_user_cooc_ref = overrides.tag_relation_user_cooc_ref; }
    if !overrides.tag_relation_pmi_scale_user.is_nan() { base.tag_relation_pmi_scale_user = overrides.tag_relation_pmi_scale_user; }
    if overrides.tag_relation_pair_aggregator != "mean" { base.tag_relation_pair_aggregator.clone_from(&overrides.tag_relation_pair_aggregator); }
    if overrides.tag_relation_max_tags != 20 { base.tag_relation_max_tags = overrides.tag_relation_max_tags; }

    // --- group weights ---
    if overrides.group_w_artist != 2.4 { base.group_w_artist = overrides.group_w_artist; }
    if overrides.group_w_character != 2.0 { base.group_w_character = overrides.group_w_character; }
    if overrides.group_w_copyright != 1.45 { base.group_w_copyright = overrides.group_w_copyright; }
    if overrides.group_w_species != 1.3 { base.group_w_species = overrides.group_w_species; }
    if overrides.group_w_general != 0.7 { base.group_w_general = overrides.group_w_general; }
    if overrides.group_w_lore != 0.4 { base.group_w_lore = overrides.group_w_lore; }

    // --- interaction / feedback ---
    if overrides.interaction_ctr_prior_alpha != 4.0 { base.interaction_ctr_prior_alpha = overrides.interaction_ctr_prior_alpha; }
    if overrides.meta_interaction_weight != 0.3 { base.meta_interaction_weight = overrides.meta_interaction_weight; }
    if overrides.feedback_decay_half_life_days != 90.0 { base.feedback_decay_half_life_days = overrides.feedback_decay_half_life_days; }
    if overrides.strong_negative_count != 3 { base.strong_negative_count = overrides.strong_negative_count; }
    if overrides.strong_negative_penalty != 0.4 { base.strong_negative_penalty = overrides.strong_negative_penalty; }
    if overrides.strong_negative_wilson_threshold != 0.55 { base.strong_negative_wilson_threshold = overrides.strong_negative_wilson_threshold; }

    // --- cold-start / smoothing ---
    if overrides.discrete_smoothing_alpha != 1.0 { base.discrete_smoothing_alpha = overrides.discrete_smoothing_alpha; }
    if overrides.discrete_pref_floor != 0.05 { base.discrete_pref_floor = overrides.discrete_pref_floor; }
    if overrides.coldstart_smoothing_boost != 2.0 { base.coldstart_smoothing_boost = overrides.coldstart_smoothing_boost; }
    if overrides.coldstart_n0 != 25.0 { base.coldstart_n0 = overrides.coldstart_n0; }

    // --- diversity / MMR ---
    if overrides.diversity_window != 32 { base.diversity_window = overrides.diversity_window; }
    if overrides.diversity_w_artist != 0.22 { base.diversity_w_artist = overrides.diversity_w_artist; }
    if overrides.diversity_w_character != 0.16 { base.diversity_w_character = overrides.diversity_w_character; }
    if overrides.diversity_w_copyright != 1.8 { base.diversity_w_copyright = overrides.diversity_w_copyright; }
    if overrides.diversity_w_species != 1.5 { base.diversity_w_species = overrides.diversity_w_species; }
    if overrides.diversity_w_general != 0.08 { base.diversity_w_general = overrides.diversity_w_general; }
    if overrides.diversity_max_penalty != 0.45 { base.diversity_max_penalty = overrides.diversity_max_penalty; }
    if overrides.diversity_interaction_damp != 0.35 { base.diversity_interaction_damp = overrides.diversity_interaction_damp; }
    // --- v5.11 Class J: diversity semantic similarity ---
    if overrides.diversity_semantic_blend != 0.0 { base.diversity_semantic_blend = overrides.diversity_semantic_blend; }
    if overrides.diversity_pmi_threshold != 0.0 { base.diversity_pmi_threshold = overrides.diversity_pmi_threshold; }
    if overrides.diversity_semantic_max_tags != 10 { base.diversity_semantic_max_tags = overrides.diversity_semantic_max_tags; }

    // --- uploader channel ---
    if overrides.uploader_n0 != 5.0 { base.uploader_n0 = overrides.uploader_n0; }
    if overrides.uploader_w_avg_score != 0.6 { base.uploader_w_avg_score = overrides.uploader_w_avg_score; }
    if overrides.uploader_w_avg_fav != 0.4 { base.uploader_w_avg_fav = overrides.uploader_w_avg_fav; }

    // --- exclusivity channel ---
    if overrides.min_exclusivity_cooc != 2 { base.min_exclusivity_cooc = overrides.min_exclusivity_cooc; }
    if overrides.exclusivity_scale != 0.5 { base.exclusivity_scale = overrides.exclusivity_scale; }
    if overrides.exclusivity_max_tags != 15 { base.exclusivity_max_tags = overrides.exclusivity_max_tags; }

    // --- novelty channel ---
    if overrides.novelty_n0 != 3.0 { base.novelty_n0 = overrides.novelty_n0; }
    if overrides.novelty_use_feedback != true { base.novelty_use_feedback = overrides.novelty_use_feedback; }

    // --- algorithmic shape ---
    if overrides.score_temperature != 0.0 { base.score_temperature = overrides.score_temperature; }
    if overrides.confidence_steepness != 1.0 { base.confidence_steepness = overrides.confidence_steepness; }
    if overrides.mmr_redundancy_exp != 1.0 { base.mmr_redundancy_exp = overrides.mmr_redundancy_exp; }
    if overrides.tag_sim_jaccard_blend != 0.0 { base.tag_sim_jaccard_blend = overrides.tag_sim_jaccard_blend; }

    // --- exploration ---
    if overrides.exploration_epsilon != 0.0 { base.exploration_epsilon = overrides.exploration_epsilon; }
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
        if let Some(name) = &chosen_name
            && let Some(ovr) = self.buckets.get(name) {
                ovr.apply_to(&mut priors);
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
            if let Ok(mtime) = file_mtime(&path)
                && last_mtime.is_none_or(|old| old < mtime) {
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
