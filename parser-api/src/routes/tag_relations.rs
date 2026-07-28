//! Tag-relation routes: resolve aliases and look up implications.
//!
//! These endpoints are **rate-limited but public** (no `OwnerToken` required)
//! because they are used by the frontend's `TasteProfileCard` to resolve
//! alias chains and filter generic tags. The data comes from the local
//! `tag_aliases` / `tag_implications` tables, which are synced from e621
//! by the background `tag_relation_import` worker.

use std::collections::{HashMap, HashSet};

use rocket::serde::json::Json;
use rocket_okapi::openapi;

use e621_account_parser_api::{
    db::{
        get_alias_consequent_cached, get_aliases_for, get_implications,
        get_implications_batch_cached, get_implied_by,
    },
    errors::ApiError,
    models::{
        TagImplicationsBatchRequest, TagImplicationsBatchResponse, TagImplicationsResponse,
        TagResolveBatchRequest, TagResolveBatchResponse, TagResolveResponse,
    },
    ratelimit::{self, ClientIp},
};

/// Resolve a tag through the alias chain to its canonical name and return
/// all known synonyms.
///
/// Example: `?tag=canyne` → `{ "query": "canyne", "canonical": "canine",
/// "synonyms": ["canid", "canine", "canis", "dog", "wolf", ...] }`
#[openapi(tag = "Tag Relations")]
#[get("/tag_relations/resolve?<tag>")]
pub(crate) async fn resolve_tag(
    tag: &str,
    client_ip: ClientIp,
) -> Result<Json<TagResolveResponse>, ApiError> {
    if tag.is_empty() {
        return Err(ApiError::BadRequest("tag parameter is required".into()));
    }
    // Generous rate limit: tag resolution is cheap (reads only).
    ratelimit::check(&format!("tag_resolve:ip:{}", client_ip.0), 120, 60)?;

    let tag_lc = tag.to_ascii_lowercase();
    let canonical = {
        let t = tag_lc.clone();
        crate::db_blocking(move || {
            get_alias_consequent_cached(&t).map_err(|e| format!("resolve alias: {e}"))
        })
        .await?
        .unwrap_or_else(|| tag_lc.clone())
    };

    // Fetch all synonyms for the canonical tag (including itself).
    let mut synonyms = {
        let c = canonical.clone();
        crate::db_blocking(move || get_aliases_for(&c).map_err(|e| format!("get aliases for: {e}")))
            .await?
    };
    // Include the canonical name and the original query if different.
    if !synonyms.contains(&canonical) {
        synonyms.push(canonical.clone());
    }
    if tag_lc != canonical && !synonyms.contains(&tag_lc) {
        synonyms.push(tag_lc.clone());
    }
    synonyms.sort();
    synonyms.dedup();

    Ok(Json(TagResolveResponse {
        query: tag_lc,
        canonical,
        synonyms,
    }))
}

/// Compatibility route for UI tag autocomplete. Keep the shorter `/tag`
/// namespace while the relation graph continues to use `/tag_relations`.
#[openapi(tag = "Tag Relations")]
#[get("/tag/resolve?<tag>")]
pub(crate) async fn resolve_tag_autocomplete(
    tag: &str,
    client_ip: ClientIp,
) -> Result<Json<TagResolveResponse>, ApiError> {
    resolve_tag(tag, client_ip).await
}

/// Look up implications for a tag: which tags it implies, and which tags
/// imply it.
///
/// Example: `?tag=canine` → `{ "tag": "canine", "implies": ["wolf", "dog"],
/// "implied_by": ["canyne", "canid"] }`
#[openapi(tag = "Tag Relations")]
#[get("/tag_relations/implications?<tag>")]
pub(crate) async fn get_tag_implications(
    tag: &str,
    client_ip: ClientIp,
) -> Result<Json<TagImplicationsResponse>, ApiError> {
    if tag.is_empty() {
        return Err(ApiError::BadRequest("tag parameter is required".into()));
    }
    ratelimit::check(&format!("tag_implications:ip:{}", client_ip.0), 120, 60)?;

    // First resolve through aliases so we look up implications for the
    // canonical name.
    let tag_lc = tag.to_ascii_lowercase();
    let canonical = crate::db_blocking({
        let tag_lc = tag_lc.clone();
        move || get_alias_consequent_cached(&tag_lc).map_err(|e| format!("resolve alias: {e}"))
    })
    .await?
    .unwrap_or_else(|| tag_lc.clone());

    // Implications where canonical is the antecedent (→ what it implies).
    let implies = crate::db_blocking({
        let canonical = canonical.clone();
        move || get_implications(&canonical).map_err(|e| format!("get implications: {e}"))
    })
    .await?;

    // Implications where canonical is the consequent (← what implies it).
    // Re-use get_aliases_for conceptually but search tag_implications
    // where consequent_name = canonical and status = 'active'.
    let implied_by = crate::db_blocking({
        let canonical = canonical.clone();
        move || get_implied_by(&canonical).map_err(|e| format!("get implied_by: {e}"))
    })
    .await?;

    Ok(Json(TagImplicationsResponse {
        tag: tag_lc,
        implies,
        implied_by,
    }))
}

/// Batch-resolve multiple tags through the alias chain. Used by the
/// frontend's TasteProfileCard to resolve all species tags at once.
///
/// Request body: `{ "tags": ["canyne", "wolf", "kitten", ...] }`
/// Response: `{ "resolved": { "canyne": "canine", ... }, "canonicals": [...] }`
#[openapi(tag = "Tag Relations")]
#[post("/tag_relations/resolve-batch", data = "<req>")]
pub(crate) async fn resolve_tag_batch(
    req: Json<TagResolveBatchRequest>,
    client_ip: ClientIp,
) -> Result<Json<TagResolveBatchResponse>, ApiError> {
    if req.tags.is_empty() {
        return Ok(Json(TagResolveBatchResponse {
            resolved: HashMap::new(),
            canonicals: Vec::new(),
        }));
    }
    // Stricter rate limit for batch (50 req/min) since it does N lookups.
    ratelimit::check(&format!("tag_resolve_batch:ip:{}", client_ip.0), 50, 60)?;

    // Deduplicate and lowercase input tags.
    let unique_tags: Vec<String> = {
        let mut seen = HashSet::new();
        req.tags
            .iter()
            .map(|t| t.to_ascii_lowercase())
            .filter(|t| seen.insert(t.clone()))
            .collect()
    };

    // Resolve each tag through the alias chain (in a single blocking task).
    let (resolved, canonicals_set) = crate::db_blocking({
        let input = unique_tags.clone();
        move || {
            let mut resolved = HashMap::with_capacity(input.len());
            let mut all_canonicals = HashSet::new();

            for tag in &input {
                let canonical = get_alias_consequent_cached(tag)
                    .map_err(|e| format!("resolve {}: {e}", tag))?
                    .unwrap_or_else(|| tag.clone());
                resolved.insert(tag.clone(), canonical.clone());
                all_canonicals.insert(canonical);
            }

            let mut canonicals: Vec<String> = all_canonicals.into_iter().collect();
            canonicals.sort();
            Ok((resolved, canonicals))
        }
    })
    .await?;

    Ok(Json(TagResolveBatchResponse {
        resolved,
        canonicals: canonicals_set,
    }))
}

/// Batch-lookup implications for multiple tags. Used by the frontend to
/// expand the signal set: if tag A is in the signal set and tag A implies
/// tag B, then tag B should also be considered signal.
///
/// Request body: `{ "tags": ["canine", "feline", ...] }`
/// Response: `{ "implications": { "canine": ["wolf", "dog"], ... } }`
#[openapi(tag = "Tag Relations")]
#[post("/tag_relations/implications-batch", data = "<req>")]
#[allow(dead_code)]
pub(crate) async fn get_tag_implications_batch(
    req: Json<TagImplicationsBatchRequest>,
    client_ip: ClientIp,
) -> Result<Json<TagImplicationsBatchResponse>, ApiError> {
    ratelimit::check(&format!("tag_impl_batch:ip:{}", client_ip.0), 50, 60)?;

    let unique_tags: Vec<String> = {
        let mut seen = HashSet::new();
        req.tags
            .iter()
            .map(|t| t.to_ascii_lowercase())
            .filter(|t| seen.insert(t.clone()))
            .collect()
    };

    if unique_tags.is_empty() {
        return Ok(Json(TagImplicationsBatchResponse {
            implications: HashMap::new(),
        }));
    }

    let implications = crate::db_blocking(move || {
        get_implications_batch_cached(&unique_tags).map_err(|e| format!("implications batch: {e}"))
    })
    .await?;

    Ok(Json(TagImplicationsBatchResponse { implications }))
}
