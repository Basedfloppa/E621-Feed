use crate::utils::Priors;
use anyhow::Context;
use arc_swap::ArcSwap;
use rocket::serde::Deserialize;
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

    /// Runtime knobs for the recommendations endpoint, prefetcher, cleanup,
    /// and IDF/tag-relation rebuild workers. All fields have defaults — the
    /// section is optional in `config.toml`.
    #[serde(default)]
    pub runtime: RuntimeConfig,

    /// Calibration / seed binary knobs. Skip in production deployments.
    #[serde(default)]
    pub backtest: BacktestConfig,

    /// Offline-serving knobs (TODO §4.1 / docs/offline-catalog.md): originals
    /// on disk, pool membership, encountered-post collection. The user's
    /// library (`accounts_post`) is *not* gated here — it is core and always
    /// imported — so defaulting every field off changes nothing about what the
    /// app collects for scoring.
    #[serde(default)]
    pub catalog: CatalogConfig,

    /// Instance ownership policy (TODO §4.5). Optional; omitting the section
    /// keeps the trusted single-owner self-hosting behaviour.
    #[serde(default)]
    pub instance: InstanceConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CatalogConfig {
    /// Cache originals on disk for the owner's saved posts (the background
    /// media worker). The library itself — `accounts_post`, which every
    /// taste/scoring read derives from — is imported unconditionally by
    /// `/process`, the direct sync and `/browse/favorites`; this toggle only
    /// decides whether their *bytes* are downloaded. Default `false`.
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
    /// Whether the media-cache worker may download originals.
    ///
    /// This used to gate the library import too (`/process`, direct sync,
    /// `/browse/favorites`), which made the optional offline cache a
    /// prerequisite for the core — with both toggles off `/process` refused
    /// outright. Those paths are unconditional now; what remains opt-in is the
    /// bytes on disk, plus the `save_all` encountered scope.
    pub fn media_cache_enabled(&self) -> bool {
        self.save_favourites || self.save_all
    }
}

/// Instance ownership policy (TODO §4.5).
///
/// Ownership is one HttpOnly cookie that `POST /session/bootstrap` mints for
/// anybody, so the only thing that separates the instance owner from a visitor
/// is the ability to *claim* the accounts that carry no device link (the ones
/// `seed`/`calibrate` wrote, or a library whose cookie was cleared). This
/// section decides how that claim is proven.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct InstanceConfig {
    /// Shared secret the operator hands to the instance owner. Empty (the
    /// default) keeps trusted single-owner self-hosting: the first
    /// `/session/bootstrap` adopts every unlinked account. Set it on anything
    /// reachable by others — `bootstrap` then mints only an empty guest token
    /// and `POST /session/enroll` becomes the sole way to claim the instance.
    ///
    /// Deliberately *not* reusing `e621_key_encryption_secret` or `admin_api`:
    /// both are unrotatable in practice (the first hashes straight into the
    /// at-rest key, so changing it orphans every stored e621 key; the second is
    /// a third-party credential only e621 can reissue), and neither could be
    /// locked again after a leak. This value rotates on its own.
    #[serde(default)]
    pub owner_secret: String,
}

impl InstanceConfig {
    /// Whether claiming the instance requires proving `owner_secret`.
    #[must_use]
    pub fn ownership_requires_secret(&self) -> bool {
        !self.owner_secret.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeConfig {
    /// Cap on local-pool candidates merged into a `/recommendations` page.
    ///
    /// The newest posts come from the live e621 request; this is the local
    /// *supplement* merged into it, to reach posts the live query's tag budget
    /// cannot express. Streams are ordered newest-first by post id, so the
    /// supplement extends the page in time instead of jumping into the middle
    /// of the catalog. Default `400`.
    ///
    /// Cutting it to 80 was measured on 2026-10-27 and not adopted: it halved
    /// the local half of the pool without moving the pool's age distribution,
    /// and freshness is a property of the ordering, not of the size. Shrink
    /// this only to cut hydration and MMR cost per page.
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
    let cfg: Config = toml::from_str(&s).context("parsing config.toml")?;
    validate_admin_credentials(&cfg)?;
    Ok(cfg)
}

/// The shared admin credentials are what every server-side e621 call
/// authenticates with (`sync.rs`, `api.rs`), so an empty value is a
/// misconfiguration rather than a way to run anonymously: e621 would answer
/// with the anonymous rate limit and the `admin_user` account would stop
/// syncing. `toml` proves only that the key exists, not that it has a value.
fn validate_admin_credentials(cfg: &Config) -> anyhow::Result<()> {
    for (name, value) in [
        ("admin_user", &cfg.admin_user),
        ("admin_api", &cfg.admin_api),
    ] {
        if value.trim().is_empty() {
            anyhow::bail!("config: `{name}` is empty — shared e621 admin credentials are required");
        }
    }
    Ok(())
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
            .field("runtime", &self.0.runtime)
            .field("catalog", &self.0.catalog)
            .field(
                "instance.owner_secret",
                &shown(&self.0.instance.owner_secret),
            )
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
    fn load_config_empty_admin_credentials_fails() {
        let toml = MINIMAL_TOML.replace("\"test_key\"", "\"\"");
        let r = parse_toml(&toml);
        assert!(r.is_err(), "empty admin_api should fail");
        let toml = MINIMAL_TOML.replace("\"test_admin\"", "\"  \"");
        let r = parse_toml(&toml);
        assert!(r.is_err(), "whitespace-only admin_user should fail");
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

    #[test]
    fn instance_defaults_to_trusted_self_hosting() {
        // No `[instance]` section: the first bootstrap claims the instance.
        let cfg = parse_toml(MINIMAL_TOML).unwrap();
        assert!(cfg.instance.owner_secret.is_empty());
        assert!(!cfg.instance.ownership_requires_secret());
    }

    #[test]
    fn instance_owner_secret_switches_to_proof_of_secret() {
        let cfg = parse_toml(&format!(
            "{MINIMAL_TOML}\n\n[instance]\nowner_secret = \"hunter2\"\n"
        ))
        .unwrap();
        assert_eq!(cfg.instance.owner_secret, "hunter2");
        assert!(cfg.instance.ownership_requires_secret());
    }

    #[test]
    fn redacted_debug_masks_the_owner_secret() {
        // The redacted view is what gets logged, and this secret is the only
        // value that can claim the instance — a leak here is a leak of the
        // instance itself.
        let cfg = parse_toml(&format!(
            "{MINIMAL_TOML}\n\n[instance]\nowner_secret = \"hunter2\"\n"
        ))
        .unwrap();
        let shown = format!("{:?}", cfg.redacted());
        assert!(!shown.contains("hunter2"), "owner secret leaked: {shown}");
        assert!(shown.contains("<redacted, 7 chars>"), "{shown}");
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
