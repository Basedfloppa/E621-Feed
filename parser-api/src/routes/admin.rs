//! Operator console — `GET /api/admin/overview` (TODO §3.2).
//!
//! This is the view of *the machine*: the media cache, the job registry, the
//! bucket pins. It is gated on the administration tier
//! (`auth::require_instance_admin`) — the browser must have linked
//! `[runtime].admin_user` with the matching `[runtime].admin_api` — and not on
//! the instance claim. Owning the instance widens what a browser may *read*;
//! operating it is a separate, narrower thing, and conflating the two would put
//! the console behind `[instance].owner_secret`, a secret whose whole job is to
//! *recover* access after clearing cookies. The account rows come from the
//! console's own scope — every account on the instance — which is deliberately
//! wider than the account picker's: an operator sees the whole instance without
//! gaining the ability to act as accounts it holds no link to. The one genuinely
//! new capability — the A/B bucket assignment —
//! lives next to the read-only bucket route in `routes::account`, and this
//! endpoint is what makes `[buckets.*]` visible at all: `pick_bucket` silently
//! ignores an explicit name that is not configured, so the console has to be
//! told which names exist.

use rocket::serde::json::Json;
use rocket_okapi::openapi;

use crate::db_blocking;
use e621_account_parser_api::{
    auth::{OwnerToken, require_instance_admin},
    db::{console_account_ids, console_accounts_overview, instance_account_count, queue_stats},
    errors::ApiError,
    jobs::{self, ProcessJobPhase},
    media_fetch_worker::worker_paused,
    models::{AdminAccount, AdminJob, AdminMedia, AdminOverview, cfg},
    ratelimit,
};

/// Accounts per page. A library can hold hundreds of accounts; the console
/// paginates because rendering 900 rows is a DOM problem, not a data one.
const ADMIN_PAGE_LIMIT: i64 = 100;

/// Operator-scoped console snapshot: accounts, totals, buckets, media, jobs.
#[openapi(tag = "Admin")]
#[get("/admin/overview?<limit>&<offset>")]
pub(crate) async fn admin_overview(
    owner: OwnerToken,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Json<AdminOverview>, ApiError> {
    require_instance_admin(&owner).await?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let limit = limit.unwrap_or(ADMIN_PAGE_LIMIT).clamp(1, 500);
    let offset = offset.unwrap_or(0).max(0);

    let (rows, mut totals) = db_blocking({
        let owner_token = owner_token.clone();
        move || console_accounts_overview(&owner_token, limit, offset)
    })
    .await
    .map_err(ApiError::from_string)?;

    // Now the same number as `totals.accounts`, because the console scope is the
    // instance for any operator. Kept separate because it answers a different
    // question — how many accounts exist here, not how many are in scope — and
    // the two agree by construction rather than by coincidence.
    let instance_accounts = db_blocking(instance_account_count)
        .await
        .map_err(ApiError::from_string)?;

    // Running jobs are counted over *every* account the console covers, not just
    // the page:
    // the page is a rendering limit, and a metric that changes when you scroll
    // is not a metric.
    let visible_ids = db_blocking({
        let owner_token = owner_token.clone();
        move || console_account_ids(&owner_token)
    })
    .await
    .map_err(ApiError::from_string)?;
    totals.jobs_running = i64::try_from(
        visible_ids
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
                linked: row.linked,
                addressable: row.addressable,
                job,
            }
        })
        .collect();

    let (pending, stored, bytes) = db_blocking(queue_stats)
        .await
        .map_err(ApiError::from_string)?;

    Ok(Json(AdminOverview {
        instance_locked: config.instance.ownership_requires_secret(),
        instance_accounts,
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
