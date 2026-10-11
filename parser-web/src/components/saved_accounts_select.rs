use std::cell::RefCell;

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use yew::{
    Callback, Event, Html, Properties, TargetCast, UseStateHandle, function_component, html,
    use_effect_with, use_state,
};
use yew_router::prelude::use_navigator;

use crate::Route;
use crate::models::{ACCOUNT_LIST_CHANGED_EVENT, api_get, read_config_from_head};
use crate::pages::UserInfo;

const CREATE_NEW_OPTION_VALUE: &str = "__create_new__";
const SELECTED_ACCOUNT_KEY: &str = "selected_account_id";

thread_local! {
    /// Last successful `/accounts` response. Every page mounts its own
    /// [`SavedAccountsSelect`] and keeps its own `selected_user`, so without a
    /// cache each SPA route change re-fetched the identical list (10 requests
    /// over 9 routes in the 2026-09-22 E2E run) and the page started with no
    /// account until that request came back.
    static ACCOUNTS_CACHE: RefCell<Option<Vec<UserInfo>>> = const { RefCell::new(None) };
}

fn cached_accounts() -> Option<Vec<UserInfo>> {
    ACCOUNTS_CACHE.with(|c| c.borrow().clone())
}

fn store_accounts(accounts: &[UserInfo]) {
    ACCOUNTS_CACHE.with(|c| *c.borrow_mut() = Some(accounts.to_vec()));
}

fn persisted_account_id() -> Option<i64> {
    web_sys::window()?
        .local_storage()
        .ok()??
        .get_item(SELECTED_ACCOUNT_KEY)
        .ok()??
        .parse()
        .ok()
}

/// Carry the persisted account choice into this page's own `selected_user`,
/// unless the page already has one or the account is gone from the list.
fn restore_persisted_selection(
    accounts: &[UserInfo],
    found_user: &UseStateHandle<Option<UserInfo>>,
    user_query: &UseStateHandle<String>,
) {
    if found_user.is_some() {
        return;
    }
    let Some(id) = persisted_account_id() else {
        return;
    };
    let Some(acc) = accounts.iter().find(|a| a.id == id) else {
        return;
    };
    found_user.set(Some(UserInfo {
        id: acc.id,
        name: acc.name.clone(),
        blacklist: acc.blacklist.clone(),
    }));
    user_query.set(acc.name.clone());
}

#[derive(Properties, PartialEq)]
pub struct SavedAccountsProps {
    pub selected_user: UseStateHandle<Option<UserInfo>>,
    pub is_loading: UseStateHandle<bool>,
}

#[function_component(SavedAccountsSelect)]
pub fn saved_accounts_select(props: &SavedAccountsProps) -> Html {
    let user_query: UseStateHandle<String> = use_state(|| "".to_string());
    // Seeded from the cache so a route change paints the list (and the restored
    // selection) without waiting for the network.
    let saved_accounts = use_state(|| cached_accounts().unwrap_or_default());
    let navigator = use_navigator();

    {
        // Refetch on mount when the cache is cold, and whenever any other
        // component dispatches the `ACCOUNT_LIST_CHANGED_EVENT` window event
        // (account created, removed, or blacklist edited)
        let saved_accounts = saved_accounts.clone();
        let found_user = props.selected_user.clone();
        let user_query = user_query.clone();
        use_effect_with((), move |_| {
            let fetch = {
                let saved_accounts = saved_accounts.clone();
                let found_user = found_user.clone();
                let user_query = user_query.clone();
                move || {
                    let saved_accounts = saved_accounts.clone();
                    let found_user = found_user.clone();
                    let user_query = user_query.clone();
                    if let Some(cfg) = read_config_from_head() {
                        wasm_bindgen_futures::spawn_local(async move {
                            let url = format!("{}/accounts", cfg.backend_domain);
                            match api_get(&url).send().await {
                                Ok(response) if response.ok() => {
                                    if let Ok(accounts) = response.json::<Vec<UserInfo>>().await {
                                        store_accounts(&accounts);
                                        saved_accounts.set(accounts.clone());
                                        restore_persisted_selection(
                                            &accounts,
                                            &found_user,
                                            &user_query,
                                        );
                                    }
                                    // A failed refetch says nothing about the
                                    // list, so the last good value stays on
                                    // screen instead of blanking the select.
                                }
                                _ => {}
                            }
                        });
                    }
                }
            };

            match cached_accounts() {
                Some(accounts) => {
                    saved_accounts.set(accounts.clone());
                    restore_persisted_selection(&accounts, &found_user, &user_query);
                }
                None => fetch(),
            }

            let listener_fetch = fetch.clone();
            let listener: Closure<dyn FnMut(web_sys::Event)> =
                Closure::new(move |_e: web_sys::Event| listener_fetch());
            if let Some(window) = web_sys::window() {
                let _ = window.add_event_listener_with_callback(
                    ACCOUNT_LIST_CHANGED_EVENT,
                    listener.as_ref().unchecked_ref(),
                );
            }
            move || {
                if let Some(window) = web_sys::window() {
                    let _ = window.remove_event_listener_with_callback(
                        ACCOUNT_LIST_CHANGED_EVENT,
                        listener.as_ref().unchecked_ref(),
                    );
                }
                drop(listener);
            }
        });
    }

    let on_select = {
        let saved_accounts = saved_accounts.clone();
        let found_user = props.selected_user.clone();
        let user_query = user_query.clone();
        let navigator = navigator.clone();

        Callback::from(move |e: Event| {
            let select: web_sys::HtmlSelectElement = e.target_unchecked_into();
            let value = select.value();

            if value == CREATE_NEW_OPTION_VALUE {
                select.set_selected_index(0);
                if let Some(nav) = navigator.as_ref() {
                    nav.push(&Route::Account);
                }
                return;
            }

            let idx = select.selected_index() as usize;
            if idx == 0 {
                return;
            }
            if let Some(account) = saved_accounts.get(idx - 1) {
                found_user.set(Some(UserInfo {
                    id: account.id,
                    name: account.name.clone(),
                    blacklist: account.blacklist.clone(),
                }));
                user_query.set(account.name.clone());
                // Persist selection so SPA navigation doesn't lose it
                if let Some(win) = web_sys::window()
                    && let Ok(Some(storage)) = win.local_storage()
                {
                    let _ = storage.set_item(SELECTED_ACCOUNT_KEY, &account.id.to_string());
                }
            }
        })
    };

    let on_clear = {
        let found_user = props.selected_user.clone();
        let user_query = user_query.clone();

        Callback::from(move |_| {
            found_user.set(None);
            user_query.set(String::new());
            if let Some(win) = web_sys::window()
                && let Ok(Some(storage)) = win.local_storage()
            {
                let _ = storage.remove_item(SELECTED_ACCOUNT_KEY);
            }
        })
    };

    html! {
        <div class="mb-4">
            <label for="saved-accounts-select" class="mb-1 block text-sm font-medium">
                <span class="text-base-content">{"Select Saved Account"}</span>
            </label>
            <div class="join w-full">
                <select
                    id="saved-accounts-select"
                    class="select join-item flex-1"
                    onchange={on_select.clone()}
                    disabled={*props.is_loading}
                >
                    <option value="" selected={props.selected_user.is_none()}>
                        {"-- Select Account --"}
                    </option>
                    {for saved_accounts.iter().map(|acc| {
                        // Without this the control rendered "-- Select Account --"
                        // even when the page had already restored an account.
                        let is_selected =
                            props.selected_user.as_ref().is_some_and(|u| u.id == acc.id);
                        html! {
                            <option value={acc.id.to_string()} selected={is_selected}>
                                {format!("{} (ID: {})", acc.name, acc.id)}
                            </option>
                        }
                    })}
                    <option value={CREATE_NEW_OPTION_VALUE}>
                        {"+ Create new account…"}
                    </option>
                </select>
                <button
                    class="btn btn-outline join-item"
                    type="button"
                    onclick={on_clear.clone()}
                    disabled={*props.is_loading}
                >
                    {"Clear"}
                </button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(id: i64, name: &str) -> UserInfo {
        UserInfo {
            id,
            name: name.to_string(),
            blacklist: String::new(),
        }
    }

    #[test]
    fn accounts_cache_roundtrip_replaces_the_previous_list() {
        store_accounts(&[acc(1, "one")]);
        assert_eq!(cached_accounts().unwrap(), vec![acc(1, "one")]);
        // A refetch that returns a different list must replace, not append.
        store_accounts(&[acc(2, "two"), acc(3, "three")]);
        let cached = cached_accounts().unwrap();
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].id, 2);
    }
}
