//! "Devices & sessions" + "Privacy" cards for the settings page.
//!
//! Lists the devices (owner tokens) that share this server's linked accounts
//! (`GET /session/devices`), lets the operator revoke any device other than the
//! current one (`POST /session/revoke`), and offers a privacy action to clear
//! the selected account's interaction model (`DELETE /account/<id>/interaction`).
//! Device ids are `sha256` hashes — raw tokens are never displayed.

use serde::Deserialize;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use crate::components::ConfirmModal;
use crate::models::{api_delete, api_get, api_post, humanize_error_body, humanize_network_error};

/// A device as returned by `GET /session/devices`.
#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct DeviceSession {
    id: String,
    is_current: bool,
    #[serde(default)]
    last_seen_at: String,
    #[serde(default)]
    active: bool,
}

#[derive(Clone, PartialEq, Properties)]
pub struct SessionDevicesCardProps {
    pub backend_url: String,
    /// Account whose interaction model can be cleared in the Privacy card.
    pub selected_account_id: Option<i64>,
}

fn short_id(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    } else {
        id.to_string()
    }
}

async fn fetch_devices(backend_url: &str) -> Result<Vec<DeviceSession>, String> {
    match api_get(&format!("{backend_url}/session/devices"))
        .send()
        .await
    {
        Ok(resp) if resp.ok() => resp
            .json::<Vec<DeviceSession>>()
            .await
            .map_err(|_| "Could not read devices.".to_string()),
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(humanize_error_body(status, &body))
        }
        Err(e) => Err(humanize_network_error(e)),
    }
}

#[function_component(SessionDevicesCard)]
pub fn session_devices_card(props: &SessionDevicesCardProps) -> Html {
    let devices = use_state(Vec::<DeviceSession>::new);
    let loading = use_state(|| true);
    let message = use_state(String::new);
    let is_error = use_state(|| false);
    let revoking = use_state(String::new);
    let confirm_revoke = use_state(|| None::<String>);
    let clearing = use_state(|| false);
    let confirm_clear = use_state(|| false);

    let backend_url = props.backend_url.clone();

    // Fetch the device list on mount.
    {
        let devices = devices.clone();
        let loading = loading.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let backend_url = backend_url.clone();
        use_effect_with((), move |_| {
            let devices = devices.clone();
            let loading = loading.clone();
            let message = message.clone();
            let is_error = is_error.clone();
            let backend_url = backend_url.clone();
            spawn_local(async move {
                match fetch_devices(&backend_url).await {
                    Ok(list) => {
                        devices.set(list);
                        message.set(String::new());
                    }
                    Err(e) => {
                        message.set(e);
                        is_error.set(true);
                    }
                }
                loading.set(false);
            });
            || ()
        });
    }

    let on_request_revoke = {
        let confirm_revoke = confirm_revoke.clone();
        Callback::from(move |id: String| confirm_revoke.set(Some(id)))
    };
    let on_cancel_revoke = {
        let confirm_revoke = confirm_revoke.clone();
        Callback::from(move |_| confirm_revoke.set(None))
    };
    let on_confirm_revoke = {
        let confirm_revoke = confirm_revoke.clone();
        let devices = devices.clone();
        let revoking = revoking.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let backend_url = backend_url.clone();
        Callback::from(move |_| {
            let Some(device_id) = (*confirm_revoke).clone() else {
                return;
            };
            confirm_revoke.set(None);
            // Re-clone inside so this closure stays `Fn` (Yew Callback).
            let devices = devices.clone();
            let revoking = revoking.clone();
            let message = message.clone();
            let is_error = is_error.clone();
            let backend_url = backend_url.clone();
            spawn_local(async move {
                revoking.set(device_id.clone());
                message.set(String::new());
                let url = format!("{backend_url}/session/revoke");
                let body = serde_json::json!({ "deviceId": device_id });
                match api_post(&url)
                    .header("Content-Type", "application/json")
                    .body(body.to_string())
                    .send()
                    .await
                {
                    Ok(resp) if resp.ok() => {}
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
                revoking.set(String::new());
                match fetch_devices(&backend_url).await {
                    Ok(list) => devices.set(list),
                    Err(e) => {
                        message.set(e);
                        is_error.set(true);
                    }
                }
            });
        })
    };

    let on_request_clear = {
        let confirm_clear = confirm_clear.clone();
        Callback::from(move |_| confirm_clear.set(true))
    };
    let on_cancel_clear = {
        let confirm_clear = confirm_clear.clone();
        Callback::from(move |_| confirm_clear.set(false))
    };
    let on_confirm_clear = {
        let confirm_clear = confirm_clear.clone();
        let clearing = clearing.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let backend_url = backend_url.clone();
        let account_id = props.selected_account_id;
        Callback::from(move |_| {
            confirm_clear.set(false);
            let Some(account_id) = account_id else {
                message.set("Select an account first.".to_string());
                is_error.set(true);
                return;
            };
            // Re-clone inside so this closure stays `Fn`.
            let clearing = clearing.clone();
            let message = message.clone();
            let is_error = is_error.clone();
            let backend_url = backend_url.clone();
            let url = format!("{backend_url}/account/{account_id}/interaction");
            clearing.set(true);
            message.set(String::new());
            spawn_local(async move {
                match api_delete(&url).send().await {
                    Ok(resp) if resp.ok() => {
                        message.set("Interaction data cleared.".to_string());
                        is_error.set(false);
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
                clearing.set(false);
            });
        })
    };

    let status_class = if *is_error {
        "alert-error"
    } else {
        "alert-success"
    };

    html! {
        <>
        <div id="settings-sessions" class="card bg-base-200 border border-base-300 shadow-sm">
            <div class="card-body">
                <h2 class="card-title text-xl">{ "Devices & sessions" }</h2>
                <p class="text-sm text-base-content/70">
                    { "Devices that share your linked accounts on this server. You can revoke access for any device other than this one." }
                </p>
                if !message.is_empty() {
                    <div class={format!("alert {status_class} text-sm py-2")}>{ (*message).clone() }</div>
                }
                if *loading {
                    <div class="text-sm text-base-content/60">{ "Loading…" }</div>
                } else if devices.is_empty() {
                    <div class="text-sm text-base-content/60">{ "No linked devices." }</div>
                } else {
                    <ul class="flex flex-col gap-2">
                    { for devices.iter().map(|d| {
                        let id = d.id.clone();
                        let badge = if d.active { "badge-success" } else { "badge-ghost" };
                        let revoke_cb = {
                            let id = id.clone();
                            let cb = on_request_revoke.clone();
                            Callback::from(move |_| cb.emit(id.clone()))
                        };
                        html! {
                            <li class="flex items-center justify-between gap-2 border border-base-300 rounded-lg p-2">
                                <div class="flex flex-col">
                                    <div class="flex items-center gap-2">
                                        <span class="font-mono text-sm">{ short_id(&d.id) }</span>
                                        if d.is_current {
                                            <span class="badge badge-primary badge-sm">{ "this device" }</span>
                                        }
                                        <span class={format!("badge badge-sm {badge}")}>
                                            { if d.active { "active" } else { "inactive" } }
                                        </span>
                                    </div>
                                    <span class="text-xs text-base-content/60">
                                        { format!("last seen {}", d.last_seen_at) }
                                    </span>
                                </div>
                                if !d.is_current {
                                    <button class="btn btn-sm btn-error" disabled={!revoking.is_empty()} onclick={revoke_cb}>
                                        { "Revoke" }
                                    </button>
                                }
                            </li>
                        }
                    })}
                    </ul>
                }
            </div>
            <ConfirmModal
                open={confirm_revoke.is_some()}
                title={"Revoke device?".to_string()}
                confirm_label={"Revoke".to_string()}
                destructive={true}
                on_confirm={on_confirm_revoke.clone()}
                on_cancel={on_cancel_revoke.clone()}
            >
                { "This immediately signs that device out and removes its access to your linked accounts. It cannot be undone from this page." }
            </ConfirmModal>
        </div>

        <div id="settings-privacy" class="card bg-base-200 border border-base-300 shadow-sm">
            <div class="card-body">
                <h2 class="card-title text-xl">{ "Privacy" }</h2>
                <p class="text-sm text-base-content/70">
                    { "Clear the interaction model (your opens / likes / hides) that drives recommendations for the selected account. The account, its favourites, blacklist and links are kept — the profile is rebuilt from fresh data by /process." }
                </p>
                <div class="flex items-center gap-2">
                    <button class="btn btn-sm btn-error" disabled={*clearing} onclick={on_request_clear.clone()}>
                        { if *clearing { "Clearing…" } else { "Clear interaction data" } }
                    </button>
                    <span class="text-xs text-base-content/60">
                        { "for the selected account" }
                    </span>
                </div>
            </div>
            <ConfirmModal
                open={*confirm_clear}
                title={"Clear interaction data?".to_string()}
                confirm_label={"Clear".to_string()}
                destructive={true}
                on_confirm={on_confirm_clear.clone()}
                on_cancel={on_cancel_clear.clone()}
            >
                { "This removes the recommendation-driving preferences for the selected account. It cannot be undone." }
            </ConfirmModal>
        </div>
        </>
    }
}
