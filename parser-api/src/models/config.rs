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
    /// Secret used to derive the AES-256-GCM key that encrypts per-account
    /// e621 API keys at rest (`crypto::encryption_key`). Leave empty only in
    /// local/dev — a fixed fallback salt is then used (still AES-GCM, but not
    /// secret against someone with the binary). Set a strong value in
    /// production `config.toml`.
    #[serde(default)]
    pub e621_key_encryption_secret: String,
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
    /// Set to `true` only when a trusted reverse proxy (nginx/Caddy) sits in
    /// front and rewrites `X-Forwarded-For`. When `false` (the default — direct
    /// bind, e.g. the shipped docker-compose with host networking and
    /// `ROCKET_PORT=8181`), the raw socket
    /// peer IP is used for rate-limit keying so a remote client cannot forge
    /// `X-Forwarded-For` to rotate per-IP buckets:
    ///     X-Forwarded-For: 1.2.3.4
    #[serde(default)]
    pub trust_proxy: bool,
    /// Hard ceiling per individual e621 HTTP attempt, in seconds.
    /// `reqwest`'s built-in `.timeout(30)` does not always fire when
    /// Cloudflare slow-streams the body (rare-byte trickle keeps the
    /// read timer happy), so we wrap each `.send()` in
    /// `tokio::time::timeout(attempt_timeout_secs)` as a non-negotiable
    /// stop. Set generously enough to allow a real slow response
    /// (~10s p99 for /favorites.json with many posts), tight enough
    /// that two failed retries can't burn five minutes. Default 30.
    #[serde(default = "default_attempt_timeout_secs")]
    pub attempt_timeout_secs: u64,

    #[serde(default = "default_user_agent")]
    pub user_agent: String,

    /// Path to the `SQLite` database file. Relative to the working directory
    /// unless an absolute path is given. Default `"database.db"`.
    #[serde(default = "default_db_path")]
    pub db_path: String,

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

    /// Local catalog & offline-serving knobs (TODO §4.1 / docs/offline-catalog.md).
    /// All fields default off so existing deployments behave exactly as today.
    #[serde(default)]
    pub catalog: CatalogConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CatalogConfig {
    /// Mode A: persist favourited posts into the local catalog when syncing
    /// favourites (`/process`, direct sync). Default `false`.
    #[serde(default = "default_catalog_save_favourites")]
    pub save_favourites: bool,

    /// Hard cap on the on-disk media folder size (bytes). `0` = unlimited.
    /// Past this, the oldest originals (by `mtime`) are LRU-evicted. The
    /// folder itself is hardcoded to `media/` (see `media_store`).
    #[serde(default = "default_catalog_media_cache_max_bytes")]
    pub media_cache_max_bytes: u64,

    /// Organize on-disk originals into per-tag folders instead of the numeric
    /// Persist pool membership locally so `get_pool_posts` works offline.
    #[serde(default = "default_catalog_pool_membership")]
    pub pool_membership: bool,

    /// Save **every post the owner encounters** (feed recommendations, browse
    /// search/trending/favorites) into the owner's catalog (`accounts_post`),
    /// so each one is grouped and its original media is queued for download by
    /// the in-server media worker. Off by default — it can grow the catalog
    /// and media cache a lot. `save_all` is independent from `save_favourites`.
    #[serde(default)]
    pub save_all: bool,
}

fn default_catalog_save_favourites() -> bool {
    false
}
fn default_catalog_media_cache_max_bytes() -> u64 {
    0
}
fn default_catalog_pool_membership() -> bool {
    false
}

impl CatalogConfig {
    /// The local catalog — and everything that feeds it (favourites sync,
    /// `/process`, the background media worker) — is active when at least one
    /// persistence toggle is on. With both off, nothing is persisted locally.
    pub fn persistence_enabled(&self) -> bool {
        self.save_favourites || self.save_all
    }
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
    /// Cap on the number of account co-occurrence pairs materialized into the
    /// MMR user-relation graph by `load_account_tag_relation`. Only the
    /// strongest `user_relation_edge_limit` pairs (by `cooc_count`) are loaded,
    /// mirroring `get_account_tag_relation_graph`. Bounds both the SQLite scan/
    /// sort and the per-row `insert_pair` graph build for very large accounts
    /// (TODO §2.2b). Default `250_000`.
    #[serde(default = "default_user_relation_edge_limit")]
    pub user_relation_edge_limit: usize,
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
    /// surfaces visible progress in the logs. Default `50_000`.
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
    /// Cooldown after a tag-relation rebuild (analogous to `idf_rebuild_cooldown`).
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
    /// Number of top tags to fetch per group (artist, character). Default 10
    /// (broad catalog). Lower values (e.g. 1) restrict to a narrow catalog.
    #[serde(default = "default_prefetch_tags_per_group")]
    pub prefetch_tags_per_group: usize,
    /// Whether to also fetch a "recent popular" stream (latest posts above
    /// the user's avg fav count) as a third prefetch bucket. Default true.
    /// When enabled the worker picks one additional top tag for artist/
    /// character and fetches by popularity.
    #[serde(default = "default_prefetch_include_recent_popular")]
    pub prefetch_include_recent_popular: bool,
    /// Per-user prefetch cooldown: only refetch an account's top tags if
    /// that account hasn't been prefetched within this many seconds. Default
    /// 86400 (24 h). Prevents the worker from hammering the same artist/
    /// character every tick.
    #[serde(default = "default_prefetch_cooldown_secs")]
    pub prefetch_cooldown_secs: u64,
    /// Accounts with a feed interaction within this many hours are served
    /// by the **hot** prefetch worker (short interval). Accounts outside
    /// this window but still within `prefetch_active_window_days` are
    /// served by the **cold** worker (long interval). Default 48 h.
    #[serde(default = "default_prefetch_hot_window_hours")]
    pub prefetch_hot_window_hours: u64,
    /// Interval for the cold prefetch worker (dormant accounts). Default
    /// 900 s = 15 min. The hot worker uses `prefetch_interval_secs`.
    #[serde(default = "default_prefetch_cold_interval_secs")]
    pub prefetch_cold_interval_secs: u64,

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
    /// `HashMap` state extracted from the `SQLite` catalog. The cache-pruner
    /// worker tracks each cache's last-touch timestamp; if no read or
    /// dirty-mark has happened within this many seconds, the loaded
    /// graph/index is swapped back to empty and the next access lazily
    /// rebuilds (same code path as cold startup → first post-eviction
    /// request runs against empty data while the async rebuild
    /// completes). Set to 0 to keep the caches resident forever.
    #[serde(default = "default_cache_idle_eviction_secs")]
    pub cache_idle_eviction_secs: u64,

    /// Interval (in seconds) for the background `tag_aliases` / `tag_implications`
    /// import worker. Fetches all pages on first run, then incremental page 1
    /// on subsequent ticks. 0 disables the worker entirely.
    #[serde(default = "default_tag_alias_import_interval_secs")]
    pub tag_alias_import_interval_secs: u64,

    // ── Adaptive rate gate ────────────────────────────────────────────
    /// Base delay for live (user-facing) e621 requests, in ms.
    /// Default 250 (4 RPS).
    #[serde(default = "default_live_rps_delay_ms")]
    pub live_rps_delay_ms: u64,

    /// Base delay for prefetch (hot/cold) e621 requests, in ms.
    /// Default 500 (2 RPS).
    #[serde(default = "default_prefetch_rps_delay_ms")]
    pub prefetch_rps_delay_ms: u64,

    /// Base delay for backfill e621 requests, in ms.
    /// Default 750 (~1.3 RPS).
    #[serde(default = "default_backfill_rps_delay_ms")]
    pub backfill_rps_delay_ms: u64,

    /// Backfill worker checks that no live request passed within this
    /// many ms before sending. If a live request was recently processed,
    /// the backfill adds extra delay proportional to recency.
    /// Default 2000 (2 s).
    #[serde(default = "default_backfill_live_window_ms")]
    pub backfill_live_window_ms: u64,

    /// Interval for the backfill worker (full retro-post scan).
    /// Default 21600 (6 h).
    #[serde(default = "default_backfill_interval_secs")]
    pub backfill_interval_secs: u64,

    /// Per-account backfill cooldown: don't re-backfill an account's
    /// tags more often than this many seconds. Default 86400 (24 h).
    #[serde(default = "default_backfill_cooldown_secs")]
    pub backfill_cooldown_secs: u64,

    /// Circuit-breaker threshold for the backfill worker. After this
    /// many consecutive e621 fetch failures, the backfill pauses for
    /// `prefetch_breaker_pause_secs`. Default 10.
    #[serde(default = "default_backfill_breaker_threshold")]
    pub backfill_breaker_threshold: u32,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            local_candidate_limit: default_local_candidate_limit(),
            dedup_lookback_days: default_dedup_lookback_days(),
            user_relation_edge_limit: default_user_relation_edge_limit(),
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
            prefetch_tags_per_group: default_prefetch_tags_per_group(),
            prefetch_include_recent_popular: default_prefetch_include_recent_popular(),
            prefetch_cooldown_secs: default_prefetch_cooldown_secs(),
            prefetch_hot_window_hours: default_prefetch_hot_window_hours(),
            prefetch_cold_interval_secs: default_prefetch_cold_interval_secs(),
            cache_validate_interval_secs: default_cache_validate_interval_secs(),
            cache_idle_eviction_secs: default_cache_idle_eviction_secs(),
            tag_alias_import_interval_secs: default_tag_alias_import_interval_secs(),
            live_rps_delay_ms: default_live_rps_delay_ms(),
            prefetch_rps_delay_ms: default_prefetch_rps_delay_ms(),
            backfill_rps_delay_ms: default_backfill_rps_delay_ms(),
            backfill_live_window_ms: default_backfill_live_window_ms(),
            backfill_interval_secs: default_backfill_interval_secs(),
            backfill_cooldown_secs: default_backfill_cooldown_secs(),
            backfill_breaker_threshold: default_backfill_breaker_threshold(),
        }
    }
}

fn default_tag_alias_import_interval_secs() -> u64 {
    86400 // 24 h — daily sync of tag aliases / implications
}

fn default_user_agent() -> String {
    "E621AccountParser/0.1 (+https://github.com/Basedfloppa/E621-Feed)".to_string()
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
    30
}
fn default_db_path() -> String {
    "database.db".to_string()
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
fn default_user_relation_edge_limit() -> usize {
    250_000
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
fn default_prefetch_tags_per_group() -> usize {
    10
}
fn default_prefetch_include_recent_popular() -> bool {
    true
}
fn default_prefetch_cooldown_secs() -> u64 {
    86400
}
fn default_prefetch_hot_window_hours() -> u64 {
    48
}
fn default_prefetch_cold_interval_secs() -> u64 {
    900
}
fn default_cache_validate_interval_secs() -> u64 {
    300 // 5 min
}
fn default_cache_idle_eviction_secs() -> u64 {
    1800 // 30 min — idle-evict IDF + tag-relation graph
}

fn default_live_rps_delay_ms() -> u64 {
    250
}
fn default_prefetch_rps_delay_ms() -> u64 {
    500
}
fn default_backfill_rps_delay_ms() -> u64 {
    750
}
fn default_backfill_live_window_ms() -> u64 {
    2000
}
fn default_backfill_interval_secs() -> u64 {
    21600 // 6 h
}
fn default_backfill_cooldown_secs() -> u64 {
    86400 // 24 h
}
fn default_backfill_breaker_threshold() -> u32 {
    10
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
/// `{ key = value }` is valid JSON after key-quoting, and `serde_json` can
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
        if let Some(json) = &self.priors
            && let Ok(overrides) = serde_json::from_value::<Priors>(json.clone())
        {
            merge_priors(p, &overrides);
        }
    }
}

/// Merge non-default values from `overrides` into `base`. The Priors default
/// for every numeric field is either 0.0 or a sentinel like `f32::NAN`; we
/// overwrite base fields when the override carries a non-default value.
/// This avoids requiring every field to be `Option<T>` in Priors itself.
fn merge_priors(base: &mut Priors, overrides: &Priors) {
    // --- mix weights ---
    if overrides.mix_sim != 0.0 {
        base.mix_sim = overrides.mix_sim;
    }
    if overrides.mix_quality != 0.0 {
        base.mix_quality = overrides.mix_quality;
    }
    if overrides.mix_recency != 0.0 {
        base.mix_recency = overrides.mix_recency;
    }
    if overrides.mix_rating != 0.0 {
        base.mix_rating = overrides.mix_rating;
    }
    if overrides.mix_media != 0.0 {
        base.mix_media = overrides.mix_media;
    }
    if overrides.mix_popularity != 0.0 {
        base.mix_popularity = overrides.mix_popularity;
    }
    if overrides.mix_interaction != 0.0 {
        base.mix_interaction = overrides.mix_interaction;
    }
    if overrides.mix_tag_relation != 0.08 {
        base.mix_tag_relation = overrides.mix_tag_relation;
    }
    if overrides.mix_uploader != 0.05 {
        base.mix_uploader = overrides.mix_uploader;
    }
    if overrides.mix_exclusivity != 0.0 {
        base.mix_exclusivity = overrides.mix_exclusivity;
    }
    if overrides.mix_novelty != 0.0 {
        base.mix_novelty = overrides.mix_novelty;
    }

    // --- IDF / freq ---
    if overrides.idf_lambda != 1.0 {
        base.idf_lambda = overrides.idf_lambda;
    }
    if overrides.idf_alpha != 1.0 {
        base.idf_alpha = overrides.idf_alpha;
    }
    if overrides.freq_alpha != 0.95 {
        base.freq_alpha = overrides.freq_alpha;
    }
    if overrides.df_floor != 0.4 {
        base.df_floor = overrides.df_floor;
    }
    if overrides.idf_max != 100.0 {
        base.idf_max = overrides.idf_max;
    }
    if overrides.bm25_k != 2.25 {
        base.bm25_k = overrides.bm25_k;
    }
    if overrides.idf_rsj_smoothing != 0.35 {
        base.idf_rsj_smoothing = overrides.idf_rsj_smoothing;
    }
    if overrides.one_sided_ratio_exp != 0.5 {
        base.one_sided_ratio_exp = overrides.one_sided_ratio_exp;
    }
    if !overrides.idf_lambda_meta.is_nan() {
        base.idf_lambda_meta = overrides.idf_lambda_meta;
    }

    // --- quality channel ---
    if overrides.quality_a != 0.5 {
        base.quality_a = overrides.quality_a;
    }
    if overrides.quality_b != 0.2 {
        base.quality_b = overrides.quality_b;
    }
    if overrides.quality_log_bias != -3.0 {
        base.quality_log_bias = overrides.quality_log_bias;
    }
    if overrides.quality_w_absolute != 0.55 {
        base.quality_w_absolute = overrides.quality_w_absolute;
    }
    if overrides.quality_w_relative_score != 0.3 {
        base.quality_w_relative_score = overrides.quality_w_relative_score;
    }
    if overrides.quality_w_relative_comments != 0.15 {
        base.quality_w_relative_comments = overrides.quality_w_relative_comments;
    }
    if overrides.quality_c != 0.3 {
        base.quality_c = overrides.quality_c;
    }

    // --- recency channel ---
    if overrides.recency_tau_days != 10.0 {
        base.recency_tau_days = overrides.recency_tau_days;
    }
    if overrides.recency_w_global != 0.4 {
        base.recency_w_global = overrides.recency_w_global;
    }
    if overrides.recency_w_personal != 0.6 {
        base.recency_w_personal = overrides.recency_w_personal;
    }
    if overrides.recency_personal_floor_frac != 1.0 {
        base.recency_personal_floor_frac = overrides.recency_personal_floor_frac;
    }
    if !overrides.recency_log_personal {
        base.recency_log_personal = overrides.recency_log_personal;
    }
    if !overrides.recency_tau_hot.is_nan() {
        base.recency_tau_hot = overrides.recency_tau_hot;
    }
    if !overrides.recency_tau_recent.is_nan() {
        base.recency_tau_recent = overrides.recency_tau_recent;
    }
    if overrides.recency_split_age_hours != 24.0 {
        base.recency_split_age_hours = overrides.recency_split_age_hours;
    }
    if overrides.recency_split_age_days != 30.0 {
        base.recency_split_age_days = overrides.recency_split_age_days;
    }

    // --- popularity channel ---
    if overrides.popularity_w_fav != 0.8 {
        base.popularity_w_fav = overrides.popularity_w_fav;
    }
    if overrides.popularity_w_duration != 0.2 {
        base.popularity_w_duration = overrides.popularity_w_duration;
    }

    // --- tag relation channel ---
    if overrides.tag_relation_w_global != 0.4 {
        base.tag_relation_w_global = overrides.tag_relation_w_global;
    }
    if overrides.tag_relation_w_personal != 0.6 {
        base.tag_relation_w_personal = overrides.tag_relation_w_personal;
    }
    if overrides.tag_relation_pmi_scale != 3.5 {
        base.tag_relation_pmi_scale = overrides.tag_relation_pmi_scale;
    }
    if overrides.tag_relation_min_cooc != 2 {
        base.tag_relation_min_cooc = overrides.tag_relation_min_cooc;
    }
    if overrides.tag_relation_user_min_cooc != 1 {
        base.tag_relation_user_min_cooc = overrides.tag_relation_user_min_cooc;
    }
    if overrides.tag_relation_cooc_ref != 16.0 {
        base.tag_relation_cooc_ref = overrides.tag_relation_cooc_ref;
    }
    if overrides.tag_relation_user_cooc_ref != 5.0 {
        base.tag_relation_user_cooc_ref = overrides.tag_relation_user_cooc_ref;
    }
    if !overrides.tag_relation_pmi_scale_user.is_nan() {
        base.tag_relation_pmi_scale_user = overrides.tag_relation_pmi_scale_user;
    }
    if overrides.tag_relation_pair_aggregator != "mean" {
        base.tag_relation_pair_aggregator
            .clone_from(&overrides.tag_relation_pair_aggregator);
    }
    if overrides.tag_relation_max_tags != 20 {
        base.tag_relation_max_tags = overrides.tag_relation_max_tags;
    }

    // --- group weights ---
    if overrides.group_w_artist != 2.4 {
        base.group_w_artist = overrides.group_w_artist;
    }
    if overrides.group_w_character != 2.0 {
        base.group_w_character = overrides.group_w_character;
    }
    if overrides.group_w_copyright != 1.45 {
        base.group_w_copyright = overrides.group_w_copyright;
    }
    if overrides.group_w_species != 1.3 {
        base.group_w_species = overrides.group_w_species;
    }
    if overrides.group_w_general != 0.7 {
        base.group_w_general = overrides.group_w_general;
    }
    if overrides.group_w_lore != 0.4 {
        base.group_w_lore = overrides.group_w_lore;
    }

    // --- interaction / feedback ---
    if overrides.interaction_ctr_prior_alpha != 4.0 {
        base.interaction_ctr_prior_alpha = overrides.interaction_ctr_prior_alpha;
    }
    if overrides.meta_interaction_weight != 0.3 {
        base.meta_interaction_weight = overrides.meta_interaction_weight;
    }
    if overrides.feedback_decay_half_life_days != 90.0 {
        base.feedback_decay_half_life_days = overrides.feedback_decay_half_life_days;
    }
    if overrides.strong_negative_count != 3 {
        base.strong_negative_count = overrides.strong_negative_count;
    }
    if overrides.strong_negative_penalty != 0.4 {
        base.strong_negative_penalty = overrides.strong_negative_penalty;
    }
    if overrides.strong_negative_wilson_threshold != 0.55 {
        base.strong_negative_wilson_threshold = overrides.strong_negative_wilson_threshold;
    }

    // --- cold-start / smoothing ---
    if overrides.discrete_smoothing_alpha != 1.0 {
        base.discrete_smoothing_alpha = overrides.discrete_smoothing_alpha;
    }
    if overrides.discrete_pref_floor != 0.05 {
        base.discrete_pref_floor = overrides.discrete_pref_floor;
    }
    if overrides.coldstart_smoothing_boost != 2.0 {
        base.coldstart_smoothing_boost = overrides.coldstart_smoothing_boost;
    }
    if overrides.coldstart_n0 != 25.0 {
        base.coldstart_n0 = overrides.coldstart_n0;
    }

    // --- diversity / MMR ---
    if overrides.diversity_window != 32 {
        base.diversity_window = overrides.diversity_window;
    }
    if overrides.diversity_w_artist != 0.22 {
        base.diversity_w_artist = overrides.diversity_w_artist;
    }
    if overrides.diversity_w_character != 0.16 {
        base.diversity_w_character = overrides.diversity_w_character;
    }
    if overrides.diversity_w_copyright != 1.8 {
        base.diversity_w_copyright = overrides.diversity_w_copyright;
    }
    if overrides.diversity_w_species != 1.5 {
        base.diversity_w_species = overrides.diversity_w_species;
    }
    if overrides.diversity_w_general != 0.08 {
        base.diversity_w_general = overrides.diversity_w_general;
    }
    if overrides.diversity_max_penalty != 0.45 {
        base.diversity_max_penalty = overrides.diversity_max_penalty;
    }
    if overrides.diversity_interaction_damp != 0.35 {
        base.diversity_interaction_damp = overrides.diversity_interaction_damp;
    }
    // --- v5.11 Class J: diversity semantic similarity ---
    if overrides.diversity_semantic_blend != 0.0 {
        base.diversity_semantic_blend = overrides.diversity_semantic_blend;
    }
    if overrides.diversity_pmi_threshold != 0.0 {
        base.diversity_pmi_threshold = overrides.diversity_pmi_threshold;
    }
    if overrides.diversity_semantic_max_tags != 10 {
        base.diversity_semantic_max_tags = overrides.diversity_semantic_max_tags;
    }

    // --- uploader channel ---
    if overrides.uploader_n0 != 5.0 {
        base.uploader_n0 = overrides.uploader_n0;
    }
    if overrides.uploader_w_avg_score != 0.6 {
        base.uploader_w_avg_score = overrides.uploader_w_avg_score;
    }
    if overrides.uploader_w_avg_fav != 0.4 {
        base.uploader_w_avg_fav = overrides.uploader_w_avg_fav;
    }

    // --- exclusivity channel ---
    if overrides.min_exclusivity_cooc != 2 {
        base.min_exclusivity_cooc = overrides.min_exclusivity_cooc;
    }
    if overrides.exclusivity_scale != 0.5 {
        base.exclusivity_scale = overrides.exclusivity_scale;
    }
    if overrides.exclusivity_max_tags != 15 {
        base.exclusivity_max_tags = overrides.exclusivity_max_tags;
    }

    // --- novelty channel ---
    if overrides.novelty_n0 != 3.0 {
        base.novelty_n0 = overrides.novelty_n0;
    }
    if !overrides.novelty_use_feedback {
        base.novelty_use_feedback = overrides.novelty_use_feedback;
    }

    // --- artist discovery channel ---
    if overrides.mix_artist_discovery != 0.0 {
        base.mix_artist_discovery = overrides.mix_artist_discovery;
    }
    if overrides.artist_discovery_n0 != 3.0 {
        base.artist_discovery_n0 = overrides.artist_discovery_n0;
    }
    if overrides.artist_discovery_novelty_bonus != 0.2 {
        base.artist_discovery_novelty_bonus = overrides.artist_discovery_novelty_bonus;
    }

    // --- algorithmic shape ---
    if overrides.score_temperature != 0.0 {
        base.score_temperature = overrides.score_temperature;
    }
    if overrides.confidence_steepness != 1.0 {
        base.confidence_steepness = overrides.confidence_steepness;
    }
    if overrides.mmr_redundancy_exp != 1.0 {
        base.mmr_redundancy_exp = overrides.mmr_redundancy_exp;
    }
    if overrides.tag_sim_jaccard_blend != 0.0 {
        base.tag_sim_jaccard_blend = overrides.tag_sim_jaccard_blend;
    }

    // --- exploration ---
    if overrides.exploration_epsilon != 0.0 {
        base.exploration_epsilon = overrides.exploration_epsilon;
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
                    let h = i64::from(account_id).wrapping_abs() as usize;
                    Some(keys[h % keys.len()].clone())
                }
            });
        if let Some(name) = &chosen_name
            && let Some(ovr) = self.buckets.get(name)
        {
            ovr.apply_to(&mut priors);
        }
        (chosen_name, priors)
    }
}

pub struct ConfigWatcher {
    pub stop: Arc<AtomicBool>,
    pub handle: Option<JoinHandle<()>>,
}

impl ConfigWatcher {
    /// Create a no-op watcher that never triggers. Used as a fallback
    /// when the real watcher fails to start.
    #[must_use]
    pub fn new_noop() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
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
    // Honor an explicit CONFIG_PATH override first (docker, test harness, or a
    // user pointing a CLI bin at a specific config). Without this, bins and the
    // server silently fall back to `./config.toml` in the cwd, which can target
    // the wrong SQLite database (see CLI bins).
    if let Ok(p) = std::env::var("CONFIG_PATH")
        && !p.trim().is_empty()
    {
        return Ok(PathBuf::from(p));
    }
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
                && last_mtime.is_none_or(|old| old < mtime)
            {
                thread::sleep(Duration::from_millis(120));

                match reload_from(&path) {
                    Ok(()) => {
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
    let p = match default_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("FATAL: {e:#}");
            std::process::exit(1);
        }
    };
    let cfg = match load_config(&p) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("FATAL: failed to load config from {}: {e:#}", p.display());
            std::process::exit(1);
        }
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Config;
    use std::io::Write;

    /// Helper: write a TOML string to a temp file and parse it.
    fn parse_toml(toml: &str) -> anyhow::Result<Config> {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{toml}").unwrap();
        f.flush().unwrap();
        load_config(f.path())
    }

    /// Minimum valid config — every required field with priors.
    const MINIMAL_TOML: &str = r#"
admin_user    = "test_admin"
admin_api     = "test_key"
tag_blacklist = ["sound"]
posts_domain  = "https://e621.net"
posts_limit   = 320
rps_delay_ms  = 250
max_retries   = 2

[priors]
now              = "2025-01-01T12:00:00Z"
recency_tau_days = 10.0
quality_a        = 0.50
quality_b        = 0.20
quality_log_bias = -3.0
mix_sim          = 0.72
mix_quality      = 0.02
mix_recency      = 0.02
mix_rating       = 0.04
mix_media        = 0.05
mix_popularity   = 0.02
mix_interaction  = 0.10
idf_lambda       = 1.0
idf_alpha        = 1.05
freq_alpha       = 0.95
quality_w_absolute          = 0.55
quality_w_relative_score    = 0.30
quality_w_relative_comments = 0.15
popularity_w_fav      = 0.80
popularity_w_duration = 0.20
recency_w_global   = 0.40
recency_w_personal = 0.60
diversity_window       = 32
diversity_w_artist     = 0.22
diversity_w_character  = 0.16
diversity_w_general    = 0.08
"#;

    // ── load_config tests ──────────────────────────────────────────────

    /// Priors with all fields at their default values.
    fn make_default_priors() -> Priors {
        Priors {
            now: "2025-01-01T00:00:00Z".parse().unwrap(),
            recency_tau_days: 10.0,
            quality_a: 0.5,
            quality_b: 0.2,
            quality_log_bias: -3.0,
            mix_sim: 0.72,
            mix_quality: 0.02,
            mix_recency: 0.02,
            mix_rating: 0.04,
            mix_media: 0.05,
            mix_popularity: 0.02,
            mix_interaction: 0.10,
            mix_tag_relation: 0.08,
            mix_uploader: 0.05,
            mix_exclusivity: 0.0,
            mix_novelty: 0.0,
            idf_lambda: 1.0,
            idf_alpha: 1.05,
            freq_alpha: 0.95,
            df_floor: 0.4,
            idf_max: 100.0,
            bm25_k: 2.25,
            idf_rsj_smoothing: 0.35,
            one_sided_ratio_exp: 0.5,
            quality_w_absolute: 0.55,
            quality_w_relative_score: 0.30,
            quality_w_relative_comments: 0.15,
            quality_c: 0.3,
            recency_w_global: 0.4,
            recency_w_personal: 0.6,
            recency_personal_floor_frac: 1.0,
            recency_log_personal: true,
            popularity_w_fav: 0.8,
            popularity_w_duration: 0.2,
            tag_relation_w_global: 0.4,
            tag_relation_w_personal: 0.6,
            tag_relation_pmi_scale: 3.5,
            tag_relation_min_cooc: 2,
            tag_relation_user_min_cooc: 1,
            tag_relation_cooc_ref: 16.0,
            tag_relation_user_cooc_ref: 5.0,
            tag_relation_max_tags: 20,
            group_w_artist: 2.4,
            group_w_character: 2.0,
            group_w_copyright: 1.45,
            group_w_species: 1.3,
            group_w_general: 0.7,
            group_w_lore: 0.4,
            coldstart_smoothing_boost: 2.0,
            interaction_ctr_prior_alpha: 4.0,
            coldstart_n0: 25.0,
            discrete_pref_floor: 0.05,
            diversity_window: 32,
            diversity_w_artist: 0.22,
            diversity_w_character: 0.16,
            diversity_w_copyright: 1.8,
            diversity_w_species: 1.5,
            diversity_w_general: 0.08,
            diversity_max_penalty: 0.45,
            diversity_interaction_damp: 0.35,
            strong_negative_count: 3,
            strong_negative_penalty: 0.4,
            strong_negative_wilson_threshold: 0.55,
            discrete_smoothing_alpha: 1.0,
            feedback_decay_half_life_days: 90.0,
            meta_interaction_weight: 0.3,
            tag_relation_pair_aggregator: "mean".into(),
            score_temperature: 0.0,
            confidence_steepness: 1.0,
            mmr_redundancy_exp: 1.0,
            tag_sim_jaccard_blend: 0.0,
            idf_lambda_meta: f32::NAN,
            tag_relation_pmi_scale_user: f32::NAN,
            recency_tau_recent: f32::NAN,
            recency_split_age_days: 30.0,
            recency_tau_hot: f32::NAN,
            recency_split_age_hours: 24.0,
            exploration_epsilon: 0.0,
            uploader_n0: 5.0,
            uploader_w_avg_score: 0.6,
            uploader_w_avg_fav: 0.4,
            min_exclusivity_cooc: 2,
            exclusivity_scale: 0.5,
            exclusivity_max_tags: 15,
            novelty_n0: 3.0,
            novelty_use_feedback: true,
            diversity_semantic_blend: 0.0,
            diversity_pmi_threshold: 0.0,
            diversity_semantic_max_tags: 10,
            diversity_user_pmi_weight: 1.0,
            exclusivity_cross_group_weight: 0.5,
            mix_artist_discovery: 0.0,
            artist_discovery_n0: 3.0,
            artist_discovery_novelty_bonus: 0.2,
        }
    }

    #[test]
    fn load_config_minimal() {
        let cfg = parse_toml(MINIMAL_TOML).expect("minimal config");
        assert_eq!(cfg.admin_user, "test_admin");
        assert_eq!(cfg.admin_api, "test_key");
        assert_eq!(cfg.posts_domain, "https://e621.net");
        assert_eq!(cfg.posts_limit, 320);
        // Defaults should be applied
        assert_eq!(cfg.attempt_timeout_secs, default_attempt_timeout_secs());
        assert_eq!(cfg.db_path, default_db_path());
        assert_eq!(cfg.e621_cache_ttl_secs, default_e621_cache_ttl_secs());
    }

    #[test]
    fn load_config_missing_required_field_fails() {
        let toml = r#"
admin_user = "x"
tag_blacklist = []
posts_domain = "https://x"
posts_limit = 10
rps_delay_ms = 1
max_retries = 1
[priors]
now = "2025-01-01T00:00:00Z"
"#;
        // Missing admin_api → should fail
        let r = parse_toml(toml);
        assert!(r.is_err(), "missing required field should fail");
    }

    #[test]
    fn load_config_invalid_toml_fails() {
        let r = parse_toml("not even toml [[[");
        assert!(r.is_err(), "garbage TOML should fail");
    }

    #[test]
    fn load_config_empty_priors_fails() {
        let toml = r#"
admin_user = "x"
admin_api = "x"
tag_blacklist = []
posts_domain = "https://x"
posts_limit = 10
rps_delay_ms = 1
max_retries = 1
"#;
        let r = parse_toml(toml);
        assert!(r.is_err(), "missing [priors] section should fail");
    }

    // ── Default values ─────────────────────────────────────────────────

    #[test]
    fn runtime_config_defaults() {
        let rt = RuntimeConfig::default();
        assert_eq!(rt.local_candidate_limit, 400);
        assert_eq!(rt.dedup_lookback_days, 14);
        assert_eq!(rt.process_fetch_concurrency, 4);
        assert_eq!(rt.drop_cooc_batch_size, 50_000);
        assert_eq!(rt.idf_rebuild_cooldown_secs, 15);
        assert_eq!(rt.tag_relation_rebuild_cooldown_secs, 15);
        assert_eq!(rt.prefetch_interval_secs, 180);
        assert_eq!(rt.cleanup_interval_secs, 21_600);
        assert_eq!(rt.catalog_retention_days, 90);
        assert_eq!(rt.orphan_retention_secs, 3600);
        assert_eq!(rt.jobs_finished_retain_secs, 600);
        assert_eq!(rt.jobs_running_timeout_secs, 86400);
        assert_eq!(rt.prefetch_active_window_days, 14);
        assert_eq!(rt.prefetch_breaker_threshold, 3);
        assert_eq!(rt.prefetch_breaker_pause_secs, 600);
        // Prefetch defaults: broad catalog + recent popular (P1 fix).
        assert_eq!(rt.prefetch_tags_per_group, 10);
        assert!(rt.prefetch_include_recent_popular);
        assert_eq!(rt.prefetch_hot_window_hours, 48);
        assert_eq!(rt.prefetch_cold_interval_secs, 900);
        assert_eq!(rt.cache_validate_interval_secs, 300);
        assert_eq!(rt.cache_idle_eviction_secs, 1800);
    }

    #[test]
    fn backtest_config_defaults() {
        let bt = BacktestConfig::default();
        assert_eq!(bt.min_favs, 100);
        assert!((bt.test_fraction - 0.20).abs() < 1e-6);
        assert_eq!(bt.negative_ratio, 10);
        assert_eq!(bt.top_k_ndcg, 20);
        assert_eq!(bt.top_k_recall, 50);
        assert_eq!(bt.max_accounts, 150);
        assert_eq!(bt.max_pages_per_user, 8);
        assert_eq!(bt.seed_owner_token, "calibration-seed");
        assert_eq!(bt.calibrate_threads, 0);
    }

    #[test]
    fn catalog_config_defaults() {
        // Catalog features are opt-in (false/0) except the media cache folder,
        // which is hardcoded to `media/` in media_store (no config knob).
        let c = CatalogConfig::default();
        assert!(!c.save_favourites);
        assert_eq!(c.media_cache_max_bytes, 0);
        assert!(!c.pool_membership);
        assert!(!c.save_all);
    }

    #[test]
    fn catalog_config_parses_from_toml() {
        let cfg = parse_toml(&format!(
            "{MINIMAL_TOML}\n\n[catalog]\nsave_favourites = true\nmedia_cache_max_bytes = 1073741824\npool_membership = true\n"
        ))
        .unwrap();
        assert!(cfg.catalog.save_favourites);
        assert_eq!(cfg.catalog.media_cache_max_bytes, 1_073_741_824);
        assert!(cfg.catalog.pool_membership);
        assert!(!cfg.catalog.save_all);
    }

    #[test]
    fn catalog_defaults_via_minimal_config() {
        let cfg = parse_toml(MINIMAL_TOML).unwrap();
        assert!(!cfg.catalog.save_favourites);
        assert!(!cfg.catalog.pool_membership);
        // Without a `[catalog]` section, `CatalogConfig` uses the derived
        // `Default` (0 / false); field-level serde defaults apply only when the
        // section is present (covered by the override test above).
        assert!(!cfg.catalog.save_all);
    }

    // ── merge_priors ───────────────────────────────────────────────────

    #[test]
    fn merge_priors_applies_non_default_values() {
        let mut base = make_default_priors();

        // Override a few fields with non-default values
        let overrides = Priors {
            mix_sim: 0.8,        // different from default 0.72
            group_w_artist: 3.0, // different from default 2.4
            ..make_default_priors()
        };

        merge_priors(&mut base, &overrides);

        assert!(
            (base.mix_sim - 0.8).abs() < 1e-6,
            "mix_sim should be overridden"
        );
        assert!(
            (base.group_w_artist - 3.0).abs() < 1e-6,
            "group_w_artist should be overridden"
        );
        // Unchanged defaults — must not have been touched
        assert!((base.mix_quality - 0.02).abs() < 1e-6);
        assert!((base.group_w_character - 2.0).abs() < 1e-6);
    }

    #[test]
    fn merge_priors_identity_when_overrides_equal_defaults() {
        let mut base = make_default_priors();
        let snapshot = base.clone();

        // Override with same value as default → merge_priors should be a no-op
        let overrides = Priors {
            mix_sim: 0.72, // same as default
            ..make_default_priors()
        };

        merge_priors(&mut base, &overrides);
        assert!(
            (base.mix_sim - snapshot.mix_sim).abs() < 1e-6,
            "mix_sim should remain unchanged when override matches default"
        );
        assert!((base.mix_quality - snapshot.mix_quality).abs() < 1e-6);
        assert!((base.idf_lambda - snapshot.idf_lambda).abs() < 1e-6);
    }

    // ── BucketOverride::apply_to ──────────────────────────────────────

    #[test]
    fn bucket_override_apply_to_modifies_priors() {
        let mut p = make_default_priors();

        let ovr = BucketOverride {
            mix_sim: Some(0.9),
            mix_quality: Some(0.05),
            mix_recency: None,
            mix_rating: None,
            mix_media: None,
            mix_popularity: None,
            mix_interaction: None,
            mix_tag_relation: None,
            priors: None,
        };
        ovr.apply_to(&mut p);

        assert!((p.mix_sim - 0.9).abs() < 1e-6, "mix_sim should be 0.9");
        assert!(
            (p.mix_quality - 0.05).abs() < 1e-6,
            "mix_quality should be 0.05"
        );
        // Unchanged fields
        assert!((p.mix_recency - 0.02).abs() < 1e-6, "mix_recency unchanged");
        assert!(
            (p.mix_interaction - 0.10).abs() < 1e-6,
            "mix_interaction unchanged"
        );
    }

    // ── Config::pick_bucket ────────────────────────────────────────────

    #[test]
    fn pick_bucket_empty_buckets_returns_none() {
        let cfg = parse_toml(MINIMAL_TOML).expect("minimal config");
        let (name, _priors) = cfg.pick_bucket(42, None);
        assert!(name.is_none(), "no buckets configured → no bucket name");
    }

    #[test]
    fn pick_bucket_explicit_name_selects_that_bucket() {
        let toml = r#"
admin_user = "x"
admin_api = "x"
tag_blacklist = []
posts_domain = "https://e621.net"
posts_limit = 10
rps_delay_ms = 1
max_retries = 1

[priors]
now = "2025-01-01T00:00:00Z"
recency_tau_days = 10.0
quality_a = 0.5
quality_b = 0.2
quality_log_bias = -3.0
mix_sim = 0.72
mix_quality = 0.02
mix_recency = 0.02
mix_rating = 0.04
mix_media = 0.05
mix_popularity = 0.02
mix_interaction = 0.10
idf_lambda = 1.0
idf_alpha = 1.05
freq_alpha = 0.95
quality_w_absolute = 0.55
quality_w_relative_score = 0.30
quality_w_relative_comments = 0.15
popularity_w_fav = 0.80
popularity_w_duration = 0.20
recency_w_global = 0.40
recency_w_personal = 0.60
diversity_window = 32
diversity_w_artist = 0.22
diversity_w_character = 0.16
diversity_w_general = 0.08

[buckets.control]
mix_sim = 0.5

[buckets.test]
mix_quality = 0.1
"#;
        let cfg = parse_toml(toml).expect("config with buckets");
        let (name, priors) = cfg.pick_bucket(42, Some("test"));
        assert_eq!(name.as_deref(), Some("test"));
        assert!((priors.mix_quality - 0.1).abs() < 1e-6);
        // mix_sim should remain at default since test bucket doesn't override it
        assert!((priors.mix_sim - 0.72).abs() < 1e-6);

        // Unknown explicit name falls through to hash selection
        let (name, _) = cfg.pick_bucket(42, Some("nonexistent"));
        assert_eq!(
            name.as_deref(),
            Some("control"),
            "falls back to hash-based when explicit name not found"
        );
    }

    #[test]
    fn pick_bucket_hash_distributes_across_buckets() {
        let toml = r#"
admin_user = "x"
admin_api = "x"
tag_blacklist = []
posts_domain = "https://e621.net"
posts_limit = 10
rps_delay_ms = 1
max_retries = 1

[priors]
now = "2025-01-01T00:00:00Z"
recency_tau_days = 10.0
quality_a = 0.5
quality_b = 0.2
quality_log_bias = -3.0
mix_sim = 0.72
mix_quality = 0.02
mix_recency = 0.02
mix_rating = 0.04
mix_media = 0.05
mix_popularity = 0.02
mix_interaction = 0.10
idf_lambda = 1.0
idf_alpha = 1.05
freq_alpha = 0.95
quality_w_absolute = 0.55
quality_w_relative_score = 0.30
quality_w_relative_comments = 0.15
popularity_w_fav = 0.80
popularity_w_duration = 0.20
recency_w_global = 0.40
recency_w_personal = 0.60
diversity_window = 32
diversity_w_artist = 0.22
diversity_w_character = 0.16
diversity_w_general = 0.08

[buckets.a]
mix_sim = 0.5
[buckets.b]
mix_sim = 0.6
"#;
        let cfg = parse_toml(toml).expect("config with 2 buckets");

        // Different account_ids should map to different buckets
        let (name_a, _) = cfg.pick_bucket(1, None);
        let (name_b, _) = cfg.pick_bucket(2, None);
        // At least sometimes they differ (depends on hash, but with 2 buckets
        // and 2 different accounts it's likely).
        assert!(name_a.is_some());
        assert!(name_b.is_some());
        // Both must be one of the defined bucket names
        assert!(
            name_a.as_deref() == Some("a") || name_a.as_deref() == Some("b"),
            "bucket must be 'a' or 'b', got {name_a:?}"
        );
        assert!(
            name_b.as_deref() == Some("a") || name_b.as_deref() == Some("b"),
            "bucket must be 'a' or 'b', got {name_b:?}"
        );
    }

    // ── Default path ───────────────────────────────────────────────────

    #[test]
    fn default_path_honors_config_path_env_and_defaults() {
        // These two properties are tested together because both mutate the
        // process env; running them as separate tests would race on the shared
        // `CONFIG_PATH` under parallel `cargo test`.
        let prior = std::env::var("CONFIG_PATH").ok();
        // env override wins ...
        unsafe { std::env::set_var("CONFIG_PATH", "/srv/my/cfg.toml") };
        assert_eq!(default_path().unwrap(), PathBuf::from("/srv/my/cfg.toml"));
        // ... but absent env falls back to config.toml.
        unsafe { std::env::remove_var("CONFIG_PATH") };
        assert_eq!(default_path().unwrap(), PathBuf::from("config.toml"));
        // Restore the caller's environment.
        if let Some(v) = prior {
            unsafe { std::env::set_var("CONFIG_PATH", v) };
        } else {
            unsafe { std::env::remove_var("CONFIG_PATH") };
        }
    }

    #[test]
    fn file_mtime_works_for_existing_file() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let mtime = file_mtime(f.path());
        assert!(mtime.is_ok(), "mtime on a temp file must succeed");
    }

    #[test]
    fn file_mtime_fails_for_nonexistent() {
        let p = PathBuf::from("/nonexistent/path/file.toml");
        let mtime = file_mtime(&p);
        assert!(mtime.is_err(), "mtime on nonexistent file must fail");
    }
}
