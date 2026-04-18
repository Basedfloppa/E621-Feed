use yew::{
    Callback, Event, Html, Properties, TargetCast, UseStateHandle, function_component, html,
    use_effect_with, use_state,
};

use crate::models::{get_or_create_owner_token, read_config_from_head};
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
