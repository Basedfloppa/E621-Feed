use reqwest::{Client, Response, StatusCode};
use rocket::serde::json;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant as StdInstant};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep, timeout};
use urlencoding::encode;

use crate::models::{Post, TruncatedAccount, UserApiResponse, UserSearchResult, cfg};

/// Global rate gate. Outbound sends share one token-bucket budget so
/// prefetchers and live fetches don't each enforce `rps_delay_ms`
/// independently. Held only across the wait, preserving FIFO order.
static RATE_GATE: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

async fn rate_gate_wait() {
    let cfg = cfg();
    let delay = Duration::from_millis(cfg.rps_delay_ms);
    let mut next = RATE_GATE.lock().await;
    let now = Instant::now();
    if *next > now {
        sleep(*next - now).await;
    }
    *next = Instant::now() + delay;
}

/// In-memory TTL cache for successful GET responses to e621. Dedupes
/// `/recommendations` round-trips that share an account or default
/// blacklist, reducing admin-key load and ban risk.
///
/// Bodies are stored as raw text (cheap re-parse, avoids `Any`-typed
/// values). Only 2xx is cached: 4xx/5xx must be retried so transient
/// Cloudflare blocks don't pin themselves for a full TTL.
///
/// The cache also supports idle-eviction (see `evict_api_cache_if_idle`):
/// every user-facing read/write touches a last-access timestamp; the
/// background cache-pruner checks this timestamp against
/// `runtime.cache_idle_eviction_secs` and drops the entire cache when
/// the box has been quiet long enough, so the ~1 GB of response bodies
/// doesn't stay resident indefinitely on an idle server.
struct CachedBody {
    body: String,
    inserted_at: std::time::Instant,
}

static API_CACHE: LazyLock<StdMutex<HashMap<String, CachedBody>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Tracks the last user-facing access to the API cache (read or write).
/// The cache pruner uses this to decide if the cache has been idle long
/// enough to justify a full eviction — same pattern as `IDF_CACHE` /
/// `GLOBAL_CACHE`. Background prefetch traffic (`bypass_cache=true`)
/// must NOT touch this timer so it doesn't prevent idle-eviction.
static API_CACHE_LAST_ACCESS: LazyLock<StdMutex<StdInstant>> =
    LazyLock::new(|| StdMutex::new(StdInstant::now()));

fn touch_api_cache_access() {
    let mut g = API_CACHE_LAST_ACCESS
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    *g = StdInstant::now();
}

fn api_cache_get(url: &str, ttl: Duration) -> Option<String> {
    if ttl.is_zero() {
        return None;
    }
    let map = API_CACHE.lock().expect("api cache poisoned");
    let entry = map.get(url)?;
    if entry.inserted_at.elapsed() < ttl {
        touch_api_cache_access();
        Some(entry.body.clone())
    } else {
        None
    }
}

/// Clear the entire e621 cache. Used on blacklist change so stale entries
/// keyed by the old blacklist don't linger until TTL expiry.
pub fn clear_api_cache() {
    let mut map = API_CACHE.lock().expect("api cache poisoned");
    map.clear();
}

fn remove_api_cache_entry(url: &str) {
    let mut map = API_CACHE.lock().expect("api cache poisoned");
    map.remove(url);
}

/// Drop every cache entry past TTL. Called by the periodic worker since
/// `api_cache_put` only evicts on insert.
pub fn prune_api_cache() -> (usize, usize) {
    let ttl = Duration::from_secs(cfg().e621_cache_ttl_secs);
    if ttl.is_zero() {
        return (0, 0);
    }
    let mut map = API_CACHE.lock().expect("api cache poisoned");
    let before = map.len();
    let now = std::time::Instant::now();
    map.retain(|_, v| now.duration_since(v.inserted_at) < ttl);
    let after = map.len();
    (before, after)
}

/// Drop the entire API cache if no user-facing request has read or written
/// it for at least `idle_secs`. Returns `(before, after)` entry counts, or
/// `(0, 0)` if eviction was skipped (idle timer still fresh or cache empty).
///
/// After eviction the next user request will cold-fill the cache as usual.
/// `idle_secs == 0` disables idle eviction.
pub fn evict_api_cache_if_idle(idle_secs: u64) -> (usize, usize) {
    if idle_secs == 0 {
        return (0, 0);
    }
    let elapsed = {
        let g = API_CACHE_LAST_ACCESS
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        g.elapsed()
    };
    if elapsed.as_secs() < idle_secs {
        return (0, 0);
    }
    let mut map = API_CACHE.lock().expect("api cache poisoned");
    let before = map.len();
    if before == 0 {
        return (0, 0);
    }
    map.clear();
    let after = 0usize;
    // Reset the access timer so consecutive pruner ticks don't log
    // spurious "cleared 0 entries" against an already-empty cache.
    {
        let mut g = API_CACHE_LAST_ACCESS
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *g = StdInstant::now();
    }
    (before, after)
}

fn api_cache_put(url: &str, body: String, ttl: Duration, max_entries: usize) {
    if ttl.is_zero() || max_entries == 0 {
        return;
    }
    let mut map = API_CACHE.lock().expect("api cache poisoned");
    if map.len() >= max_entries {
        // Drop oldest 10% in one O(n) pass — strict LRU bookkeeping isn't
        // worth it given how rarely we cross `max_entries`.
        let now = std::time::Instant::now();
        let mut keys_by_age: Vec<(String, std::time::Instant)> = map
            .iter()
            .map(|(k, v)| (k.clone(), v.inserted_at))
            .collect();
        keys_by_age.sort_by_key(|(_, t)| *t);
        let evict_n = max_entries / 10 + 1;
        for (k, _) in keys_by_age.into_iter().take(evict_n) {
            map.remove(&k);
        }
        // Also drop everything past the TTL boundary regardless.
        map.retain(|_, v| now.duration_since(v.inserted_at) < ttl);
    }
    touch_api_cache_access();
    map.insert(
        url.to_string(),
        CachedBody {
            body,
            inserted_at: std::time::Instant::now(),
        },
    );
}

/// Authenticated GET with optional cache + rate-gate + retry. All e621
/// calls funnel through here so cache and rate limits stay consistent.
///
/// * `bypass_cache` — when true the response is fetched live from e621
///   and the result is NOT written into the shared cache. Use for
///   background prefetch traffic so it doesn't pollute the user-facing
///   cache or evict entries that real requests will need.
/// * `cache_ttl_secs` — per-call TTL override (0 = use global default).
async fn fetch_authed_text(
    url: String,
    bypass_cache: bool,
    cache_ttl_secs: u64,
) -> Result<String, String> {
    let cfg = cfg();
    let ttl = Duration::from_secs(if cache_ttl_secs > 0 {
        cache_ttl_secs
    } else {
        cfg.e621_cache_ttl_secs
    });
    let max_entries = cfg.e621_cache_max_entries;

    if !bypass_cache && let Some(body) = api_cache_get(&url, ttl) {
        debug!("e621 cache hit: {url}");
        return Ok(body);
    }

    let client = get_client();
    let resp = send_with_retry(
        client
            .get(&url)
            .basic_auth(cfg.admin_user.clone(), Some(cfg.admin_api.clone())),
    )
    .await
    .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("body read failed: {e}"))?;

    if !status.is_success() {
        // Don't cache Cloudflare/rate-limit pages — would pin an outage past recovery.
        let preview = body_preview(&body);
        return Err(format!("returned {status}: {preview}"));
    }

    if !bypass_cache {
        api_cache_put(&url, body.clone(), ttl, max_entries);
    } else {
        debug!("e621 cache bypassed (prefetch): {url}");
    }
    Ok(body)
}

/// First non-empty line, capped to 160 chars — keeps Cloudflare HTML
/// walls out of logs and error strings while preserving enough text to
/// distinguish block page vs rate limit vs JSON error envelope.
fn body_preview(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(160)
        .collect()
}

fn build_url(path: &str, params: &[(&str, String)]) -> String {
    let cfg = cfg();
    let url = if params.is_empty() {
        format!("{}/{path}", cfg.posts_domain)
    } else {
        let qs = params
            .iter()
            .map(|(k, v)| format!("{k}={}", encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}/{path}?{qs}", cfg.posts_domain)
    };
    trace!("build_url: path={path} -> {url}");
    url
}

static HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    let cfg = cfg();
    info!(
        "Building shared HTTP client (user_agent={})",
        cfg.user_agent
    );
    Client::builder()
        .user_agent(cfg.user_agent.clone())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .build()
        .map_err(|e| {
            error!("Failed to build client: {e}");
            format!("Failed to build client: {e}")
        })
        .unwrap()
});

fn get_client() -> &'static Client {
    &HTTP_CLIENT
}

/// Pull diagnostic headers off a response into a compact string suitable
/// for log lines. Captures Cloudflare's edge identifiers, e621's
/// origin timing/request id, and any rate-limit hints. Empty fields
/// are skipped so the log isn't full of `n/a` noise.
fn diag_headers(resp: &Response) -> String {
    let h = resp.headers();
    let take = |name: &str| {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| format!(" {name}={s}"))
            .unwrap_or_default()
    };
    format!(
        "{cf_ray}{cf_cache}{cf_mit}{server}{ratelimit}{rl_remaining}{rl_reset}{retry_after}{runtime}{req_id}",
        cf_ray = take("cf-ray"),
        cf_cache = take("cf-cache-status"),
        cf_mit = take("cf-mitigated"),
        server = take("server"),
        ratelimit = take("x-ratelimit-limit"),
        rl_remaining = take("x-ratelimit-remaining"),
        rl_reset = take("x-ratelimit-reset"),
        retry_after = take("retry-after"),
        runtime = take("x-runtime"),
        req_id = take("x-request-id"),
    )
}

/// Best-effort URL stringification of a `RequestBuilder` for logs. We
/// `try_clone+build` so the original builder remains usable. Returns
/// "<unknown-url>" on any failure rather than panicking.
fn builder_url(builder: &reqwest::RequestBuilder) -> String {
    builder
        .try_clone()
        .and_then(|b| b.build().ok())
        .map(|r| r.url().to_string())
        .unwrap_or_else(|| "<unknown-url>".to_string())
}

/// Classify a `reqwest::Error` into a short tag so log greppers can
/// distinguish connection-level vs body-stream-level failures without
/// reading full Debug output. The body-vs-connect distinction is the
/// one that matters most when diagnosing Cloudflare throttling.
///
/// Order matters: `body` is checked before `timeout` because a body
/// stream that times out mid-read flips BOTH predicates true, and the
/// body case is the operationally interesting one (Cloudflare slow-
/// lane throttling) — we don't want it hidden behind the generic
/// `timeout` tag.
fn err_kind(e: &reqwest::Error) -> &'static str {
    if e.is_connect() {
        "connect"
    } else if e.is_body() {
        "body"
    } else if e.is_timeout() {
        "timeout"
    } else if e.is_decode() {
        "decode"
    } else if e.is_request() {
        "request"
    } else if e.is_redirect() {
        "redirect"
    } else if e.is_status() {
        "status"
    } else {
        "other"
    }
}

async fn send_with_retry(builder: reqwest::RequestBuilder) -> Result<Response, String> {
    let mut delay: Duration = Duration::from_millis(300);
    let cfg = cfg();
    // Captured once so we can include the URL in error/warning logs
    // even after `builder` has been moved into `.send()`.
    let url_for_logs = builder_url(&builder);

    for attempt in 0..=cfg.max_retries {
        debug!(
            "HTTP attempt {}/{}: {} (rps_delay={}ms)",
            attempt + 1,
            cfg.max_retries + 1,
            url_for_logs,
            cfg.rps_delay_ms
        );

        rate_gate_wait().await;

        let attempt_start = std::time::Instant::now();
        // Hard ceiling per attempt. `reqwest::ClientBuilder::timeout`
        // SHOULD enforce the total request budget, but in practice
        // Cloudflare's slow-lane throttle (favorites.json under
        // admin-token throttle) trickles bytes through which keeps
        // reqwest's internal timers happy — observed attempts run
        // 76-178 seconds against a 30s configured timeout. This
        // outer `tokio::time::timeout` is non-negotiable: when it
        // fires, the request future is dropped, the underlying
        // socket is closed, and retry logic gets a clean state.
        let attempt_budget = Duration::from_secs(cfg.attempt_timeout_secs.max(5));
        let req = builder.try_clone().ok_or_else(|| {
            let m = "unable to clone request".to_string();
            error!("{m}");
            m
        })?;
        let send_fut = req.send();
        let result = match timeout(attempt_budget, send_fut).await {
            Ok(inner) => inner,
            Err(_elapsed) => {
                let elapsed = attempt_start.elapsed();
                if attempt < cfg.max_retries {
                    warn!(
                        "Request hard-timeout after {:.2}s (budget {:?}) on attempt {}/{} for {}. Retrying in {:?}",
                        elapsed.as_secs_f64(),
                        attempt_budget,
                        attempt + 1,
                        cfg.max_retries + 1,
                        url_for_logs,
                        delay
                    );
                    sleep(delay).await;
                    delay = delay.saturating_mul(2);
                    continue;
                }
                error!(
                    "Request hard-timeout after {:.2}s on final attempt {} for {}",
                    elapsed.as_secs_f64(),
                    cfg.max_retries + 1,
                    url_for_logs
                );
                return Err(format!(
                    "request failed after retries [hard-timeout]: {:.2}s budget exceeded",
                    attempt_budget.as_secs_f64()
                ));
            }
        };
        return match result {
            Ok(resp) => {
                let status = resp.status();
                let elapsed = attempt_start.elapsed();
                trace!("HTTP status received: {status}");

                if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                    && attempt < cfg.max_retries
                {
                    let diag = diag_headers(&resp);
                    warn!(
                        "Request got {} ({:.2}s) for {}.{} Backing off for {:?} (attempt {}/{})",
                        status,
                        elapsed.as_secs_f64(),
                        url_for_logs,
                        diag,
                        delay,
                        attempt + 1,
                        cfg.max_retries + 1
                    );
                    sleep(delay).await;
                    delay = delay.saturating_mul(2);
                    continue;
                }

                let diag = diag_headers(&resp);
                if status.is_success() {
                    debug!(
                        "Request succeeded {} ({:.2}s) for {}.{}",
                        status,
                        elapsed.as_secs_f64(),
                        url_for_logs,
                        diag
                    );
                } else {
                    warn!(
                        "Request completed with non-retryable status {} ({:.2}s) for {}.{}",
                        status,
                        elapsed.as_secs_f64(),
                        url_for_logs,
                        diag
                    );
                }
                Ok(resp)
            }
            Err(e) => {
                let elapsed = attempt_start.elapsed();
                let kind = err_kind(&e);
                if attempt < cfg.max_retries {
                    warn!(
                        "Request error [{kind}] on attempt {}/{} ({:.2}s) for {}: {}. Retrying in {:?}",
                        attempt + 1,
                        cfg.max_retries + 1,
                        elapsed.as_secs_f64(),
                        url_for_logs,
                        e,
                        delay
                    );
                    sleep(delay).await;
                    delay = delay.saturating_mul(2);
                    continue;
                }
                error!(
                    "Request failed after {} attempts (final attempt {:.2}s) [{kind}] for {}: {}",
                    cfg.max_retries + 1,
                    elapsed.as_secs_f64(),
                    url_for_logs,
                    e
                );
                Err(format!("request failed after retries [{kind}]: {e}"))
            }
        };
    }

    error!("send_with_retry exhausted attempts but reached unreachable branch");
    Err("unreachable".into())
}

#[derive(Debug)]
pub enum FavoritesPageError {
    Request(String),
    Malformed(String),
}

impl FavoritesPageError {
    pub fn is_malformed(&self) -> bool {
        matches!(self, Self::Malformed(_))
    }
}

impl std::fmt::Display for FavoritesPageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(message) | Self::Malformed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for FavoritesPageError {}

/// Fetch one page of favourites for `account`.
///
/// Returns the posts from one favourites page.
///
/// An empty vector is reserved for a valid empty API page. HTTP failures and
/// malformed 2xx bodies are returned as errors so callers cannot mistake a
/// schema change or error envelope for the end of pagination.
pub async fn get_favorites(
    account: &TruncatedAccount,
    page: i32,
) -> Result<Vec<Post>, FavoritesPageError> {
    info!("Fetching favorites: user_id={} page={}", account.id, page);

    let cfg = cfg();
    let url = build_url(
        "favorites.json",
        &[
            ("user_id", account.id.to_string()),
            ("limit", cfg.posts_limit.to_string()),
            ("page", page.to_string()),
            ("v2", true.to_string()),
            ("mode", "extended".to_string()),
        ],
    );
    debug!("GET (auth) /favorites.json?user_id=…&limit=…&page={page}");

    let body = match fetch_authed_text(url.clone(), false, 0).await {
        Ok(b) => b,
        Err(e) => {
            warn!("favorites request failed: {e}");
            return Err(FavoritesPageError::Request(format!(
                "favorites page {page}: {e}"
            )));
        }
    };

    let posts = json::from_str::<Vec<Post>>(&body).map_err(|e| {
        let preview = body_preview(&body);
        remove_api_cache_entry(&url);
        warn!("favorites page {page} parse failed: {e}; first bytes: {preview}");
        FavoritesPageError::Malformed(format!(
            "favorites page {page}: malformed 200 response: {e}; first bytes: {preview}"
        ))
    })?;

    info!("Fetched {} favorite posts", posts.len());
    Ok(posts)
}

pub async fn get_account(account: &TruncatedAccount) -> Result<UserApiResponse, String> {
    info!(
        "Fetching account: id={} name='{}'",
        account.id, account.name
    );
    let cfg = cfg();
    let url = format!("{}/users/{}.json", cfg.posts_domain, account.id);
    debug!("GET (auth) {url}");
    let body = fetch_authed_text(url, false, 0)
        .await
        .map_err(|e| format!("account request {e}"))?;
    let parsed = json::from_str::<UserApiResponse>(&body)
        .map_err(|e| format!("account parse failed: {e}"))?;
    info!("Fetched account successfully for id={}", account.id);
    Ok(parsed)
}

/// Page through the e621 user list, ordered by `order` (e.g.
/// `post_upload_count`, `name`, `date`). Up to ~320 results per page.
pub async fn search_users(order: &str, page: i32) -> Result<Vec<UserSearchResult>, String> {
    let url = build_url(
        "users.json",
        &[
            ("search[order]", order.to_string()),
            ("limit", "320".to_string()),
            ("page", page.to_string()),
        ],
    );
    let body = fetch_authed_text(url, false, 0)
        .await
        .map_err(|e| format!("users search {e}"))?;
    // Endpoint returns either `{ "users": [...] }` or a bare array.
    if let Ok(arr) = json::from_str::<Vec<UserSearchResult>>(&body) {
        return Ok(arr);
    }
    #[derive(serde::Deserialize)]
    struct Wrap {
        users: Vec<UserSearchResult>,
    }
    let wrapped: Wrap =
        json::from_str(&body).map_err(|e| format!("users search parse failed: {e}"))?;
    Ok(wrapped.users)
}

/// `UserApiResponse` for a given user id; needed for `favorite_count`
/// which the search endpoint does not include.
pub async fn get_user_by_id(uid: i32) -> Result<UserApiResponse, String> {
    let cfg = cfg();
    let url = format!("{}/users/{}.json", cfg.posts_domain, uid);
    let body = fetch_authed_text(url, false, 0)
        .await
        .map_err(|e| format!("user-by-id {e}"))?;
    json::from_str::<UserApiResponse>(&body).map_err(|e| format!("user-by-id parse failed: {e}"))
}

/// Resolve a user by name via `/users/<name>.json`. Lets first-time
/// visitors look up accounts before they're linked to a device.
pub async fn get_user_by_name(name: &str) -> Result<UserApiResponse, String> {
    let cfg = cfg();
    let url = format!("{}/users/{}.json", cfg.posts_domain, encode(name));
    let body = fetch_authed_text(url, false, 0)
        .await
        .map_err(|e| format!("user-by-name {e}"))?;
    json::from_str::<UserApiResponse>(&body).map_err(|e| format!("user-by-name parse failed: {e}"))
}

/// Normalise the order of blacklist tags so two semantically identical
/// blacklists produce the same cache key regardless of line order.
fn normalise_blacklist(bl: &str) -> String {
    let mut tags: Vec<&str> = bl
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    tags.sort();
    tags.join(" -")
}

/// `get_posts` with a caller-supplied `tags` query — used by the
/// prefetcher to warm the catalog. Blacklist still applied.
///
/// This function bypasses the shared API cache by design — the prefetcher
/// runs on a background schedule and its requests should never evict or
/// pollute entries that user-facing `/recommendations` calls depend on.
pub async fn get_posts_by_tags(
    blacklist_tags: &str,
    tags_query: &str,
    page: Option<i32>,
) -> Result<Vec<Post>, String> {
    let blacklist = if blacklist_tags.trim().is_empty() {
        String::new()
    } else {
        let normalised = normalise_blacklist(blacklist_tags);
        if normalised.is_empty() {
            String::new()
        } else {
            format!("-{normalised}")
        }
    };
    let combined = if blacklist.is_empty() {
        tags_query.to_string()
    } else if tags_query.trim().is_empty() {
        blacklist
    } else {
        format!("{tags_query} {blacklist}")
    };
    let cfg = cfg();
    let url = build_url(
        "posts.json",
        &[
            ("limit", cfg.posts_limit.to_string()),
            ("page", page.unwrap_or(1).to_string()),
            ("tags", combined),
            ("v2", true.to_string()),
            ("mode", "extended".to_string()),
        ],
    );
    // Bypass cache — prefetch traffic must not pollute user-facing cache.
    let body = fetch_authed_text(url, true, 0)
        .await
        .map_err(|e| format!("posts request {e}"))?;
    let posts =
        json::from_str::<Vec<Post>>(&body).map_err(|e| format!("posts parse failed: {e}"))?;
    Ok(posts)
}

/// Fetch posts by their IDs from e621. Uses `id:12345,67890` syntax.
/// Respects the global rate gate and TTL cache.
pub async fn get_posts_by_ids(ids: &[i64]) -> Result<Vec<Post>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let id_list: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    let tags = format!("id:{}", id_list.join(","));
    let cfg = cfg();
    let url = build_url(
        "posts.json",
        &[
            ("limit", cfg.posts_limit.to_string()),
            ("tags", tags),
            ("page", "1".to_string()),
            ("v2", true.to_string()),
            ("mode", "extended".to_string()),
        ],
    );
    let body = fetch_authed_text(url, true, 0)
        .await
        .map_err(|e| format!("posts by ids request: {e}"))?;
    let posts =
        json::from_str::<Vec<Post>>(&body).map_err(|e| format!("posts parse failed: {e}"))?;
    Ok(posts)
}

/// Per-page TTL so the first feed page (which users see most often)
/// refreshes faster than deeper scroll pages. Page 1: 2 minutes, other
/// pages: configured global TTL.
fn posts_cache_ttl(page: Option<i32>) -> u64 {
    match page.unwrap_or(1) {
        1 => 120,                       // first page — fresh content matters
        _ => cfg().e621_cache_ttl_secs, // deeper pages — longer TTL
    }
}

pub async fn get_posts(account: &TruncatedAccount, page: Option<i32>) -> Result<Vec<Post>, String> {
    let blacklisted_tags = account.blacklist.clone();
    let blacklist = if blacklisted_tags.trim().is_empty() {
        String::new()
    } else {
        let normalised = normalise_blacklist(&blacklisted_tags);
        if normalised.is_empty() {
            String::new()
        } else {
            format!("-{normalised}")
        }
    };
    debug!(
        "Preparing posts fetch: page={} blacklist_len={}",
        page.unwrap_or(1),
        if blacklist.is_empty() {
            0
        } else {
            blacklist.split_whitespace().count()
        }
    );
    let cfg = cfg();
    let ttl_secs = posts_cache_ttl(page);
    let url = build_url(
        "posts.json",
        &[
            ("limit", cfg.posts_limit.to_string()),
            ("page", page.unwrap_or(1).to_string()),
            ("tags", blacklist),
            ("v2", true.to_string()),
            ("mode", "extended".to_string()),
        ],
    );
    debug!("GET (auth) {url}");
    let body = fetch_authed_text(url, false, ttl_secs)
        .await
        .map_err(|e| format!("posts request {e}"))?;
    let posts =
        json::from_str::<Vec<Post>>(&body).map_err(|e| format!("posts parse failed: {e}"))?;

    info!("Fetched {} posts", posts.len());
    Ok(posts)
}

/// Fetch one page of tag relation data from e621.
/// Used for both `/tag_aliases.json` and `/tag_implications.json`.
pub async fn fetch_tag_relations<T>(endpoint: &str, page: i32) -> Result<Vec<T>, String>
where
    T: serde::de::DeserializeOwned,
{
    let limit = crate::models::cfg().posts_limit.to_string();
    let url = build_url(endpoint, &[("limit", limit), ("page", page.to_string())]);
    debug!("GET (auth) /{endpoint} page={page}");
    let body = fetch_authed_text(url, false, 86400)
        .await
        .map_err(|e| format!("{endpoint} request page {page}: {e}"))?;
    let entries: Vec<T> = rocket::serde::json::from_str(&body)
        .map_err(|e| format!("{endpoint} parse failed page {page}: {e}"))?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::normalise_blacklist;

    #[test]
    fn normalise_blacklist_sorts_and_drops_blanks() {
        assert_eq!(normalise_blacklist(""), "");
        assert_eq!(normalise_blacklist("   \n\n  "), "");
        assert_eq!(normalise_blacklist("solo"), "solo");
        // Lines are trimmed, sorted, and re-joined with the " -" separator
        // so two blacklists differing only in line order share a cache key.
        assert_eq!(normalise_blacklist("  b \n a \n c "), "a -b -c");
        assert_eq!(normalise_blacklist("c\nb\na"), "a -b -c");
        assert_eq!(normalise_blacklist("z\n\n\ny"), "y -z");
    }
}
