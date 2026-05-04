use crate::components::ConfirmModal;
use crate::models::{
    api_delete, api_get, api_patch, api_post, dispatch_account_list_changed, humanize_error_body,
    read_config_from_head,
};
use crate::pages::UserInfo;
use gloo_timers::callback::Timeout;
use std::collections::HashMap;
use web_sys::{HtmlInputElement, HtmlTextAreaElement};
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
    let editing_id = use_state(|| None::<i64>);
    let edit_draft = use_state(String::new);
    let edit_saving = use_state(|| false);
    let remove_error: UseStateHandle<Option<String>> = use_state(|| None);
    let experiment_buckets: UseStateHandle<HashMap<i64, String>> = use_state(HashMap::new);
    let pending_remove: UseStateHandle<Option<UserInfo>> = use_state(|| None);
    let default_blacklist: UseStateHandle<Vec<String>> = use_state(Vec::new);
    {
        let default_blacklist = default_blacklist.clone();
        use_effect_with((), move |_| {
            if let Some(cfg) = read_config_from_head() {
                wasm_bindgen_futures::spawn_local(async move {
                    let url = format!("{}/defaults/blacklist", cfg.backend_domain);
                    if let Ok(resp) = api_get(&url).send().await {
                        if resp.ok() {
                            #[derive(serde::Deserialize)]
                            struct Resp {
                                blacklist: Vec<String>,
                            }
                            if let Ok(parsed) = resp.json::<Resp>().await {
                                default_blacklist.set(parsed.blacklist);
                            }
                        }
                    }
                });
            }
            || ()
        });
    }

    {
        let message = message.clone();
        let is_error = *error;
        let current_message = (*message).clone();
        use_effect_with(
            (current_message.clone(), is_error),
            move |(msg, err): &(String, bool)| {
                let timeout = if !msg.is_empty() && !*err {
                    Some(Timeout::new(5_000, move || {
                        message.set(String::new());
                    }))
                } else {
                    None
                };
                move || drop(timeout)
            },
        );
    }

    {
        let saved_accounts = saved_accounts.clone();
        use_effect_with((), move |_| {
            if let Some(cfg) = read_config_from_head() {
                wasm_bindgen_futures::spawn_local(async move {
                    let url = format!("{}/accounts", cfg.backend_domain);
                    if let Ok(response) = api_get(&url).send().await {
                        if response.ok() {
                            if let Ok(accounts) = response.json::<Vec<UserInfo>>().await {
                                saved_accounts.set(accounts);
                            }
                        }
                    }
                });
            }
            || ()
        });
    }

    {
        let saved_accounts = saved_accounts.clone();
        let experiment_buckets = experiment_buckets.clone();
        let ids: Vec<i64> = (*saved_accounts).iter().map(|a| a.id).collect();
        use_effect_with(ids, move |ids: &Vec<i64>| {
            let ids = ids.clone();
            let cfg = read_config_from_head();
            if !ids.is_empty() {
                if let Some(cfg) = cfg {
                    wasm_bindgen_futures::spawn_local(async move {
                        let mut found: HashMap<i64, String> = HashMap::new();
                        for id in ids {
                            let url = format!(
                                "{}/account/{}/experiment_bucket",
                                cfg.backend_domain, id
                            );
                            if let Ok(resp) = api_get(&url).send().await {
                                if resp.ok() {
                                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                                        if let Some(name) = v.get("bucket").and_then(|b| b.as_str())
                                        {
                                            found.insert(id, name.to_string());
                                        }
                                    }
                                }
                            }
                        }
                        experiment_buckets.set(found);
                    });
                }
            }
            || ()
        });
    }

    let on_id_change = {
        let id = id.clone();
        Callback::from(move |e: Event| {
            let input: HtmlInputElement = e.target_unchecked_into();
            let cleaned: String = input.value().chars().filter(|c| c.is_ascii_digit()).collect();
            if cleaned != input.value() {
                input.set_value(&cleaned);
            }
            id.set(cleaned);
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

    let on_edit_draft_change = {
        let edit_draft = edit_draft.clone();
        Callback::from(move |e: Event| {
            let input: HtmlTextAreaElement = e.target_unchecked_into();
            edit_draft.set(input.value());
        })
    };

    let start_edit = {
        let editing_id = editing_id.clone();
        let edit_draft = edit_draft.clone();
        let saved_accounts = saved_accounts.clone();
        let message = message.clone();
        let error = error.clone();

        Callback::from(move |account_id: i64| {
            let Some(cfg) = read_config_from_head() else {
                return;
            };

            editing_id.set(Some(account_id));

            if let Some(existing) = (*saved_accounts).iter().find(|a| a.id == account_id) {
                edit_draft.set(existing.blacklist.clone());
            } else {
                edit_draft.set(String::new());
            }

            let edit_draft = edit_draft.clone();
            let message = message.clone();
            let error = error.clone();

            wasm_bindgen_futures::spawn_local(async move {
                let url = format!("{}/account/{}/blacklist", cfg.backend_domain, account_id);
                match api_get(&url).send().await {
                    Ok(resp) if resp.ok() => {
                        if let Ok(payload) = resp.json::<BlacklistResponse>().await {
                            edit_draft.set(payload.blacklist);
                        }
                    }
                    Ok(resp) => {
                        message.set(format!(
                            "Failed to load blacklist (status {})",
                            resp.status()
                        ));
                        error.set(true);
                    }
                    Err(e) => {
                        message.set(format!("Network error loading blacklist: {e}"));
                        error.set(true);
                    }
                }
            });
        })
    };

    let cancel_edit = {
        let editing_id = editing_id.clone();
        let edit_draft = edit_draft.clone();
        Callback::from(move |_| {
            editing_id.set(None);
            edit_draft.set(String::new());
        })
    };

    // Click handler on the row's "Remove" button: stage the target so
    // the modal renders. The actual DELETE happens only after the user
    // confirms via `on_remove_confirm`.
    let on_remove = {
        let saved_accounts = saved_accounts.clone();
        let pending_remove = pending_remove.clone();
        Callback::from(move |account_id: i64| {
            if let Some(target) = (*saved_accounts).iter().find(|a| a.id == account_id) {
                pending_remove.set(Some(target.clone()));
            }
        })
    };

    let on_remove_cancel = {
        let pending_remove = pending_remove.clone();
        Callback::from(move |_| pending_remove.set(None))
    };

    let on_remove_confirm = {
        let saved_accounts = saved_accounts.clone();
        let editing_id = editing_id.clone();
        let edit_draft = edit_draft.clone();
        let remove_error = remove_error.clone();
        let pending_remove = pending_remove.clone();
        Callback::from(move |_| {
            let Some(target) = (*pending_remove).clone() else {
                return;
            };
            pending_remove.set(None);

            let Some(cfg) = read_config_from_head() else {
                remove_error.set(Some(
                    "App configuration failed to load — please reload the page.".to_string(),
                ));
                return;
            };
            let url = format!("{}/account/{}", cfg.backend_domain, target.id);

            let saved_accounts = saved_accounts.clone();
            let editing_id = editing_id.clone();
            let edit_draft = edit_draft.clone();
            let remove_error = remove_error.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match api_delete(&url).send().await {
                    Ok(resp) if resp.ok() => {
                        let new_list: Vec<UserInfo> = saved_accounts
                            .iter()
                            .filter(|u| u.id != target.id)
                            .cloned()
                            .collect();
                        saved_accounts.set(new_list);
                        if *editing_id == Some(target.id) {
                            editing_id.set(None);
                            edit_draft.set(String::new());
                        }
                        remove_error.set(None);
                        dispatch_account_list_changed();
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        remove_error.set(Some(humanize_error_body(status, &body)));
                    }
                    Err(e) => {
                        remove_error.set(Some(format!("Network error: {e}")));
                    }
                }
            });
        })
    };

    let save_edit = {
        let editing_id = editing_id.clone();
        let edit_draft = edit_draft.clone();
        let edit_saving = edit_saving.clone();
        let saved_accounts = saved_accounts.clone();
        let message = message.clone();
        let error = error.clone();

        Callback::from(move |_| {
            let Some(account_id) = *editing_id else {
                return;
            };
            let Some(cfg) = read_config_from_head() else {
                return;
            };

            edit_saving.set(true);

            let body = serde_json::json!({
                "blacklist": (*edit_draft).clone(),
            })
            .to_string();

            let editing_id = editing_id.clone();
            let edit_draft = edit_draft.clone();
            let edit_saving = edit_saving.clone();
            let saved_accounts = saved_accounts.clone();
            let message = message.clone();
            let error = error.clone();

            wasm_bindgen_futures::spawn_local(async move {
                let url = format!("{}/account/{}/blacklist", cfg.backend_domain, account_id);
                let response = api_patch(&url)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await;

                edit_saving.set(false);

                match response {
                    Ok(resp) if resp.ok() => match resp.json::<UserInfo>().await {
                        Ok(saved) => {
                            let mut accounts = (*saved_accounts).clone();
                            if let Some(existing) =
                                accounts.iter_mut().find(|acc| acc.id == saved.id)
                            {
                                *existing = saved;
                            }
                            saved_accounts.set(accounts);
                            editing_id.set(None);
                            edit_draft.set(String::new());
                            message.set("Blacklist updated".to_string());
                            error.set(false);
                        }
                        Err(e) => {
                            message.set(format!("Failed to parse response: {e}"));
                            error.set(true);
                        }
                    },
                    Ok(resp) => {
                        let status = resp.status();
                        let text = resp
                            .text()
                            .await
                            .unwrap_or_else(|_| "Unknown error".to_string());
                        let humanized = humanize_error_body(status, &text);
                        web_sys::console::error_1(
                            &format!("blacklist update failed (HTTP {status}): {text}").into(),
                        );
                        message.set(humanized);
                        error.set(true);
                    }
                    Err(e) => {
                        message.set(format!("Network error: {e}"));
                        error.set(true);
                    }
                }
            });
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

            let Some(cfg) = read_config_from_head() else {
                message
                    .set("App configuration failed to load — please reload the page.".to_string());
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
            // Match server-side `validate_account_id` (1..=100_000_000) so
            // the user gets a clear local error instead of a 400 round-trip.
            if !(1..=100_000_000).contains(&account_id) {
                message.set("Account ID must be between 1 and 100000000".to_string());
                error.set(true);
                loading.set(false);
                return;
            }

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
                "id": account_id,
                "name": raw_name,
                "blacklist": raw_blacklist,
            });

            let message = message.clone();
            let error = error.clone();
            let loading = loading.clone();
            let saved_accounts = saved_accounts.clone();
            let id_for_reset = id.clone();
            let name_for_reset = name.clone();
            let blacklist_for_reset = blacklist.clone();

            wasm_bindgen_futures::spawn_local(async move {
                let response = api_post(&format!("{0}/account", cfg.backend_domain))
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
                            if let Some(existing) =
                                accounts.iter_mut().find(|acc| acc.id == saved.id)
                            {
                                *existing = saved.clone();
                            } else {
                                accounts.push(saved);
                            }
                            saved_accounts.set(accounts);
                            message.set("Account created successfully!".to_string());
                            error.set(false);
                            id_for_reset.set(String::new());
                            name_for_reset.set(String::new());
                            blacklist_for_reset.set(String::new());
                            dispatch_account_list_changed();
                        } else {
                            let status = resp.status();
                            let error_msg = resp
                                .text()
                                .await
                                .unwrap_or_else(|_| "Unknown error".to_string());
                            let humanized = humanize_error_body(status, &error_msg);
                            web_sys::console::error_1(
                                &format!("account create failed (HTTP {status}): {error_msg}")
                                    .into(),
                            );
                            message.set(humanized);
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

    let accounts_list = (*saved_accounts).clone();
    let current_edit = *editing_id;

    let pending_label = pending_remove
        .as_ref()
        .map(|u| format!("'{}' (ID {})", u.name, u.id))
        .unwrap_or_default();

    html! {
        <div class="container mt-5" id="account-page">
            <h1 class="visually-hidden">{"Account"}</h1>
            <ConfirmModal
                open={pending_remove.is_some()}
                title={"Remove account from this device?".to_string()}
                confirm_label={"Remove".to_string()}
                cancel_label={"Cancel".to_string()}
                destructive=true
                on_confirm={on_remove_confirm.clone()}
                on_cancel={on_remove_cancel.clone()}
            >
                <p class="mb-2">
                    { format!("This will unlink {} from this device.", pending_label) }
                </p>
                <p class="text-body-secondary mb-0 small">
                    { "If no other devices are linked, the account's stored favourites, \
                       blacklist and preference profile are also deleted from the server. \
                       The e621 account itself is unaffected." }
                </p>
            </ConfirmModal>
            <div class="row justify-content-center">
                <div class="col-md-6">
                    if !accounts_list.is_empty() {
                        <div class="card shadow mb-4">
                            <div class="card-body">
                                <h2 class="card-title text-center mb-4">{"Saved Accounts"}</h2>
                                {
                                    if let Some(err) = &*remove_error {
                                        html! {
                                            <div class="alert alert-danger py-2 px-3 mb-3" role="alert">
                                                { err }
                                            </div>
                                        }
                                    } else { html!{} }
                                }
                                <ul class="list-group">
                                    {for accounts_list.iter().map(|acc| {
                                        let is_editing = current_edit == Some(acc.id);
                                        let acc_id = acc.id;
                                        let start_edit = start_edit.clone();
                                        let cancel_edit = cancel_edit.clone();
                                        let save_edit = save_edit.clone();
                                        let on_remove = on_remove.clone();
                                        let on_draft_change = on_edit_draft_change.clone();
                                        let draft_value = (*edit_draft).clone();
                                        let is_saving = *edit_saving;
                                        let bucket = experiment_buckets.get(&acc.id).cloned();
                                        html! {
                                            <li class="list-group-item">
                                                <div class="d-flex justify-content-between align-items-center gap-2">
                                                    <span>
                                                        <strong>{&acc.name}</strong>
                                                        {format!(" (ID: {})", acc.id)}
                                                        if let Some(bucket_name) = bucket {
                                                            <span
                                                                class="badge text-bg-info ms-2"
                                                                title="A/B experiment variant currently used for recommendations on this account"
                                                            >
                                                                { format!("variant: {bucket_name}") }
                                                            </span>
                                                        }
                                                    </span>
                                                    <div class="btn-group btn-group-sm" role="group">
                                                        if is_editing {
                                                            <button
                                                                class="btn btn-outline-secondary"
                                                                onclick={Callback::from(move |_| cancel_edit.emit(()))}
                                                                disabled={is_saving}
                                                            >
                                                                {"Cancel"}
                                                            </button>
                                                        } else {
                                                            <button
                                                                class="btn btn-outline-primary"
                                                                onclick={Callback::from(move |_| start_edit.emit(acc_id))}
                                                            >
                                                                {"Edit blacklist"}
                                                            </button>
                                                            <button
                                                                class="btn btn-outline-danger"
                                                                title="Unlink this account from the device"
                                                                onclick={Callback::from(move |_| on_remove.emit(acc_id))}
                                                            >
                                                                {"Remove"}
                                                            </button>
                                                        }
                                                    </div>
                                                </div>
                                                if is_editing {
                                                    <div class="mt-3">
                                                        <textarea
                                                            class="form-control"
                                                            rows="5"
                                                            value={draft_value}
                                                            onchange={on_draft_change}
                                                            disabled={is_saving}
                                                        />
                                                        <div class="form-text mb-2">
                                                            {"One tag per line. Leave empty to fall back to the default blacklist."}
                                                        </div>
                                                        <button
                                                            class="btn btn-primary btn-sm"
                                                            onclick={Callback::from(move |_| save_edit.emit(()))}
                                                            disabled={is_saving}
                                                        >
                                                            { if is_saving { "Saving..." } else { "Save" } }
                                                        </button>
                                                    </div>
                                                }
                                            </li>
                                        }
                                    })}
                                </ul>
                            </div>
                        </div>
                    }

                    <div class="card shadow">
                        <div class="card-body">
                            <h2 class="card-title text-center mb-4">{"Create New Account"}</h2>
                            <form onsubmit={onsubmit}>
                                <div class="mb-3">
                                    <label for="account-id" class="form-label">{"Account ID"}</label>
                                    <input
                                        type="text"
                                        inputmode="numeric"
                                        pattern="[0-9]+"
                                        maxlength="9"
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
                                        class="form-control"
                                        id="account-blacklist"
                                        rows="5"
                                        value={(*blacklist).clone()}
                                        onchange={on_blacklist_change}
                                        placeholder={"One tag per line, for example:\ngore\nyoung -rating:s\n-fav:yourname"}
                                        disabled={*loading}
                                    />
                                    <div class="form-text">
                                        { "Optional. Paste the blacklist from your e621 account settings, or leave empty to use the default." }
                                        if !default_blacklist.is_empty() {
                                            <div class="mt-1">
                                                { "Default applied if empty: " }
                                                <code>{ default_blacklist.join(", ") }</code>
                                            </div>
                                        }
                                    </div>
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

#[derive(serde::Deserialize)]
struct BlacklistResponse {
    blacklist: String,
}
