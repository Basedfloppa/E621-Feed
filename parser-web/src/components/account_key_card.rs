//! "Account key + direct sync" card for the settings page.
//!
//! Manages the selected account's per-user e621 API key (Settings → Account):
//! add / test / rotate / revoke, showing which operations use it (`key/state`),
//! and a read-only direct-sync trigger + last-sync status (`sync` / `sync/status`).
//! No key material is ever displayed back — the UI only shows booleans +
//! timestamps returned by the backend.

use serde::Deserialize;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::components::ConfirmModal;
use crate::models::{
    api_delete, api_get, api_post, api_put, humanize_error_body, humanize_network_error,
};

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct AccountKeyState {
    #[serde(default)]
    has_key: bool,
    #[serde(default)]
    added_at: Option<String>,
    #[serde(default)]
    verified_at: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    operations: Vec<String>,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct KeyVerifyResult {
    #[serde(default)]
    valid: bool,
    #[serde(default)]
    name: String,
    #[serde(default)]
    verified_at: Option<String>,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct SyncStatus {
    #[serde(default)]
    has_key: bool,
    #[serde(default)]
    last_synced_at: Option<String>,
    #[serde(default)]
    datasets: Vec<String>,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct SyncSummary {
    #[serde(default)]
    favorites_persisted: usize,
    #[serde(default)]
    blacklist_imported: bool,
    #[serde(default)]
    synced_at: Option<String>,
}

#[derive(Clone, PartialEq, Properties)]
pub struct AccountKeyCardProps {
    pub backend_url: String,
    pub selected_account_id: Option<i64>,
}

#[function_component(AccountKeyCard)]
pub fn account_key_card(props: &AccountKeyCardProps) -> Html {
    let key_state = use_state(|| None::<AccountKeyState>);
    let sync_status = use_state(|| None::<SyncStatus>);
    let key_input = use_state(String::new);
    let confirm_revoke = use_state(|| false);
    let busy = use_state(|| false);
    let testing = use_state(|| false);
    let syncing = use_state(|| false);
    let test_result = use_state(|| None::<KeyVerifyResult>);
    let message = use_state(String::new);
    let is_error = use_state(|| false);

    let account_id = props.selected_account_id;
    let backend_url = props.backend_url.clone();

    // Load key state + sync status whenever the selected account changes.
    {
        let key_state = key_state.clone();
        let sync_status = sync_status.clone();
        let key_input = key_input.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let test_result = test_result.clone();
        let backend_url = backend_url.clone();
        use_effect_with(account_id, move |account_id| {
            // `account_id` is `&Option<i64>` here; deref to a Copy value so it
            // can safely move into the `spawn_local` future.
            let account_id = *account_id;
            let key_state = key_state.clone();
            let sync_status = sync_status.clone();
            let key_input = key_input.clone();
            let message = message.clone();
            let is_error = is_error.clone();
            let test_result = test_result.clone();
            let backend_url = backend_url.clone();
            spawn_local(async move {
                // Reset drafts + flags whenever the selected account changes so
                // a typed (unsaved) key or a stale error from the previous
                // account doesn't linger on the new selection.
                key_input.set(String::new());
                message.set(String::new());
                is_error.set(false);
                test_result.set(None);
                let Some(account_id) = account_id else {
                    key_state.set(None);
                    sync_status.set(None);
                    return;
                };
                let mut err: Option<String> = None;
                match api_get(&format!("{backend_url}/account/{account_id}/key/state"))
                    .send()
                    .await
                {
                    Ok(resp) if resp.ok() => match resp.json::<AccountKeyState>().await {
                        Ok(s) => key_state.set(Some(s)),
                        Err(_) => err = Some("Could not read account key state.".to_string()),
                    },
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        err = Some(humanize_error_body(status, &body));
                    }
                    Err(e) => err = Some(humanize_network_error(e)),
                }
                // Surface a sync/status read failure too (best-effort), but
                // don't clobber a more important key-state error.
                if err.is_none() {
                    match api_get(&format!("{backend_url}/account/{account_id}/sync/status"))
                        .send()
                        .await
                    {
                        Ok(resp) if resp.ok() => {
                            if let Ok(s) = resp.json::<SyncStatus>().await {
                                sync_status.set(Some(s))
                            }
                        }
                        Ok(resp) => {
                            let status = resp.status();
                            let body = resp.text().await.unwrap_or_default();
                            err = Some(humanize_error_body(status, &body));
                        }
                        Err(e) => err = Some(humanize_network_error(e)),
                    }
                }
                if let Some(e) = err {
                    message.set(e);
                    is_error.set(true);
                }
            });
        });
    }

    let save_key = {
        let key_input = key_input.clone();
        let busy = busy.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let key_state = key_state.clone();
        let backend_url = backend_url.clone();
        let test_result = test_result.clone();
        Callback::from(move |_| {
            let key_input = key_input.clone();
            let busy = busy.clone();
            let message = message.clone();
            let is_error = is_error.clone();
            let key_state = key_state.clone();
            let backend_url = backend_url.clone();
            let test_result = test_result.clone();
            let Some(account_id) = account_id else {
                return;
            };
            let key = (*key_input).clone();
            if key.trim().is_empty() {
                message.set("Paste an e621 API key first.".to_string());
                is_error.set(true);
                return;
            }
            spawn_local(async move {
                busy.set(true);
                message.set(String::new());
                is_error.set(false);
                test_result.set(None);
                let url = format!("{backend_url}/account/{account_id}/key");
                let body = serde_json::json!({ "key": key }).to_string();
                let result = api_put(&url)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await;
                match result {
                    Ok(resp) if resp.ok() => {
                        if let Ok(s) = resp.json::<AccountKeyState>().await {
                            key_state.set(Some(s))
                        }
                        key_input.set(String::new());
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        message.set(humanize_error_body(status, &body));
                        is_error.set(true);
                    }
                    Err(e) => {
                        message.set(humanize_network_error(e));
                        is_error.set(true);
                    }
                }
                busy.set(false);
            });
        })
    };

    let test_key = {
        let testing = testing.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let test_result = test_result.clone();
        let key_state = key_state.clone();
        let backend_url = backend_url.clone();
        Callback::from(move |_| {
            let testing = testing.clone();
            let message = message.clone();
            let is_error = is_error.clone();
            let test_result = test_result.clone();
            let key_state = key_state.clone();
            let backend_url = backend_url.clone();
            let Some(account_id) = account_id else {
                return;
            };
            spawn_local(async move {
                testing.set(true);
                message.set(String::new());
                is_error.set(false);
                let url = format!("{backend_url}/account/{account_id}/key/test");
                match api_post(&url).send().await {
                    Ok(resp) if resp.ok() => match resp.json::<KeyVerifyResult>().await {
                        Ok(r) => {
                            test_result.set(Some(r));
                            // Refresh the state (verified_at) after a successful test.
                            if let Ok(inner) =
                                api_get(&format!("{backend_url}/account/{account_id}/key/state"))
                                    .send()
                                    .await
                                && inner.ok()
                                && let Ok(s) = inner.json::<AccountKeyState>().await
                            {
                                key_state.set(Some(s));
                            }
                        }
                        Err(_) => {
                            message.set("Could not parse verification result.".to_string());
                            is_error.set(true);
                        }
                    },
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        message.set(humanize_error_body(status, &body));
                        is_error.set(true);
                    }
                    Err(e) => {
                        message.set(humanize_network_error(e));
                        is_error.set(true);
                    }
                }
                testing.set(false);
            });
        })
    };

    // Revoke: DELETE /account/<id>/key (confirmed via modal).
    let do_revoke = {
        let key_state = key_state.clone();
        let sync_status = sync_status.clone();
        let busy = busy.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let confirm_revoke = confirm_revoke.clone();
        let test_result = test_result.clone();
        let backend_url = backend_url.clone();
        Callback::from(move |_| {
            let key_state = key_state.clone();
            let sync_status = sync_status.clone();
            let busy = busy.clone();
            let message = message.clone();
            let is_error = is_error.clone();
            let confirm_revoke = confirm_revoke.clone();
            let test_result = test_result.clone();
            let backend_url = backend_url.clone();
            let Some(account_id) = account_id else {
                return;
            };
            spawn_local(async move {
                busy.set(true);
                message.set(String::new());
                is_error.set(false);
                let url = format!("{backend_url}/account/{account_id}/key");
                match api_delete(&url).send().await {
                    Ok(resp) if resp.ok() => {
                        key_state.set(None);
                        sync_status.set(None);
                        test_result.set(None);
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        message.set(humanize_error_body(status, &body));
                        is_error.set(true);
                    }
                    Err(e) => {
                        message.set(humanize_network_error(e));
                        is_error.set(true);
                    }
                }
                // Close the confirmation modal whether or not the delete
                // succeeded — the outcome is surfaced in `message`.
                confirm_revoke.set(false);
                busy.set(false);
            });
        })
    };

    // Sync trigger: POST /account/<id>/sync (read-only).
    let run_sync = {
        let syncing = syncing.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let sync_status = sync_status.clone();
        let backend_url = backend_url.clone();
        Callback::from(move |_| {
            let syncing = syncing.clone();
            let message = message.clone();
            let is_error = is_error.clone();
            let sync_status = sync_status.clone();
            let backend_url = backend_url.clone();
            let Some(account_id) = account_id else {
                return;
            };
            spawn_local(async move {
                syncing.set(true);
                message.set(String::new());
                is_error.set(false);
                let url = format!("{backend_url}/account/{account_id}/sync");
                match api_post(&url).send().await {
                    Ok(resp) if resp.ok() => {
                        let _ = resp.json::<SyncSummary>().await;
                        if let Ok(inner) =
                            api_get(&format!("{backend_url}/account/{account_id}/sync/status"))
                                .send()
                                .await
                            && inner.ok()
                            && let Ok(s) = inner.json::<SyncStatus>().await
                        {
                            sync_status.set(Some(s));
                        }
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        message.set(humanize_error_body(status, &body));
                        is_error.set(true);
                    }
                    Err(e) => {
                        message.set(humanize_network_error(e));
                        is_error.set(true);
                    }
                }
                syncing.set(false);
            });
        })
    };

    let has_key = key_state.as_ref().map(|s| s.has_key).unwrap_or(false);
    let last_synced = sync_status
        .as_ref()
        .and_then(|s| s.last_synced_at.clone())
        .or_else(|| key_state.as_ref().and_then(|s| s.verified_at.clone()));

    html! {
        <div class="card bg-base-100 shadow">
            <div class="card-body text-base-content">
                <h2 class="card-title text-xl">{ "e621 Account Key & Sync" }</h2>
                <p class="text-sm text-base-content/70 mb-2">
                    { "Set this account's e621 API key to prove ownership and enable read-only direct sync of favorites, blacklist and profile tags. Sync reads from e621 only — nothing is uploaded." }
                </p>
                if account_id.is_none() {
                    <p class="text-base-content/70 text-center py-2">
                        { "Select an account above to manage its key." }
                    </p>
                } else {
                    if has_key {
                        <div class="badge badge-success gap-1">{"✓ Key configured"}</div>
                        <ul class="text-sm text-base-content/70 list-disc list-inside space-y-0.5">
                            <li>{ format!("Added: {}", key_state.as_ref().and_then(|s| s.added_at.clone()).unwrap_or_default()) }</li>
                            <li>{ format!("Verified: {}", key_state.as_ref().and_then(|s| s.verified_at.clone()).unwrap_or_else(|| "never".to_string())) }</li>
                            <li>{ format!("Uses: {}", key_state.as_ref().map(|s| s.operations.join(", ")).unwrap_or_default()) }</li>
                        </ul>
                        <div class="flex flex-wrap gap-2 mt-2">
                            <button class="btn btn-sm btn-outline btn-primary"
                                disabled={*testing}
                                onclick={test_key}>
                                { if *testing { "Testing…" } else { "Test key" } }
                            </button>
                            <button class="btn btn-sm btn-outline"
                                onclick={{ let confirm_revoke = confirm_revoke.clone(); Callback::from(move |_| confirm_revoke.set(true)) }}
                                disabled={*busy}>
                                { "Revoke key" }
                            </button>
                        </div>
                    } else {
                        <p class="text-base-content/70 text-sm">{ "No e621 API key configured for this account yet. Add one to unlock direct sync." }</p>
                    }

                    if let Some(r) = test_result.as_ref() {
                        if r.valid {
                            <p class="text-success text-sm mt-2">{"✓ Key verified against e621."}</p>
                        } else {
                            <p class="text-error text-sm mt-2">{"✗ Key rejected by e621 — check it in your e621 account settings."}</p>
                        }
                    }

                    <div class="form-control mt-4">
                        <label class="label pb-1">
                            <span class="label-text font-medium">{ "e621 API key" }</span>
                        </label>
                        <div class="flex items-center gap-2">
                            <input type="password" class="input input-bordered input-sm flex-1 min-w-0"
                                placeholder={ if has_key { "New key to rotate…" } else { "Paste e621 API key…" } }
                                value={(*key_input).clone()}
                                oninput={let key_input = key_input.clone(); Callback::from(move |e: InputEvent| {
                                    key_input.set(e.target_unchecked_into::<HtmlInputElement>().value());
                                })}
                            />
                            <button class="btn btn-sm btn-primary shrink-0" disabled={*busy} onclick={save_key}>
                                { if has_key { "Rotate key" } else { "Save key" } }
                            </button>
                        </div>
                    </div>

                    <div class="divider"></div>

                    <div class="flex items-center justify-between gap-2">
                        <div>
                            <span class="font-semibold">{ "Direct sync" }</span>
                            <span class="block text-sm text-base-content/70">
                                { if let Some(ts) = &last_synced {
                                    format!("Last synced: {ts}")
                                } else {
                                    "Not synced yet.".to_string()
                                } }
                            </span>
                            <span class="block text-xs text-base-content/60">
                                { "Read-only: favorites, blacklist, profile tags." }
                            </span>
                        </div>
                        <button class="btn btn-sm btn-outline btn-primary" disabled={*syncing || !has_key} onclick={run_sync}>
                            { if *syncing { "Syncing…" } else { "Sync now" } }
                        </button>
                    </div>
                }

                if !message.is_empty() {
                    <p class={ if *is_error { "text-error text-sm mt-2" } else { "text-success text-sm mt-2" } }>
                        { &*message }
                    </p>
                }
            </div>

            <ConfirmModal
                open={*confirm_revoke}
                title={"Revoke e621 API key".to_string()}
                confirm_label={"Revoke".to_string()}
                destructive={true}
                on_confirm={do_revoke}
                on_cancel={{ let confirm_revoke = confirm_revoke.clone(); Callback::from(move |_| confirm_revoke.set(false)) }}
            >
                { "This removes the stored key. Direct sync will stop until you add the key again." }
            </ConfirmModal>
        </div>
    }
}
