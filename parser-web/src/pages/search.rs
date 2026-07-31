use serde::Deserialize;
use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlInputElement, window};
use yew::prelude::*;

use crate::components::*;
use crate::models::*;
use crate::pages::UserInfo;

#[derive(Debug, Deserialize)]
struct TagResolveResponse {
    canonical: String,
    synonyms: Vec<String>,
}

/// Read a display setting from unified settings_show_* key, falling back
/// to an old per-page key for backward compatibility.
pub fn read_display_setting(suffix: &str, old: &str, default: bool) -> bool {
    let new_key = format!("settings_show_{}", suffix);
    let storage = || window().and_then(|w| w.local_storage().ok().flatten());
    storage()
        .and_then(|s| s.get_item(&new_key).ok().flatten())
        .or_else(|| storage().and_then(|s| s.get_item(old).ok().flatten()))
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(default)
}

/// Server-proxied e621 post search with alias-aware tag suggestions.
#[function_component(SearchPage)]
pub fn search_page() -> Html {
    let _settings_tick = use_settings_tick();
    let selected_user = use_state(|| Option::<UserInfo>::None);
    let accounts_loading = use_state(|| false);
    let input = use_state(String::new);
    let submitted_query = use_state(String::new);
    let suggestions = use_state(Vec::<String>::new);
    let suggestions_loading = use_state(|| false);
    let suggestions_error = use_state(|| Option::<String>::None);

    let backend_url = read_config_from_head()
        .map(|cfg| cfg.backend_domain)
        .unwrap_or_default();

    // Resolve the final token because e621 queries can contain multiple tags
    // and operators. `resolve` also returns aliases as useful completions.
    {
        let input_value = (*input).clone();
        let backend_url = backend_url.clone();
        let suggestions = suggestions.clone();
        let suggestions_loading = suggestions_loading.clone();
        let suggestions_error = suggestions_error.clone();
        use_effect_with(input_value, move |value| {
            let tag = value.split_whitespace().last().unwrap_or("").to_string();
            if tag.is_empty() || backend_url.is_empty() {
                suggestions.set(Vec::new());
                suggestions_error.set(None);
                suggestions_loading.set(false);
            } else {
                suggestions_loading.set(true);
                suggestions_error.set(None);
                let url = format!(
                    "{}/tag/resolve?tag={}",
                    backend_url,
                    urlencoding::encode(&tag)
                );
                let suggestions = suggestions.clone();
                let suggestions_loading = suggestions_loading.clone();
                let suggestions_error = suggestions_error.clone();
                spawn_local(async move {
                    match api_get(&url).send().await {
                        Ok(response) if response.ok() => {
                            match response.json::<TagResolveResponse>().await {
                                Ok(response) => {
                                    let mut values = response.synonyms;
                                    if !values.iter().any(|item| item == &response.canonical) {
                                        values.push(response.canonical);
                                    }
                                    values.sort();
                                    values.dedup();
                                    suggestions.set(values);
                                }
                                Err(_) => suggestions_error.set(Some(
                                    "Tag suggestions could not be read. Try again.".to_string(),
                                )),
                            }
                        }
                        Ok(response) => {
                            let status = response.status();
                            let body = response.text().await.unwrap_or_default();
                            suggestions_error.set(Some(humanize_error_body(status, &body)));
                        }
                        Err(error) => suggestions_error.set(Some(humanize_network_error(error))),
                    }
                    suggestions_loading.set(false);
                });
            }
            || ()
        });
    }

    let on_input = {
        let input = input.clone();
        let suggestions = suggestions.clone();
        Callback::from(move |event: InputEvent| {
            let value = event.target_unchecked_into::<HtmlInputElement>().value();
            let previous = (*input).clone();
            // A datalist returns only its selected value. Reattach it to the
            // existing query so selecting an alias never discards prior tags.
            if suggestions.iter().any(|suggestion| suggestion == &value)
                && previous.split_whitespace().count() > 1
            {
                let prefix = previous
                    .rsplit_once(char::is_whitespace)
                    .map(|(prefix, _)| format!("{prefix} "))
                    .unwrap_or_default();
                input.set(format!("{prefix}{value}"));
            } else {
                input.set(value);
            }
        })
    };
    let on_submit = {
        let input = input.clone();
        let submitted_query = submitted_query.clone();
        Callback::from(move |event: SubmitEvent| {
            event.prevent_default();
            submitted_query.set(input.trim().to_string());
        })
    };

    let score_results = use_state(|| {
        let storage = || window().and_then(|browser| browser.local_storage().ok().flatten());
        storage()
            .and_then(|s| s.get_item("settings_score_results").ok().flatten())
            .or_else(|| storage().and_then(|s| s.get_item("search_score_results").ok().flatten()))
            .and_then(|value| value.parse::<bool>().ok())
            .unwrap_or(false)
    });
    let score_cutoff_pct = use_state(|| {
        let storage = || window().and_then(|browser| browser.local_storage().ok().flatten());
        storage()
            .and_then(|s| s.get_item("settings_score_cutoff_pct").ok().flatten())
            .or_else(|| {
                storage().and_then(|s| s.get_item("search_score_cutoff_pct").ok().flatten())
            })
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(0.0)
            .clamp(0.0, 95.0)
    });
    let fetch_url = selected_user.as_ref().and_then(|user| {
        (!submitted_query.trim().is_empty() && !backend_url.is_empty()).then(|| {
            let endpoint = if *score_results {
                "search_scored"
            } else {
                "search"
            };
            format!(
                "{}/browse/{endpoint}/{}?query={}",
                backend_url,
                user.id,
                urlencoding::encode(submitted_query.trim())
            )
        })
    });
    let show_rating = read_display_setting("rating", "search_show_rating", true);
    let show_affinity = read_display_setting("affinity", "search_show_affinity", false);
    let show_score = read_display_setting("score", "search_show_score", true);
    let show_post_number = read_display_setting("post_number", "search_show_post_number", true);
    let show_desc = read_display_setting("desc", "search_show_desc", true);
    let show_metadata = read_display_setting("metadata", "search_show_metadata", false);
    let show_breakdown = read_display_setting("breakdown", "search_show_breakdown", false);
    let show_detailed_breakdown = read_display_setting(
        "show_detailed_breakdown",
        "search_show_detailed_breakdown",
        false,
    );

    macro_rules! persist_bool {
        ($state:expr, $key:expr) => {{
            let value = *$state;
            use_effect_with(value, move |value| {
                if let Some(storage) =
                    window().and_then(|browser| browser.local_storage().ok().flatten())
                {
                    let _ = storage.set_item($key, &value.to_string());
                }
                || ()
            });
        }};
    }
    persist_bool!(score_results, "settings_score_results");
    {
        let cutoff = *score_cutoff_pct;
        use_effect_with(cutoff, move |cutoff| {
            if let Some(storage) =
                window().and_then(|browser| browser.local_storage().ok().flatten())
            {
                let _ = storage.set_item("settings_score_cutoff_pct", &cutoff.to_string());
            }
            || ()
        });
    }

    html! {
        <div class="m-4 gap-3">
            <h1 class="text-2xl font-semibold text-base-content mb-3">{ "Search" }</h1>
            <div class="flex flex-wrap gap-3 items-end mb-3">
                <SavedAccountsSelect selected_user={selected_user.clone()} is_loading={accounts_loading} />
                <form class="flex-1 min-w-64" onsubmit={on_submit}>
                    <label class="fieldset-label" for="post-search-query">{ "e621 tags" }</label>
                    <div class="join w-full">
                        <input
                            id="post-search-query"
                            class="input join-item w-full"
                            type="search"
                            list="tag-suggestions"
                            value={(*input).clone()}
                            oninput={on_input}
                            placeholder="for example: wolf rating:s"
                            aria-describedby="tag-suggestion-status"
                        />
                        <button class="btn btn-primary join-item" type="submit">{ "Search" }</button>
                    </div>
                    <datalist id="tag-suggestions">
                        { for suggestions.iter().map(|tag| html! { <option value={tag.clone()} /> }) }
                    </datalist>
                    <p id="tag-suggestion-status" class="text-xs text-base-content/70 mt-1" aria-live="polite">
                        {
                            if *suggestions_loading { "Loading tag aliases…" }
                            else if let Some(error) = &*suggestions_error { error.as_str() }
                            else if !input.trim().is_empty() && suggestions.is_empty() { "No tag aliases found." }
                            else if !suggestions.is_empty() { "Use ↑/↓ to choose a tag alias, or continue typing an e621 query." }
                            else { "" }
                        }
                    </p>
                </form>
            </div>
            if let Some(url) = fetch_url {
                <PostGrid
                    fetch_url={url}
                    scored={*score_results}
                    score_cutoff_pct={if *score_results { Some(*score_cutoff_pct) } else { None }}
                    backend_url={backend_url}
                    account_id={selected_user.as_ref().map(|user| user.id as i32).unwrap_or_default()}
                    show_rating={show_rating}
                    show_affinity={show_affinity}
                    show_score={show_score}
                    show_post_number={show_post_number}
                    show_desc={show_desc}
                    show_metadata={show_metadata}
                    show_breakdown={show_breakdown}
                    show_detailed_breakdown={show_detailed_breakdown}
                    empty_message={"No posts matched this query."}
                />
            } else {
                <p class="text-base-content/70 text-center my-8">{
                    if selected_user.is_none() { "Select an account to search." } else { "Enter one or more e621 tags to search." }
                }</p>
            }
        </div>
    }
}
