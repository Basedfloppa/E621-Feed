//! Background repair for catalog posts imported before e621's v2 media shape.
//!
//! Unlike an account `/process`, this scans the shared catalog, including
//! orphaned recommendation candidates, and fills missing preview/sample/original
//! URLs in small rate-gated batches.

use std::{collections::HashSet, time::Duration};

use crate::{api, db};

const BATCH_SIZE: usize = 50;
const CADENCE: Duration = Duration::from_mins(15);

pub fn spawn_media_hydrator() {
    rocket::tokio::spawn(async {
        // Let migrations and normal startup work settle before using e621.
        rocket::tokio::time::sleep(Duration::from_secs(15)).await;
        loop {
            hydrate_catalog_once().await;
            rocket::tokio::time::sleep(CADENCE).await;
        }
    });
}

/// Run one catalog repair pass. Exposed for deterministic local-HTTP
/// integration tests as well as the scheduled worker.
pub async fn hydrate_catalog_once() {
    match rocket::tokio::task::spawn_blocking(|| db::purge_deleted_catalog_posts(500)).await {
        Ok(Ok(removed)) if removed > 0 => {
            info!("[media-hydrator] purged {removed} locally deleted posts");
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => warn!("[media-hydrator] deleted-post purge failed: {error}"),
        Err(error) => warn!("[media-hydrator] deleted-post purge task panicked: {error}"),
    }
    let ids = match rocket::tokio::task::spawn_blocking(|| {
        db::collect_post_ids_needing_hydration(BATCH_SIZE)
    })
    .await
    {
        Ok(Ok(ids)) => ids,
        Ok(Err(error)) => {
            warn!("[media-hydrator] scan failed: {error}");
            Vec::new()
        }
        Err(error) => {
            warn!("[media-hydrator] scan task panicked: {error}");
            Vec::new()
        }
    };
    if !ids.is_empty() {
        let requested = ids.len();
        match api::get_posts_by_ids(&ids).await {
            Ok(posts) => {
                let count = posts.len();
                let requested_set: HashSet<i64> = ids.iter().copied().collect();
                let returned: HashSet<i64> = posts.iter().map(|post| post.id).collect();
                // Protect the shared catalog: (1) only upsert posts we actually
                // requested — an unfiltered/expanded response must not inject
                // unrelated rows into the corpus; (2) only treat an id as
                // "missing/deleted" when the response is provably complete
                // (returned as many ids as requested). A truncated/partial
                // response (e.g. `posts_limit` < batch size, transient index
                // lag, or an e621 filter regression) must never cause a c
                // ascading `DELETE FROM posts` on live, still-present rows.
                let to_upsert: Vec<_> = posts
                    .into_iter()
                    .filter(|p| requested_set.contains(&p.id))
                    .collect();
                let missing: Vec<i64> = if count == requested {
                    ids.iter()
                        .copied()
                        .filter(|id| !returned.contains(id))
                        .collect()
                } else {
                    Vec::new()
                };
                match rocket::tokio::task::spawn_blocking(move || {
                    db::upsert_catalog_posts(&to_upsert)?;
                    db::replace_posts_tags_batch(&to_upsert)?;
                    let purged = db::delete_catalog_posts_by_ids(&missing)?;
                    Ok::<_, String>(((), purged))
                })
                .await
                {
                    Ok(Ok(((), purged))) => {
                        info!(
                            "[media-hydrator] scanned {requested} incomplete posts; e621 repaired {count}, purged {purged} absent posts"
                        );
                    }
                    Ok(Err(error)) => warn!("[media-hydrator] save failed: {error}"),
                    Err(error) => warn!("[media-hydrator] save task panicked: {error}"),
                }
            }
            Err(error) => warn!("[media-hydrator] e621 fetch failed: {error}"),
        }
    }
}
