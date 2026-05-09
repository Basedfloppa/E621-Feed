use reqwest::{Client, Response, StatusCode};
use rocket::serde::json;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};
use urlencoding::encode;

use crate::models::{
    Post, PostsApiResponse, TruncatedAccount, UserApiResponse, UserSearchResult, cfg,
};

/// Global rate gate. Every outbound HTTP send takes this lock and waits until
/// `next_send_at` arrives, ensuring background prefetchers and live request
/// fetches share the same token-bucket budget rather than each enforcing
/// `rps_delay_ms` independently. The lock is held only across the wait, not
/// across the network round-trip — so 250 ms cadence is preserved without
/// serializing the requests themselves.
static RATE_GATE: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

async fn rate_gate_wait() {
    let cfg = cfg();
    let delay = Duration::from_millis(cfg.rps_delay_ms);
    let mut next = RATE_GATE.lock().await;
    let now = Instant::now();
    if *next > now {
        let wait = *next - now;
        // Bound the held-lock window: drop the guard, sleep, reacquire would
        // re-order callers. Holding through the sleep keeps order FIFO and
        // sleeps are at most rps_delay_ms (~250ms typical).
        sleep(wait).await;
    }
    *next = Instant::now() + delay;
}

/// In-memory TTL cache for successful GET responses to e621. The hot
/// case is `/recommendations` — two devices on the same account, or two
/// accounts with the same default blacklist, otherwise turn into
/// duplicate `posts.json` round-trips against the admin's API key. The
/// audit (C-3, C-4) showed the same admin key under Cloudflare scrutiny
/// already, so deduping outbound traffic also reduces ban risk.
///
/// Stored bodies are raw text — re-parsing on hit is cheap relative to
/// a network round-trip, and avoids an `Any`-typed cache value. Only
/// 2xx responses are cached: 4xx/5xx (especially Cloudflare 403/520)
/// must be re-tried so a transient block doesn't pin itself for the
/// full TTL.
struct CachedBody {
    body: String,
    inserted_at: std::time::Instant,
}

static API_CACHE: LazyLock<StdMutex<HashMap<String, CachedBody>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn api_cache_get(url: &str, ttl: Duration) -> Option<String> {
    if ttl.is_zero() {
        return None;
    }
    let map = API_CACHE.lock().expect("api cache poisoned");
    let entry = map.get(url)?;
    if entry.inserted_at.elapsed() < ttl {
        Some(entry.body.clone())
    } else {
        None
    }
}

/// Drop every cache entry past TTL. Used by the periodic cache-validator
/// worker so stale bodies don't sit in memory after a traffic burst —
/// `api_cache_put` only evicts on insert, which leaves the cache
/// frozen at peak size when the request volume drops. Returns
/// `(before, after)` for logging.
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

fn api_cache_put(url: &str, body: String, ttl: Duration, max_entries: usize) {
    if ttl.is_zero() || max_entries == 0 {
        return;
    }
    let mut map = API_CACHE.lock().expect("api cache poisoned");
    if map.len() >= max_entries {
        // Drop the oldest 10% in one pass — cheap O(n) compared to
        // bookkeeping a strict LRU list, and the threshold gets hit
        // rarely (only past `max_entries`). Stale-by-TTL entries that
        // have been idle for a while are also caught here.
        let now = std::time::Instant::now();
        let mut keys_by_age: Vec<(String, std::time::Instant)> = map
            .iter()
            .filter(|(_, v)| now.duration_since(v.inserted_at) < ttl * 2)
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
    map.insert(
        url.to_string(),
        CachedBody {
            body,
            inserted_at: std::time::Instant::now(),
        },
    );
}

/// Authenticated GET with cache + rate-gate + retry. Returns the body
/// text on 2xx, or a formatted error string for callers to log/wrap.
/// All call sites that hit e621 funnel through here so the cache and
/// rate limits stay consistent.
async fn fetch_authed_text(url: String) -> Result<String, String> {
    let cfg = cfg();
    let ttl = Duration::from_secs(cfg.e621_cache_ttl_secs);
    let max_entries = cfg.e621_cache_max_entries;

    if let Some(body) = api_cache_get(&url, ttl) {
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
        // Don't poison the cache with a Cloudflare/rate-limit page —
        // that would pin a 10-minute outage even after upstream recovers.
        let preview = body_preview(&body);
        return Err(format!("returned {status}: {preview}"));
    }

    api_cache_put(&url, body.clone(), ttl, max_entries);
    Ok(body)
}

/// Trim a non-success response body down to one short line for logs and
/// error-string propagation. The audit (S-7) flagged that Cloudflare's
/// `<!DOCTYPE html>... <!--[if lt IE 7]> ...` walls were being copied
/// verbatim into both stderr and the user-facing 500 string, blowing up
/// log volume and shipping ~120 bytes of useless markup down to the
/// frontend on every failure. First non-empty line, capped to 160
/// characters, is enough to recognise "CF block page" vs "rate limit"
/// vs "JSON error envelope" without dragging the rest along.
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
    info!("Building shared HTTP client (user_agent={})", cfg.user_agent);
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

async fn send_with_retry(builder: reqwest::RequestBuilder) -> Result<Response, String> {
    let mut delay: Duration = Duration::from_millis(300);
    let cfg = cfg();

    for attempt in 0..=cfg.max_retries {
        if let Some(b) = builder.try_clone() {
            match b.build() {
                Ok(req) => debug!(
                    "HTTP attempt {}/{}: {} {} (rps_delay={}ms)",
                    attempt + 1,
                    cfg.max_retries + 1,
                    req.method(),
                    req.url(),
                    cfg.rps_delay_ms
                ),
                Err(e) => warn!("Could not build request for logging: {e}"),
            }
        } else {
            warn!(
                "Unable to clone request for logging on attempt {}",
                attempt + 1
            );
        }

        rate_gate_wait().await;

        return match builder
            .try_clone()
            .ok_or_else(|| {
                let m = "unable to clone request".to_string();
                error!("{m}");
                m
            })?
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                trace!("HTTP status received: {status}");

                if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                    && attempt < cfg.max_retries
                {
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("n/a");
                    warn!(
                        "Request got {} (retry-after: {}). Backing off for {:?} (attempt {}/{})",
                        status,
                        retry_after,
                        delay,
                        attempt + 1,
                        cfg.max_retries + 1
                    );
                    sleep(delay).await;
                    delay = delay.saturating_mul(2);
                    continue;
                }

                if status.is_success() {
                    info!("Request succeeded with {status}");
                } else {
                    warn!("Request completed with non-retryable status {status}");
                }
                Ok(resp)
            }
            Err(e) => {
                if attempt < cfg.max_retries {
                    warn!(
                        "Request error on attempt {}/{}: {:?}. Retrying in {:?}",
                        attempt + 1,
                        cfg.max_retries + 1,
                        e,
                        delay
                    );
                    sleep(delay).await;
                    delay = delay.saturating_mul(2);
                    continue;
                }
                error!(
                    "Request failed after {} attempts: {}",
                    cfg.max_retries + 1,
                    e
                );
                Err(format!("request failed after retries: {e}"))
            }
        };
    }

    error!("send_with_retry exhausted attempts but reached unreachable branch");
    Err("unreachable".into())
}

pub async fn get_favorites(account: &TruncatedAccount, page: i32) -> Vec<Post> {
    info!("Fetching favorites: user_id={} page={}", account.id, page);

    let cfg = cfg();
    let url = build_url(
        "favorites.json",
        &[
            ("user_id", account.id.to_string()),
            ("limit", cfg.posts_limit.to_string()),
            ("page", page.to_string()),
        ],
    );
    debug!("GET (auth) /favorites.json?user_id=…&limit=…&page={page}");

    let body = match fetch_authed_text(url).await {
        Ok(b) => b,
        Err(e) => {
            warn!("favorites request failed: {e}");
            return Vec::new();
        }
    };

    let posts = match json::from_str::<PostsApiResponse>(&body) {
        Ok(r) => r.posts,
        Err(e) => {
            let preview = body_preview(&body);
            warn!("favorites parse failed: {e}; first bytes: {preview}");
            return Vec::new();
        }
    };

    info!("Fetched {} favorite posts", posts.len());
    posts
}

pub async fn get_account(account: &TruncatedAccount) -> Result<UserApiResponse, String> {
    info!(
        "Fetching account: id={} name='{}'",
        account.id, account.name
    );
    let cfg = cfg();
    let url = format!("{}/users/{}.json", cfg.posts_domain, account.id);
    debug!("GET (auth) {url}");
    let body = fetch_authed_text(url)
        .await
        .map_err(|e| format!("account request {e}"))?;
    let parsed = json::from_str::<UserApiResponse>(&body)
        .map_err(|e| format!("account parse failed: {e}"))?;
    info!("Fetched account successfully for id={}", account.id);
    Ok(parsed)
}

/// Page through the e621 user list, ordered by `order` (e.g.
/// `post_upload_count`, `name`, `date`). Returns up to ~320 results per page.
/// Used by the `seed` binary to bootstrap calibration data without going
/// through the regular account-creation flow.
pub async fn search_users(order: &str, page: i32) -> Result<Vec<UserSearchResult>, String> {
    let url = build_url(
        "users.json",
        &[
            ("search[order]", order.to_string()),
            ("limit", "320".to_string()),
            ("page", page.to_string()),
        ],
    );
    let body = fetch_authed_text(url)
        .await
        .map_err(|e| format!("users search {e}"))?;
    // The endpoint occasionally responds with `{ "users": [...] }` and
    // occasionally with a bare array. Handle both.
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

/// Returns the full `UserApiResponse` for a given user id (used to read
/// `favorite_count`, which the search endpoint does not include). Public
/// counterpart to `get_account` — that one needs a `TruncatedAccount`, this
/// one only needs an id.
pub async fn get_user_by_id(uid: i32) -> Result<UserApiResponse, String> {
    let cfg = cfg();
    let url = format!("{}/users/{}.json", cfg.posts_domain, uid);
    let body = fetch_authed_text(url)
        .await
        .map_err(|e| format!("user-by-id {e}"))?;
    json::from_str::<UserApiResponse>(&body).map_err(|e| format!("user-by-id parse failed: {e}"))
}

/// Resolve a user by name. e621's `/users/<x>.json` endpoint accepts
/// either a numeric id or a name string, so the shape mirrors
/// `get_user_by_id`. Used by the `/api/user/name/<name>` route as a
/// fallback when the username isn't linked to the device yet — the
/// audit (M-2) called out that the previous "search only finds
/// already-linked accounts" UX trapped first-time visitors.
pub async fn get_user_by_name(name: &str) -> Result<UserApiResponse, String> {
    let cfg = cfg();
    let url = format!("{}/users/{}.json", cfg.posts_domain, encode(name));
    let body = fetch_authed_text(url)
        .await
        .map_err(|e| format!("user-by-name {e}"))?;
    json::from_str::<UserApiResponse>(&body)
        .map_err(|e| format!("user-by-name parse failed: {e}"))
}

/// Same as `get_posts` but with a caller-supplied `tags` query — used by the
/// background prefetcher to warm the catalog with content matched to specific
/// users' top tags. The blacklist still applies, prepended to the query.
pub async fn get_posts_by_tags(
    blacklist_tags: &str,
    tags_query: &str,
    page: Option<i32>,
) -> Result<Vec<Post>, String> {
    let bl = blacklist_tags.trim();
    let blacklist = if bl.is_empty() {
        String::new()
    } else {
        format!("-{}", bl.replace('\n', " -"))
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
            ("page", page.unwrap_or(0).to_string()),
            ("tags", combined),
        ],
    );
    let body = fetch_authed_text(url)
        .await
        .map_err(|e| format!("posts request {e}"))?;
    let posts = json::from_str::<PostsApiResponse>(&body)
        .map_err(|e| format!("posts parse failed: {e}"))?
        .posts;
    Ok(posts)
}

pub async fn get_posts(account: &TruncatedAccount, page: Option<i32>) -> Result<Vec<Post>, String> {
    let blacklisted_tags = account.blacklist.clone();
    let blacklist = if blacklisted_tags.trim().is_empty() {
        String::new()
    } else {
        format!("-{}", blacklisted_tags.replace('\n', " -"))
    };
    debug!(
        "Preparing posts fetch: page={} blacklist_len={}",
        page.unwrap_or(0),
        blacklist.split_whitespace().count()
    );
    let cfg = cfg();
    let url = build_url(
        "posts.json",
        &[
            ("limit", cfg.posts_limit.to_string()),
            ("page", page.unwrap_or(0).to_string()),
            ("tags", blacklist),
        ],
    );
    debug!("GET (auth) {url}");
    let body = fetch_authed_text(url)
        .await
        .map_err(|e| format!("posts request {e}"))?;
    let posts = json::from_str::<PostsApiResponse>(&body)
        .map_err(|e| format!("posts parse failed: {e}"))?
        .posts;

    info!("Fetched {} posts", posts.len());
    Ok(posts)
}
