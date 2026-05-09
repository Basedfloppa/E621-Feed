//! In-memory token-bucket rate limiter.
//!
//! Caps per-device and per-IP traffic on `/account`, `/process`, and
//! `/recommendations` so each request can't be amplified into an
//! admin-authenticated e621 round-trip.
//!
//! * `Mutex<HashMap<...>>` — buckets are 16 bytes, lock contention is fine.
//! * Token bucket — burst tolerant for users catching up after a tab background.
//! * Periodic GC drops full+idle entries so spraying distinct keys
//!   (random tokens, spoofed XFF) can't grow the table without bound.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use rocket::Request;
use rocket::request::{self, FromRequest};
use rocket_okapi::request::{OpenApiFromRequest, RequestHeaderInput};

use crate::errors::ApiError;

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
    /// Capacity at last access — used by GC to detect full+idle entries.
    burst: f64,
}

const GC_INTERVAL: Duration = Duration::from_secs(300);
const GC_IDLE_THRESHOLD: Duration = Duration::from_secs(900);
/// Past this we evict the oldest half regardless of idle state to avoid OOM.
const MAX_BUCKETS: usize = 50_000;

static BUCKETS: LazyLock<Mutex<HashMap<String, Bucket>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LAST_GC: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

/// Take one token from the named bucket. Returns `Err(TooManyRequests)`
/// when none available. Example: `check("acct_create:ip:1.2.3.4", 5, 5)`.
pub fn check(key: &str, per_min: u32, burst: u32) -> Result<(), ApiError> {
    let rate_per_sec = f64::from(per_min) / 60.0;
    let burst_f = f64::from(burst.max(1));
    let now = Instant::now();

    let mut map = BUCKETS.lock().expect("ratelimit map poisoned");

    let bucket = map.entry(key.to_string()).or_insert(Bucket {
        tokens: burst_f,
        last_refill: now,
        burst: burst_f,
    });
    bucket.burst = burst_f;

    let dt = now.duration_since(bucket.last_refill).as_secs_f64();
    bucket.tokens = (bucket.tokens + dt * rate_per_sec).min(burst_f);
    bucket.last_refill = now;

    let allowed = if bucket.tokens >= 1.0 {
        bucket.tokens -= 1.0;
        true
    } else {
        false
    };

    // GC under the same lock; expensive pruning is gated behind the timer.
    maybe_gc(&mut map, now);

    if allowed {
        Ok(())
    } else {
        Err(ApiError::TooManyRequests(format!(
            "rate limit exceeded ({per_min}/min); slow down and retry"
        )))
    }
}

/// Force a prune cycle bypassing `LAST_GC`. Called by the periodic
/// worker since `maybe_gc` only runs inside `check()`.
pub fn prune_buckets() -> (usize, usize) {
    let now = Instant::now();
    let mut map = BUCKETS.lock().expect("ratelimit map poisoned");
    let before = map.len();
    let oversized = map.len() > MAX_BUCKETS;
    if oversized {
        let mut entries: Vec<(String, Instant)> =
            map.iter().map(|(k, v)| (k.clone(), v.last_refill)).collect();
        entries.sort_by_key(|(_, t)| *t);
        for (k, _) in entries.into_iter().take(map.len() / 2) {
            map.remove(&k);
        }
    } else {
        map.retain(|_, b| {
            !(b.tokens >= b.burst && now.duration_since(b.last_refill) > GC_IDLE_THRESHOLD)
        });
    }
    let mut last_gc = LAST_GC.lock().expect("ratelimit gc poisoned");
    *last_gc = now;
    let after = map.len();
    (before, after)
}

fn maybe_gc(map: &mut HashMap<String, Bucket>, now: Instant) {
    let mut last_gc = LAST_GC.lock().expect("ratelimit gc poisoned");
    let due = now.duration_since(*last_gc) >= GC_INTERVAL;
    let oversized = map.len() > MAX_BUCKETS;
    if !due && !oversized {
        return;
    }

    if oversized {
        // Drop the half refilled longest ago.
        let mut entries: Vec<(String, Instant)> =
            map.iter().map(|(k, v)| (k.clone(), v.last_refill)).collect();
        entries.sort_by_key(|(_, t)| *t);
        for (k, _) in entries.into_iter().take(map.len() / 2) {
            map.remove(&k);
        }
    } else {
        map.retain(|_, b| {
            !(b.tokens >= b.burst && now.duration_since(b.last_refill) > GC_IDLE_THRESHOLD)
        });
    }
    *last_gc = now;
}

/// Client IP for rate-limit keying. Trusts leftmost `X-Forwarded-For`
/// behind nginx; falls back to the socket peer.
pub fn client_ip(req: &Request<'_>) -> String {
    if let Some(xff) = req.headers().get_one("x-forwarded-for") {
        if let Some(first) = xff.split(',').next() {
            let trimmed = first.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    req.client_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Request guard exposing the rate-limit client IP to handlers.
pub struct ClientIp(pub String);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ClientIp {
    type Error = Infallible;

    async fn from_request(req: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        request::Outcome::Success(ClientIp(client_ip(req)))
    }
}

impl<'r> OpenApiFromRequest<'r> for ClientIp {
    fn from_request_input(
        _gen: &mut rocket_okapi::r#gen::OpenApiGenerator,
        _name: String,
        _required: bool,
    ) -> rocket_okapi::Result<RequestHeaderInput> {
        Ok(RequestHeaderInput::None)
    }

    fn get_responses(
        _gen: &mut rocket_okapi::r#gen::OpenApiGenerator,
    ) -> rocket_okapi::Result<rocket_okapi::okapi::openapi3::Responses> {
        Ok(rocket_okapi::okapi::openapi3::Responses::default())
    }
}

