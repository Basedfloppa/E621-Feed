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
        self, delete_all_device_links_for_token, find_device_token_by_id, get_account_by_id,
        get_account_by_name, get_account_feed_settings, get_account_interactions_for_export,
        get_account_preference_profile, get_account_tag_relation_graph, get_accounts_for_owner,
        get_tag_counts, list_device_sessions, restore_feed_interactions, set_account,
        set_preferred_tags, update_device_blacklist,
    },
    errors::ApiError,
    models::{
        AccountDataExport, AccountDataImport, AccountFeedSettings, AccountFeedSettingsPatch,
        AccountPreferenceProfile, BlacklistPayload, DeviceScopedAccount, DeviceSession,
        ExportAccountSummary, PreferredTagPayload, RevokeDeviceRequest, TagCount,
        TagRelationScoring, TruncatedAccount, UserApiResponse, cfg,
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

/// List the sessions/devices linked to the requesting `owner_token`: every
/// device sharing any of the token's accounts, with the accounts each device
/// is linked to. Device ids are `sha256` hex — raw tokens are never exposed.
#[openapi(tag = "Session")]
#[get("/session/devices")]
pub(crate) async fn get_session_devices(
    owner: OwnerToken,
) -> Result<Json<Vec<DeviceSession>>, ApiError> {
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let devices = db_blocking(move || {
        list_device_sessions(&owner_token).map_err(|e| format!("Failed to list devices: {e}"))
    })
    .await?;
    Ok(Json(devices))
}

/// Revoke another device's session (by the public `sha256` id from
/// `GET /session/devices`): the target device token is added to the
/// revocation denylist and its account links are severed. The current
/// device cannot revoke itself here (use `DELETE /api/session`).
#[openapi(tag = "Session")]
#[post("/session/revoke", data = "<payload>")]
pub(crate) async fn revoke_device_session(
    owner: OwnerToken,
    payload: Json<RevokeDeviceRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let owner_token = owner.0;
    ratelimit::check(&format!("session_revoke:owner:{owner_token}"), 10, 60)?;
    let revoke_id = payload.into_inner().device_id;
    let owner_for = owner_token.clone();
    let revoke_id_for = revoke_id.clone();
    let device_token = db_blocking(move || {
        find_device_token_by_id(&owner_for, &revoke_id_for)
            .map_err(|e| format!("Failed to resolve device session: {e}"))
    })
    .await?;
    let Some(device_token) = device_token else {
        return Err(ApiError::NotFound("device session not found".into()));
    };

    // Add the token to the in-memory + persisted revocation denylist.
    crate::auth::revoke(&device_token)
        .map_err(|e| ApiError::Internal(format!("Failed to revoke device session: {e}")))?;

    let sever = device_token.clone();
    let links_removed = db_blocking(move || {
        delete_all_device_links_for_token(&sever)
            .map_err(|e| format!("Failed to sever device links: {e}"))
    })
    .await?;

    crate::audit::event("session.device_revoked")
        .field("device_id", revoke_id)
        .field("links_removed", links_removed as i64)
        .emit();
    Ok(Json(serde_json::json!({
        "revoked": true,
        "linksRemoved": links_removed
    })))
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
        UserApiResponse::FullUser(u) => (u.id, u.name),
    };
    Ok(Json(TruncatedAccount {
        id: resolved_id,
        name: resolved_name,
        blacklist: String::new(),
    }))
}

#[openapi(tag = "Accounts")]
#[get("/accounts?<limit>&<offset>")]
pub(crate) async fn list_accounts(
    owner: OwnerToken,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Json<Vec<TruncatedAccount>>, ApiError> {
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let mut accounts = db_blocking(move || get_accounts_for_owner(&owner_token)).await?;
    // Pagination: advance past `offset` entries, then return at most `limit`.
    let skip = offset.unwrap_or(0).min(accounts.len());
    accounts.drain(..skip);
    if let Some(lim) = limit {
        accounts.truncate(lim);
    }
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
    // Track A/B bucket distribution of newly created accounts.
    let bucket = e621_account_parser_api::models::cfg()
        .pick_bucket(acc_id_for_audit, None)
        .0
        .unwrap_or_else(|| "none".to_string());
    e621_account_parser_api::metrics::METRICS
        .experiment_bucket_accounts
        .with_label_values(&[&bucket])
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
                .map(std::string::ToString::to_string),
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
            .is_some_and(|tag| tag.split(',').any(|t| t.trim() == "*" || t.trim() == etag));
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
    e621_account_parser_api::audit::event("token.revoked")
        .field("account_id", account_id)
        .field("reason", "device_unlink")
        .emit();
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
    // Track A/B bucket distribution of deleted accounts.
    let bucket = e621_account_parser_api::models::cfg()
        .pick_bucket(account_id, None)
        .0
        .unwrap_or_else(|| "none".to_string());
    e621_account_parser_api::metrics::METRICS
        .experiment_bucket_accounts
        .with_label_values(&[&bucket])
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
    // Blacklist writes flush the shared e621 cache; cap them per owner so a
    // client can't keep the cache cold or burn the writer by replaying writes.
    ratelimit::check(&format!("blacklist:owner:{owner_token}"), 60, 10)?;
    let body = payload.into_inner();
    let normalized_blacklist = normalize_optional_blacklist(body.blacklist.as_deref());
    let (updated, changed) = db_blocking(move || {
        update_device_blacklist(&owner_token, account_id, &normalized_blacklist).map_err(|e| {
            let m = format!("Failed to update blacklist: {e}");
            error!("{m}");
            m
        })
    })
    .await?;

    if changed {
        // Blacklist change invalidates all cached e621 responses (keys contain
        // the old blacklist as a query parameter). Clear the whole cache rather
        // than trying to pattern-match individual keys. Only flush on an actual
        // change so replaying an unchanged value can't keep the cache cold.
        api::clear_api_cache();
        // A digest built under a weaker/older blacklist must not be replayed
        // for up to 6h — the digest cache is separate from the e621 API cache.
        crate::routes::digest::clear_digest_cache();
    }

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

/// Consolidated read of all per-account feed/recommendation settings.
/// Returns blacklist, preferred tags, and experiment bucket in one call.
#[openapi(tag = "Accounts")]
#[get("/account/<account_id>/feed_settings")]
pub(crate) async fn get_feed_settings(
    account_id: i32,
    owner: OwnerToken,
) -> Result<Json<AccountFeedSettings>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let settings = db_blocking(move || {
        get_account_feed_settings(&owner_token, account_id).map_err(|e| {
            let m = format!("Failed to get feed settings: {e}");
            error!("{m}");
            m
        })
    })
    .await?;

    // The account may not have a persisted bucket (NULL = not bucketed yet).
    // Match the recommendations endpoint: hash the account into a configured
    // bucket on the fly so the UI shows the effective bucket.
    let effective_bucket = settings.experiment_bucket.clone().or_else(|| {
        let (name, _) = cfg().pick_bucket(account_id, None);
        name
    });
    let mut settings = settings;
    settings.experiment_bucket = effective_bucket;
    Ok(Json(settings))
}

/// Partial update of per-account feed settings. Accepts `{ blacklist, preferred_tags }`
/// — only present fields are updated. Returns the full current settings after the update.
#[openapi(tag = "Accounts")]
#[patch("/account/<account_id>/feed_settings", data = "<payload>")]
pub(crate) async fn patch_feed_settings(
    account_id: i32,
    owner: OwnerToken,
    payload: Json<AccountFeedSettingsPatch>,
) -> Result<Json<AccountFeedSettings>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    // feed_settings can write the blacklist (flushing the shared cache) and
    // preferred tags (rebuilding scoring state) — cap it per owner.
    ratelimit::check(&format!("feed_settings:owner:{owner_token}"), 60, 10)?;
    let patch = payload.into_inner();

    // Apply partial updates in order.
    if let Some(blacklist) = &patch.blacklist {
        let normalized = if blacklist.is_empty() {
            String::new()
        } else {
            validation::normalize_blacklist(blacklist)
        };
        // Enforce the same blacklist limits (size, control chars, HTML/script
        // probes) that the dedicated /blacklist route applies — the feed
        // settings path previously skipped `validate_blacklist_text`, letting a
        // client persist arbitrarily large blobs into the shared table.
        validation::validate_blacklist_text(&normalized)?;
        let token = owner_token.clone();
        let (_, changed) = db_blocking(move || {
            update_device_blacklist(&token, account_id, &normalized).map_err(|e| {
                let m = format!("Failed to update blacklist from feed_settings: {e}");
                error!("{m}");
                m
            })
        })
        .await?;
        if changed {
            // Blacklist change invalidates e621 API cache (only on real change).
            api::clear_api_cache();
            crate::routes::digest::clear_digest_cache();
        }
    }

    if let Some(preferred_tags) = &patch.preferred_tags {
        // Enforce the same 50-tag cap / weight / group / duplicate rules as the
        // dedicated PUT /preferred_tags route — this path previously bypassed
        // `validate_preferred_tag_payload`, letting a client trigger an
        // arbitrarily large delete+insert loop under the global writer mutex.
        validation::validate_preferred_tag_payload(&PreferredTagPayload {
            preferred_tags: preferred_tags.clone(),
        })?;
        let token = owner_token.clone();
        let tags = preferred_tags.clone();
        db_blocking(move || {
            set_preferred_tags(&token, account_id, &tags).map_err(|e| {
                let m = format!("Failed to update preferred tags from feed_settings: {e}");
                error!("{m}");
                m
            })
        })
        .await?;
    }

    // Return the full current state.
    let settings = db_blocking(move || {
        get_account_feed_settings(&owner_token, account_id).map_err(|e| {
            let m = format!("Failed to get feed settings after update: {e}");
            error!("{m}");
            m
        })
    })
    .await?;

    // Same effective-bucket resolution as GET: when the account has no
    // persisted bucket, hash into a configured bucket on the fly.
    let effective_bucket = settings.experiment_bucket.clone().or_else(|| {
        let (name, _) = cfg().pick_bucket(account_id, None);
        name
    });
    let mut settings = settings;
    settings.experiment_bucket = effective_bucket;
    Ok(Json(settings))
}

/// Full account data snapshot for backup / migration. Returns the
/// account identity, effective blacklist, preferred tags, experiment
/// bucket, and the current preference profile in one JSON document.
///
/// Import restores only the user-settable fields — the profile is
/// derived state recomputed by `/process`.
#[openapi(tag = "Accounts")]
#[get("/account/<account_id>/export")]
pub(crate) async fn export_account(
    account_id: i32,
    owner: OwnerToken,
) -> Result<Json<AccountDataExport>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let owner_for_auth = owner_token.clone();
    let (name, settings, profile, interactions) = db_blocking(move || {
        let account = get_account_by_id(&owner_for_auth, account_id)
            .map_err(|e| format!("Failed to validate account access: {e}"))?;
        let settings = get_account_feed_settings(&owner_for_auth, account_id)
            .map_err(|e| format!("Failed to get feed settings: {e}"))?;
        let profile = get_account_preference_profile(account_id)
            .map_err(|e| format!("Failed to get profile: {e}"))?;
        let interactions = get_account_interactions_for_export(account_id, 100_000)
            .map_err(|e| format!("Failed to export interactions: {e}"))?;
        Ok::<_, String>((account.name, settings, profile, interactions))
    })
    .await?;

    let export = AccountDataExport {
        account: ExportAccountSummary {
            id: account_id,
            name,
        },
        blacklist: settings.blacklist,
        preferred_tags: settings.preferred_tags,
        experiment_bucket: settings.experiment_bucket,
        profile,
        interactions,
    };
    Ok(Json(export))
}

/// Restore user-settable account settings from an export payload.
/// Accepts `{ blacklist, preferred_tags }` — only present fields are
/// updated. The `profile` field of an export is ignored (it is derived
/// state recomputed by `/process`). Returns the full current settings
/// after the update.
#[openapi(tag = "Accounts")]
#[post("/account/<account_id>/import", data = "<payload>")]
pub(crate) async fn import_account(
    account_id: i32,
    owner: OwnerToken,
    payload: Json<AccountDataImport>,
) -> Result<Json<AccountFeedSettings>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let import = payload.into_inner();

    if let Some(blacklist) = &import.blacklist {
        let normalized = if blacklist.is_empty() {
            String::new()
        } else {
            validation::normalize_blacklist(blacklist)
        };
        // Same blacklist size/control-char/HTML limits as the /blacklist route.
        validation::validate_blacklist_text(&normalized)?;
        let token = owner_token.clone();
        let (_, changed) = db_blocking(move || {
            update_device_blacklist(&token, account_id, &normalized).map_err(|e| {
                let m = format!("Failed to update blacklist from import: {e}");
                error!("{m}");
                m
            })
        })
        .await?;
        if changed {
            // Blacklist change invalidates e621 API cache (only on real change).
            api::clear_api_cache();
            crate::routes::digest::clear_digest_cache();
        }
    }

    if let Some(preferred_tags) = &import.preferred_tags {
        validation::validate_preferred_tag_payload(&PreferredTagPayload {
            preferred_tags: preferred_tags.clone(),
        })?;
        let token = owner_token.clone();
        let tags = preferred_tags.clone();
        db_blocking(move || {
            set_preferred_tags(&token, account_id, &tags).map_err(|e| {
                let m = format!("Failed to update preferred tags from import: {e}");
                error!("{m}");
                m
            })
        })
        .await?;
    }

    // Restore the interaction model (open/like/hide/… events). Explicitly
    // verify ownership first — unlike the blacklist/tags writers, the restore
    // path writes raw `feed_interactions` rows and must not touch an account
    // this token isn't linked to.
    if let Some(interactions) = &import.interactions {
        let token = owner_token.clone();
        let interactions = interactions.clone();
        db_blocking(move || {
            get_account_by_id(&token, account_id)
                .map_err(|e| format!("Failed to validate account access for import: {e}"))?;
            restore_feed_interactions(account_id, &interactions).map_err(|e| {
                let m = format!("Failed to restore interactions from import: {e}");
                error!("{m}");
                m
            })?;
            Ok::<_, String>(())
        })
        .await?;
    }

    // Return the full current state (same shape as GET /feed_settings).
    let settings = db_blocking(move || {
        get_account_feed_settings(&owner_token, account_id).map_err(|e| {
            let m = format!("Failed to get feed settings after import: {e}");
            error!("{m}");
            m
        })
    })
    .await?;

    let effective_bucket = settings.experiment_bucket.clone().or_else(|| {
        let (name, _) = cfg().pick_bucket(account_id, None);
        name
    });
    let mut settings = settings;
    settings.experiment_bucket = effective_bucket;
    Ok(Json(settings))
}
