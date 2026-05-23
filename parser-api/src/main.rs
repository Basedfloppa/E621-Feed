#[macro_use]
extern crate rocket;

use rocket::futures::lock::Mutex;
use rocket::http::CookieJar;
use rocket::request::Request;
use rocket::serde::json::{serde_json, Json};
use rocket::shield::Shield;
#[cfg(debug_assertions)]
use rocket::State;
use rusqlite::Result;

use e621_account_parser_api::{
    audit,
    auth::{self, OwnerToken},
    db::{get_account_by_id, DbInit},
    errors::ApiError,
    jobs,
    jobs::{BeginResult, ProcessJobState},
    models::{cfg, default_path, reload_from, start_config_watcher},
    pipeline,
    prefetch,
    ratelimit::{self, ClientIp},
    validation,
};
#[cfg(debug_assertions)]
use rocket_okapi::okapi::openapi3::OpenApi;
use rocket_okapi::{openapi, openapi_get_routes_spec, settings::OpenApiSettings};
#[cfg(debug_assertions)]
use rocket_okapi::swagger_ui::{make_swagger_ui, SwaggerUIConfig};

mod routes;

/// Run a blocking rusqlite closure on `spawn_blocking` so the SQLite
/// call doesn't park the request's Tokio worker. Translates JoinHandle
/// panics to `Err`.
pub(crate) async fn db_blocking<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    match rocket::tokio::task::spawn_blocking(f).await {
        Ok(res) => res,
        Err(e) => Err(format!("DB task panicked: {e}")),
    }
}

/// Kicks off a background `/process` job and returns immediately with the
/// current job state. If a job for this account is already running, returns
/// the existing state instead of starting a new one.
#[openapi(tag = "Processing")]
#[post("/process/<account_id>")]
async fn process_posts(
    account_id: i32,
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
    db_blocking(move || get_account_by_id(&owner_for_check, account_id).map_err(|e| e.to_string()))
        .await?;

    match jobs::try_begin(account_id) {
        BeginResult::AlreadyRunning(state) => {
            audit::event("process.already")
                .field("account_id", account_id)
                .emit();
            Ok(Json(state))
        }
        BeginResult::Started(state) => {
            tokio::spawn(async move {
                let result = pipeline::run_process(account_id, owner_token).await;
                if let Err(ref e) = result {
                    warn!("/process for {account_id} failed: {e}");
                    audit::event("process.failed")
                        .field("account_id", account_id)
                        .field("error", e)
                        .emit();
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
    db_blocking(move || get_account_by_id(&owner_token, account_id).map_err(|e| e.to_string()))
        .await?;
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
            db_blocking(move || auth::revoke(&token)).await.map_err(|e| {
                warn!("session revoke failed: {e}");
                ApiError::Internal("Failed to revoke session".into())
            })?;
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
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
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

#[launch]
async fn rocket() -> _ {
    let path = default_path().unwrap();
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
    let watcher = start_config_watcher(path).unwrap();

    let settings = OpenApiSettings::new();
    let (api_routes, spec) = openapi_get_routes_spec![
        settings:
        process_posts,
        process_status,
        routes::feed::log_feed_interaction,
        routes::feed::log_feed_interaction_batch,
        routes::account::list_accounts,
        routes::account::get_account_tag_counts,
        routes::account::get_account_id,
        routes::account::get_account_name,
        routes::account::create_account,
        routes::account::delete_account,
        routes::account::get_account_blacklist,
        routes::account::update_account_blacklist,
        routes::account::get_account_preferred_tags,
        routes::account::set_account_preferred_tags,
        routes::account::get_account_experiment_bucket,
        routes::feed::get_recommendations,
        routes::feed::get_recommendations_continue,
        routes::feed::get_similar_posts,
        routes::digest::get_daily_digest,
        get_default_blacklist,
        session_bootstrap,
        session_clear
    ];

    // Empty Shield: nginx already sets stricter security headers. Rocket's
    // defaults (`XFO: SAMEORIGIN`, etc.) would conflict with nginx's `DENY`.
    let r = rocket::build()
        .manage(Mutex::new(watcher))
        .manage(spec)
        .mount("/api", api_routes)
        .mount("/api", rocket::routes![routes::account::get_account_tag_relations])
        .register("/api", catchers![catch_404, catch_422, catch_500])
        .attach(Shield::new())
        .attach(DbInit);

    // Swagger UI / OpenAPI doc leak the full route map; mount only in
    // dev. nginx 404s these paths in prod as defense-in-depth.
    #[cfg(debug_assertions)]
    let r = r
        .mount("/api", routes![openapi_json])
        .mount(
            "/api/swagger-ui",
            make_swagger_ui(&SwaggerUIConfig {
                url: "/api/openapi.json".to_owned(),
                ..Default::default()
            }),
        );

    prefetch::spawn_prefetch_workers();
    e621_account_parser_api::cache_pruner::spawn_cache_pruner();

    attach_cors(r)
}
