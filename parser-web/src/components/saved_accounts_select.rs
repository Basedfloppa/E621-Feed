use yew::{
    Callback, Event, Html, MouseEvent, Properties, TargetCast, UseStateHandle, function_component,
    html, use_effect_with, use_state,
};

use crate::models::{get_or_create_owner_token, humanize_error_body, read_config_from_head};
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
    let remove_error: UseStateHandle<Option<String>> = use_state(|| None);

    {
        let saved_accounts = saved_accounts.clone();
        use_effect_with((), move |_| {
            if let (Some(cfg), Some(owner_token)) = (read_config_from_head(), get_or_create_owner_token()) {
                wasm_bindgen_futures::spawn_local(async move {
                    let url = format!("{}/accounts?owner_token={}", cfg.backend_domain, urlencoding::encode(&owner_token));
                    match reqwasm::http::Request::get(&url).send().await {
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
            || ()
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

    // Remove the currently-selected saved account from the device.
    let on_remove = {
        let saved_accounts = saved_accounts.clone();
        let selected = props.selected_user.clone();
        let user_query = user_query.clone();
        let remove_error = remove_error.clone();
        Callback::from(move |_e: MouseEvent| {
            let Some(account) = (*selected).clone() else {
                return;
            };
            let confirmed = web_sys::window()
                .and_then(|w| {
                    w.confirm_with_message(&format!(
                        "Remove '{}' (ID {}) from this device's saved accounts? \
                         The account itself isn't deleted on e621.",
                        account.name, account.id
                    ))
                    .ok()
                })
                .unwrap_or(false);
            if !confirmed {
                return;
            }

            let Some(cfg) = read_config_from_head() else {
                remove_error.set(Some(
                    "App configuration failed to load — please reload the page.".to_string(),
                ));
                return;
            };
            let Some(owner_token) = get_or_create_owner_token() else {
                remove_error.set(Some("Missing device token".to_string()));
                return;
            };
            let url = format!(
                "{}/account/{}?owner_token={}",
                cfg.backend_domain,
                account.id,
                urlencoding::encode(&owner_token)
            );

            let saved_accounts = saved_accounts.clone();
            let selected = selected.clone();
            let user_query = user_query.clone();
            let remove_error = remove_error.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match reqwasm::http::Request::delete(&url).send().await {
                    Ok(resp) if resp.ok() => {
                        let new_list: Vec<UserInfo> = saved_accounts
                            .iter()
                            .filter(|u| u.id != account.id)
                            .cloned()
                            .collect();
                        saved_accounts.set(new_list);
                        selected.set(None);
                        user_query.set(String::new());
                        remove_error.set(None);
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

    html! {
        <div class="mb-4">
            <label class="form-label">{"Select Saved Account"}</label>
            <div class="input-group">
                <select
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
                    class="btn btn-outline-danger"
                    type="button"
                    onclick={on_remove.clone()}
                    disabled={*props.is_loading || props.selected_user.is_none()}
                    title="Unlink the selected account from this device"
                >
                    {"Remove"}
                </button>
                <button
                    class="btn btn-outline-secondary"
                    type="button"
                    onclick={on_clear.clone()}
                    disabled={*props.is_loading}
                >
                    {"Clear"}
                </button>
            </div>
            {
                if let Some(err) = &*remove_error {
                    html! {
                        <div class="alert alert-danger mt-2 mb-0 py-2 px-3" role="alert">
                            { err }
                        </div>
                    }
                } else { html!{} }
            }
        </div>
    }
}
