//! In-memory token-bucket rate limiter.
//!
//! Closes audit findings H-2 (no rate limiting at all) and limits the
//! amplification surface from H-1 / M-7: a single device or IP can no
//! longer flood `/account`, `/process`, or `/recommendations` and turn
//! every request into an admin-authenticated e621 round-trip.
//!
//! Implementation choices:
//!
//! * One `Mutex<HashMap<...>>` instead of pulling in `dashmap` — lock
//!   contention is fine at this app's traffic and the alternative
//!   would add a dependency for very little gain. Buckets are tiny
//!   (16 bytes), and the critical section is a few field updates.
//! * Token bucket rather than fixed-window counter — burst tolerant
//!   (a real user catching up after a tab background isn't punished)
//!   and dead simple to reason about.
//! * Simple periodic GC on insert: every `GC_INTERVAL` we drop entries
//!   that are full and idle. Without this an adversary spraying
//!   distinct keys (random tokens, spoofed XFF) could grow the table
//!   without bound.
//!
//! Returns `ApiError::TooManyRequests` so the response carries the
//! same JSON envelope as every other failure (Phase 3 work).

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
    /// Capacity at last access — used during GC to detect "fully refilled
    /// and idle" entries that are safe to drop.
    burst: f64,
}

const GC_INTERVAL: Duration = Duration::from_secs(300);
const GC_IDLE_THRESHOLD: Duration = Duration::from_secs(900);
/// Hard upper bound — past this we evict the oldest half regardless of
/// whether they're idle, because something is wrong (or hostile) and we
/// shouldn't OOM the process trying to track it.
const MAX_BUCKETS: usize = 50_000;

static BUCKETS: LazyLock<Mutex<HashMap<String, Bucket>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LAST_GC: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

/// Take one token from the named bucket. `per_min` sets the refill
/// rate, `burst` the bucket capacity (and the initial fill on first
/// access). Returns `Err(TooManyRequests)` when no tokens are available.
///
/// Example: `check("acct_create:ip:1.2.3.4", 5, 5)` allows 5 requests
/// per minute with bursts up to 5 (no extra slack beyond steady state).
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
    // Refill capacity may have been raised since last access; track the
    // most recent value so GC has correct "is this full?" data.
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

    // GC pass — done while we already hold the map lock so we don't
    // re-acquire. Cheap enough at our table sizes; expensive pruning
    // is gated behind the timer.
    maybe_gc(&mut map, now);

    if allowed {
        Ok(())
    } else {
        Err(ApiError::TooManyRequests(format!(
            "rate limit exceeded ({per_min}/min); slow down and retry"
        )))
    }
}

fn maybe_gc(map: &mut HashMap<String, Bucket>, now: Instant) {
    let mut last_gc = LAST_GC.lock().expect("ratelimit gc poisoned");
    let due = now.duration_since(*last_gc) >= GC_INTERVAL;
    let oversized = map.len() > MAX_BUCKETS;
    if !due && !oversized {
        return;
    }

    if oversized {
        // Drop the half that was refilled longest ago — they're the
        // least likely to be in active use right now.
        let mut entries: Vec<(String, Instant)> =
            map.iter().map(|(k, v)| (k.clone(), v.last_refill)).collect();
        entries.sort_by_key(|(_, t)| *t);
        for (k, _) in entries.into_iter().take(map.len() / 2) {
            map.remove(&k);
        }
    } else {
        // Periodic prune: drop fully-refilled and idle buckets.
        map.retain(|_, b| {
            !(b.tokens >= b.burst && now.duration_since(b.last_refill) > GC_IDLE_THRESHOLD)
        });
    }
    *last_gc = now;
}

/// Resolve the client IP for use as a rate-limit key. Behind nginx,
/// `X-Forwarded-For` carries the real address; we trust the leftmost
/// entry the same way most proxies do. Falls back to Rocket's view of
/// the socket peer when the header is absent (dev / direct connections).
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

/// Request guard that resolves to the rate-limit-friendly client IP.
/// Adding this as a route parameter is the idiomatic way to read the
/// IP without each handler re-implementing the XFF parsing dance.
pub struct ClientIp(pub String);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ClientIp {
    type Error = Infallible;

    async fn from_request(req: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        request::Outcome::Success(ClientIp(client_ip(req)))
    }
}

/// `ClientIp` is purely an internal request-derived string — it doesn't
/// surface in the OpenAPI parameter list, so the implementation is the
/// "no extra spec input" stub. Required for rocket-okapi to accept it
/// as a parameter on `#[openapi]` routes.
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

