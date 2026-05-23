use serde::de::DeserializeOwned;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::RequestInit;
use web_sys::{Request, RequestMode, Response, window};
use yew::prelude::*;

use crate::components::*;
use crate::models::*;
use crate::pages::UserInfo;

#[function_component(DigestPage)]
pub fn digest_page() -> Html {
    let posts = use_state(Vec::<ScoredPost>::new);
    let is_loading = use_state(|| false);
    let error = use_state(|| Option::<String>::None);
    let selected_user = use_state(|| Option::<UserInfo>::None);
    let is_full = use_state(|| false);

    let fetch_digest = {
        let posts = posts.clone();
        let is_loading = is_loading.clone();
        let error = error.clone();
        let selected_user = selected_user.clone();
        let is_full = is_full.clone();

        Callback::from(move |_| {
            let Some(user) = (*selected_user).clone() else {
                error.set(Some("Select an account to load the digest.".to_string()));
                return;
            };
            let Some(cfg) = read_config_from_head() else {
                error.set(Some("App configuration failed to load.".to_string()));
                return;
            };

            let full_param = if *is_full { "?full=true" } else { "" };
            let url = format!(
                "{}/digest/{}{}",
                cfg.backend_domain, user.id, full_param
            );

            is_loading.set(true);
            error.set(None);

            let posts = posts.clone();
            let is_loading = is_loading.clone();
            let error = error.clone();

            spawn_local(async move {
                match fetch_json::<Vec<ScoredPost>>(&url).await {
                    Ok(result) => {
                        posts.set(result);
                        is_loading.set(false);
                    }
                    Err(e) => {
                        error.set(Some(e));
                        is_loading.set(false);
                    }
                }
            });
        })
    };

    // Auto-fetch when user changes.
    {
        let fetch_digest = fetch_digest.clone();
        let selected_user = selected_user.clone();
        use_effect_with(selected_user.clone(), move |_| {
            fetch_digest.emit(());
            || ()
        });
    }

    let on_refresh = {
        let fetch_digest = fetch_digest.clone();
        Callback::from(move |_: MouseEvent| {
            fetch_digest.emit(());
        })
    };

    let on_toggle_full = {
        let is_full = is_full.clone();
        let fetch_digest = fetch_digest.clone();
        Callback::from(move |_: MouseEvent| {
            let new_val = !*is_full;
            is_full.set(new_val);
            // Re-fetch after toggling mode.
            let fetch = fetch_digest.clone();
            spawn_local(async move {
                fetch.emit(());
            });
        })
    };

    html! {
        <div class="container-fluid mt-3">
            <div class="d-flex align-items-center gap-2 mb-3 flex-wrap">
                <h2 class="mb-0 me-auto">{ "Daily Digest" }</h2>
                <SavedAccountsSelect
                    selected_user={selected_user.clone()}
                    is_loading={is_loading.clone()}
                />
                <button
                    class="btn btn-outline-primary"
                    onclick={on_refresh}
                    disabled={*is_loading}
                >
                    <i class="bi bi-arrow-clockwise" aria-hidden="true"></i>
                    { " Refresh" }
                </button>
                <button
                    class={classes!(
                        "btn",
                        if *is_full { "btn-primary" } else { "btn-outline-secondary" },
                    )}
                    onclick={on_toggle_full}
                    disabled={*is_loading}
                >
                    { if *is_full { "Full digest" } else { "Quick digest" } }
                </button>
            </div>

            if let Some(ref e) = *error {
                <div class="alert alert-danger" role="alert">{ e }</div>
            }

            if *is_loading {
                <div class="d-flex justify-content-center my-5">
                    <div class="spinner-border" role="status">
                        <span class="visually-hidden">{ "Loading..." }</span>
                    </div>
                </div>
            }

            if !*is_loading && posts.is_empty() && error.is_none() {
                <div class="alert alert-info" role="alert">
                    { "No posts yet. Select an account above and wait for the digest to load." }
                </div>
            }

            <div class="row g-2">
                { for posts.iter().enumerate().map(|(i, sp)| {
                    let Some(cfg) = read_config_from_head() else {
                        return html! {};
                    };
                    html! {
                        <div class="col-xs-6 col-sm-5 col-md-4 col-lg-3 col-xl-2 col-xxl-1 d-flex justify-content-center">
                            <PostCard
                                post={std::rc::Rc::new(sp.post.clone())}
                                affinity={sp.score}
                                backend_url={cfg.backend_domain.clone()}
                                account_id={selected_user.as_ref().map(|u| u.id as i32).unwrap_or(0)}
                                session_id={String::new()}
                                position={i as i32}
                                breakdown={sp.breakdown.clone()}
                            />
                        </div>
                    }
                }) }
            </div>
        </div>
    }
}

async fn fetch_json<T: DeserializeOwned>(url: &str) -> Result<T, String> {
    let window = window().ok_or("No window available".to_string())?;

    let opts = RequestInit::new();
    opts.set_method("GET");
    opts.set_mode(RequestMode::Cors);
    opts.set_credentials(web_sys::RequestCredentials::Include);

    let request = Request::new_with_str_and_init(url, &opts)
        .map_err(|e| format!("Failed to create request: {e:?}"))?;

    let resp_value = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("Fetch promise rejected: {e:?}"))?;

    let resp: Response = resp_value
        .dyn_into()
        .map_err(|_| "Failed to cast Response".to_string())?;

    if !resp.ok() {
        let status = resp.status();
        let status_text = resp.status_text();
        return Err(format!("HTTP {status}: {status_text}"));
    }

    let text_promise = resp
        .text()
        .map_err(|e| format!("Failed to get text promise: {e:?}"))?;
    let text = wasm_bindgen_futures::JsFuture::from(text_promise)
        .await
        .map_err(|e| format!("Text future rejected: {e:?}"))?
        .as_string()
        .ok_or("Response text is not a string".to_string())?;

    serde_json::from_str(&text).map_err(|e| format!("JSON parse error: {e}"))
}
