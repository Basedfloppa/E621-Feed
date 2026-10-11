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
    /// Collect favourites into the local catalog: posts served by
    /// `/browse/favorites` and imported by `/process` (favourites sync) are
    /// persisted (posts + `accounts_post` + tags), and the background media
    /// worker downloads their originals. `save_all` implies this scope.
    /// Default `false`.
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

    /// Additionally collect every post the owner encounters — `/digest`,
    /// `/search`, `/trending` (and the feed) — into the local catalog, on top
    /// of the favourites scope (`save_favourites`). Off by default — it can
    /// grow the catalog and media cache a lot. `save_all` is independent from
    /// `save_favourites`.
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
    /// Any local collection is active when at least one persistence toggle is
    /// on. This gates the favourites scope (`/browse/favorites`, `/process`,
    /// direct sync) and the media-cache worker; with both off nothing is
    /// collected and the worker idles.
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

    /// Anchor tags per group (`artist`, `character`) selected for the local
    /// candidate pool. Ranking is by IDF-weighted frequency, not raw `count`:
    /// raw counts surface the generic tags every account shares (`male`,
    /// `solo`, `anthro`), which collapses every account's pool onto the same
    /// globally popular posts. Default `20`.
    #[serde(default = "default_candidate_anchor_tags")]
    pub candidate_anchor_tags: usize,
    /// Lookback window for the recent-popular stream. Default `60`.
    #[serde(default = "default_candidate_recent_days")]
    pub candidate_recent_days: i64,
    /// Per-stream ceiling as a fraction of `local_candidate_limit`. Streams are
    /// capped independently because their densities differ by orders of
    /// magnitude — artist tags match far more posts than character tags, so a
    /// shared pool counter lets the densest stream consume every slot and the
    /// rest contribute nothing. Default `0.34` (five streams, the three
    /// inferred ones sized for roughly equal shares).
    #[serde(default = "default_candidate_stream_share")]
    pub candidate_stream_share: f64,
    /// Ceiling for the explicit preferred-tags stream. Preferred tags are the
    /// only signal the user states rather than one we infer, so this stream is
    /// allowed to be larger than any single inferred one. Default `120`.
    #[serde(default = "default_candidate_preferred_limit")]
    pub candidate_preferred_limit: usize,
    /// Ceiling for the co-occurrence-neighbour stream (tags that co-occur with
    /// the account's own). Default `80`.
    #[serde(default = "default_candidate_cooccurrence_limit")]
    pub candidate_cooccurrence_limit: usize,
    /// Minimum `cooc_count` for a partner tag to seed the neighbour stream.
    /// Pairs seen once are overwhelmingly incidental. Default `2`.
    #[serde(default = "default_candidate_cooccurrence_min_cooc")]
    pub candidate_cooccurrence_min_cooc: i64,

    /// Total number of terms e621 accepts in a `tags` query; above this it
    /// answers 422. Clamped to the protocol ceiling at the call site, so a
    /// larger value cannot break every request. Default `40`.
    #[serde(default = "default_e621_tag_limit")]
    pub e621_tag_limit: usize,
    /// Slots of `e621_tag_limit` reserved for the live positive boost group —
    /// preferred tags plus IDF anchors, OR-ed together so the live query
    /// narrows to the account's taste instead of the global default feed.
    /// `0` reproduces the historical blacklist-only behaviour. Default `12`.
    #[serde(default = "default_e621_boost_budget")]
    pub e621_boost_budget: usize,
    /// Slots of `e621_tag_limit` reserved for blacklist exclusions. `0` means
    /// "whatever remains after the search terms and the boost group". This is
    /// not a safety limit: live posts are filtered against the full blacklist
    /// in-process, so exclusions dropped here cost recall, not compliance.
    #[serde(default)]
    pub e621_blacklist_budget: usize,
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
            candidate_anchor_tags: default_candidate_anchor_tags(),
            candidate_recent_days: default_candidate_recent_days(),
            candidate_stream_share: default_candidate_stream_share(),
            candidate_preferred_limit: default_candidate_preferred_limit(),
            candidate_cooccurrence_limit: default_candidate_cooccurrence_limit(),
            candidate_cooccurrence_min_cooc: default_candidate_cooccurrence_min_cooc(),
            e621_tag_limit: default_e621_tag_limit(),
            e621_boost_budget: default_e621_boost_budget(),
            e621_blacklist_budget: 0,
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
fn default_candidate_anchor_tags() -> usize {
    20
}
fn default_candidate_recent_days() -> i64 {
    60
}
fn default_candidate_stream_share() -> f64 {
    0.34
}
fn default_candidate_preferred_limit() -> usize {
    120
}
fn default_candidate_cooccurrence_limit() -> usize {
    80
}
fn default_candidate_cooccurrence_min_cooc() -> i64 {
    2
}
fn default_e621_tag_limit() -> usize {
    40
}
fn default_e621_boost_budget() -> usize {
    12
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

    /// Threads in the bounded rayon pool used by `calibrate`'s
    /// **database-backed** parallel sections (account hydration during
    /// dataset preparation).
    ///
    /// `0` = auto = `nproc / 2`, matching the `.cargo/config.toml`
    /// build-jobs cap so a multi-hour grid leaves half the box free.
    ///
    /// This pool must stay below the r2d2 reader pool size — `db::pool`
    /// hardcodes 16 — or hydration starts failing with `timed out
    /// waiting for connection` for a large share of accounts. That
    /// coupling, not caution, is why the default is half the cores.
    /// Scoring is not bound that way; see `calibrate_score_threads`.
    #[serde(default = "default_calibrate_threads")]
    pub calibrate_threads: usize,

    /// Threads in the rayon pool used by the **scoring** loops
    /// (`metrics::score_with_progress`, `cache::score_with_cache`).
    ///
    /// Those loops run entirely off the in-memory dataset and never
    /// touch SQLite, so they are not bound by the connection pool and
    /// may use every core. `0` = auto = `nproc`.
    #[serde(default = "default_calibrate_score_threads")]
    pub calibrate_score_threads: usize,
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
            calibrate_score_threads: default_calibrate_score_threads(),
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
fn default_calibrate_score_threads() -> usize {
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
        if let Some(json) = &self.priors {
            // `Priors` only deserialises when a `now` timestamp is present, and
            // an override legitimately carries knobs — never the evaluation
            // clock. Without injecting the base's own clock the deserialisation
            // fails on the missing field and the whole override is dropped.
            let now = p.now;
            let mut value = json.clone();
            if let Some(obj) = value.as_object_mut() {
                obj.entry("now").or_insert_with(|| serde_json::json!(now));
            }
            match serde_json::from_value::<Priors>(value) {
                Ok(overrides) => merge_priors(p, &overrides),
                // Never swallow a malformed override: a silently ignored bucket
                // is indistinguishable from a bucket with no effect.
                Err(err) => warn!("ignoring [buckets.*] priors override: {err}"),
            }
        }
    }
}

/// The `Priors` field defaults, frozen once. `Priors::default()` is the single
/// source of truth for them; its `now` (the evaluation clock) is not compared
/// anywhere in `merge_priors`.
fn priors_field_defaults() -> &'static Priors {
    static DEFAULTS: LazyLock<Priors> = LazyLock::new(Priors::default);
    &DEFAULTS
}

/// Whether an override carries a value, given the field's default. The float
/// defaults include `NaN` sentinels meaning "this knob is off", and `NaN != NaN`
/// is true — the naive comparison would call every override set and a sentinel
/// could never be written back.
fn override_set_f32(value: f32, default: f32) -> bool {
    if default.is_nan() {
        !value.is_nan()
    } else {
        value != default
    }
}

/// Merge non-default values from `overrides` into `base`. "Non-default" means
/// "differs from this field's value in `Priors::default()`" — the comparisons
/// below read the defaults from there instead of repeating them as literals.
/// A literal is a second copy of a default that nothing updates, and a stale
/// one changes which overrides count as set (that is how `diversity_head_limit`,
/// `diversity_user_pmi_weight` and `exclusivity_cross_group_weight` were dead).
/// This avoids requiring every field to be `Option<T>` in Priors itself.
fn merge_priors(base: &mut Priors, overrides: &Priors) {
    // Field defaults come from one place, frozen once. Rebuilding a
    // `Priors` per call would also read the clock (`now` defaults to
    // `Utc::now()`), which nothing here compares.
    let d = priors_field_defaults();
    // --- mix weights ---
    if override_set_f32(overrides.mix_sim, d.mix_sim) {
        base.mix_sim = overrides.mix_sim;
    }
    if override_set_f32(overrides.mix_quality, d.mix_quality) {
        base.mix_quality = overrides.mix_quality;
    }
    if override_set_f32(overrides.mix_recency, d.mix_recency) {
        base.mix_recency = overrides.mix_recency;
    }
    if override_set_f32(overrides.mix_rating, d.mix_rating) {
        base.mix_rating = overrides.mix_rating;
    }
    if override_set_f32(overrides.mix_media, d.mix_media) {
        base.mix_media = overrides.mix_media;
    }
    if override_set_f32(overrides.mix_popularity, d.mix_popularity) {
        base.mix_popularity = overrides.mix_popularity;
    }
    if override_set_f32(overrides.mix_interaction, d.mix_interaction) {
        base.mix_interaction = overrides.mix_interaction;
    }
    if override_set_f32(overrides.mix_tag_relation, d.mix_tag_relation) {
        base.mix_tag_relation = overrides.mix_tag_relation;
    }
    if override_set_f32(overrides.mix_uploader, d.mix_uploader) {
        base.mix_uploader = overrides.mix_uploader;
    }
    if override_set_f32(overrides.mix_exclusivity, d.mix_exclusivity) {
        base.mix_exclusivity = overrides.mix_exclusivity;
    }
    if override_set_f32(overrides.mix_novelty, d.mix_novelty) {
        base.mix_novelty = overrides.mix_novelty;
    }

    // --- IDF / freq ---
    if override_set_f32(overrides.idf_lambda, d.idf_lambda) {
        base.idf_lambda = overrides.idf_lambda;
    }
    if override_set_f32(overrides.idf_alpha, d.idf_alpha) {
        base.idf_alpha = overrides.idf_alpha;
    }
    if override_set_f32(overrides.freq_alpha, d.freq_alpha) {
        base.freq_alpha = overrides.freq_alpha;
    }
    if override_set_f32(overrides.df_floor, d.df_floor) {
        base.df_floor = overrides.df_floor;
    }
    if override_set_f32(overrides.idf_max, d.idf_max) {
        base.idf_max = overrides.idf_max;
    }
    if override_set_f32(overrides.bm25_k, d.bm25_k) {
        base.bm25_k = overrides.bm25_k;
    }
    if override_set_f32(overrides.idf_rsj_smoothing, d.idf_rsj_smoothing) {
        base.idf_rsj_smoothing = overrides.idf_rsj_smoothing;
    }
    if override_set_f32(overrides.one_sided_ratio_exp, d.one_sided_ratio_exp) {
        base.one_sided_ratio_exp = overrides.one_sided_ratio_exp;
    }
    if override_set_f32(overrides.idf_lambda_meta, d.idf_lambda_meta) {
        base.idf_lambda_meta = overrides.idf_lambda_meta;
    }

    // --- quality channel ---
    if override_set_f32(overrides.quality_a, d.quality_a) {
        base.quality_a = overrides.quality_a;
    }
    if override_set_f32(overrides.quality_b, d.quality_b) {
        base.quality_b = overrides.quality_b;
    }
    if override_set_f32(overrides.quality_log_bias, d.quality_log_bias) {
        base.quality_log_bias = overrides.quality_log_bias;
    }
    if override_set_f32(overrides.quality_w_absolute, d.quality_w_absolute) {
        base.quality_w_absolute = overrides.quality_w_absolute;
    }
    if override_set_f32(
        overrides.quality_w_relative_score,
        d.quality_w_relative_score,
    ) {
        base.quality_w_relative_score = overrides.quality_w_relative_score;
    }
    if override_set_f32(
        overrides.quality_w_relative_comments,
        d.quality_w_relative_comments,
    ) {
        base.quality_w_relative_comments = overrides.quality_w_relative_comments;
    }
    if override_set_f32(overrides.quality_c, d.quality_c) {
        base.quality_c = overrides.quality_c;
    }

    // --- recency channel ---
    if override_set_f32(overrides.recency_tau_days, d.recency_tau_days) {
        base.recency_tau_days = overrides.recency_tau_days;
    }
    if override_set_f32(overrides.recency_w_global, d.recency_w_global) {
        base.recency_w_global = overrides.recency_w_global;
    }
    if override_set_f32(overrides.recency_w_personal, d.recency_w_personal) {
        base.recency_w_personal = overrides.recency_w_personal;
    }
    if override_set_f32(
        overrides.recency_personal_floor_frac,
        d.recency_personal_floor_frac,
    ) {
        base.recency_personal_floor_frac = overrides.recency_personal_floor_frac;
    }
    if overrides.recency_log_personal != d.recency_log_personal {
        base.recency_log_personal = overrides.recency_log_personal;
    }
    if override_set_f32(overrides.recency_tau_hot, d.recency_tau_hot) {
        base.recency_tau_hot = overrides.recency_tau_hot;
    }
    if override_set_f32(overrides.recency_tau_recent, d.recency_tau_recent) {
        base.recency_tau_recent = overrides.recency_tau_recent;
    }
    if override_set_f32(overrides.recency_split_age_hours, d.recency_split_age_hours) {
        base.recency_split_age_hours = overrides.recency_split_age_hours;
    }
    if override_set_f32(overrides.recency_split_age_days, d.recency_split_age_days) {
        base.recency_split_age_days = overrides.recency_split_age_days;
    }

    // --- popularity channel ---
    if override_set_f32(overrides.popularity_w_fav, d.popularity_w_fav) {
        base.popularity_w_fav = overrides.popularity_w_fav;
    }
    if override_set_f32(overrides.popularity_w_duration, d.popularity_w_duration) {
        base.popularity_w_duration = overrides.popularity_w_duration;
    }

    // --- tag relation channel ---
    if override_set_f32(overrides.tag_relation_w_global, d.tag_relation_w_global) {
        base.tag_relation_w_global = overrides.tag_relation_w_global;
    }
    if override_set_f32(overrides.tag_relation_w_personal, d.tag_relation_w_personal) {
        base.tag_relation_w_personal = overrides.tag_relation_w_personal;
    }
    if override_set_f32(overrides.tag_relation_pmi_scale, d.tag_relation_pmi_scale) {
        base.tag_relation_pmi_scale = overrides.tag_relation_pmi_scale;
    }
    if overrides.tag_relation_min_cooc != d.tag_relation_min_cooc {
        base.tag_relation_min_cooc = overrides.tag_relation_min_cooc;
    }
    if overrides.tag_relation_user_min_cooc != d.tag_relation_user_min_cooc {
        base.tag_relation_user_min_cooc = overrides.tag_relation_user_min_cooc;
    }
    if override_set_f32(overrides.tag_relation_cooc_ref, d.tag_relation_cooc_ref) {
        base.tag_relation_cooc_ref = overrides.tag_relation_cooc_ref;
    }
    if override_set_f32(
        overrides.tag_relation_user_cooc_ref,
        d.tag_relation_user_cooc_ref,
    ) {
        base.tag_relation_user_cooc_ref = overrides.tag_relation_user_cooc_ref;
    }
    if override_set_f32(
        overrides.tag_relation_pmi_scale_user,
        d.tag_relation_pmi_scale_user,
    ) {
        base.tag_relation_pmi_scale_user = overrides.tag_relation_pmi_scale_user;
    }
    if overrides.tag_relation_pair_aggregator != d.tag_relation_pair_aggregator {
        base.tag_relation_pair_aggregator
            .clone_from(&overrides.tag_relation_pair_aggregator);
    }
    if overrides.tag_relation_max_tags != d.tag_relation_max_tags {
        base.tag_relation_max_tags = overrides.tag_relation_max_tags;
    }

    // --- group weights ---
    if override_set_f32(overrides.group_w_artist, d.group_w_artist) {
        base.group_w_artist = overrides.group_w_artist;
    }
    if override_set_f32(overrides.group_w_character, d.group_w_character) {
        base.group_w_character = overrides.group_w_character;
    }
    if override_set_f32(overrides.group_w_copyright, d.group_w_copyright) {
        base.group_w_copyright = overrides.group_w_copyright;
    }
    if override_set_f32(overrides.group_w_species, d.group_w_species) {
        base.group_w_species = overrides.group_w_species;
    }
    if override_set_f32(overrides.group_w_general, d.group_w_general) {
        base.group_w_general = overrides.group_w_general;
    }
    if override_set_f32(overrides.group_w_lore, d.group_w_lore) {
        base.group_w_lore = overrides.group_w_lore;
    }

    // --- interaction / feedback ---
    if override_set_f32(
        overrides.interaction_ctr_prior_alpha,
        d.interaction_ctr_prior_alpha,
    ) {
        base.interaction_ctr_prior_alpha = overrides.interaction_ctr_prior_alpha;
    }
    if override_set_f32(overrides.meta_interaction_weight, d.meta_interaction_weight) {
        base.meta_interaction_weight = overrides.meta_interaction_weight;
    }
    if override_set_f32(
        overrides.feedback_decay_half_life_days,
        d.feedback_decay_half_life_days,
    ) {
        base.feedback_decay_half_life_days = overrides.feedback_decay_half_life_days;
    }
    if overrides.strong_negative_count != d.strong_negative_count {
        base.strong_negative_count = overrides.strong_negative_count;
    }
    if override_set_f32(overrides.strong_negative_penalty, d.strong_negative_penalty) {
        base.strong_negative_penalty = overrides.strong_negative_penalty;
    }
    if override_set_f32(
        overrides.strong_negative_wilson_threshold,
        d.strong_negative_wilson_threshold,
    ) {
        base.strong_negative_wilson_threshold = overrides.strong_negative_wilson_threshold;
    }

    // --- cold-start / smoothing ---
    if override_set_f32(
        overrides.discrete_smoothing_alpha,
        d.discrete_smoothing_alpha,
    ) {
        base.discrete_smoothing_alpha = overrides.discrete_smoothing_alpha;
    }
    if override_set_f32(overrides.discrete_pref_floor, d.discrete_pref_floor) {
        base.discrete_pref_floor = overrides.discrete_pref_floor;
    }
    if override_set_f32(
        overrides.coldstart_smoothing_boost,
        d.coldstart_smoothing_boost,
    ) {
        base.coldstart_smoothing_boost = overrides.coldstart_smoothing_boost;
    }
    if override_set_f32(overrides.coldstart_n0, d.coldstart_n0) {
        base.coldstart_n0 = overrides.coldstart_n0;
    }

    // --- diversity / MMR ---
    if overrides.diversity_window != d.diversity_window {
        base.diversity_window = overrides.diversity_window;
    }
    if override_set_f32(overrides.diversity_w_artist, d.diversity_w_artist) {
        base.diversity_w_artist = overrides.diversity_w_artist;
    }
    if override_set_f32(overrides.diversity_w_character, d.diversity_w_character) {
        base.diversity_w_character = overrides.diversity_w_character;
    }
    if override_set_f32(overrides.diversity_w_copyright, d.diversity_w_copyright) {
        base.diversity_w_copyright = overrides.diversity_w_copyright;
    }
    if override_set_f32(overrides.diversity_w_species, d.diversity_w_species) {
        base.diversity_w_species = overrides.diversity_w_species;
    }
    if override_set_f32(overrides.diversity_w_general, d.diversity_w_general) {
        base.diversity_w_general = overrides.diversity_w_general;
    }
    if override_set_f32(overrides.diversity_max_penalty, d.diversity_max_penalty) {
        base.diversity_max_penalty = overrides.diversity_max_penalty;
    }
    if override_set_f32(
        overrides.diversity_interaction_damp,
        d.diversity_interaction_damp,
    ) {
        base.diversity_interaction_damp = overrides.diversity_interaction_damp;
    }
    // v5.12 Class L: production-only MMR head bound. 0 = whole list.
    if overrides.diversity_head_limit != d.diversity_head_limit {
        base.diversity_head_limit = overrides.diversity_head_limit;
    }
    // --- v5.11 Class J: diversity semantic similarity ---
    if override_set_f32(
        overrides.diversity_semantic_blend,
        d.diversity_semantic_blend,
    ) {
        base.diversity_semantic_blend = overrides.diversity_semantic_blend;
    }
    if override_set_f32(overrides.diversity_pmi_threshold, d.diversity_pmi_threshold) {
        base.diversity_pmi_threshold = overrides.diversity_pmi_threshold;
    }
    if overrides.diversity_semantic_max_tags != d.diversity_semantic_max_tags {
        base.diversity_semantic_max_tags = overrides.diversity_semantic_max_tags;
    }
    if override_set_f32(
        overrides.diversity_user_pmi_weight,
        d.diversity_user_pmi_weight,
    ) {
        base.diversity_user_pmi_weight = overrides.diversity_user_pmi_weight;
    }

    // --- uploader channel ---
    if override_set_f32(overrides.uploader_n0, d.uploader_n0) {
        base.uploader_n0 = overrides.uploader_n0;
    }
    if override_set_f32(overrides.uploader_w_avg_score, d.uploader_w_avg_score) {
        base.uploader_w_avg_score = overrides.uploader_w_avg_score;
    }
    if override_set_f32(overrides.uploader_w_avg_fav, d.uploader_w_avg_fav) {
        base.uploader_w_avg_fav = overrides.uploader_w_avg_fav;
    }

    // --- exclusivity channel ---
    if overrides.min_exclusivity_cooc != d.min_exclusivity_cooc {
        base.min_exclusivity_cooc = overrides.min_exclusivity_cooc;
    }
    if override_set_f32(overrides.exclusivity_scale, d.exclusivity_scale) {
        base.exclusivity_scale = overrides.exclusivity_scale;
    }
    if overrides.exclusivity_max_tags != d.exclusivity_max_tags {
        base.exclusivity_max_tags = overrides.exclusivity_max_tags;
    }
    if override_set_f32(
        overrides.exclusivity_cross_group_weight,
        d.exclusivity_cross_group_weight,
    ) {
        base.exclusivity_cross_group_weight = overrides.exclusivity_cross_group_weight;
    }

    // --- novelty channel ---
    if override_set_f32(overrides.novelty_n0, d.novelty_n0) {
        base.novelty_n0 = overrides.novelty_n0;
    }
    if overrides.novelty_use_feedback != d.novelty_use_feedback {
        base.novelty_use_feedback = overrides.novelty_use_feedback;
    }

    // --- artist discovery channel ---
    if override_set_f32(overrides.mix_artist_discovery, d.mix_artist_discovery) {
        base.mix_artist_discovery = overrides.mix_artist_discovery;
    }
    if override_set_f32(overrides.artist_discovery_n0, d.artist_discovery_n0) {
        base.artist_discovery_n0 = overrides.artist_discovery_n0;
    }
    if override_set_f32(
        overrides.artist_discovery_novelty_bonus,
        d.artist_discovery_novelty_bonus,
    ) {
        base.artist_discovery_novelty_bonus = overrides.artist_discovery_novelty_bonus;
    }

    // --- algorithmic shape ---
    if override_set_f32(overrides.score_temperature, d.score_temperature) {
        base.score_temperature = overrides.score_temperature;
    }
    if override_set_f32(overrides.confidence_steepness, d.confidence_steepness) {
        base.confidence_steepness = overrides.confidence_steepness;
    }
    if override_set_f32(overrides.mmr_redundancy_exp, d.mmr_redundancy_exp) {
        base.mmr_redundancy_exp = overrides.mmr_redundancy_exp;
    }
    if override_set_f32(overrides.tag_sim_jaccard_blend, d.tag_sim_jaccard_blend) {
        base.tag_sim_jaccard_blend = overrides.tag_sim_jaccard_blend;
    }

    // --- exploration ---
    if override_set_f32(overrides.exploration_epsilon, d.exploration_epsilon) {
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
    // Log a REDACTED view: the raw `{:#?}` of `Config` printed `admin_api` and
    // `e621_key_encryption_secret` in cleartext whenever debug logging was on.
    debug!(
        "[config] current value (secrets redacted):\n{:#?}",
        arc.redacted()
    );
    Ok(())
}

/// A `Debug` view of [`Config`] with credentials masked.
///
/// Deliberately hand-written instead of logging the derived `Debug`: the raw
/// `{:#?}` printed `admin_api` and `e621_key_encryption_secret` in cleartext
/// whenever debug logging was enabled. Use this for any config logging.
pub struct RedactedSecrets<'a>(&'a Config);

impl std::fmt::Debug for RedactedSecrets<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shown = |s: &str| {
            if s.is_empty() {
                "<empty>".to_string()
            } else {
                format!("<redacted, {} chars>", s.chars().count())
            }
        };
        f.debug_struct("Config")
            .field("admin_user", &self.0.admin_user)
            .field("admin_api", &shown(&self.0.admin_api))
            .field(
                "e621_key_encryption_secret",
                &shown(&self.0.e621_key_encryption_secret),
            )
            .field("posts_domain", &self.0.posts_domain)
            .field("posts_limit", &self.0.posts_limit)
            .field("db_path", &self.0.db_path)
            .field("trust_proxy", &self.0.trust_proxy)
            .field("e621_cache_ttl_secs", &self.0.e621_cache_ttl_secs)
            .field("e621_cache_max_entries", &self.0.e621_cache_max_entries)
            .field("buckets", &self.0.buckets.keys().collect::<Vec<_>>())
            .field("runtime", &self.0.runtime)
            .field("catalog", &self.0.catalog)
            .field("priors", &self.0.priors)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// Redacted `Debug` view — see [`RedactedSecrets`].
    #[must_use]
    pub fn redacted(&self) -> RedactedSecrets<'_> {
        RedactedSecrets(self)
    }
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

    /// Priors with all fields at their default values. The `now` field is
    /// pinned so fixtures stay deterministic; everything else comes from
    /// `Priors::default()`, which is the single source of truth for defaults.
    fn make_default_priors() -> Priors {
        Priors {
            now: "2025-01-01T00:00:00Z".parse().unwrap(),
            ..Priors::default()
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
    fn diversity_head_limit_defaults_to_the_whole_list() {
        // Absent from MINIMAL_TOML → 0 → the pre-existing full-list MMR.
        let cfg = parse_toml(MINIMAL_TOML).expect("minimal config");
        assert_eq!(cfg.priors.diversity_head_limit, 0);

        // An explicit bound is honoured.
        let toml = format!("{MINIMAL_TOML}\ndiversity_head_limit = 128\n");
        let cfg = parse_toml(&toml).expect("config with head limit");
        assert_eq!(cfg.priors.diversity_head_limit, 128);
    }

    #[test]
    fn load_config_missing_required_field_fails() {
        let toml = r#"
admin_user = "x"
tag_blacklist = []
posts_domain = "https://x"
posts_limit = 10
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

    /// A bucket override is the documented way to A/B a priors change
    /// (`[buckets.x] priors = { … }`, see `BucketOverride::priors`). It must
    /// actually reach `Priors`: the JSON is handed to
    /// `serde_json::from_value::<Priors>`, and `Priors` carries a required `now`
    /// field that no override would ever spell out.
    #[test]
    fn bucket_priors_json_override_reaches_priors() {
        let toml = format!(
            r#"{MINIMAL_TOML}
[buckets.experiment]
priors = {{ diversity_head_limit = 128, diversity_user_pmi_weight = 2.0, exclusivity_cross_group_weight = 0.9 }}
"#
        );
        let cfg = parse_toml(&toml).expect("config with a priors override");
        let (name, priors) = cfg.pick_bucket(42, Some("experiment"));
        assert_eq!(name.as_deref(), Some("experiment"));
        assert_eq!(
            priors.diversity_head_limit, 128,
            "diversity_head_limit override must reach Priors"
        );
        assert!(
            (priors.diversity_user_pmi_weight - 2.0).abs() < 1e-6,
            "diversity_user_pmi_weight override must reach Priors"
        );
        assert!(
            (priors.exclusivity_cross_group_weight - 0.9).abs() < 1e-6,
            "exclusivity_cross_group_weight override must reach Priors"
        );
    }

    /// Every knob of `Priors` has to be copied by `merge_priors`, otherwise
    /// `[buckets.x] priors = { … }` silently ignores it — which is exactly how
    /// `diversity_head_limit`, `diversity_user_pmi_weight` and
    /// `exclusivity_cross_group_weight` were dead. This scans the sources so a
    /// field added later cannot slip through again. `now` is the evaluation
    /// clock rather than a knob and is deliberately not merged.
    #[test]
    fn merge_priors_covers_every_priors_field() {
        let priors_src = include_str!("../utils/scorer/priors.rs");
        let struct_body = priors_src
            .split_once("pub struct Priors {")
            .expect("Priors struct declaration")
            .1
            .split_once("\n}")
            .expect("end of the Priors struct")
            .0;
        let fields: Vec<&str> = struct_body
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub "))
            .filter_map(|line| line.split_once(':').map(|(name, _)| name))
            .collect();
        assert!(
            fields.len() > 80,
            "sanity check: parsed only {} Priors fields",
            fields.len()
        );

        let config_src = include_str!("config.rs");
        let merge_body = config_src
            .split_once("fn merge_priors(")
            .expect("merge_priors declaration")
            .1
            .split_once("\n}\n")
            .expect("end of merge_priors")
            .0;
        let handled: Vec<&str> = merge_body
            .match_indices("overrides.")
            .map(|(start, needle)| {
                let rest = &merge_body[start + needle.len()..];
                let end = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len());
                &rest[..end]
            })
            .collect();

        let missing: Vec<&str> = fields
            .iter()
            .copied()
            .filter(|field| *field != "now")
            .filter(|field| !handled.contains(field))
            .collect();
        assert!(
            missing.is_empty(),
            "merge_priors does not copy: {missing:?}"
        );
    }

    /// The defaults `merge_priors` compares against must come from
    /// `Priors::default()`, never from literals: a literal is a second copy of a
    /// default that nothing updates, so changing the default in `priors.rs`
    /// would silently leave the old value treated as "unset". This scans the
    /// body so a literal cannot come back. The body is flattened first, because
    /// `rustfmt` wraps long `override_set_f32(…)` calls across lines.
    #[test]
    fn merge_priors_compares_against_priors_default() {
        let config_src = include_str!("config.rs");
        let body = config_src
            .split_once("fn merge_priors(")
            .expect("merge_priors declaration")
            .1
            .split_once("\n}\n")
            .expect("end of merge_priors")
            .0;
        let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");

        assert!(
            flat.contains("let d = priors_field_defaults();"),
            "merge_priors must take its defaults from `Priors::default()`"
        );
        assert!(
            !flat.contains("is_nan"),
            "NaN sentinels belong to `override_set_f32`, not to inline comparisons"
        );

        // Every `!=` must compare against `d.<field>`.
        let mut rest = flat.as_str();
        while let Some(pos) = rest.find("!= ") {
            let after = &rest[pos + 3..];
            assert!(
                after.starts_with("d."),
                "merge_priors compares against a literal: != {}",
                &after[..after.len().min(40)]
            );
            rest = after;
        }

        // Every `override_set_f32(…)` must receive `overrides.<field>` and the
        // matching `d.<field>`.
        let mut calls = 0;
        let mut rest = flat.as_str();
        while let Some(pos) = rest.find("override_set_f32(") {
            let after = &rest[pos + "override_set_f32(".len()..];
            let call = after[..after.find(')').unwrap_or(after.len())].trim();
            assert!(
                call.starts_with("overrides.") && call.contains(", d."),
                "override_set_f32 must receive the field default from `d`: {call}"
            );
            calls += 1;
            rest = after;
        }
        assert!(
            calls > 70,
            "sanity check: only {calls} override_set_f32 calls found, the scan is broken"
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
