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

fn stored_bool(key: &str, default: bool) -> bool {
    window()
        .and_then(|browser| browser.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(key).ok().flatten())
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Server-proxied e621 post search with alias-aware tag suggestions.
#[function_component(SearchPage)]
pub fn search_page() -> Html {
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

    let score_results = use_state(|| stored_bool("search_score_results", false));
    let score_cutoff_pct = use_state(|| {
        window()
            .and_then(|browser| browser.local_storage().ok().flatten())
            .and_then(|storage| storage.get_item("search_score_cutoff_pct").ok().flatten())
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
    let show_rating = use_state(|| stored_bool("search_show_rating", true));
    let show_affinity = use_state(|| stored_bool("search_show_affinity", false));
    let show_score = use_state(|| stored_bool("search_show_score", true));
    let show_post_number = use_state(|| stored_bool("search_show_post_number", true));
    let show_desc = use_state(|| stored_bool("search_show_desc", true));
    let show_metadata = use_state(|| stored_bool("search_show_metadata", false));
    let show_breakdown = use_state(|| stored_bool("search_show_breakdown", false));
    let show_detailed_breakdown =
        use_state(|| stored_bool("search_show_detailed_breakdown", false));

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
    persist_bool!(score_results, "search_score_results");
    persist_bool!(show_rating, "search_show_rating");
    persist_bool!(show_affinity, "search_show_affinity");
    persist_bool!(show_score, "search_show_score");
    persist_bool!(show_post_number, "search_show_post_number");
    persist_bool!(show_desc, "search_show_desc");
    persist_bool!(show_metadata, "search_show_metadata");
    persist_bool!(show_breakdown, "search_show_breakdown");
    {
        let cutoff = *score_cutoff_pct;
        use_effect_with(cutoff, move |cutoff| {
            if let Some(storage) =
                window().and_then(|browser| browser.local_storage().ok().flatten())
            {
                let _ = storage.set_item("search_score_cutoff_pct", &cutoff.to_string());
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
                <div class="fieldset min-w-52">
                    <label class="label cursor-pointer justify-start gap-2 py-1">
                        <input type="checkbox" class="toggle toggle-sm" checked={*score_results}
                            onchange={{ let state = score_results.clone(); Callback::from(move |_: Event| state.set(!*state)) }} />
                        <span class="label-text">{ "Score results" }</span>
                    </label>
                    <div class={classes!("join", "join-sm", if !*score_results { "opacity-50" } else { "" })} role="group" aria-label="Score cutoff" aria-disabled={(!*score_results).to_string()}>
                        { for [("Wide", 0.0f32), ("Balanced", 30.0), ("Strict", 60.0)].iter().map(|(label, cutoff)| {
                            let state = score_cutoff_pct.clone();
                            let active = (*score_cutoff_pct - *cutoff).abs() < 0.1;
                            let cutoff = *cutoff;
                            html! { <button type="button" disabled={!*score_results} class={classes!("btn", "btn-outline", if active { "btn-active" } else { "" })} onclick={Callback::from(move |_| state.set(cutoff))}>{ *label }</button> }
                        }) }
                    </div>
                </div>
                <details class="dropdown dropdown-end">
                    <summary class="btn btn-outline"><IconSliders />{ " Display" }</summary>
                    <div class="menu dropdown-content p-3 shadow bg-base-100 rounded-box w-72 z-50" style="min-width:260px;">
                        <span class="text-xs text-base-content/70 block mb-1">{ "Badges" }</span>
                        <label class="label cursor-pointer py-1">
                            <span class="text-base-content">{ "Rating badge" }</span>
                            <input type="checkbox" class="toggle toggle-sm" checked={*show_rating}
                                onchange={{ let state = show_rating.clone(); Callback::from(move |_: Event| state.set(!*state)) }} />
                        </label>
                        <label class="label cursor-pointer py-1">
                            <span class="text-base-content">{ "Affinity score" }</span>
                            <input type="checkbox" class="toggle toggle-sm" checked={*show_affinity} disabled={!*score_results}
                                onchange={{ let state = show_affinity.clone(); Callback::from(move |_: Event| state.set(!*state)) }} />
                        </label>
                        <label class="label cursor-pointer py-1">
                            <span class="text-base-content">{ "Post score" }</span>
                            <input type="checkbox" class="toggle toggle-sm" checked={*show_score}
                                onchange={{ let state = show_score.clone(); Callback::from(move |_: Event| state.set(!*state)) }} />
                        </label>
                        <label class="label cursor-pointer py-1">
                            <span class="text-base-content">{ "Post number" }</span>
                            <input type="checkbox" class="toggle toggle-sm" checked={*show_post_number}
                                onchange={{ let state = show_post_number.clone(); Callback::from(move |_: Event| state.set(!*state)) }} />
                        </label>
                        <div class="divider my-1"></div>
                        <span class="text-xs text-base-content/70 block mb-1">{ "Cards" }</span>
                        <label class="label cursor-pointer py-1">
                            <span class="text-base-content">{ "Post text / tags" }</span>
                            <input type="checkbox" class="toggle toggle-sm" checked={*show_desc}
                                onchange={{ let state = show_desc.clone(); Callback::from(move |_: Event| state.set(!*state)) }} />
                        </label>
                        <label class="label cursor-pointer py-1">
                            <span class="text-base-content">{ "File metadata" }</span>
                            <input type="checkbox" class="toggle toggle-sm" checked={*show_metadata}
                                onchange={{ let state = show_metadata.clone(); Callback::from(move |_: Event| state.set(!*state)) }} />
                        </label>
                        <label class="label cursor-pointer py-1">
                            <span class="text-base-content">{ "Score breakdown" }</span>
                            <input type="checkbox" class="toggle toggle-sm" checked={*show_breakdown} disabled={!*score_results}
                                onchange={{ let state = show_breakdown.clone(); Callback::from(move |_: Event| state.set(!*state)) }} />
                        </label>
                    </div>
                </details>
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
