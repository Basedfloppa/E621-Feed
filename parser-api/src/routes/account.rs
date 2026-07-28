//! Account-scoped routes: tag counts, lookup by name/id, list, create,
//! delete, blacklist read/update, experiment-bucket read, tag-relation graph.

use std::hash::{DefaultHasher, Hasher};
use std::io::Cursor;

use rocket::http::{ContentType, Header, Status};
use rocket::request::{self, FromRequest, Request};
use rocket::response::{self, Responder, Response};
use rocket::serde::json::{Json, serde_json};

use rocket_okapi::openapi;

use crate::db_blocking;
use e621_account_parser_api::auth::OwnerToken;
use e621_account_parser_api::{
    api,
    db::{
        self, get_account_by_id, get_account_by_name, get_account_preference_profile,
        get_account_tag_relation_graph, get_accounts_for_owner, get_tag_counts, set_account,
        update_device_blacklist,
    },
    errors::ApiError,
    models::{
        AccountPreferenceProfile, BlacklistPayload, DeviceScopedAccount, PreferredTagPayload,
        TagCount, TagRelationScoring, TruncatedAccount, UserApiResponse, cfg,
    },
    ratelimit::{self, ClientIp},
    validation,
};

/// Normalise the client-supplied `Option<String>` blacklist. The default
/// fallback is applied **at DB write** (`db::set_account` /
/// `db::update_device_blacklist`) — this helper just trims/dedupes and
/// returns the (possibly empty) text. Empty in → empty out → DB layer
/// substitutes `cfg().tag_blacklist`.
fn normalize_optional_blacklist(input: Option<&str>) -> String {
    input
        .map(validation::normalize_blacklist)
        .unwrap_or_default()
}

#[openapi(tag = "Accounts")]
#[get("/account/<account_id>/tag_counts")]
pub(crate) async fn get_account_tag_counts(
    account_id: i32,
    owner: OwnerToken,
) -> Result<Json<Vec<TagCount>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let counts = db_blocking(move || {
        get_account_by_id(&owner_token, account_id)
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

/// Return the account's preference profile (rating, media, quality, recency
/// stats) used for the "Your Taste Profile" dashboard.
#[openapi(tag = "Users")]
#[get("/account/<account_id>/profile")]
pub(crate) async fn get_account_profile(
    account_id: i32,
    owner: OwnerToken,
) -> Result<Json<AccountPreferenceProfile>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let profile = db_blocking(move || {
        get_account_by_id(&owner_token, account_id)
            .map_err(|e| format!("Failed to validate account access: {e}"))?;
        get_account_preference_profile(account_id).map_err(|e| {
            let m = format!("Failed to get profile: {e}");
            error!("{m}");
            m
        })
    })
    .await?;
    Ok(Json(profile))
}

#[openapi(tag = "Users")]
#[get("/user/name/<name>")]
pub(crate) async fn get_account_name(
    name: &str,
    owner: OwnerToken,
) -> Result<Json<TruncatedAccount>, ApiError> {
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let name_owned = name.to_string();

    let owner_for_local = owner_token.clone();
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
#[get("/user/id/<id>")]
pub(crate) async fn get_account_id(
    id: i32,
    owner: OwnerToken,
) -> Result<Json<TruncatedAccount>, ApiError> {
    validation::validate_account_id(id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;

    // Mirror `/user/name/<name>` semantics: try local first, fall back
    // to an e621 lookup if the account isn't saved on this device yet.
    // Without the fallback, the SPA's search-by-ID flow couldn't
    // surface an unsaved account at all (the route would 404), so the
    // "looks like this account isn't saved — create it?" prompt on the
    // home page never had data to show.
    let owner_for_local = owner_token.clone();
    let local = db_blocking(move || get_account_by_id(&owner_for_local, id)).await;
    if let Ok(acc) = local {
        return Ok(Json(acc));
    }

    ratelimit::check(&format!("user_lookup:owner:{owner_token}"), 30, 10)?;
    let response = api::get_user_by_id(id).await.map_err(|e| {
        error!("e621 user lookup for id={id} failed: {e}");
        ApiError::NotFound(format!("No account found for ID {id}"))
    })?;
    let (resolved_id, resolved_name) = match response {
        UserApiResponse::FullCurrentUser(u) => (u.id, u.name),
        UserApiResponse::FullUser(u) => (u.id, u.name),
    };
    Ok(Json(TruncatedAccount {
        id: resolved_id,
        name: resolved_name,
        blacklist: String::new(),
    }))
}

#[openapi(tag = "Accounts")]
#[get("/accounts")]
pub(crate) async fn list_accounts(
    owner: OwnerToken,
) -> Result<Json<Vec<TruncatedAccount>>, ApiError> {
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let accounts = db_blocking(move || get_accounts_for_owner(&owner_token)).await?;
    Ok(Json(accounts))
}

#[openapi(tag = "Accounts")]
#[post("/account", data = "<account>")]
pub(crate) async fn create_account(
    account: Json<DeviceScopedAccount>,
    owner: OwnerToken,
    client_ip: ClientIp,
) -> Result<Json<TruncatedAccount>, ApiError> {
    let acc = account.into_inner();
    let owner_token = owner.0;
    validation::validate_device_scoped_account(&acc)?;
    ratelimit::check(&format!("acct_create:ip:{}", client_ip.0), 5, 5)?;
    ratelimit::check(&format!("acct_create:owner:{owner_token}"), 10, 10)?;

    let resolved = match api::get_user_by_id(acc.id).await {
        Ok(r) => r,
        Err(e) => {
            warn!("e621 lookup for id={} failed: {e}", acc.id);
            return Err(ApiError::BadRequest(format!(
                "could not verify account {} on e621",
                acc.id
            )));
        }
    };
    let (resolved_id, resolved_name) = match resolved {
        UserApiResponse::FullCurrentUser(u) => (u.id, u.name),
        UserApiResponse::FullUser(u) => (u.id, u.name),
    };
    if resolved_id != acc.id || !resolved_name.eq_ignore_ascii_case(acc.name.trim()) {
        return Err(ApiError::BadRequest(format!(
            "name does not match e621 user {resolved_id} ('{resolved_name}')"
        )));
    }
    let canonical_name = resolved_name;
    // Just normalise; DB layer applies the default if input is empty.
    let normalized_blacklist = normalize_optional_blacklist(acc.blacklist.as_deref());

    let acc_id_for_audit = acc.id;
    let name_for_audit = canonical_name.clone();
    let result = db_blocking(move || {
        set_account(&owner_token, acc.id, &canonical_name, &normalized_blacklist).map_err(|e| {
            let m = format!("Failed to set account: {e}");
            error!("{m}");
            m
        })
    })
    .await?;
    e621_account_parser_api::audit::event("account.set")
        .field("account_id", acc_id_for_audit)
        .field("name", name_for_audit)
        .emit();
    e621_account_parser_api::metrics::METRICS
        .accounts_created_total
        .inc();
    e621_account_parser_api::metrics::METRICS
        .accounts_total
        .inc();
    Ok(Json(result))
}

pub(crate) struct IfNoneMatch(Option<String>);

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

pub(crate) struct EtagJson {
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

#[get("/account/<account_id>/tag_relations?<top>&<min_cooc>")]
pub(crate) async fn get_account_tag_relations(
    account_id: i32,
    owner: OwnerToken,
    top: Option<usize>,
    min_cooc: Option<i64>,
    if_none_match: IfNoneMatch,
) -> Result<EtagJson, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let top = top.unwrap_or(60).clamp(2, 250);
    let min_cooc = min_cooc.unwrap_or(2).max(1);
    let mut payload = db_blocking(move || {
        get_account_by_id(&owner_token, account_id)
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
#[get("/account/<account_id>/blacklist")]
pub(crate) async fn get_account_blacklist(
    account_id: i32,
    owner: OwnerToken,
) -> Result<Json<BlacklistPayload>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let account = db_blocking(move || {
        get_account_by_id(&owner_token, account_id)
            .map_err(|e| format!("Failed to get account: {e}"))
    })
    .await?;
    // Wrap in `Some` for symmetry with the input shape — the GET response
    // always carries the persisted text, never null.
    Ok(Json(BlacklistPayload {
        blacklist: Some(account.blacklist),
    }))
}

#[openapi(tag = "Accounts")]
#[get("/account/<account_id>/experiment_bucket")]
pub(crate) async fn get_account_experiment_bucket(
    account_id: i32,
    owner: OwnerToken,
) -> Result<Json<serde_json::Value>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let explicit = db_blocking(move || {
        get_account_by_id(&owner_token, account_id)
            .map_err(|e| format!("Failed to validate account access: {e}"))?;
        db::get_account_experiment_bucket(account_id)
            .map_err(|e| format!("Failed to read experiment bucket: {e}"))
    })
    .await?;
    let (bucket, _) = cfg().pick_bucket(account_id, explicit.as_deref());
    Ok(Json(serde_json::json!({ "bucket": bucket })))
}

/// Sever device → account link. Cascade in `delete_device_link` drops the
/// underlying account row + derived tables when this was the last link.
#[openapi(tag = "Accounts")]
#[delete("/account/<account_id>")]
pub(crate) async fn delete_account(account_id: i32, owner: OwnerToken) -> Result<(), ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    let removed = db_blocking(move || db::delete_device_link(&owner_token, account_id)).await?;
    if removed == 0 {
        return Err(ApiError::NotFound(
            "No account found for this device token".into(),
        ));
    }
    e621_account_parser_api::audit::event("account.deleted")
        .field("account_id", account_id)
        .field("removed_links", removed)
        .emit();
    e621_account_parser_api::metrics::METRICS
        .accounts_deleted_total
        .inc();
    e621_account_parser_api::metrics::METRICS
        .accounts_total
        .dec();
    Ok(())
}

#[openapi(tag = "Accounts")]
#[patch("/account/<account_id>/blacklist", data = "<payload>")]
pub(crate) async fn update_account_blacklist(
    account_id: i32,
    owner: OwnerToken,
    payload: Json<BlacklistPayload>,
) -> Result<Json<TruncatedAccount>, ApiError> {
    validation::validate_account_id(account_id)?;
    validation::validate_blacklist_payload(&payload)?;
    let owner_token = owner.0;
    let body = payload.into_inner();
    let normalized_blacklist = normalize_optional_blacklist(body.blacklist.as_deref());
    let updated = db_blocking(move || {
        update_device_blacklist(&owner_token, account_id, &normalized_blacklist).map_err(|e| {
            let m = format!("Failed to update blacklist: {e}");
            error!("{m}");
            m
        })
    })
    .await?;

    // Blacklist change invalidates all cached e621 responses (keys contain
    // the old blacklist as a query parameter). Clear the whole cache rather
    // than trying to pattern-match individual keys.
    api::clear_api_cache();

    Ok(Json(updated))
}

#[openapi(tag = "Accounts")]
#[get("/account/<account_id>/preferred_tags")]
pub(crate) async fn get_account_preferred_tags(
    account_id: i32,
    owner: OwnerToken,
) -> Result<Json<PreferredTagPayload>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let tags = db_blocking(move || {
        get_account_by_id(&owner_token, account_id)
            .map_err(|e| format!("Failed to get account: {e}"))?;
        db::get_account_preferred_tags(account_id)
            .map_err(|e| format!("Failed to get preferred tags: {e}"))
    })
    .await?;
    Ok(Json(PreferredTagPayload {
        preferred_tags: tags,
    }))
}

#[openapi(tag = "Accounts")]
#[put("/account/<account_id>/preferred_tags", data = "<payload>")]
pub(crate) async fn set_account_preferred_tags(
    account_id: i32,
    owner: OwnerToken,
    payload: Json<PreferredTagPayload>,
) -> Result<Json<PreferredTagPayload>, ApiError> {
    validation::validate_account_id(account_id)?;
    validation::validate_preferred_tag_payload(&payload)?;
    let owner_token = owner.0;
    let body = payload.into_inner();
    let tags_for_write = body.preferred_tags.clone();
    let owner_for_write = owner_token.clone();
    db_blocking(move || {
        db::set_preferred_tags(&owner_for_write, account_id, &tags_for_write).map_err(|e| {
            let m = format!("Failed to set preferred tags: {e}");
            error!("{m}");
            m
        })
    })
    .await?;
    // Return current state (re-read to confirm).
    let tags = db_blocking(move || {
        db::get_account_preferred_tags(account_id)
            .map_err(|e| format!("Failed to get preferred tags: {e}"))
    })
    .await?;
    Ok(Json(PreferredTagPayload {
        preferred_tags: tags,
    }))
}
