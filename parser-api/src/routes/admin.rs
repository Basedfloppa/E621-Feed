//! Owner console — `GET /api/admin/overview` (TODO §3.2).
//!
//! This is the owner's view of *their own* instance, so it adds no new
//! authority: the account rows come from the same `account_device_links`
//! filter `/accounts` already applies, which is why §3.2 says the console
//! needs a UI rather than new routes. The one genuinely new capability — the
//! A/B bucket assignment — lives next to the read-only bucket route in
//! `routes::account`, and this endpoint is what makes `[buckets.*]` visible at
//! all: `pick_bucket` silently ignores an explicit name that is not configured,
//! so the console has to be told which names exist.

use rocket::serde::json::Json;
use rocket_okapi::openapi;

use crate::db_blocking;
use e621_account_parser_api::{
    auth::OwnerToken,
    db::{account_ownership_counts, owned_account_ids, owner_accounts_overview, queue_stats},
    errors::ApiError,
    jobs::{self, ProcessJobPhase},
    media_fetch_worker::worker_paused,
    models::{AdminAccount, AdminJob, AdminMedia, AdminOverview, cfg},
    ratelimit,
};

/// Accounts per page. A library can hold hundreds of accounts; the console
/// paginates because rendering 900 rows is a DOM problem, not a data one.
const ADMIN_PAGE_LIMIT: i64 = 100;

/// Owner-scoped console snapshot: accounts, totals, buckets, media, jobs.
#[openapi(tag = "Admin")]
#[get("/admin/overview?<limit>&<offset>")]
pub(crate) async fn admin_overview(
    owner: OwnerToken,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Json<AdminOverview>, ApiError> {
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let limit = limit.unwrap_or(ADMIN_PAGE_LIMIT).clamp(1, 500);
    let offset = offset.unwrap_or(0).max(0);
    let seed_token = cfg().backtest.seed_owner_token.clone();

    let (rows, mut totals) = db_blocking({
        let owner_token = owner_token.clone();
        move || owner_accounts_overview(&owner_token, limit, offset)
    })
    .await
    .map_err(ApiError::from_string)?;

    // `adoptable_accounts` is the same helper `/session/state` uses, so the
    // console and the settings card can never disagree about what a claim
    // would link.
    let (_, adoptable) = db_blocking({
        let owner_token = owner_token.clone();
        move || account_ownership_counts(Some(&owner_token), &seed_token)
    })
    .await
    .map_err(ApiError::from_string)?;

    // Running jobs are counted over *every* owned account, not just the page:
    // the page is a rendering limit, and a metric that changes when you scroll
    // is not a metric.
    let owned_ids = db_blocking({
        let owner_token = owner_token.clone();
        move || owned_account_ids(&owner_token)
    })
    .await
    .map_err(ApiError::from_string)?;
    totals.jobs_running = i64::try_from(
        owned_ids
            .iter()
            .filter(|id| jobs::get_state(**id).is_some_and(|s| s.phase == ProcessJobPhase::Running))
            .count(),
    )
    .unwrap_or(i64::MAX);

    let config = cfg();
    let mut buckets: Vec<String> = config.buckets.keys().cloned().collect();
    buckets.sort();

    let accounts: Vec<AdminAccount> = rows
        .into_iter()
        .map(|row| {
            let job = jobs::get_state(row.id).map(|s| AdminJob {
                phase: match s.phase {
                    ProcessJobPhase::Running => "running",
                    ProcessJobPhase::Done => "done",
                    ProcessJobPhase::Failed => "failed",
                }
                .to_string(),
                pages_total: s.pages_total,
                pages_done: s.pages_done,
                elapsed_secs: s.elapsed_secs,
                error: s.error,
            });
            // The effective bucket needs `Config`, which is why it is resolved
            // here and not in SQL. Resolved with the account's own explicit pin
            // so a stale pin shows up as itself rather than as "unassigned".
            let (bucket, _) = config.pick_bucket(row.id, row.bucket_explicit.as_deref());
            AdminAccount {
                id: row.id,
                name: row.name,
                bucket_explicit: row.bucket_explicit,
                bucket,
                has_key: row.has_key,
                saved_posts: row.saved_posts,
                job,
            }
        })
        .collect();

    let (pending, stored, bytes) = db_blocking(queue_stats)
        .await
        .map_err(ApiError::from_string)?;

    Ok(Json(AdminOverview {
        instance_locked: config.instance.ownership_requires_secret(),
        adoptable_accounts: adoptable,
        totals,
        buckets,
        media: AdminMedia {
            enabled: config.catalog.media_cache_enabled(),
            paused: worker_paused(),
            pending,
            stored,
            bytes,
        },
        accounts,
    }))
}
