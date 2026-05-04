use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use yew::{
    Callback, Event, Html, Properties, TargetCast, UseStateHandle, function_component, html,
    use_effect_with, use_state,
};

use crate::models::{ACCOUNT_LIST_CHANGED_EVENT, api_get, read_config_from_head};
use crate::pages::UserInfo;

#[derive(Properties, PartialEq)]
pub struct SavedAccountsProps {
    pub selected_user: UseStateHandle<Option<UserInfo>>,
    pub is_loading: UseStateHandle<bool>,
}

#[function_component(SavedAccountsSelect)]
pub fn saved_accounts_select(props: &SavedAccountsProps) -> Html {
    let user_query: UseStateHandle<String> = use_state(|| "".to_string());
    let saved_accounts = use_state(Vec::<UserInfo>::new);

    {
        // Refetch on mount and whenever any other component dispatches the
        // `ACCOUNT_LIST_CHANGED_EVENT` window event (account created, removed,
        // or blacklist edited)
        let saved_accounts = saved_accounts.clone();
        use_effect_with((), move |_| {
            let saved_accounts_for_fetch = saved_accounts.clone();
            let fetch = move || {
                let saved_accounts = saved_accounts_for_fetch.clone();
                if let Some(cfg) = read_config_from_head() {
                    wasm_bindgen_futures::spawn_local(async move {
                        let url = format!("{}/accounts", cfg.backend_domain);
                        match api_get(&url).send().await {
                            Ok(response) if response.ok() => {
                                match response.json::<Vec<UserInfo>>().await {
                                    Ok(accounts) => saved_accounts.set(accounts),
                                    Err(_) => saved_accounts.set(Vec::new()),
                                }
                            }
                            _ => saved_accounts.set(Vec::new()),
                        }
                    });
                }
            };
            fetch();

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

        Callback::from(move |e: Event| {
            let select: web_sys::HtmlSelectElement = e.target_unchecked_into();
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
            }
        })
    };

    let on_clear = {
        let found_user = props.selected_user.clone();
        let user_query = user_query.clone();

        Callback::from(move |_| {
            found_user.set(None);
            user_query.set(String::new());
        })
    };

    html! {
        <div class="mb-4">
            <label class="form-label" for="saved-accounts-select">{"Select Saved Account"}</label>
            <div class="input-group">
                <select
                    id="saved-accounts-select"
                    class="form-select"
                    onchange={on_select.clone()}
                    disabled={*props.is_loading}
                >
                    <option value="" selected={props.selected_user.is_none()}>
                        {"-- Select Account --"}
                    </option>
                    {for saved_accounts.iter().map(|acc| {
                        html! {
                            <option value={acc.id.to_string()}>
                                {format!("{} (ID: {})", acc.name, acc.id)}
                            </option>
                        }
                    })}
                </select>
                <button
                    class="btn btn-outline-secondary"
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
