//! "Instance ownership" card for the settings page.
//!
//! There is no account login in this app: the `HttpOnly` owner cookie *is* the
//! identity, and a fresh browser starts with one that owns nothing. On a
//! single-owner instance that is invisible, because the first
//! `/session/bootstrap` claims every account that belongs to nobody. Once
//! `[instance].owner_secret` is set, claiming becomes an explicit act — and this
//! card is the only way to perform it from a browser, since `/session/enroll`
//! would otherwise be API-only.

use serde::Deserialize;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::models::{
    api_get, api_post, dispatch_account_list_changed, humanize_error_body, humanize_network_error,
};

/// `GET /session/state`.
#[derive(Clone, Deserialize, PartialEq)]
struct InstanceState {
    instance_locked: bool,
    owned_accounts: i64,
    adoptable_accounts: i64,
}

#[derive(Clone, PartialEq, Properties)]
pub struct InstanceCardProps {
    pub backend_url: String,
}

/// What this browser holds and what is still claimed by nobody.
///
/// A free function rather than inline markup so the plural forms can be pinned
/// by a test that needs no DOM.
fn accounts_summary(owned: i64, adoptable: i64) -> String {
    let mine = match owned {
        0 => "This browser owns no accounts yet.".to_string(),
        1 => "This browser owns 1 account.".to_string(),
        n => format!("This browser owns {n} accounts."),
    };
    match adoptable {
        0 => mine,
        1 => format!("{mine} 1 account belongs to nobody."),
        n => format!("{mine} {n} accounts belong to nobody."),
    }
}

async fn fetch_state(backend_url: &str) -> Result<InstanceState, String> {
    match api_get(&format!("{backend_url}/session/state"))
        .send()
        .await
    {
        Ok(resp) if resp.ok() => resp
            .json::<InstanceState>()
            .await
            .map_err(|_| "Could not read the instance state.".to_string()),
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(humanize_error_body(status, &body))
        }
        Err(e) => Err(humanize_network_error(e)),
    }
}

#[function_component(InstanceCard)]
pub fn instance_card(props: &InstanceCardProps) -> Html {
    let state = use_state(|| None::<InstanceState>);
    let load_error = use_state(String::new);
    let secret = use_state(String::new);
    let busy = use_state(|| false);
    let message = use_state(String::new);
    let is_error = use_state(|| false);
    // Bumped after a successful claim: ownership changed, so the counts the
    // card shows have to be read again.
    let reload = use_state(|| 0u32);

    let backend_url = props.backend_url.clone();

    {
        let state = state.clone();
        let load_error = load_error.clone();
        let backend_url = backend_url.clone();
        use_effect_with(*reload, move |_| {
            spawn_local(async move {
                match fetch_state(&backend_url).await {
                    Ok(fresh) => state.set(Some(fresh)),
                    Err(e) => load_error.set(e),
                }
            });
            || ()
        });
    }

    let claim = {
        let secret = secret.clone();
        let busy = busy.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let reload = reload.clone();
        let backend_url = backend_url.clone();
        Callback::from(move |_| {
            let typed = secret.trim().to_string();
            if typed.is_empty() {
                message.set("Enter the instance owner secret first.".to_string());
                is_error.set(true);
                return;
            }
            let secret = secret.clone();
            let busy = busy.clone();
            let message = message.clone();
            let is_error = is_error.clone();
            let reload = reload.clone();
            let backend_url = backend_url.clone();
            spawn_local(async move {
                busy.set(true);
                message.set(String::new());
                is_error.set(false);
                let body = serde_json::json!({ "secret": typed }).to_string();
                let result = api_post(&format!("{backend_url}/session/enroll"))
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await;
                match result {
                    Ok(resp) if resp.ok() => {
                        let adopted = resp
                            .json::<serde_json::Value>()
                            .await
                            .ok()
                            .and_then(|v| v.get("adopted").and_then(serde_json::Value::as_i64))
                            .unwrap_or(0);
                        // The secret must not stay in the DOM once it worked.
                        secret.set(String::new());
                        message.set(match adopted {
                            0 => "Claimed. This instance was already yours.".to_string(),
                            1 => "Claimed: 1 account is now linked to this browser.".to_string(),
                            n => {
                                format!("Claimed: {n} accounts are now linked to this browser.")
                            }
                        });
                        dispatch_account_list_changed();
                        reload.set(reload.wrapping_add(1));
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

    let loaded = (*state).clone();
    let has_state = loaded.is_some();
    let locked = loaded.as_ref().is_some_and(|s| s.instance_locked);
    let summary = loaded
        .as_ref()
        .map(|s| accounts_summary(s.owned_accounts, s.adoptable_accounts))
        .unwrap_or_default();
    let status_class = if *is_error {
        "alert-error"
    } else {
        "alert-success"
    };

    html! {
        <div id="settings-instance" class="card bg-base-100 shadow">
            <div class="card-body text-base-content">
                <h2 class="card-title text-xl">{ "Instance ownership" }</h2>
                <p class="text-sm text-base-content/70">
                    { "This app has no account login: the cookie in this browser is the identity, and a new one owns nothing." }
                </p>
                if !message.is_empty() {
                    <div role="alert" class={format!("alert {status_class} text-sm py-2")}>
                        { (*message).clone() }
                    </div>
                }
                if !load_error.is_empty() {
                    <div role="alert" class="alert alert-error text-sm py-2">
                        { (*load_error).clone() }
                    </div>
                }
                if has_state {
                    <p class="text-sm">{ summary }</p>
                    if locked {
                        <p class="text-sm text-base-content/70">
                            { "This instance is claimed by its owner. If that is you, prove it with the instance secret: the accounts that belong to nobody are then linked to this browser." }
                        </p>
                        <div class="flex items-center gap-2">
                            <input type="password" class="input input-sm w-full"
                                placeholder="Instance owner secret…"
                                value={(*secret).clone()}
                                oninput={let secret = secret.clone(); Callback::from(move |e: InputEvent| {
                                    secret.set(e.target_unchecked_into::<HtmlInputElement>().value());
                                })}
                            />
                            <button class="btn btn-sm btn-primary shrink-0" disabled={*busy} onclick={claim}>
                                { if *busy { "Claiming…" } else { "Claim instance" } }
                            </button>
                        </div>
                        <p class="text-xs text-base-content/60">
                            { "The [instance].owner_secret from config.toml. Accounts already linked to another browser are never taken." }
                        </p>
                    } else {
                        <p class="text-sm text-base-content/70">
                            { "This instance is open: it has no owner secret, so the first browser to open it claims everything that belongs to nobody. Set [instance].owner_secret to require proof before claiming." }
                        </p>
                    }
                } else if load_error.is_empty() {
                    <div class="text-sm text-base-content/60">{ "Loading…" }</div>
                }
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_counts_accounts_in_every_plural_form() {
        assert_eq!(accounts_summary(0, 0), "This browser owns no accounts yet.");
        assert_eq!(
            accounts_summary(1, 1),
            "This browser owns 1 account. 1 account belongs to nobody."
        );
        assert_eq!(
            accounts_summary(2, 900),
            "This browser owns 2 accounts. 900 accounts belong to nobody."
        );
    }
}
