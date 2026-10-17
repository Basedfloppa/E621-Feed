//! What this browser may do on the instance, and what it may read.
//!
//! There is no login here: the `HttpOnly` owner cookie *is* the identity, and a
//! fresh browser starts with one that owns nothing. Two independent answers come
//! back from `GET /session/state`, and keeping them apart is the point of this
//! module:
//!
//! * `is_owner` — this browser holds the instance claim, so it reads every
//!   account on the instance. The claim is a row about the instance, not a set
//!   of account links: claiming writes no links, and `[instance].owner_secret`
//!   moves it between browsers.
//! * `is_admin` — this browser linked `[runtime].admin_user` with a key matching
//!   `[runtime].admin_api`. Only this opens the operator surfaces: the local
//!   catalog (a view over the saved posts held on this instance) and the console
//!   (the media cache, the job registry, the bucket pins).
//!
//! The server gates them on the same rules (`auth::require_instance_admin` for
//! both, `db::VISIBLE_ACCOUNTS_SQL` for what an owner reads), so this module is
//! what keeps the UI from advertising a door that only answers 403.
//!
//! `GET /session/state` is the same endpoint the settings card reads, so the
//! contract lives here once. The listener on `ACCOUNT_LIST_CHANGED_EVENT`
//! re-reads it after a link, unlink or claim.

use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::Event;
use yew::prelude::*;

use super::api_client::api_get;
use super::events::ACCOUNT_LIST_CHANGED_EVENT;

/// `GET /session/state`.
#[derive(Clone, Deserialize, PartialEq)]
pub struct OwnershipState {
    /// `[instance].owner_secret` is set, so claiming requires proof.
    pub instance_locked: bool,
    /// Somebody holds the claim.
    pub instance_claimed: bool,
    /// This browser holds it: it reads every account on the instance.
    pub is_owner: bool,
    /// This browser linked `[runtime].admin_user` with a key matching
    /// `[runtime].admin_api`, so the console and the local catalog are its to
    /// open. Not implied by [`Self::is_owner`] — the claim widens *reads*, and
    /// operating the instance is a separate, narrower credential.
    pub is_admin: bool,
    /// Nobody has been made an operator *under the current key*. False once an
    /// operator exists; true again after `admin_api` is rotated.
    pub admin_available: bool,
    /// Accounts this browser can address — every account on the instance when it
    /// is the owner, only the linked ones otherwise.
    pub owned_accounts: i64,
    /// Accounts on the instance: what holding the claim covers.
    pub instance_accounts: i64,
}

/// Does this browser operate the instance?
///
/// The single predicate behind the operator surfaces — the local catalog and the
/// console. The server gates both on the same rule
/// (`auth::require_instance_admin`), so this only keeps the UI from advertising
/// a door that answers 403.
///
/// There is no matching helper for `is_owner`: the claim no longer gates a
/// surface of its own, only the *scope* the server already enforces, so nothing
/// in the SPA needs to ask. The settings card reads the field directly to say who
/// holds the claim.
pub fn is_admin(state: &Option<OwnershipState>) -> bool {
    state.as_ref().is_some_and(|s| s.is_admin)
}

/// Is the local catalog this browser's to open?
///
/// One definition shared with [`catalog_entry_visible`], because a screen and the
/// entry that leads to it must never disagree.
pub fn operates_catalog(state: &OwnershipState) -> bool {
    state.is_admin
}

/// Whether the Catalog nav entry belongs in the menu.
pub fn catalog_entry_visible(state: &Option<OwnershipState>) -> bool {
    is_admin(state)
}

/// Whether the operator console (`/admin`) belongs in the menu.
///
/// Same rule as the catalog, and for the same reason: both act on the machine
/// rather than on one account, so a browser that merely linked an account — or
/// that only holds the instance claim — has no business seeing either entry.
pub fn admin_entry_visible(state: &Option<OwnershipState>) -> bool {
    is_admin(state)
}

/// Is the owner-claim card on the settings page worth rendering?
///
/// Only while nobody operates the instance under the current key: once an
/// operator exists, `POST /api/account` for `[runtime].admin_user` would be
/// refused anyway, so the form must not be dangled. A rotated `admin_api` makes
/// this true again — which is the whole reason the tier is "available" rather
/// than "taken" for good.
///
/// This is the settings-page half of the same predicate the server's refusal is
/// written against, so the card can never be offered for a link the server would
/// reject.
pub fn can_offer_owner_claim(state: &Option<OwnershipState>) -> bool {
    state
        .as_ref()
        .is_some_and(|s| s.admin_available && !s.is_admin)
}

/// Read the state once. `None` means "could not tell", never "owns nothing".
pub async fn fetch_ownership(backend_url: &str) -> Option<OwnershipState> {
    let resp = api_get(&format!("{backend_url}/session/state"))
        .send()
        .await
        .ok()?;
    if !resp.ok() {
        return None;
    }
    resp.json::<OwnershipState>().await.ok()
}

/// Live ownership state: `None` until the first read lands.
#[hook]
pub fn use_ownership(backend_url: &str) -> UseStateHandle<Option<OwnershipState>> {
    let state = use_state(|| None::<OwnershipState>);
    let backend = backend_url.to_string();
    {
        let state = state.clone();
        use_effect_with(backend, move |backend| {
            // Cloned per caller only to make the closure `Fn`: the effect body
            // calls it immediately and the event listener calls it later.
            let fetch = {
                let state = state.clone();
                let backend = backend.clone();
                move || {
                    let state = state.clone();
                    let backend = backend.clone();
                    spawn_local(async move {
                        if let Some(s) = fetch_ownership(&backend).await {
                            state.set(Some(s));
                        }
                    });
                }
            };
            fetch();

            let on_change = fetch.clone();
            let listener = Closure::<dyn FnMut(Event)>::new(move |_: Event| on_change());
            let win = web_sys::window();
            if let Some(w) = win.as_ref() {
                let _ = w.add_event_listener_with_callback(
                    ACCOUNT_LIST_CHANGED_EVENT,
                    listener.as_ref().unchecked_ref(),
                );
            }
            move || {
                if let Some(w) = win.as_ref() {
                    let _ = w.remove_event_listener_with_callback(
                        ACCOUNT_LIST_CHANGED_EVENT,
                        listener.as_ref().unchecked_ref(),
                    );
                }
            }
        });
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(is_owner: bool, owned: i64, instance: i64) -> OwnershipState {
        OwnershipState {
            instance_locked: true,
            instance_claimed: true,
            is_owner,
            is_admin: false,
            admin_available: false,
            owned_accounts: owned,
            instance_accounts: instance,
        }
    }

    /// A browser with the administration grant and no claim on the instance —
    /// the case the split exists for.
    fn admin_state(is_admin: bool, admin_available: bool) -> OwnershipState {
        OwnershipState {
            is_admin,
            admin_available,
            ..state(false, 1, 1)
        }
    }

    /// Owning the instance is not operating it. This is the whole point of the
    /// split: `owner_secret` exists to *recover* access after clearing cookies,
    /// so it must not also be the key to the console.
    #[test]
    fn owning_the_instance_does_not_open_the_operator_surfaces() {
        let owner = state(true, 900, 900);
        assert!(owner.is_owner, "the claim is what widens reads");
        assert!(
            !operates_catalog(&owner),
            "the claim widens reads, not control"
        );
        assert!(!catalog_entry_visible(&Some(owner.clone())));
        assert!(!admin_entry_visible(&Some(owner)));
    }

    /// Linking every account in a library opens nothing either — the operator is
    /// the configured account, not whoever linked the most.
    #[test]
    fn linking_accounts_does_not_open_the_operator_surfaces() {
        let many_links = state(false, 900, 900);
        assert!(!operates_catalog(&many_links));
        assert!(!catalog_entry_visible(&Some(many_links.clone())));
        assert!(!admin_entry_visible(&Some(many_links)));
    }

    /// The operator sees both entries, whether or not it also holds the claim.
    #[test]
    fn the_administration_grant_opens_both_entries() {
        for candidate in [
            admin_state(true, false),
            OwnershipState {
                is_owner: true,
                ..admin_state(true, false)
            },
        ] {
            assert!(operates_catalog(&candidate));
            assert!(catalog_entry_visible(&Some(candidate.clone())));
            assert!(admin_entry_visible(&Some(candidate)));
        }
    }

    /// An unread state hides the entries: "could not tell" is not "may operate".
    #[test]
    fn an_unread_state_hides_both_entries() {
        assert!(!catalog_entry_visible(&None));
        assert!(!admin_entry_visible(&None));
        assert!(!can_offer_owner_claim(&None));
    }

    /// The console is operator-only for the same reason the catalog is, so the
    /// two entries must never disagree — a menu that offers one and hides the
    /// other would be the bug this pins.
    #[test]
    fn the_admin_entry_follows_the_same_rule_as_the_catalog() {
        for candidate in [
            None,
            Some(state(false, 0, 0)),
            Some(state(false, 900, 900)),
            Some(state(true, 0, 900)),
            Some(state(true, 37, 37)),
            Some(admin_state(true, false)),
            Some(admin_state(false, true)),
        ] {
            assert_eq!(
                admin_entry_visible(&candidate),
                catalog_entry_visible(&candidate),
                "the two operator entries must agree"
            );
        }
        assert!(admin_entry_visible(&Some(admin_state(true, false))));
    }

    /// The owner-claim card is offered only to the browser that could still use
    /// it: not once somebody operates the instance, and not to the operator
    /// itself. A rotated key makes the first case true again.
    #[test]
    fn the_owner_claim_is_offered_only_while_the_tier_is_open() {
        assert!(
            can_offer_owner_claim(&Some(admin_state(false, true))),
            "nobody operates the instance yet, so claiming it is worth offering"
        );
        assert!(
            !can_offer_owner_claim(&Some(admin_state(true, false))),
            "the operator must not be offered the claim again"
        );
        assert!(
            !can_offer_owner_claim(&Some(admin_state(false, false))),
            "once the tier is taken, the claim would be refused anyway"
        );
    }

    /// The card is offered on the tier being open alone. There is no account to
    /// know about, because the server does not report one — which is what keeps
    /// the form identical on a fresh install and on a populated instance, and
    /// what stops the state from disclosing the administration account.
    #[test]
    fn the_claim_is_offered_without_knowing_which_account_it_is_about() {
        assert!(
            can_offer_owner_claim(&Some(admin_state(false, true))),
            "an open tier is the whole condition"
        );
    }
}
