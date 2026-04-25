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

use crate::models::{
    BlacklistPayload, DeviceScopedAccount, FeedInteractionRequest, ScoredPost,
    TagRelationScoring, TruncatedAccount, UserApiResponse, cfg,
    default_path, reload_from, start_config_watcher,
};
use crate::utils::{
    ScoringContext, current_global_relation, current_idf, diversify_scored_posts, mark_idf_dirty,
};
use crate::{
    db::{
        DbInit, get_account_by_id, get_account_by_name, get_account_preference_profile,
        get_account_tag_relation_graph, get_accounts_for_owner, get_tag_counts,
        record_feed_interaction, refresh_account_profiles, set_account, update_device_blacklist,
        upsert_catalog_posts,
    },
    models::{Post, TagCount},
};
use rocket_okapi::okapi::openapi3::OpenApi;
use rocket_okapi::{openapi, openapi_get_routes_spec, settings::OpenApiSettings, swagger_ui::*};

mod api;
mod db;
mod jobs;
mod models;
mod utils;

use jobs::{BeginResult, ProcessJobState};

const PROCESS_FETCH_CONCURRENCY: usize = 4;

/// Kicks off a background `/process` job and returns immediately with the
/// current job state. If a job for this account is already running, returns
/// the existing state instead of starting a new one.
#[openapi(tag = "Processing")]
#[post("/process/<account_id>?<owner_token>")]
async fn process_posts(
    account_id: i32,
    owner_token: &str,
) -> Result<Json<ProcessJobState>, String> {
    let _ = get_account_by_id(owner_token, account_id).map_err(|e| e.to_string())?;

    match jobs::try_begin(account_id) {
        BeginResult::AlreadyRunning(state) => Ok(Json(state)),
        BeginResult::Started(state) => {
            let owner_token_owned = owner_token.to_string();
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
) -> Result<Json<Option<ProcessJobState>>, String> {
    let _ = get_account_by_id(owner_token, account_id).map_err(|e| e.to_string())?;
    Ok(Json(jobs::get_state(account_id)))
}

async fn run_process(account_id: i32, owner_token: String) -> Result<(), String> {
    let cfg = cfg();
    let blacklist: HashSet<String> =
        cfg.tag_blacklist.iter().map(|s| s.to_lowercase()).collect();

    let account = get_account_by_id(&owner_token, account_id).map_err(|e| e.to_string())?;
    let user = api::get_account(&account).await?;
    let favcount = match user {
        UserApiResponse::FullCurrentUser(u) => u.favorite_count,
        UserApiResponse::FullUser(u) => u.favorite_count,
    };
    let pages =
        (favcount / cfg.posts_limit) + (if favcount % cfg.posts_limit > 0 { 1 } else { 0 });
    jobs::set_pages_total(account_id, pages);

    db::drop_account_posts(account_id)
        .map_err(|e| format!("Failed to drop account posts: {e}"))?;

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
        .buffer_unordered(PROCESS_FETCH_CONCURRENCY);

    while let Some((i, posts)) = stream.next().await {
        info!("{} post(s) found on page {}", posts.len(), i);
        db::save_posts(&posts, account.id).map_err(|e| format!("Failed to save posts: {e}"))?;
        db::save_posts_tags_batch(&posts, &blacklist)
            .map_err(|e| format!("Failed to save tags for page {i}: {e}"))?;
        jobs::record_page_done(account_id);
    }
    mark_idf_dirty();

    refresh_account_profiles(account_id)
        .map_err(|e| format!("Failed to refresh account profiles: {e}"))?;
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
) -> Result<Json<Vec<TagCount>>, String> {
    get_account_by_id(owner_token, account_id)
        .map_err(|e| format!("Failed to validate account access: {e}"))?;
    match get_tag_counts(account_id) {
        Ok(counts) => Ok(Json(counts.to_vec())),
        Err(e) => {
            let error_msg = format!("Failed to get tag counts: {e}");
            eprintln!("{error_msg}");
            Err(error_msg)
        }
    }
}

#[openapi(tag = "Users")]
#[get("/user/name/<name>?<owner_token>")]
async fn get_account_name(name: &str, owner_token: &str) -> Result<Json<TruncatedAccount>, String> {
    match get_account_by_name(owner_token, name.to_string()) {
        Ok(account) => Ok(Json(account)),
        Err(e) => {
            let error_msg = format!("Failed to get account: {e}");
            eprintln!("{error_msg}");
            Err(error_msg)
        }
    }
}

#[openapi(tag = "Users")]
#[get("/user/id/<id>?<owner_token>")]
async fn get_account_id(id: i32, owner_token: &str) -> Result<Json<TruncatedAccount>, String> {
    match get_account_by_id(owner_token, id) {
        Ok(account) => Ok(Json(account)),
        Err(e) => {
            let error_msg = format!("Failed to get account: {e}");
            eprintln!("{error_msg}");
            Err(error_msg)
        }
    }
}

#[openapi(tag = "Accounts")]
#[get("/accounts?<owner_token>")]
async fn list_accounts(owner_token: &str) -> Result<Json<Vec<TruncatedAccount>>, String> {
    get_accounts_for_owner(owner_token).map(Json)
}

#[openapi(tag = "Accounts")]
#[post("/account", data = "<account>")]
async fn create_account(
    account: Json<DeviceScopedAccount>,
) -> Result<Json<TruncatedAccount>, String> {
    match set_account(
        &account.owner_token,
        account.id,
        &account.name,
        &account.blacklist,
    ) {
        Ok(saved) => Ok(Json(saved)),
        Err(e) => {
            let error_msg = format!("Failed to get account: {e}");
            eprintln!("{error_msg}");
            Err(error_msg)
        }
    }
}

struct IfNoneMatch(Option<String>);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for IfNoneMatch {
    type Error = std::convert::Infallible;
    async fn from_request(req: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        request::Outcome::Success(IfNoneMatch(
            req.headers().get_one("If-None-Match").map(|s| s.to_string()),
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
            .map(|tag| {
                tag.split(',').any(|t| t.trim() == "*" || t.trim() == etag)
            })
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
) -> Result<EtagJson, String> {
    get_account_by_id(owner_token, account_id)
        .map_err(|e| format!("Failed to validate account access: {e}"))?;
    let top = top.unwrap_or(60).clamp(2, 250);
    let min_cooc = min_cooc.unwrap_or(2).max(1);
    let mut payload = get_account_tag_relation_graph(account_id, top, min_cooc)?;

    let priors = &cfg().priors;
    let n_fav = payload.account_post_count.max(0) as f32;
    let conf = n_fav / (n_fav + 50.0);
    let w_personal = priors.tag_relation_w_personal.max(0.0) * conf;
    let w_global =
        priors.tag_relation_w_global.max(0.0) + priors.tag_relation_w_personal.max(0.0) * (1.0 - conf);
    payload.scoring = TagRelationScoring {
        w_global,
        w_personal,
        pmi_scale: priors.tag_relation_pmi_scale.max(1e-3),
        cooc_ref: priors.tag_relation_cooc_ref.max(1.0),
        user_cooc_ref: priors.tag_relation_user_cooc_ref.max(1.0),
        min_cooc_global: priors.tag_relation_min_cooc.max(1),
        min_cooc_user: priors.tag_relation_user_min_cooc.max(1),
    };
    EtagJson::new(&payload, &if_none_match)
}

#[openapi(tag = "Accounts")]
#[get("/account/<account_id>/blacklist?<owner_token>")]
async fn get_account_blacklist(
    account_id: i32,
    owner_token: &str,
) -> Result<Json<BlacklistPayload>, String> {
    let account = get_account_by_id(owner_token, account_id)
        .map_err(|e| format!("Failed to get account: {e}"))?;
    Ok(Json(BlacklistPayload {
        blacklist: account.blacklist,
    }))
}

#[openapi(tag = "Accounts")]
#[patch("/account/<account_id>/blacklist?<owner_token>", data = "<payload>")]
async fn update_account_blacklist(
    account_id: i32,
    owner_token: &str,
    payload: Json<BlacklistPayload>,
) -> Result<Json<TruncatedAccount>, String> {
    match update_device_blacklist(owner_token, account_id, &payload.blacklist) {
        Ok(saved) => Ok(Json(saved)),
        Err(e) => {
            let error_msg = format!("Failed to update blacklist: {e}");
            eprintln!("{error_msg}");
            Err(error_msg)
        }
    }
}

#[openapi(tag = "Recommendations")]
#[post("/interaction", data = "<payload>")]
async fn log_feed_interaction(payload: Json<FeedInteractionRequest>) -> Result<(), String> {
    record_feed_interaction(&payload)
}

#[openapi(tag = "Recommendations")]
#[get("/recommendations/<account_id>?<owner_token>&<page>&<affinity_threshold>")]
async fn get_recommendations(
    account_id: i32,
    owner_token: &str,
    page: Option<i32>,
    affinity_threshold: Option<f32>,
) -> Result<Json<Vec<ScoredPost>>, std::io::Error> {
    let cfg = cfg();

    let mut priors = cfg.priors.clone();
    priors.now = Utc::now();

    let tags: Vec<TagCount> = get_tag_counts(account_id)
        .map_err(|e| std::io::Error::other(format!("Failed to get tag counts: {e}")))?
        .to_vec();
    let profile = get_account_preference_profile(account_id)
        .map_err(|e| std::io::Error::other(format!("Failed to get account profile: {e}")))?;

    let account = get_account_by_id(owner_token, account_id)
        .map_err(|e| std::io::Error::other(format!("Failed to get account: {e}")))?;
    let posts: Vec<Post> = api::get_posts(&account, page)
        .await
        .map_err(|e| std::io::Error::other(format!("Failed to fetch posts: {e}")))?;
    upsert_catalog_posts(&posts).map_err(|e| {
        std::io::Error::other(format!("Failed to store recommendation catalog posts: {e}"))
    })?;
    // Skip the per-pair cooccurrence upsert here: candidate browse posts are
    // not part of the user's truth set, and the heavy O(T²) work would land
    // on the request path. /process is the only place that drives the graph.
    db::save_posts_tags_batch_no_cooc(&posts, &HashSet::new())
        .map_err(|e| std::io::Error::other(format!("Failed to store recommendation tags: {e}")))?;
    mark_idf_dirty();
    let idf = current_idf();
    let global_relation = current_global_relation();
    let user_relation = db::load_account_tag_relation(account_id, &tags).map_err(|e| {
        std::io::Error::other(format!("Failed to load user tag relation graph: {e}"))
    })?;

    let ctx = ScoringContext::new(
        &tags,
        &cfg.group_weights,
        &priors,
        &idf,
        &profile,
        &global_relation,
        &user_relation,
    );

    let mut scored: Vec<ScoredPost> = Vec::with_capacity(posts.len());
    for post in posts {
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
        .attach(DbInit);

    attach_cors(r)
}
