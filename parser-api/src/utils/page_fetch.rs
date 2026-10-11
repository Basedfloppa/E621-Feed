//! Contiguous page fetching for the seed binary's favourites import.
//!
//! Pages go out **one at a time**, and that is a decision rather than an
//! oversight: overlapping them was measured against live e621 and bought
//! nothing. The gate (`load_monitor`) serialises request *starts*, roughly
//! 0.8 s apart, and under a window of four e621 answered each request slower
//! (~1.0-1.4 s against ~0.83 s sequential) — which cancels the overlap, while
//! a short listing can start up to `window - 1` extra pages past its end.

use std::future::Future;

/// Fetch pages `1..=pages` sequentially.
///
/// Returns the **contiguous non-empty prefix** of the page results, in page
/// order, and stops at the first empty page (a short favourites listing means
/// the caller's page count was too optimistic — or the listing went private
/// mid-import).
///
/// The first error aborts the pass: partial page sets would be silently
/// mistaken for a short listing, which is exactly the ambiguity the
/// stop-on-empty rule already has to live with.
pub async fn fetch_pages<T, E, F, Fut>(pages: usize, mut fetch: F) -> Result<Vec<Vec<T>>, E>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<Vec<T>, E>>,
{
    let mut batches = Vec::new();
    for page in 1..=pages {
        let batch = fetch(page).await?;
        if batch.is_empty() {
            break;
        }
        batches.push(batch);
    }
    Ok(batches)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::fetch_pages;

    #[tokio::test]
    async fn returns_the_pages_in_order() {
        let batches = fetch_pages(4, |page| async move { Ok::<_, String>(vec![page]) })
            .await
            .expect("all pages succeed");

        assert_eq!(batches, vec![vec![1], vec![2], vec![3], vec![4]]);
    }

    #[tokio::test]
    async fn keeps_at_most_one_request_in_flight() {
        // Guards the "no concurrent window" decision: re-adding one must break
        // this test rather than quietly reintroduce the slower path.
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let (inflight, high) = (Arc::clone(&in_flight), Arc::clone(&peak));
        let batches = fetch_pages(3, move |page| {
            let in_flight = Arc::clone(&inflight);
            let peak = Arc::clone(&high);
            async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, String>(vec![page])
            }
        })
        .await
        .expect("all pages succeed");

        assert_eq!(batches, vec![vec![1], vec![2], vec![3]]);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stops_at_the_first_empty_page() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&started);

        let batches = fetch_pages::<usize, String, _, _>(8, move |page| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock().expect("started lock").push(page);
                if page == 3 {
                    Ok(Vec::new())
                } else {
                    Ok(vec![page])
                }
            }
        })
        .await
        .expect("empty page is not an error");

        assert_eq!(batches, vec![vec![1], vec![2]]);
        assert_eq!(
            *started.lock().expect("started lock"),
            vec![1, 2, 3],
            "the empty page itself is requested and nothing past it is"
        );
    }

    #[tokio::test]
    async fn an_error_aborts_the_pass() {
        let error = fetch_pages::<usize, String, _, _>(6, |page| async move {
            if page == 2 {
                Err("page 2 exploded".to_string())
            } else {
                Ok(vec![page])
            }
        })
        .await
        .expect_err("the failing page must surface");

        assert_eq!(error, "page 2 exploded");
    }

    #[tokio::test]
    async fn no_pages_means_no_requests() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);

        let batches = fetch_pages(0, move |page| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok::<_, String>(vec![page])
            }
        })
        .await
        .expect("nothing to fetch");

        assert!(batches.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
