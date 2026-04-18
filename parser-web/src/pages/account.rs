use crate::models::{get_or_create_owner_token, read_config_from_head};
use crate::pages::UserInfo;
use reqwasm::http::Request;
use web_sys::HtmlInputElement;
use yew::prelude::*;

#[function_component(Account)]
pub fn account_creator() -> Html {
    let id = use_state(String::new);
    let name = use_state(String::new);
    let blacklist = use_state(String::new);
    let message = use_state(String::new);
    let error = use_state(|| false);
    let loading = use_state(|| false);

    let saved_accounts = use_state(Vec::<UserInfo>::new);

    let on_id_change = {
        let id = id.clone();
        Callback::from(move |e: Event| {
            let input: HtmlInputElement = e.target_unchecked_into();
            id.set(input.value());
        })
    };

    let on_name_change = {
        let name = name.clone();
        Callback::from(move |e: Event| {
            let input: HtmlInputElement = e.target_unchecked_into();
            name.set(input.value());
        })
    };

    let on_blacklist_change = {
        let blacklist = blacklist.clone();
        Callback::from(move |e: Event| {
            let input: HtmlInputElement = e.target_unchecked_into();
            blacklist.set(input.value());
        })
    };

    let onsubmit = {
        let id = id.clone();
        let name = name.clone();
        let blacklist = blacklist.clone();
        let message = message.clone();
        let error = error.clone();
        let loading = loading.clone();
        let saved_accounts = saved_accounts.clone();

        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            loading.set(true);

            let cfg = read_config_from_head().unwrap();
            let Some(owner_token) = get_or_create_owner_token() else {
                message.set("Failed to create local device token".to_string());
                error.set(true);
                loading.set(false);
                return;
            };
            let raw_id = id.trim().to_string();
            let raw_name = name.trim().to_string();
            let raw_blacklist = blacklist.trim().to_string();

            if raw_id.is_empty() || raw_name.is_empty() {
                message.set("All fields are required".to_string());
                error.set(true);
                loading.set(false);
                return;
            }

            let account_id = match raw_id.parse::<i64>() {
                Ok(id) => id,
                Err(_) => {
                    message.set("Invalid account ID. Must be a number".to_string());
                    error.set(true);
                    loading.set(false);
                    return;
                }
            };

            let exists = (*saved_accounts)
                .iter()
                .any(|u| u.id == account_id || u.name.eq_ignore_ascii_case(&raw_name));

            if exists {
                message.set("An account with this ID or Username already exists.".to_string());
                error.set(true);
                loading.set(false);
                return;
            }

            let account = UserInfo {
                id: account_id,
                name: raw_name.clone(),
                blacklist: raw_blacklist.clone(),
            };

            let payload = serde_json::json!({
                "owner_token": owner_token,
                "id": account_id,
                "name": raw_name,
                "blacklist": raw_blacklist,
            });

            let message = message.clone();
            let error = error.clone();
            let loading = loading.clone();
            let saved_accounts = saved_accounts.clone();

            wasm_bindgen_futures::spawn_local(async move {
                let response = Request::post(&format!("{0}/account", cfg.backend_domain))
                    .header("Content-Type", "application/json")
                    .body(payload.to_string())
                    .send()
                    .await;

                loading.set(false);

                match response {
                    Ok(resp) => {
                        if resp.status() >= 200 && resp.status() < 300 {
                            let saved = resp.json::<UserInfo>().await.unwrap_or(account);
                            let mut accounts = (*saved_accounts).clone();
                            if let Some(existing) = accounts.iter_mut().find(|acc| acc.id == saved.id) {
                                *existing = saved.clone();
                            } else {
                                accounts.push(saved);
                            }
                            saved_accounts.set(accounts);
                            message.set("Account created successfully!".to_string());
                            error.set(false);
                        } else {
                            let error_msg = resp
                                .text()
                                .await
                                .unwrap_or_else(|_| "Unknown error".to_string());
                            message.set(format!(
                                "Error: {} (Status: {})",
                                error_msg,
                                resp.status()
                            ));
                            error.set(true);
                        }
                    }
                    Err(e) => {
                        message.set(format!("Network error: {e}"));
                        error.set(true);
                    }
                }

                loading.set(false);
            });
        })
    };

    let message_class = if message.is_empty() {
        "d-none"
    } else if *error {
        "alert alert-danger mt-3"
    } else {
        "alert alert-success mt-3"
    };

    html! {
        <div class="container mt-5" id="account-page">
            <div class="row justify-content-center">
                <div class="col-md-6">
                    <div class="card shadow">
                        <div class="card-body">
                            <h1 class="card-title text-center mb-4">{"Create New Account"}</h1>
                            <form onsubmit={onsubmit}>
                                <div class="mb-3">
                                    <label for="account-id" class="form-label">{"Account ID"}</label>
                                    <input
                                        type="number"
                                        class="form-control"
                                        id="account-id"
                                        value={(*id).clone()}
                                        onchange={on_id_change}
                                        placeholder="Enter numeric account ID"
                                        disabled={*loading}
                                    />
                                </div>

                                <div class="mb-3">
                                    <label for="account-name" class="form-label">{"Username"}</label>
                                    <input
                                        type="text"
                                        class="form-control"
                                        id="account-name"
                                        value={(*name).clone()}
                                        onchange={on_name_change}
                                        placeholder="Enter your username"
                                        disabled={*loading}
                                    />
                                </div>

                                 <div class="mb-3">
                                    <label for="account-blacklist" class="form-label">{"Blacklist"}</label>
                                    <textarea
                                        type="text-area"
                                        class="form-control"
                                        id="account-blacklist"
                                        value={(*blacklist).clone()}
                                        onchange={on_blacklist_change}
                                        placeholder="Enter your blacklisted tags, each one on the separate line"
                                        disabled={*loading}
                                    />
                                </div>

                                <button
                                    type="submit"
                                    class="btn btn-primary w-100"
                                    disabled={*loading}
                                >
                                    { if *loading {
                                        html! {
                                            <span>
                                                <span class="spinner-border spinner-border-sm me-2" role="status" aria-hidden="true"></span>
                                                {"Creating..."}
                                            </span>
                                        }
                                    } else {
                                        "Create Account".into()
                                    }}
                                </button>

                                <div class={message_class} role="alert">
                                    {&*message}
                                </div>
                            </form>
                        </div>
                    </div>
                </div>
            </div>
        </div>
    }
}
