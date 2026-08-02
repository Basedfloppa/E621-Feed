#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::too_many_lines
)]

#[macro_use]
extern crate rocket;

// jemalloc: cuts RSS by returning freed pages to the kernel promptly.
// Build with:
//   cargo build --release --features jemalloc
// Without it, glibc's ptmalloc keeps freed HashMap pages in internal
// free-lists, so `top` / Grafana RSS stays high even after idle-eviction
// clears the IDF, tag-relation graph, and API caches.
#[cfg(all(target_os = "linux", feature = "jemalloc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(debug_assertions)]
use rocket::State;
use rocket::data::ByteUnit;
use rocket::futures::lock::Mutex;
#[cfg(debug_assertions)]
use rocket::http::Method;
use rocket::http::{CookieJar, Header, Status};
use rocket::request::Request;
use rocket::response::status;
use rocket::serde::json::{Json, serde_json};
use rocket::shield::{Policy, Shield};
use schemars::JsonSchema;
use serde::Serialize;

use e621_account_parser_api::{
    audit,
    auth::{self, OwnerToken},
    db::{DbInit, get_account_by_id},
    db_blocking,
    errors::ApiError,
    jobs,
    jobs::{BeginResult, ProcessJobState},
    models::{cfg, default_path, reload_from, start_config_watcher},
    pipeline, prefetch,
    ratelimit::{self, ClientIp},
    validation,
};
#[cfg(debug_assertions)]
use rocket_okapi::okapi::openapi3::OpenApi;
#[cfg(debug_assertions)]
use rocket_okapi::swagger_ui::{SwaggerUIConfig, make_swagger_ui};
use rocket_okapi::{openapi, openapi_get_routes_spec, settings::OpenApiSettings};

mod routes;
mod serve_embedded;

/// Custom Shield policy for Content-Security-Policy.
/// Uses the configured `posts_domain` to allow images/media from e621.
#[derive(Default)]
struct Csp(String);

impl Policy for Csp {
    const NAME: &'static str = "Content-Security-Policy";

    fn header(&self) -> Header<'static> {
        Header::new(Self::NAME, self.0.clone())
    }
}

#[derive(Serialize, JsonSchema)]
struct HealthResponse {
    database: bool,
    caches: bool,
    e621: bool,
}

/// Readiness probe for the database, scoring caches, and upstream e621.
/// It is intentionally unauthenticated so an orchestrator can use it.
#[openapi(tag = "Operations")]
#[get("/health")]
async fn healthcheck(client_ip: ClientIp) -> status::Custom<Json<HealthResponse>> {
    // Public endpoint: bound probe storms without involving the admin key.
    if ratelimit::check(&format!("health:{}", client_ip.0), 12, 60).is_err() {
        return status::Custom(
            Status::TooManyRequests,
            Json(HealthResponse {
                database: false,
                caches: false,
                e621: false,
            }),
        );
    }
    let (database, e621) = tokio::join!(
        db_blocking(e621_account_parser_api::db::check_database_health),
        e621_account_parser_api::api::check_e621_reachable(),
    );
    // These accessors lazily build their snapshots. A successful call means
    // recommendation scoring can use both caches immediately.
    e621_account_parser_api::utils::current_idf();
    e621_account_parser_api::utils::current_global_relation();
    let response = HealthResponse {
        database: database.is_ok(),
        caches: true,
        e621: e621.is_ok(),
    };
    let status = if response.database && response.caches && response.e621 {
        Status::Ok
    } else {
        Status::ServiceUnavailable
    };
    status::Custom(status, Json(response))
}

/// Kicks off a background `/process` job and returns immediately with the
/// current job state. If a job for this account is already running, returns
/// the existing state instead of starting a new one.
///
/// `mode` query: `auto` (default), `full`, or `incremental`. See
/// [`pipeline::ProcessMode`].
#[openapi(tag = "Processing")]
#[post("/process/<account_id>?<mode>")]
async fn process_posts(
    account_id: i32,
    mode: Option<String>,
    owner: OwnerToken,
    client_ip: ClientIp,
) -> Result<Json<ProcessJobState>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    // /process kicks off a long-running e621 fetch loop on the admin
    // token; cap per-owner and per-IP so click-storms can't lap us.
    ratelimit::check(&format!("process:owner:{owner_token}"), 3, 3)?;
    ratelimit::check(&format!("process:ip:{}", client_ip.0), 5, 5)?;
    let owner_for_check = owner_token.clone();
    db_blocking(move || get_account_by_id(&owner_for_check, account_id).map_err(|e| e.clone()))
        .await?;
    let process_mode = mode
        .as_deref()
        .unwrap_or("")
        .parse::<pipeline::ProcessMode>()
        .map_err(ApiError::BadRequest)?;

    match jobs::try_begin(account_id) {
        BeginResult::AlreadyRunning(state) => {
            audit::event("process.already")
                .field("account_id", account_id)
                .emit();
            Ok(Json(state))
        }
        BeginResult::Started(state) => {
            tokio::spawn(async move {
                let result =
                    pipeline::run_process_with_mode(account_id, owner_token, process_mode).await;
                if let Err(ref e) = result {
                    warn!("/process for {account_id} failed: {e}");
                    audit::event("process.failed")
                        .field("account_id", account_id)
                        .field("error", e)
                        .emit();
                    e621_account_parser_api::metrics::METRICS
                        .process_runs_total
                        .with_label_values(&["failed"])
                        .inc();
                }
                jobs::finish(account_id, result);
            });
            Ok(Json(state))
        }
    }
}

#[openapi(tag = "Processing")]
#[get("/process/<account_id>/status")]
async fn process_status(
    account_id: i32,
    owner: OwnerToken,
) -> Result<Json<Option<ProcessJobState>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    db_blocking(move || get_account_by_id(&owner_token, account_id).map_err(|e| e.clone())).await?;
    Ok(Json(jobs::get_state(account_id)))
}

#[openapi(tag = "Accounts")]
#[get("/defaults/blacklist")]
fn get_default_blacklist() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "blacklist": cfg().default_account_blacklist }))
}

/// Refresh the cookie's 400-day expiry, or mint a fresh 256-bit CSPRNG
/// token if absent. The per-IP cap stops bootstrap from being a free
/// token-mint primitive that XFF rotation could combo into amplification
/// of admin-authenticated e621 calls.
#[openapi(tag = "Session")]
#[post("/session/bootstrap")]
fn session_bootstrap(
    cookies: &CookieJar<'_>,
    client_ip: ClientIp,
) -> Result<Json<serde_json::Value>, ApiError> {
    ratelimit::check(&format!("bootstrap:ip:{}", client_ip.0), 5, 5)?;

    if let Some(c) = cookies.get(auth::OWNER_TOKEN_COOKIE) {
        if validation::validate_owner_token(c.value()).is_ok() {
            cookies.add(auth::build_owner_cookie(c.value().to_string()));
            return Ok(Json(serde_json::json!({ "minted": false })));
        }
        cookies.add(auth::build_owner_cookie_clear());
    }

    // 32 random bytes → base64url ≈ 43 chars (≈256 bits entropy).
    let token = mint_owner_token();
    cookies.add(auth::build_owner_cookie(token));
    audit::event("session.bootstrap")
        .field("minted", true)
        .emit();
    Ok(Json(serde_json::json!({ "minted": true })))
}

/// Explicit logout — clear the cookie AND record the revocation
/// server-side so a leaked cookie can't be replayed. Idempotent.
#[openapi(tag = "Session")]
#[delete("/session")]
async fn session_clear(cookies: &CookieJar<'_>) -> Result<Json<serde_json::Value>, ApiError> {
    if let Some(c) = cookies.get(auth::OWNER_TOKEN_COOKIE) {
        let token = c.value().to_string();
        if validation::validate_owner_token(&token).is_ok() {
            let token_for_revoke = token.clone();
            db_blocking(move || auth::revoke(&token_for_revoke))
                .await
                .map_err(|e| {
                    warn!("session revoke failed: {e}");
                    ApiError::Internal("Failed to revoke session".into())
                })?;
            audit::event("token.revoked")
                .field("reason", "logout")
                .emit();
        }
    }
    cookies.add(auth::build_owner_cookie_clear());
    audit::event("session.cleared").emit();
    Ok(Json(serde_json::json!({ "cleared": true })))
}

fn mint_owner_token() -> String {
    let mut buf = [0u8; 32];
    // OS CSPRNG. Fail loud rather than risk handing out a deterministic token.
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    base64_url_encode(&buf)
}

fn base64_url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n =
            (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8) | u32::from(bytes[i + 2]);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = u32::from(bytes[i]) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
    } else if rem == 2 {
        let n = (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
    }
    out
}

#[cfg(debug_assertions)]
#[get("/openapi.json")]
fn openapi_json(spec: &State<OpenApi>) -> Json<OpenApi> {
    Json(spec.inner().clone())
}

// Catchers — uniform JSON error envelope on `/api`. Rocket's default
// HTML pages would break the frontend's `resp.ok() && resp.json()` flow.
// Registered on `/api` only so the SPA fallback for `/` still serves index.html.
#[catch(404)]
fn catch_404(_req: &Request<'_>) -> ApiError {
    ApiError::NotFound("Resource not found".into())
}

#[catch(422)]
fn catch_422(_req: &Request<'_>) -> ApiError {
    ApiError::BadRequest("Unprocessable request body".into())
}

#[catch(500)]
fn catch_500(_req: &Request<'_>) -> ApiError {
    ApiError::Internal("Internal server error".into())
}

#[cfg(debug_assertions)]
fn attach_cors(rocket: rocket::Rocket<rocket::Build>) -> rocket::Rocket<rocket::Build> {
    // Cookie auth needs `credentials: "include"`, which requires an
    // explicit (non-wildcard) origin list. `trunk serve` splits SPA
    // (:8000) and API (:8080); production has one origin behind nginx.
    let exact = rocket_cors::AllowedOrigins::some_exact(&[
        "http://localhost:8000",
        "http://127.0.0.1:8000",
    ]);
    let cors = rocket_cors::CorsOptions {
        allowed_origins: exact,
        allow_credentials: true,
        allowed_methods: vec![
            Method::Get,
            Method::Post,
            Method::Put,
            Method::Patch,
            Method::Delete,
            Method::Options,
        ]
        .into_iter()
        .map(From::from)
        .collect(),
        allowed_headers: rocket_cors::AllowedHeaders::all(),
        ..Default::default()
    }
    .to_cors()
    .expect("Failed to set CORS options");
    rocket.attach(cors)
}

#[cfg(not(debug_assertions))]
fn attach_cors(rocket: rocket::Rocket<rocket::Build>) -> rocket::Rocket<rocket::Build> {
    rocket
}

/// Build and configure the Rocket instance.
async fn build_rocket() -> rocket::Rocket<rocket::Build> {
    info!("Starting server...");
    let path = default_path().unwrap_or_else(|e| {
        error!("Failed to resolve config path: {e:#}; falling back to config.toml");
        std::path::PathBuf::from("config.toml")
    });
    // Startup config load: failure here means defaults are used, which
    // can subtly change behaviour (e621 base URL, blacklist, etc.).
    // Surface so an operator notices instead of silently running with
    // unintended defaults.
    if let Err(e) = reload_from(&path) {
        audit::event("startup.config_load_failed")
            .field("path", path.display())
            .field("error", e)
            .emit_err();
    }
    let watcher = match start_config_watcher(path) {
        Ok(w) => w,
        Err(e) => {
            error!("Failed to start config file watcher: {e:#}");
            error!(
                "Server starting without config hot-reload; changes to config.toml will require a restart"
            );
            e621_account_parser_api::models::ConfigWatcher::new_noop()
        }
    };

    let settings = OpenApiSettings::new();
    let (api_routes, spec) = openapi_get_routes_spec![
        settings:
        process_posts,
        process_status,
        healthcheck,
        routes::feed::log_feed_interaction,
        routes::feed::log_feed_interaction_batch,
        routes::feed::undo_feed_interaction,
        routes::feed::clear_account_interactions,
        routes::feed::get_account_interactions,
        routes::account::list_accounts,
        routes::account::get_account_tag_counts,
        routes::account::get_account_profile,
        routes::account::get_account_id,
        routes::account::get_account_name,
        routes::account::create_account,
        routes::account::delete_account,
        routes::account::get_account_blacklist,
        routes::account::update_account_blacklist,
        routes::account::get_account_preferred_tags,
        routes::account::set_account_preferred_tags,
        routes::account::get_account_experiment_bucket,
        routes::account::get_feed_settings,
        routes::account::patch_feed_settings,
        routes::account::export_account,
        routes::account::import_account,
        routes::feed::get_recommendations,
        routes::feed::get_recommendations_continue,
        routes::feed::get_similar_posts,
        routes::digest::get_daily_digest,
        routes::browse::get_trending,
        routes::browse::get_trending_scored,
        routes::browse::get_favorites,
        routes::browse::search_posts,
        routes::browse::search_scored_posts,
        get_default_blacklist,
        session_bootstrap,
        session_clear,
        routes::tag_relations::resolve_tag,
        routes::tag_relations::resolve_tag_autocomplete,
        routes::tag_relations::resolve_tag_batch,
        routes::taste_profile::get_taste_profile,
        routes::tag_relations::get_tag_implications,
        routes::tag_relations::get_tag_implications_batch,
    ];

    // Increase JSON limit for batch tag resolution (400+ tags = ~130KB).
    // Default 64KiB is too small; 512KiB is safe for any realistic payload.
    // Increase JSON limit for batch tag resolution (400+ tags = ~130KB).
    // Keep existing env config (port, address, etc.) by reading via figment.
    let figment = rocket::Config::figment().merge((
        "limits",
        rocket::data::Limits::default().limit("json", ByteUnit::Kibibyte(512)),
    ));

    let mut shield = Shield::new();
    // CSP: allow loading images and media from the configured e621 domain
    // (static1.e621.net, static2.e621.net, etc.) and its subdomains.
    // Default config uses "https://e621.net" which becomes "https://*.e621.net".
    let posts_host = url::Url::parse(&cfg().posts_domain)
        .ok()
        .and_then(|u| u.host_str().map(std::string::ToString::to_string))
        .unwrap_or_default();
    if !posts_host.is_empty() {
        let wildcard = format!("https://*.{posts_host}");
        let csp = format!(
            "default-src 'self'; \
             script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'; \
             style-src 'self' 'unsafe-inline'; \
             font-src 'self'; \
             img-src 'self' data: {wildcard}; \
             media-src {wildcard}; \
             connect-src 'self'; \
             frame-ancestors 'none'; \
             base-uri 'self'; \
             form-action 'self'",
        );
        shield = shield.enable(Csp(csp));
    }
    let r = rocket::custom(figment)
        .manage(Mutex::new(watcher))
        .manage(spec)
        .mount("/api", api_routes)
        .mount(
            "/api",
            rocket::routes![routes::account::get_account_tag_relations],
        )
        // tag_relations routes are already in openapi_get_routes_spec! above
        .mount("/api", rocket::routes![routes::get_metrics])
        .register("/api", catchers![catch_404, catch_422, catch_500])
        .attach(shield)
        .attach(DbInit)
        // Serve the embedded frontend (SPA + static assets) on the root path.
        // The `/api` mount is kept separate; Rocket prefers the more specific
        // `/api` mount for `/api/*` requests, so they never reach the SPA.
        .mount("/", serve_embedded::routes());

    // Swagger UI / OpenAPI doc leak the full route map; mount only in
    // dev. nginx 404s these paths in prod as defense-in-depth.
    #[cfg(debug_assertions)]
    let r = r.mount("/api", routes![openapi_json]).mount(
        "/api/swagger-ui",
        make_swagger_ui(&SwaggerUIConfig {
            url: "/api/openapi.json".to_owned(),
            ..Default::default()
        }),
    );

    prefetch::spawn_prefetch_workers();
    e621_account_parser_api::cache_pruner::spawn_cache_pruner();
    e621_account_parser_api::media_hydrator::spawn_media_hydrator();
    e621_account_parser_api::db::spawn_tag_relation_importer();

    // Seed the A/B bucket distribution gauge from the current account table.
    // Runs on a detached task so it doesn't hold non-Send values across await.
    rocket::tokio::spawn(async move {
        let result = db_blocking(e621_account_parser_api::db::count_accounts_by_bucket).await;
        if let Ok(counts) = result {
            for (bucket, count) in counts {
                e621_account_parser_api::metrics::METRICS
                    .experiment_bucket_accounts
                    .with_label_values(&[&bucket])
                    .set(count as i64);
            }
        }
    });

    attach_cors(r)
}

#[rocket::main]
async fn main() {
    let rocket = build_rocket().await;

    // Ignite first to get a `Rocket<Ignite>` which has the `.shutdown()` handle.
    let rocket = rocket.ignite().await.expect("Rocket ignition failed");

    // Spawn a signal watcher that triggers Rocket's built-in graceful
    // shutdown on SIGTERM (Unix) in addition to the default Ctrl+C (SIGINT).
    // Rocket drains in-flight requests before exiting.
    #[cfg(unix)]
    {
        use rocket::tokio::select;
        use rocket::tokio::signal::unix::{SignalKind, signal};
        use rocket::tokio::spawn;

        let handle = rocket.shutdown();
        spawn(async move {
            let mut sigterm = signal(SignalKind::terminate()).expect("Failed to register SIGTERM");
            let mut sigint = signal(SignalKind::interrupt()).expect("Failed to register SIGINT");
            select! {
                _ = sigterm.recv() => {
                    info!("Received SIGTERM — starting graceful shutdown");
                }
                _ = sigint.recv() => {
                    info!("Received SIGINT — starting graceful shutdown");
                }
            }
            info!("Shutdown signal received, draining in-flight requests...");
            handle.notify();
        });
    }

    info!("Server started");
    let _ = rocket.launch().await;
    info!("Shutdown complete");
}

#[cfg(test)]
mod tests {
    use super::attach_cors;
    use rocket::http::Header;
    use rocket::local::asynchronous::Client;

    #[get("/cors-test")]
    fn cors_test() -> &'static str {
        "ok"
    }

    #[rocket::async_test]
    async fn dev_cors_allows_trunk_origin_with_credentials() {
        let rocket = attach_cors(rocket::build().mount("/", routes![cors_test]));
        let client = Client::tracked(rocket).await.expect("valid test Rocket");

        let response = client
            .get("/cors-test")
            .header(Header::new("Origin", "http://localhost:8000"))
            .dispatch()
            .await;

        assert_eq!(response.status(), rocket::http::Status::Ok);
        assert_eq!(
            response.headers().get_one("Access-Control-Allow-Origin"),
            Some("http://localhost:8000")
        );
        assert_eq!(
            response
                .headers()
                .get_one("Access-Control-Allow-Credentials"),
            Some("true")
        );
    }
}
