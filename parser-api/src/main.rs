#[macro_use]
extern crate rocket;

use chrono::Utc;
use rocket::http::{ContentType, Header, Status};
use rocket::request::{self, FromRequest, Request};
use rocket::response::{self, Responder, Response};
use rocket::{State, get};
use rocket::{
    futures::{lock::Mutex, stream::StreamExt},
    serde::json::{Json, serde_json},
};
use rocket_cors::AllowedOrigins;
use rusqlite::Result;
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hasher};
use std::io::Cursor;

use e621_account_parser_api::{
    api, db,
    errors::ApiError,
    jobs, models, prefetch,
    ratelimit::{self, ClientIp},
    utils, validation,
};

use db::{
    DbInit, collect_local_candidate_ids, get_account_by_id, get_account_by_name,
    get_account_preference_profile, get_account_tag_relation_graph, get_accounts_for_owner,
    get_owned_post_ids, get_recently_seen_post_ids, get_tag_counts, hydrate_posts_by_ids,
    record_feed_interaction, refresh_account_profiles, set_account, update_device_blacklist,
    upsert_catalog_posts,
};
use models::{
    BlacklistPayload, DeviceScopedAccount, FeedInteractionRequest, Post, ScoredPost, TagCount,
    TagRelationScoring, TruncatedAccount, UserApiResponse, cfg, default_path, reload_from,
    start_config_watcher,
};
use rocket_okapi::okapi::openapi3::OpenApi;
use rocket_okapi::{openapi, openapi_get_routes_spec, settings::OpenApiSettings, swagger_ui::*};
use utils::{
    ScoringContext, current_global_relation, current_idf, diversify_scored_posts, mark_idf_dirty,
};

use jobs::{BeginResult, ProcessJobState};

/// Run a blocking rusqlite closure on the spawn_blocking pool so it doesn't
/// park the request's Tokio worker for the duration of the SQLite call.
/// Returns the closure's `Result<T, String>` flat, plus a panic-to-Err
/// translation for the JoinHandle.
async fn db_blocking<F, T>(f: F) -> Result<T, String>
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
#[post("/process/<account_id>?<owner_token>")]
async fn process_posts(
    account_id: i32,
    owner_token: &str,
    client_ip: ClientIp,
) -> Result<Json<ProcessJobState>, ApiError> {
    validation::validate_owner_token(owner_token)?;
    validation::validate_account_id(account_id)?;
    // /process is the most expensive route. Cap it hard per device 
    ratelimit::check(&format!("process:owner:{owner_token}"), 1, 1)?;
    ratelimit::check(&format!("process:ip:{}", client_ip.0), 5, 5)?;
    let owner_token_owned = owner_token.to_string();
    let owner_for_check = owner_token_owned.clone();
    db_blocking(move || get_account_by_id(&owner_for_check, account_id).map_err(|e| e.to_string()))
        .await?;

    match jobs::try_begin(account_id) {
        BeginResult::AlreadyRunning(state) => Ok(Json(state)),
        BeginResult::Started(state) => {
            tokio::spawn(async move {
                let result = run_process(account_id, owner_token_owned).await;
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
#[get("/process/<account_id>/status?<owner_token>")]
async fn process_status(
    account_id: i32,
    owner_token: &str,
) -> Result<Json<Option<ProcessJobState>>, ApiError> {
    validation::validate_owner_token(owner_token)?;
    validation::validate_account_id(account_id)?;
    let owner = owner_token.to_string();
    db_blocking(move || get_account_by_id(&owner, account_id).map_err(|e| e.to_string())).await?;
    Ok(Json(jobs::get_state(account_id)))
}

async fn run_process(account_id: i32, owner_token: String) -> Result<(), String> {
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

    db_blocking(move || {
        db::drop_account_posts(account_id).map_err(|e| format!("Failed to drop account posts: {e}"))
    })
    .await?;

    // Fetch pages in parallel; writes stay serial because SQLite is
    // single-writer anyway.
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
            db::save_posts_tags_batch(&posts, &bl, true)
                .map_err(|e| format!("Failed to save tags for page {i}: {e}"))?;
            Ok(())
        })
        .await?;
        jobs::record_page_done(account_id);
    }
    mark_idf_dirty();

    db_blocking(move || {
        refresh_account_profiles(account_id)
            .map_err(|e| format!("Failed to refresh account profiles: {e}"))
    })
    .await?;
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
#[get("/account/<account_id>/tag_counts?<owner_token>")]
async fn get_account_tag_counts(
    account_id: i32,
    owner_token: &str,
) -> Result<Json<Vec<TagCount>>, ApiError> {
    validation::validate_owner_token(owner_token)?;
    validation::validate_account_id(account_id)?;
    let owner = owner_token.to_string();
    let counts = db_blocking(move || {
        get_account_by_id(&owner, account_id)
            .map_err(|e| format!("Failed to validate account access: {e}"))?;
        get_tag_counts(account_id).map_err(|e| {
            let m = format!("Failed to get tag counts: {e}");
            error!("{m}");
            m
        })
    })
    .await?;
    Ok(Json(counts))
}

#[openapi(tag = "Users")]
#[get("/user/name/<name>?<owner_token>")]
async fn get_account_name(
    name: &str,
    owner_token: &str,
) -> Result<Json<TruncatedAccount>, ApiError> {
    validation::validate_owner_token(owner_token)?;
    let owner = owner_token.to_string();
    let name_owned = name.to_string();

    let owner_for_local = owner.clone();
    let name_for_local = name_owned.clone();
    let local = db_blocking(move || get_account_by_name(&owner_for_local, name_for_local)).await;
    if let Ok(acc) = local {
        return Ok(Json(acc));
    }

    ratelimit::check(&format!("user_lookup:owner:{owner_token}"), 30, 10)?;
    let response = api::get_user_by_name(&name_owned).await.map_err(|e| {
        error!("e621 user lookup for '{name_owned}' failed: {e}");
        ApiError::NotFound(format!("No account found for '{name_owned}'"))
    })?;
    let (id, resolved_name) = match response {
        UserApiResponse::FullCurrentUser(u) => (u.id, u.name),
        UserApiResponse::FullUser(u) => (u.id, u.name),
    };
    Ok(Json(TruncatedAccount {
        id,
        name: resolved_name,
        blacklist: String::new(),
    }))
}

#[openapi(tag = "Users")]
#[get("/user/id/<id>?<owner_token>")]
async fn get_account_id(id: i32, owner_token: &str) -> Result<Json<TruncatedAccount>, ApiError> {
    validation::validate_owner_token(owner_token)?;
    validation::validate_account_id(id)?;
    let owner = owner_token.to_string();
    let acc = db_blocking(move || {
        get_account_by_id(&owner, id).map_err(|e| {
            let m = format!("Failed to get account: {e}");
            error!("{m}");
            m
        })
    })
    .await?;
    Ok(Json(acc))
}

#[openapi(tag = "Accounts")]
#[get("/accounts?<owner_token>")]
async fn list_accounts(owner_token: &str) -> Result<Json<Vec<TruncatedAccount>>, ApiError> {
    validation::validate_owner_token(owner_token)?;
    let owner = owner_token.to_string();
    let accounts = db_blocking(move || get_accounts_for_owner(&owner)).await?;
    Ok(Json(accounts))
}

#[openapi(tag = "Accounts")]
#[post("/account", data = "<account>")]
async fn create_account(
    account: Json<DeviceScopedAccount>,
    client_ip: ClientIp,
) -> Result<Json<TruncatedAccount>, ApiError> {
    let acc = account.into_inner();
    validation::validate_device_scoped_account(&acc)?;
    // Per-IP cap on account creation: H-1 showed how a single client
    // could spray thousands of bogus accounts in seconds, each one
    // becoming an admin-authenticated prefetch target afterwards.
    ratelimit::check(&format!("acct_create:ip:{}", client_ip.0), 5, 5)?;
    ratelimit::check(&format!("acct_create:owner:{}", acc.owner_token), 10, 10)?;
    let result = db_blocking(move || {
        set_account(&acc.owner_token, acc.id, &acc.name, &acc.blacklist).map_err(|e| {
            let m = format!("Failed to set account: {e}");
            error!("{m}");
            m
        })
    })
    .await?;
    Ok(Json(result))
}

struct IfNoneMatch(Option<String>);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for IfNoneMatch {
    type Error = std::convert::Infallible;
    async fn from_request(req: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        request::Outcome::Success(IfNoneMatch(
            req.headers()
                .get_one("If-None-Match")
                .map(|s| s.to_string()),
        ))
    }
}

struct EtagJson {
    body: Vec<u8>,
    etag: String,
    not_modified: bool,
}

impl EtagJson {
    fn new<T: serde::Serialize>(payload: &T, if_none_match: &IfNoneMatch) -> Result<Self, String> {
        let body = serde_json::to_vec(payload).map_err(|e| format!("serialize: {e}"))?;
        let mut h = DefaultHasher::new();
        h.write(&body);
        let etag = format!("\"{:x}\"", h.finish());
        let not_modified = if_none_match
            .0
            .as_deref()
            .map(|tag| tag.split(',').any(|t| t.trim() == "*" || t.trim() == etag))
            .unwrap_or(false);
        Ok(Self {
            body,
            etag,
            not_modified,
        })
    }
}

impl<'r> Responder<'r, 'static> for EtagJson {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        let mut build = Response::build();
        build
            .header(Header::new("ETag", self.etag))
            .header(Header::new("Cache-Control", "private, max-age=60"));
        if self.not_modified {
            build.status(Status::NotModified);
        } else {
            build
                .header(ContentType::JSON)
                .sized_body(self.body.len(), Cursor::new(self.body));
        }
        build.ok()
    }
}

#[get("/account/<account_id>/tag_relations?<owner_token>&<top>&<min_cooc>")]
async fn get_account_tag_relations(
    account_id: i32,
    owner_token: &str,
    top: Option<usize>,
    min_cooc: Option<i64>,
    if_none_match: IfNoneMatch,
) -> Result<EtagJson, ApiError> {
    validation::validate_owner_token(owner_token)?;
    validation::validate_account_id(account_id)?;
    let top = top.unwrap_or(60).clamp(2, 250);
    let min_cooc = min_cooc.unwrap_or(2).max(1);
    let owner = owner_token.to_string();
    let mut payload = db_blocking(move || {
        get_account_by_id(&owner, account_id)
            .map_err(|e| format!("Failed to validate account access: {e}"))?;
        get_account_tag_relation_graph(account_id, top, min_cooc)
    })
    .await?;

    let priors = &cfg().priors;
    let n_fav = payload.account_post_count.max(0) as f32;
    let conf = n_fav / (n_fav + priors.coldstart_n0.max(1.0));
    let w_personal = priors.tag_relation_w_personal.max(0.0) * conf;
    let w_global = priors.tag_relation_w_global.max(0.0)
        + priors.tag_relation_w_personal.max(0.0) * (1.0 - conf);
    payload.scoring = TagRelationScoring {
        w_global,
        w_personal,
        pmi_scale: priors.tag_relation_pmi_scale.max(1e-3),
        cooc_ref: priors.tag_relation_cooc_ref.max(1.0),
        user_cooc_ref: priors.tag_relation_user_cooc_ref.max(1.0),
        min_cooc_global: priors.tag_relation_min_cooc.max(1),
        min_cooc_user: priors.tag_relation_user_min_cooc.max(1),
    };
    EtagJson::new(&payload, &if_none_match).map_err(ApiError::from)
}

#[openapi(tag = "Accounts")]
#[get("/account/<account_id>/blacklist?<owner_token>")]
async fn get_account_blacklist(
    account_id: i32,
    owner_token: &str,
) -> Result<Json<BlacklistPayload>, ApiError> {
    validation::validate_owner_token(owner_token)?;
    validation::validate_account_id(account_id)?;
    let owner = owner_token.to_string();
    let account = db_blocking(move || {
        get_account_by_id(&owner, account_id).map_err(|e| format!("Failed to get account: {e}"))
    })
    .await?;
    Ok(Json(BlacklistPayload {
        blacklist: account.blacklist,
    }))
}

/// Sever the device → account link. The underlying `accounts` row stays
/// intact so other devices that linked the same account aren't affected;
/// any orphaned rows are removed by the periodic GC (Phase 6).
#[openapi(tag = "Accounts")]
#[delete("/account/<account_id>?<owner_token>")]
async fn delete_account(account_id: i32, owner_token: &str) -> Result<(), ApiError> {
    validation::validate_owner_token(owner_token)?;
    validation::validate_account_id(account_id)?;
    let owner = owner_token.to_string();
    let removed = db_blocking(move || db::delete_device_link(&owner, account_id)).await?;
    if removed == 0 {
        return Err(ApiError::NotFound(
            "No account found for this device token".into(),
        ));
    }
    Ok(())
}

#[openapi(tag = "Accounts")]
#[patch("/account/<account_id>/blacklist?<owner_token>", data = "<payload>")]
async fn update_account_blacklist(
    account_id: i32,
    owner_token: &str,
    payload: Json<BlacklistPayload>,
) -> Result<Json<TruncatedAccount>, ApiError> {
    validation::validate_owner_token(owner_token)?;
    validation::validate_account_id(account_id)?;
    validation::validate_blacklist_payload(&payload)?;
    let owner = owner_token.to_string();
    let body = payload.into_inner();
    let updated = db_blocking(move || {
        update_device_blacklist(&owner, account_id, &body.blacklist).map_err(|e| {
            let m = format!("Failed to update blacklist: {e}");
            error!("{m}");
            m
        })
    })
    .await?;
    Ok(Json(updated))
}

#[openapi(tag = "Recommendations")]
#[post("/interaction", data = "<payload>")]
async fn log_feed_interaction(payload: Json<FeedInteractionRequest>) -> Result<(), ApiError> {
    let body = payload.into_inner();
    validation::validate_feed_interaction(&body)?;
    // Tight per-device cap — feed scrolling fires impressions per card,
    // a normal session is well under this. Anything higher is either a
    // bug (re-render storm) or an attempt to write-flood SQLite.
    ratelimit::check(
        &format!("interaction:owner:{}", body.owner_token),
        120,
        60,
    )?;
    db_blocking(move || record_feed_interaction(&body))
        .await
        .map_err(ApiError::from)
}

#[openapi(tag = "Recommendations")]
#[get("/recommendations/<account_id>?<owner_token>&<page>&<affinity_threshold>")]
async fn get_recommendations(
    account_id: i32,
    owner_token: &str,
    page: Option<i32>,
    affinity_threshold: Option<f32>,
) -> Result<Json<Vec<ScoredPost>>, ApiError> {
    validation::validate_owner_token(owner_token)?;
    validation::validate_account_id(account_id)?;
    // Each /recommendations issues an admin-authenticated e621 round-trip.
    // Cap per device so a misbehaving infinite-scroll loop can't burn
    // through the admin quota; the burst is intentionally close to the
    // steady rate so a brief flurry from rAF still passes.
    ratelimit::check(&format!("recs:owner:{owner_token}"), 60, 30)?;
    let cfg = cfg();
    // Cloned (cheap, primitives) so the closure below owns its copy and we
    // don't need to hold the Arc<Config> across the blocking task boundary.
    let runtime = cfg.runtime.clone();

    let owner = owner_token.to_string();
    // Pull everything the request needs from SQLite in a single blocking
    // hop: profile, blacklist, user-relation graph, the two dedup sets
    // (seen + owned), and the A/B bucket assignment. Done before the network
    // round-trip so the SQL latency overlaps with the e621 RTT.
    let (
        tags,
        profile,
        account,
        user_relation,
        seen_ids,
        owned_ids,
        local_candidate_ids,
        explicit_bucket,
    ) = db_blocking(move || -> Result<_, String> {
        let tags: Vec<TagCount> =
            get_tag_counts(account_id).map_err(|e| format!("Failed to get tag counts: {e}"))?;
        let profile = get_account_preference_profile(account_id)
            .map_err(|e| format!("Failed to get account profile: {e}"))?;
        let account = get_account_by_id(&owner, account_id)
            .map_err(|e| format!("Failed to get account: {e}"))?;
        let user_relation = db::load_account_tag_relation(account_id, &tags)
            .map_err(|e| format!("Failed to load user tag relation graph: {e}"))?;
        let seen_ids = get_recently_seen_post_ids(account_id, runtime.dedup_lookback_days)
            .map_err(|e| format!("Failed to load seen post ids: {e}"))?;
        let owned_ids = get_owned_post_ids(account_id)
            .map_err(|e| format!("Failed to load owned post ids: {e}"))?;
        let local_ids = collect_local_candidate_ids(account_id, runtime.local_candidate_limit)
            .map_err(|e| format!("Failed to collect local candidate ids: {e}"))?;
        let bucket = db::get_account_experiment_bucket(account_id)
            .map_err(|e| format!("Failed to load experiment bucket: {e}"))?;
        Ok((
            tags,
            profile,
            account,
            user_relation,
            seen_ids,
            owned_ids,
            local_ids,
            bucket,
        ))
    })
    .await?;

    let (bucket_name, mut priors) = cfg.pick_bucket(account_id, explicit_bucket.as_deref());
    if let Some(bucket) = &bucket_name {
        debug!("recommendations account={account_id} bucket={bucket}");
    }
    priors.now = Utc::now();

    let live_posts: Vec<Post> = api::get_posts(&account, page)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to fetch posts: {e}")))?;

    // Catalog persistence is fire-and-forget: it's a write contender against
    // the SQLite single writer, and infinite scroll fires this several times
    // per second. Pulling it off the request path means the response shape
    // doesn't depend on whether the previous page's writes have flushed.
    // The IDF index is updated *incrementally* via `bump_idf` inside
    // save_posts_tags_batch, so newly-seen tags participate in scoring on
    // the very next page rather than waiting for a full rebuild.
    {
        let posts_for_persist = live_posts.clone();
        rocket::tokio::spawn(async move {
            let res = rocket::tokio::task::spawn_blocking(move || -> Result<(), String> {
                upsert_catalog_posts(&posts_for_persist)
                    .map_err(|e| format!("Failed to store recommendation catalog posts: {e}"))?;
                // Skip cooccurrence: candidate browse posts aren't user truth.
                db::save_posts_tags_batch(&posts_for_persist, &HashSet::new(), false)
                    .map_err(|e| format!("Failed to store recommendation tags: {e}"))?;
                Ok(())
            })
            .await;
            if let Ok(Err(e)) = res {
                warn!("background recommendation persist failed: {e}");
            } else if let Err(e) = res {
                warn!("background recommendation persist task panicked: {e}");
            }
        });
    }

    // Hydrate local candidates (skipping anything the user owns or already
    // saw, plus anything already in the live page so we don't waste a row).
    let live_ids: HashSet<i64> = live_posts.iter().map(|p| p.id).collect();
    let local_to_hydrate: Vec<i64> = local_candidate_ids
        .into_iter()
        .filter(|id| !live_ids.contains(id) && !owned_ids.contains(id) && !seen_ids.contains(id))
        .collect();
    let local_posts = if local_to_hydrate.is_empty() {
        Vec::new()
    } else {
        db_blocking(move || hydrate_posts_by_ids(&local_to_hydrate)).await?
    };

    // Filter live e621 posts through the same dedup lens. owned_ids is the
    // user's own favourites — discovering them in a "discover" feed is noise.
    let mut combined: Vec<Post> = Vec::with_capacity(live_posts.len() + local_posts.len());
    for post in live_posts {
        if owned_ids.contains(&post.id) || seen_ids.contains(&post.id) {
            continue;
        }
        combined.push(post);
    }
    let mut combined_ids: HashSet<i64> = combined.iter().map(|p| p.id).collect();
    for post in local_posts {
        if combined_ids.insert(post.id) {
            combined.push(post);
        }
    }

    let idf = current_idf();
    let global_relation = current_global_relation();

    let ctx = ScoringContext::new(
        &tags,
        &cfg.group_weights,
        &priors,
        &idf,
        &profile,
        &global_relation,
        &user_relation,
    );

    let mut scored: Vec<ScoredPost> = Vec::with_capacity(combined.len());
    for post in combined {
        let (s, breakdown) = ctx.score(&post);
        scored.push(ScoredPost {
            post,
            score: s,
            breakdown: Some(breakdown),
        });
    }

    if let Some(threshold) = affinity_threshold {
        scored.retain(|sp| sp.score >= threshold);
    }
    let scored = diversify_scored_posts(scored, &priors);

    Ok(Json(scored))
}

#[get("/openapi.json")]
fn openapi_json(spec: &State<OpenApi>) -> Json<OpenApi> {
    Json(spec.inner().clone())
}

// Catchers — uniform JSON error envelope for everything Rocket would
// otherwise serve as a default HTML page (S-3 in the audit). These are
// registered on the `/api` scope only, so the SPA fallback for `/`
// keeps returning index.html instead of a JSON 404 to a browser.
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
    let cors = rocket_cors::CorsOptions::default()
        .allowed_origins(AllowedOrigins::all())
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
        log_feed_interaction,
        list_accounts,
        get_account_tag_counts,
        get_account_id,
        get_account_name,
        create_account,
        delete_account,
        get_account_blacklist,
        update_account_blacklist,
        get_recommendations
    ];

    let r = rocket::build()
        .manage(Mutex::new(watcher))
        .manage(spec)
        .mount("/api", api_routes)
        .mount("/api", rocket::routes![get_account_tag_relations])
        .mount("/api", routes![openapi_json])
        .mount(
            "/api/swagger-ui",
            make_swagger_ui(&SwaggerUIConfig {
                url: "/api/openapi.json".to_owned(),
                ..Default::default()
            }),
        )
        .register("/api", catchers![catch_404, catch_422, catch_500])
        .attach(DbInit);

    prefetch::spawn_prefetch_workers();

    attach_cors(r)
}
