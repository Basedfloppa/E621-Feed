use reqwasm::http::Request;
use yew::{
    Callback, Html, MouseEvent, Properties, UseStateHandle, function_component, html,
    use_effect_with, use_state,
};

use crate::models::get_or_create_owner_token;
use crate::pages::{TagCount, UserInfo};

#[derive(Properties, PartialEq)]
pub struct AnalyzeButtonProps {
    pub found_user: UseStateHandle<Option<UserInfo>>,
    pub error: UseStateHandle<Option<String>>,
    pub api_base: String,
    pub tag_count: UseStateHandle<Vec<TagCount>>,
    pub is_loading: UseStateHandle<bool>,
}

#[function_component(FetchAnalyzeButton)]
pub fn fetch_analyze_button(props: &AnalyzeButtonProps) -> Html {
    let is_analyzing = use_state(|| false);
    let is_fetching = use_state(|| false);

    let fetch_tags = {
        let api_base = props.api_base.clone();
        let found_user = props.found_user.clone();
        let is_fetching = is_fetching.clone();
        let is_loading = props.is_loading.clone();
        let tag_count = props.tag_count.clone();
        let error = props.error.clone();

        Callback::from(move |_| {
            let api_base = api_base.clone();
            let owner_token = get_or_create_owner_token();

            if found_user.is_none() {
                error.set(Some("No user selected".to_string()));
                return;
            }

            let Some(owner_token) = owner_token else {
                error.set(Some("Missing device token".to_string()));
                return;
            };

            let user_id = found_user.as_ref().unwrap().id;
            let tag_count = tag_count.clone();
            let is_fetching = is_fetching.clone();
            let is_loading = is_loading.clone();
            let error = error.clone();

            is_fetching.set(true);
            is_loading.set(true);
            error.set(None);

            wasm_bindgen_futures::spawn_local(async move {
                match Request::get(&format!(
                    "{}/account/{}/tag_counts?owner_token={}",
                    &api_base,
                    user_id,
                    urlencoding::encode(&owner_token)
                ))
                    .send()
                    .await
                {
                    Ok(response) => {
                        if response.ok() {
                            match response.json::<Vec<TagCount>>().await {
                                Ok(counts) => {
                                    tag_count.set(counts);
                                    error.set(None);
                                }
                                Err(e) => {
                                    error.set(Some(format!("Failed to parse tag data: {e}")));
                                }
                            }
                        } else {
                            let status = response.status();
                            if status != 404 {
                                let text = response
                                    .text()
                                    .await
                                    .unwrap_or_else(|_| "Unknown error".into());
                                error.set(Some(format!("Error {status}: {text}")));
                            }
                        }
                    }
                    Err(e) => {
                        error.set(Some(format!("Network error: {e}")));
                    }
                }

                is_fetching.set(false);
                is_loading.set(false);
            });
        })
    };

    let analyze_tags = {
        let fetch_tags = fetch_tags.clone();
        let api_base = props.api_base.clone();
        let found_user = props.found_user.clone();
        let is_analyzing = is_analyzing.clone();
        let is_loading = props.is_loading.clone();
        let error = props.error.clone();

        Callback::from(move |_| {
            let fetch_tags = fetch_tags.clone();
            let api_base = api_base.clone();
            let owner_token = get_or_create_owner_token();

            if found_user.is_none() {
                error.set(Some("No user selected".to_string()));
                return;
            }

            let Some(owner_token) = owner_token else {
                error.set(Some("Missing device token".to_string()));
                return;
            };

            let user_id = found_user.as_ref().unwrap().id;
            let is_analyzing = is_analyzing.clone();
            let is_loading = is_loading.clone();
            let error = error.clone();

            is_analyzing.set(true);
            is_loading.set(true);
            error.set(None);

            wasm_bindgen_futures::spawn_local(async move {
                match Request::post(&format!(
                    "{}/process/{}?owner_token={}",
                    &api_base,
                    user_id,
                    urlencoding::encode(&owner_token)
                ))
                    .send()
                    .await
                {
                    Ok(response) => {
                        if !response.ok() {
                            let status = response.status();
                            let text = response
                                .text()
                                .await
                                .unwrap_or_else(|_| "Unknown error".into());
                            error.set(Some(format!("Processing error {status}: {text}")));
                        }
                    }
                    Err(e) => {
                        error.set(Some(format!("Processing error: {e}")));
                    }
                }
                is_analyzing.set(false);
                is_loading.set(false);

                if let Ok(synthetic_event) = MouseEvent::new("click") {
                    fetch_tags.emit(synthetic_event);
                } else {
                    error.set(Some("Failed to trigger fetch after analysis".to_string()));
                }
            });
        })
    };

    {
        let fetch_tags = fetch_tags.clone();
        use_effect_with((*props.found_user).clone(), move |user| {
            if user.is_some() {
                if let Ok(e) = MouseEvent::new("click") {
                    fetch_tags.emit(e);
                }
            }
        });
    }

    let busy = *is_analyzing || *is_fetching;
    let label = if *is_analyzing {
        "Analyzing..."
    } else if *is_fetching {
        "Loading..."
    } else if props.tag_count.is_empty() {
        "Analyze Tags"
    } else {
        "Re-analyze Tags"
    };

    html! {
        <div class="d-grid gap-2 mb-4">
            <button
                class="btn btn-warning"
                onclick={analyze_tags}
                disabled={*props.is_loading || props.found_user.is_none()}
                aria-busy={busy.to_string()}
            >
                { if busy {
                    html! {
                        <span>
                            <span class="spinner-border spinner-border-sm me-2" role="status" aria-hidden="true"></span>
                            { label }
                        </span>
                    }
                } else {
                    html! { { label } }
                }}
            </button>
            <small class="text-muted">
                { "Scans your favourites on e621 and builds a tag profile used for recommendations. This may take a minute for large accounts." }
            </small>
        </div>
    }
}
