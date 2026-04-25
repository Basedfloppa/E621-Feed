use reqwest::{Client, Response, StatusCode};
use rocket::serde::json;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::time::sleep;
use urlencoding::encode;

use crate::{
    cfg,
    models::{Post, PostsApiResponse, TruncatedAccount, UserApiResponse},
};

fn build_url(path: &str, params: &[(&str, String)]) -> String {
    let cfg = cfg();
    let url = if params.is_empty() {
        format!("{}/{path}", cfg.posts_domain)
    } else {
        let qs = params
            .iter()
            .map(|(k, v)| format!("{k}={}", encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}/{path}?{qs}", cfg.posts_domain)
    };
    trace!("build_url: path={path} -> {url}");
    url
}

static HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    info!("Building shared HTTP client");
    Client::builder()
        .user_agent(format!("account scraper (by {0})", cfg().admin_user))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .build()
        .map_err(|e| {
            error!("Failed to build client: {e}");
            format!("Failed to build client: {e}")
        })
        .unwrap()
});

fn get_client() -> &'static Client {
    &HTTP_CLIENT
}

async fn send_with_retry(builder: reqwest::RequestBuilder) -> Result<Response, String> {
    let mut delay: Duration = Duration::from_millis(300);
    let cfg = cfg();

    for attempt in 0..=cfg.max_retries {
        if let Some(b) = builder.try_clone() {
            match b.build() {
                Ok(req) => debug!(
                    "HTTP attempt {}/{}: {} {} (rps_delay={}ms)",
                    attempt + 1,
                    cfg.max_retries + 1,
                    req.method(),
                    req.url(),
                    cfg.rps_delay_ms
                ),
                Err(e) => warn!("Could not build request for logging: {e}"),
            }
        } else {
            warn!(
                "Unable to clone request for logging on attempt {}",
                attempt + 1
            );
        }

        sleep(Duration::from_millis(cfg.rps_delay_ms)).await;

        return match builder
            .try_clone()
            .ok_or_else(|| {
                let m = "unable to clone request".to_string();
                error!("{m}");
                m
            })?
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                trace!("HTTP status received: {status}");

                if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                    && attempt < cfg.max_retries
                {
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("n/a");
                    warn!(
                        "Request got {} (retry-after: {}). Backing off for {:?} (attempt {}/{})",
                        status,
                        retry_after,
                        delay,
                        attempt + 1,
                        cfg.max_retries + 1
                    );
                    sleep(delay).await;
                    delay = delay.saturating_mul(2);
                    continue;
                }

                if status.is_success() {
                    info!("Request succeeded with {status}");
                } else {
                    warn!("Request completed with non-retryable status {status}");
                }
                Ok(resp)
            }
            Err(e) => {
                if attempt < cfg.max_retries {
                    warn!(
                        "Request error on attempt {}/{}: {:?}. Retrying in {:?}",
                        attempt + 1,
                        cfg.max_retries + 1,
                        e,
                        delay
                    );
                    sleep(delay).await;
                    delay = delay.saturating_mul(2);
                    continue;
                }
                error!("Request failed after {} attempts: {}", cfg.max_retries + 1, e);
                Err(format!("request failed after retries: {e}"))
            }
        };
    }

    error!("send_with_retry exhausted attempts but reached unreachable branch");
    Err("unreachable".into())
}

pub async fn get_favorites(account: &TruncatedAccount, page: i32) -> Vec<Post> {
    info!("Fetching favorites: user_id={} page={}", account.id, page);

    let cfg = cfg();
    let client = get_client();
    let url = build_url(
        "favorites.json",
        &[
            ("user_id", account.id.to_string()),
            ("limit", cfg.posts_limit.to_string()),
            ("page", page.to_string()),
        ],
    );
    debug!("GET (auth) /favorites.json?user_id=…&limit=…&page={page}");

    let resp = match send_with_retry(
        client
            .get(url)
            .basic_auth(cfg.admin_user.clone(), Some(cfg.admin_api.clone())),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("favorites request failed: {e}");
            return Vec::new();
        }
    };

    let status = resp.status();
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            warn!("reading favorites body failed: {e}");
            return Vec::new();
        }
    };

    if !status.is_success() {
        let preview = body.chars().take(200).collect::<String>();
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                warn!("favorites auth failed ({status}). Body: {preview}");
            }
            StatusCode::TOO_MANY_REQUESTS => {
                warn!("favorites rate limited (429). Body: {preview}");
            }
            _ => warn!("favorites non-success {status}. Body: {preview}"),
        }
        return Vec::new();
    }

    let posts = match json::from_str::<PostsApiResponse>(&body) {
        Ok(r) => r.posts,
        Err(e) => {
            let preview = body.chars().take(200).collect::<String>();
            warn!("favorites parse failed: {e}; first bytes: {preview}");
            return Vec::new();
        }
    };

    info!("Fetched {} favorite posts", posts.len());
    posts
}

pub async fn get_account(account: &TruncatedAccount) -> Result<UserApiResponse, String> {
    info!(
        "Fetching account: id={} name='{}'",
        account.id, account.name
    );
    let cfg = cfg();
    let client = get_client();
    let url = format!("{}/users/{}.json", cfg.posts_domain, account.id);
    debug!("GET (auth) {url}");
    let resp = send_with_retry(
        client
            .get(url)
            .basic_auth(cfg.admin_user.clone(), Some(cfg.admin_api.clone())),
    )
    .await
    .map_err(|e| format!("account request failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("account body read failed: {e}"))?;
    if !status.is_success() {
        let preview = body.chars().take(200).collect::<String>();
        return Err(format!("account request returned {status}: {preview}"));
    }
    let parsed = json::from_str::<UserApiResponse>(&body)
        .map_err(|e| format!("account parse failed: {e}"))?;
    info!("Fetched account successfully for id={}", account.id);
    Ok(parsed)
}

pub async fn get_posts(account: &TruncatedAccount, page: Option<i32>) -> Result<Vec<Post>, String> {
    let blacklisted_tags = account.blacklist.clone();
    let blacklist = if blacklisted_tags.trim().is_empty() {
        String::new()
    } else {
        format!("-{}", blacklisted_tags.replace('\n', " -"))
    };
    debug!(
        "Preparing posts fetch: page={} blacklist_len={}",
        page.unwrap_or(0),
        blacklist.split_whitespace().count()
    );
    let cfg = cfg();
    let client = get_client();
    let url = build_url(
        "posts.json",
        &[
            ("limit", cfg.posts_limit.to_string()),
            ("page", page.unwrap_or(0).to_string()),
            ("tags", blacklist),
        ],
    );
    debug!("GET (auth) {url}");
    let resp = send_with_retry(
        client
            .get(url)
            .basic_auth(cfg.admin_user.clone(), Some(cfg.admin_api.clone())),
    )
    .await
    .map_err(|e| format!("posts request failed: {e}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("posts body read failed: {e}"))?;
    if !status.is_success() {
        let preview = body.chars().take(200).collect::<String>();
        return Err(format!("posts request returned {status}: {preview}"));
    }
    let posts = json::from_str::<PostsApiResponse>(&body)
        .map_err(|e| format!("posts parse failed: {e}"))?
        .posts;

    info!("Fetched {} posts", posts.len());
    Ok(posts)
}
