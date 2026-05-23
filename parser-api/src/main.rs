#[macro_use]
extern crate rocket;

use rocket::futures::{lock::Mutex, stream::StreamExt};
use rocket::http::CookieJar;
use rocket::request::Request;
use rocket::serde::json::{serde_json, Json};
use rocket::shield::Shield;
#[cfg(debug_assertions)]
use rocket::State;
use rusqlite::Result;
use std::collections::HashSet;

use e621_account_parser_api::{
    api,
    auth::{self, OwnerToken},
    db,
    db::{get_account_by_id, refresh_account_profiles_skip_cooc, DbInit},
    errors::ApiError,
    jobs,
    jobs::{BeginResult, ProcessJobState},
    models::{cfg, default_path, reload_from, start_config_watcher, Post, UserApiResponse},
    prefetch,
    ratelimit::{self, ClientIp},
    utils::{mark_idf_dirty, PipelineMetrics},
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
        BeginResult::AlreadyRunning(state) => Ok(Json(state)),
        BeginResult::Started(state) => {
            tokio::spawn(async move {
                let result = run_process(account_id, owner_token).await;
                if let Err(ref e) = result {
                    warn!("/process for {account_id} failed: {e}");
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

// The macro reassigns `phase_start` after every phase; the final
// reassignment is intentional but unread (function returns), which
// trips `unused_assignments`. Allowed at function scope so the
// macro body stays uniform.
#[allow(unused_assignments)]
async fn run_process(account_id: i32, owner_token: String) -> Result<(), String> {
    let mut pipe = PipelineMetrics::new("process");
    // `phase_start` is rebased after every `record_phase!` so the recorded
    // `elapsed_ms` is the delta since the previous phase (matching the
    // docstring on `JobPhaseRecord`), rather than a monotonically growing
    // total since the start of the function.
    let mut phase_start = std::time::Instant::now();

    let cfg = cfg();
    let blacklist: HashSet<String> = cfg.tag_blacklist.iter().map(|s| s.to_lowercase()).collect();

    let account =
        db_blocking(move || get_account_by_id(&owner_token, account_id).map_err(|e| e.to_string()))
            .await?;
    let user = api::get_account(&account).await?;
    let favcount = match user {
        UserApiResponse::FullCurrentUser(u) => u.favorite_count,
        UserApiResponse::FullUser(u) => u.favorite_count,
    };
    let pages = (favcount / cfg.posts_limit) + (if favcount % cfg.posts_limit > 0 { 1 } else { 0 });
    jobs::set_pages_total(account_id, pages);
    macro_rules! record_phase {
        ($name:expr) => {{
            let elapsed = phase_start.elapsed().as_secs_f64() * 1000.0;
            jobs::record_phase(account_id, $name, elapsed);
            pipe.mark($name);
            let secs = elapsed / 1000.0;
            info!("[process {account_id}] phase '{name}' done in {secs:.1}s", name = $name);
            phase_start = std::time::Instant::now();
        }};
    }
    record_phase!("init");

    db_blocking(move || {
        db::drop_account_posts(account_id).map_err(|e| format!("Failed to drop account posts: {e}"))
    })
    .await?;
    record_phase!("drop_old");

    // Cooccurrence rows can run into the millions for a single account; a
    // monolithic DELETE pins the writer mutex for the full scan and starves
    // every other write (including the status polling-side metadata
    // updates) for minutes. Batch the delete so we release the lock between
    // chunks and emit a log line per batch — gives the user a visible
    // heartbeat instead of a frozen UI.
    let drop_cooc_batch = cfg.runtime.drop_cooc_batch_size.max(1_000);
    let deleted_cooc = db_blocking(move || {
        db::drop_account_cooccurrence_batched(
            account_id,
            drop_cooc_batch,
            |batch, total| {
                info!(
                    "[process {account_id}] drop_cooc batch -{batch} (total deleted: {total})"
                );
            },
        )
        .map_err(|e| format!("Failed to drop account cooccurrence: {e}"))
    })
    .await?;
    info!("[process {account_id}] drop_cooc complete: {deleted_cooc} rows");
    record_phase!("drop_cooc");

    // Fetch pages in parallel; writes stay serial (SQLite is single-writer).
    let account_for_fetch = account.clone();
    let blacklist_for_fetch = blacklist.clone();
    let mut stream = rocket::futures::stream::iter(1..=pages)
        .map(move |i| {
            let acc = account_for_fetch.clone();
            let bl = blacklist_for_fetch.clone();
            async move {
                let raw = api::get_favorites(&acc, i).await;
                let posts: Vec<Post> = raw
                    .into_iter()
                    .map(|p| strip_blacklisted_tags(p, &bl))
                    .collect();
                (i, posts)
            }
        })
        .buffer_unordered(cfg.runtime.process_fetch_concurrency.max(1));

    let acc_id = account.id;
    while let Some((i, posts)) = stream.next().await {
        let posts_len = posts.len();
        info!("{posts_len} post(s) found on page {i}");
        let bl = blacklist.clone();
        db_blocking(move || -> Result<(), String> {
            db::save_posts(&posts, acc_id).map_err(|e| format!("Failed to save posts: {e}"))?;
            db::save_posts_tags_batch(&posts, &bl, true, Some(acc_id))
                .map_err(|e| format!("Failed to save tags for page {i}: {e}"))?;
            Ok(())
        })
        .await?;
        jobs::record_page_done(account_id);
    }
    mark_idf_dirty();
    record_phase!("fetch_and_save");

    db_blocking(move || {
        // Cooccurrence was built incrementally during save_posts_tags_batch,
        // so skip the expensive full rebuild here.
        refresh_account_profiles_skip_cooc(account_id)
            .map_err(|e| format!("Failed to refresh account profiles: {e}"))
    })
    .await?;
    record_phase!("profile_refresh");
    pipe.finish_and_log();
    Ok(())
}

fn strip_blacklisted_tags(mut p: Post, blacklist: &HashSet<String>) -> Post {
    let filter = |v: &mut Vec<String>| {
        v.retain(|t| !blacklist.contains(&t.to_lowercase().trim().to_string()));
    };
    filter(&mut p.tags.artist);
    filter(&mut p.tags.character);
    filter(&mut p.tags.copyright);
    filter(&mut p.tags.general);
    filter(&mut p.tags.lore);
    filter(&mut p.tags.meta);
    filter(&mut p.tags.species);
    p
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
    let _ = reload_from(&path);
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
