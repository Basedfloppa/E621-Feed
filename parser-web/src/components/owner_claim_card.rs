//! "Owner claim" card for the settings page.
//!
//! The administration tier is neither the instance claim nor an account link: it
//! is `[runtime].admin_user` linked with `[runtime].admin_api`, and it is what
//! opens the console and the local catalog. The server writes the grant only when
//! the key matches, so this card is a proof, not a permission toggle — nothing
//! here can hand anybody the tier, it can only demonstrate the key.
//!
//! It is offered while nobody operates the instance under the current key, and it
//! is the *only* place the claim is advertised. The form always asks for the
//! account as well as the key: `/session/state` deliberately does not report
//! which account `[runtime].admin_user` names, so a browser that has not proved
//! the key is never told which account holds the machine.
//!
//! A successful claim dispatches `ACCOUNT_LIST_CHANGED_EVENT`, which is what the
//! shared ownership hook listens on — so the nav entries appear and this card
//! takes itself off the page without a reload.

use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlInputElement, SubmitEvent};
use yew::prelude::*;

use crate::models::{
    api_post, can_offer_owner_claim, dispatch_account_list_changed, humanize_error_body,
    humanize_network_error, use_ownership,
};

#[derive(Clone, PartialEq, Properties)]
pub struct OwnerClaimCardProps {
    pub backend_url: String,
}

/// What the card tells the operator it is about to do.
///
/// A free function rather than inline markup so it can be pinned by a test that
/// needs no DOM.
fn claim_body() -> &'static str {
    "Give the e621 account named in [runtime].admin_user together with its numeric id, and prove \
     the [runtime].admin_api key from config.toml. That links the account to this browser and \
     opens the console and the local catalog — on this browser and on no other."
}

/// What the server said after a successful claim.
fn claimed_message(name: &str) -> String {
    format!("Claimed: this browser now operates the instance as {name}.")
}

#[function_component(OwnerClaimCard)]
pub fn owner_claim_card(props: &OwnerClaimCardProps) -> Html {
    let ownership = use_ownership(&props.backend_url);

    let id = use_state(String::new);
    let name = use_state(String::new);
    let key = use_state(String::new);
    let busy = use_state(|| false);
    let message = use_state(String::new);
    let is_error = use_state(|| false);

    // "Could not read the state" is not "the tier is open": `can_offer_owner_claim`
    // answers false for an unread state, so the form never renders against a 403.
    if !can_offer_owner_claim(&ownership) {
        return Html::default();
    }

    let body = claim_body();

    let onsubmit = {
        let backend_url = props.backend_url.clone();
        let id_input = id.clone();
        let name_input = name.clone();
        let key_input = key.clone();
        let busy = busy.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            if *busy {
                return;
            }

            let account_id = id_input.trim().parse::<i64>().ok();
            let account_name = name_input.trim().to_string();
            let typed_key = key_input.trim().to_string();

            let complain = |m: &str| {
                message.set(m.to_string());
                is_error.set(true);
            };
            if account_name.is_empty() {
                complain("Enter the e621 username of the administration account.");
                return;
            }
            if account_id.is_none() {
                complain("Enter the numeric e621 account ID of the administration account.");
                return;
            }
            if typed_key.is_empty() {
                complain("Enter the [runtime].admin_api key from config.toml.");
                return;
            }

            let backend_url = backend_url.clone();
            let key_input = key_input.clone();
            let busy = busy.clone();
            let message = message.clone();
            let is_error = is_error.clone();
            let claimed_name = account_name.clone();
            spawn_local(async move {
                busy.set(true);
                message.set(String::new());
                is_error.set(false);

                let payload = serde_json::json!({
                    "id": account_id,
                    "name": account_name,
                    "api_key": typed_key,
                });
                let result = api_post(&format!("{backend_url}/account"))
                    .header("Content-Type", "application/json")
                    .body(payload.to_string())
                    .send()
                    .await;

                match result {
                    Ok(resp) if resp.ok() => {
                        // The key must not stay in the DOM once it worked.
                        key_input.set(String::new());
                        message.set(claimed_message(&claimed_name));
                        // Re-reads `/session/state`, which is what puts the
                        // console and catalog entries in the menu and takes this
                        // card off the page.
                        dispatch_account_list_changed();
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

    let status_class = if *is_error {
        "alert-error"
    } else {
        "alert-success"
    };

    html! {
        <div id="settings-owner-claim" class="card bg-base-100 shadow">
            <div class="card-body text-base-content">
                <h2 class="card-title text-xl">{ "Owner claim" }</h2>
                <p class="text-sm text-base-content/70">{ body }</p>
                <p class="text-sm text-base-content/70 mb-1">
                    { "This is not the instance claim: owning the instance widens what this browser \
                       reads, and it does not open the console or the catalog. Only the \
                       [runtime].admin_api key does." }
                </p>
                <form onsubmit={onsubmit}>
                    <fieldset class="fieldset w-full mb-2">
                        <legend class="fieldset-legend">{ "Account ID" }</legend>
                        <input
                            type="text"
                            inputmode="numeric"
                            pattern="[0-9]+"
                            maxlength="9"
                            class="input w-full box-border"
                            id="owner-claim-account-id"
                            value={(*id).clone()}
                            oninput={let id = id.clone(); Callback::from(move |e: InputEvent| {
                                id.set(e.target_unchecked_into::<HtmlInputElement>().value());
                            })}
                            placeholder="Numeric e621 account ID"
                            disabled={*busy}
                        />
                    </fieldset>
                    <fieldset class="fieldset w-full mb-2">
                        <legend class="fieldset-legend">{ "Username" }</legend>
                        <input
                            type="text"
                            class="input w-full box-border"
                            id="owner-claim-account-name"
                            value={(*name).clone()}
                            oninput={let name = name.clone(); Callback::from(move |e: InputEvent| {
                                name.set(e.target_unchecked_into::<HtmlInputElement>().value());
                            })}
                            placeholder="e621 username"
                            disabled={*busy}
                        />
                    </fieldset>
                    <fieldset class="fieldset w-full mb-3">
                        <legend class="fieldset-legend">{ "[runtime].admin_api" }</legend>
                        <input
                            type="password"
                            class="input w-full box-border"
                            id="owner-claim-api-key"
                            value={(*key).clone()}
                            oninput={let key = key.clone(); Callback::from(move |e: InputEvent| {
                                key.set(e.target_unchecked_into::<HtmlInputElement>().value());
                            })}
                            placeholder="Paste the key from config.toml"
                            autocomplete="off"
                            spellcheck={false}
                            disabled={*busy}
                        />
                    </fieldset>
                    <button type="submit" class="btn btn-primary w-full" disabled={*busy}>
                        { if *busy { "Claiming…" } else { "Claim owner" } }
                    </button>
                    <div class={format!("alert {status_class} text-sm py-2 mt-3")} role="alert"
                         hidden={message.is_empty()}>
                        { (*message).clone() }
                    </div>
                </form>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim must not be confused with the instance claim — that confusion is
    /// what the tier was split out to end, and it would send the reader to
    /// `owner_secret`, which does not open the console.
    #[test]
    fn the_body_does_not_promise_that_the_instance_claim_opens_anything() {
        let body = claim_body();
        assert!(
            !body.contains("owner_secret"),
            "the card must not point at the instance secret: {body}"
        );
        assert!(
            body.contains("[runtime].admin_api") && body.contains("[runtime].admin_user"),
            "the credential and the account to prove it for must both be named: {body}"
        );
    }

    /// The server no longer reports which account `[runtime].admin_user` names,
    /// so the body must not invent one: that disclosure is the whole reason the
    /// field was removed from `/session/state`.
    #[test]
    fn the_body_names_the_configuration_not_a_specific_account() {
        let body = claim_body();
        assert!(
            !body.contains("zorolin") && !body.contains("ID 6"),
            "no account may be disclosed before the key is proved: {body}"
        );
    }

    #[test]
    fn the_success_message_names_the_account_that_was_claimed() {
        assert!(claimed_message("zorolin").contains("zorolin"));
    }
}
