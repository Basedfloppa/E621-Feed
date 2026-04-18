#[macro_use]
extern crate rocket;

use chrono::Utc;
use rocket::{State, get};
use rocket::{futures::lock::Mutex, serde::json::Json};
use rusqlite::Result;
use std::collections::HashSet;
use rocket_cors::AllowedOrigins;

use crate::models::{
    DeviceScopedAccount, FeedInteractionRequest, ScoredPost, TruncatedAccount, UserApiResponse,
    cfg, default_path, reload_from, start_config_watcher,
};
use crate::{
    db::{
        DbInit, get_account_by_id, get_account_by_name, get_account_preference_profile,
        get_accounts_for_owner, get_tag_counts, record_feed_interaction, refresh_account_profiles,
        set_account, upsert_catalog_posts,
    },
    models::{Post, TagCount},
    rocket::serde::json
};
use rocket_okapi::okapi::openapi3::OpenApi;
use rocket_okapi::{openapi, openapi_get_routes_spec, settings::OpenApiSettings, swagger_ui::*};
use crate::utils::{IdfIndex, diversify_scored_posts, score_post};

mod api;
mod db;
mod models;
mod utils;

#[openapi(tag = "Processing")]
#[post("/process/<account_id>?<owner_token>")]
async fn process_posts(account_id: i32, owner_token: &str) -> Result<String, String> {
    let cfg = cfg();
    let blacklist: HashSet<String> = cfg
        .tag_blacklist
        .iter()
        .map(|s| s.to_lowercase())
        .collect();
    let account = get_account_by_id(owner_token, account_id).map_err(|e| e.to_string())?;
    let user = api::get_account(&account).await;
    let favcount = match user {
        UserApiResponse::FullCurrentUser(u) => u.favorite_count,
        UserApiResponse::FullUser(u) => u.favorite_count,
    };
    let pages = (favcount / cfg.posts_limit) + (if favcount % cfg.posts_limit > 0 { 1 } else { 0 });

    db::drop_account_posts(account_id).map_err(|e| format!("Failed to drop account posts: {e}"))?;

    for i in 1..=pages {
        let raw_posts = api::get_favorites(&account, i).await;
        let posts: Vec<Post> = raw_posts
            .into_iter()
            .map(|p| strip_blacklisted_tags(p, &blacklist))
            .collect();
        info!("{} post(s) found on page {}", posts.len(), i);

        db::save_posts(&posts, account.id).map_err(|e| format!("Failed to save posts: {e}"))?;

        db::save_posts_tags_batch(&posts, &blacklist)
            .map_err(|e| format!("Failed to save tags for page {i}: {e}"))?;
    }

    refresh_account_profiles(account_id)
        .map_err(|e| format!("Failed to refresh account profiles: {e}"))?;
    Ok(json::to_string(&"okay :3").unwrap())
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
async fn get_account_tag_counts(account_id: i32, owner_token: &str) -> Result<Json<Vec<TagCount>>, String> {
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
async fn create_account(account: Json<DeviceScopedAccount>) -> Result<Json<TruncatedAccount>, String> {
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
    let posts: Vec<Post> = api::get_posts(&account, page).await;
    upsert_catalog_posts(&posts)
        .map_err(|e| std::io::Error::other(format!("Failed to store recommendation catalog posts: {e}")))?;
    db::save_posts_tags_batch(&posts, &HashSet::new())
        .map_err(|e| std::io::Error::other(format!("Failed to store recommendation tags: {e}")))?;
    let idf = IdfIndex::from_db(db::get_tags_df, db::post_count, priors.now)
        .map_err(|e| std::io::Error::other(format!("Failed to build IDF index: {e}")))?;

    let mut scored: Vec<ScoredPost> = Vec::with_capacity(posts.len());
    for post in posts {
        let tmp_post = post.clone();

        let (s, breakdown) = score_post(
            &tags,
            &post,
            &cfg.group_weights,
            &priors,
            &idf,
            &profile,
        );

        scored.push(ScoredPost {
            post: tmp_post,
            score: s,
            breakdown: Some(breakdown),
        });
    }

    if let Some(threshold) = affinity_threshold {
        scored.retain(|sp| sp.score >= threshold);
    }

    let scored = diversify_scored_posts(scored);

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
        log_feed_interaction,
        list_accounts,
        get_account_tag_counts,
        get_account_id,
        get_account_name,
        create_account,
        get_recommendations
    ];

    let r = rocket::build()
        .manage(Mutex::new(watcher))
        .manage(spec)
        .mount("/api", api_routes)
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
