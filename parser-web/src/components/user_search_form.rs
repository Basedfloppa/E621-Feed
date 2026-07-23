use std::cell::Cell;
use web_sys::{HtmlInputElement, InputEvent}; // <-- from web_sys
use yew::{
    Callback, Html, Properties, TargetCast, UseStateHandle, function_component, html, use_mut_ref,
    use_state,
};

use crate::models::{api_get, humanize_error_body};
use crate::pages::UserInfo;

#[derive(Properties, PartialEq)]
pub struct UserSearchProps {
    pub found_user: UseStateHandle<Option<UserInfo>>,
    pub is_loading: UseStateHandle<bool>,
    pub api_base: String,
    pub error: UseStateHandle<Option<String>>,
}

#[function_component(UserSearchForm)]
pub fn user_search_form(props: &UserSearchProps) -> Html {
    let user_query = use_state(String::new);
    let inflight = use_mut_ref(|| Cell::new(false));

    let on_input = {
        let user_query = user_query.clone();
        Callback::from(move |e: InputEvent| {
            let input: HtmlInputElement = e.target_unchecked_into();
            user_query.set(input.value());
        })
    };

    let fetch_user = {
        let api_base = props.api_base.clone();
        let user_query = user_query.clone();
        let found_user = props.found_user.clone();
        let is_loading = props.is_loading.clone();
        let error = props.error.clone();
        let inflight = inflight.clone();

        Callback::from(move |_| {
            if inflight.borrow().get() {
                return;
            }
            let mut query = user_query.to_string();
            query = query.trim().to_string();
            if query.is_empty() {
                error.set(Some("Please enter a username or ID".into()));
                return;
            }

            inflight.borrow().set(true);
            is_loading.set(true);
            error.set(None);

            let is_id = query.parse::<i64>().is_ok();
            let encoded = if is_id {
                query.clone()
            } else {
                urlencoding::encode(&query).to_string()
            };

            let url = if is_id {
                format!("{api_base}/user/id/{encoded}")
            } else {
                format!("{api_base}/user/name/{encoded}")
            };

            let found_user = found_user.clone();
            let is_loading = is_loading.clone();
            let error = error.clone();
            let inflight_done = inflight.clone();

            wasm_bindgen_futures::spawn_local(async move {
                match api_get(&url).send().await {
                    Ok(response) => {
                        let status = response.status();
                        let text = response.text().await.unwrap_or_default();
                        if (200..300).contains(&status) {
                            match serde_json::from_str::<UserInfo>(&text) {
                                Ok(user) => {
                                    found_user.set(Some(user));
                                    error.set(None);
                                }
                                Err(e) => {
                                    error.set(Some(format!("Failed to parse user data: {e}")));
                                }
                            }
                        } else {
                            error.set(Some(humanize_error_body(status, &text)));
                        }
                    }
                    Err(e) => error.set(Some(format!("Network error: {e}"))),
                }
                is_loading.set(false);
                inflight_done.borrow().set(false);
            });
        })
    };

    html! {
        <div class="mb-3">
            <label for="user-search-input">
                <span class="text-base-content">{"Search by Username or ID"}</span>
            </label>
            <div class="join w-full">
                <input
                    id="user-search-input"
                    type="text"
                    class="input input-bordered join-item flex-1"
                    value={(*user_query).clone()}
                    oninput={on_input}
                    placeholder="Enter username or ID"
                    disabled={*props.is_loading}
                />
                <button
                    class="btn btn-primary join-item"
                    type="button"
                    onclick={fetch_user}
                    disabled={*props.is_loading}
                >
                    {"Search"}
                </button>
            </div>
        </div>
    }
}
