//! "Instance ownership" card for the settings page.
//!
//! There is no account login in this app: the `HttpOnly` owner cookie *is* the
//! identity, and a fresh browser starts with one that owns nothing. Ownership is
//! a claim on the instance — it links no accounts — so this card is where the
//! operator's `[instance].owner_secret` is turned into that claim. It is also
//! the recovery path: proving the secret moves an existing claim to this browser,
//! which is what makes clearing cookies harmless.

use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::models::{
    OwnershipState, api_get, api_post, dispatch_account_list_changed, humanize_error_body,
    humanize_network_error,
};

#[derive(Clone, PartialEq, Properties)]
pub struct InstanceCardProps {
    pub backend_url: String,
}

/// What this browser holds and what the instance is.
///
/// A free function rather than inline markup so the wording can be pinned by a
/// test that needs no DOM.
fn ownership_summary(state: &OwnershipState) -> String {
    let accounts = match state.instance_accounts {
        0 => "no accounts".to_string(),
        1 => "1 account".to_string(),
        n => format!("{n} accounts"),
    };
    if state.is_owner {
        return format!("This browser owns this instance, so it can address all {accounts} on it.");
    }
    if state.instance_claimed {
        return format!(
            "This instance is owned by another browser and holds {accounts}. Proving the secret \
             below moves the ownership here."
        );
    }
    format!("This instance is unclaimed and holds {accounts}.")
}

/// What the server said a successful `POST /session/enroll` did.
///
/// The three cases are genuinely different outcomes for the operator, so they do
/// not collapse into one "done": `transferred` in particular means another
/// browser just lost the claim.
fn claim_message(claim: &str) -> &'static str {
    match claim {
        "transferred" => {
            "Moved here: this browser is now the instance owner. The browser that held it no \
             longer is."
        }
        "already-owned" => "This instance was already yours.",
        _ => "Claimed: this browser is now the instance owner.",
    }
}

async fn fetch_state(backend_url: &str) -> Result<OwnershipState, String> {
    match api_get(&format!("{backend_url}/session/state"))
        .send()
        .await
    {
        Ok(resp) if resp.ok() => resp
            .json::<OwnershipState>()
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
    let state = use_state(|| None::<OwnershipState>);
    let load_error = use_state(String::new);
    let secret = use_state(String::new);
    let busy = use_state(|| false);
    let message = use_state(String::new);
    let is_error = use_state(|| false);
    // Bumped after a successful claim: ownership changed, so the state the card
    // shows has to be read again.
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
                        let claim = resp
                            .json::<serde_json::Value>()
                            .await
                            .ok()
                            .and_then(|v| {
                                v.get("claim")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string)
                            })
                            .unwrap_or_default();
                        // The secret must not stay in the DOM once it worked.
                        secret.set(String::new());
                        message.set(claim_message(&claim).to_string());
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
    let is_owner = loaded.as_ref().is_some_and(|s| s.is_owner);
    let locked = loaded.as_ref().is_some_and(|s| s.instance_locked);
    let summary = loaded.as_ref().map(ownership_summary).unwrap_or_default();
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
                    { "This app has no account login: the cookie in this browser is the identity. \
                       The claim is about the instance — it links no accounts and it does not open \
                       the console or the catalog, which answer to the administration key below." }
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
                    if is_owner {
                        <p class="text-sm text-base-content/70">
                            { "Nothing to prove: every account on the instance is in scope for this \
                               browser. The console and the local catalog are a separate, narrower \
                               credential — see the owner claim below." }
                        </p>
                    } else if locked {
                        <p class="text-sm text-base-content/70">
                            { "Prove it with the instance secret from config.toml. This writes no \
                               account links." }
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
                            { "The [instance].owner_secret from config.toml. Proving it moves the \
                               claim to this browser — the previous one loses it." }
                        </p>
                    } else {
                        <p class="text-sm text-base-content/70">
                            { "This instance has no owner secret, so the first browser to open it \
                               claims it. Set [instance].owner_secret to require proof before \
                               claiming." }
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

    /// The card only ever renders the *ownership* half of the session state, so
    /// the administration flags are constant here: this card says who holds the
    /// claim, and the console says who operates the instance.
    fn state(is_owner: bool, claimed: bool, accounts: i64) -> OwnershipState {
        OwnershipState {
            instance_locked: true,
            instance_claimed: claimed,
            is_owner,
            is_admin: false,
            admin_available: false,
            owned_accounts: if is_owner { accounts } else { 0 },
            instance_accounts: accounts,
        }
    }

    #[test]
    fn the_summary_names_the_three_states() {
        assert_eq!(
            ownership_summary(&state(true, true, 1)),
            "This browser owns this instance, so it can address all 1 account on it."
        );
        assert!(
            ownership_summary(&state(false, true, 900)).contains("owned by another browser"),
            "a claimed instance this browser does not own has to say so"
        );
        assert!(
            ownership_summary(&state(false, false, 0)).contains("unclaimed"),
            "an unclaimed instance is a different state again"
        );
        assert!(ownership_summary(&state(true, true, 0)).contains("no accounts"));
    }

    /// Transferring is the outcome that changes something for somebody else, so
    /// it must not read like a first claim.
    #[test]
    fn the_claim_message_distinguishes_a_transfer() {
        assert!(claim_message("transferred").contains("no longer is"));
        assert!(claim_message("already-owned").contains("already"));
        assert!(claim_message("claimed").contains("now the instance owner"));
        // Unknown labels fall back to the neutral success wording rather than
        // claiming a transfer that may not have happened.
        assert!(claim_message("").contains("now the instance owner"));
    }
}
