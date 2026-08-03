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

const KEY_SAVED: &str = "search_saved";
const KEY_HISTORY: &str = "search_history";
const HISTORY_CAP: usize = 10;

/// Read a JSON `Vec<String>` list from localStorage (recent search history /
/// saved searches). Never panics — missing or corrupt data yields an empty list.
fn read_search_list(key: &str) -> Vec<String> {
    let storage = || window().and_then(|w| w.local_storage().ok().flatten());
    storage()
        .and_then(|s| s.get_item(key).ok().flatten())
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default()
}

/// Persist a JSON `Vec<String>` list to localStorage.
fn write_search_list(key: &str, list: &[String]) {
    if let Some(storage) = window().and_then(|w| w.local_storage().ok().flatten())
        && let Ok(raw) = serde_json::to_string(list)
    {
        let _ = storage.set_item(key, &raw);
    }
}

/// Move `item` to the front, dropping duplicates, capped at `cap`. Pure so
/// the history/fixed-cap behaviour is unit-testable without the DOM.
fn push_front_unique(list: &[String], item: &str, cap: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(list.len() + 1);
    out.push(item.to_string());
    for existing in list {
        if existing != item {
            out.push(existing.clone());
        }
    }
    out.truncate(cap);
    out
}

/// Prepend `item` unless it is already present. Pure.
fn push_unique(list: &[String], item: &str) -> Vec<String> {
    if list.iter().any(|existing| existing == item) {
        list.to_vec()
    } else {
        let mut out = vec![item.to_string()];
        out.extend(list.iter().cloned());
        out
    }
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

    // Saved searches + recent search history, persisted in localStorage.
    let saved = use_state(Vec::<String>::new);
    let history = use_state(Vec::<String>::new);
    {
        let saved = saved.clone();
        let history = history.clone();
        use_effect_with((), move |_| {
            saved.set(read_search_list(KEY_SAVED));
            history.set(read_search_list(KEY_HISTORY));
            || ()
        });
    }

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
    // Record a search into the recent history: dedupe, move to top, cap.
    let on_record = {
        let history = history.clone();
        Callback::from(move |q: String| {
            let q = q.trim().to_string();
            if q.is_empty() {
                return;
            }
            let list = push_front_unique(&history, &q, HISTORY_CAP);
            write_search_list(KEY_HISTORY, &list);
            history.set(list);
        })
    };

    // One-tap re-run from a saved / recent chip: fill the field and submit.
    let on_run_search = {
        let input = input.clone();
        let submitted_query = submitted_query.clone();
        let record = on_record.clone();
        Callback::from(move |q: String| {
            let q = q.trim().to_string();
            if q.is_empty() {
                return;
            }
            input.set(q.clone());
            submitted_query.set(q.clone());
            record.emit(q);
        })
    };

    // Save the current input as a saved search (no duplicates).
    let on_save = {
        let input = input.clone();
        let saved = saved.clone();
        Callback::from(move |_| {
            let q = input.trim().to_string();
            if q.is_empty() {
                return;
            }
            let list = push_unique(&saved, &q);
            if list != *saved {
                write_search_list(KEY_SAVED, &list);
                saved.set(list);
            }
        })
    };

    // Remove one saved search.
    let on_remove_saved = {
        let saved = saved.clone();
        Callback::from(move |q: String| {
            let mut list: Vec<String> = (*saved).clone();
            list.retain(|item| item != &q);
            write_search_list(KEY_SAVED, &list);
            saved.set(list);
        })
    };

    // Clear the whole recent-history list.
    let on_clear_history = {
        let history = history.clone();
        Callback::from(move |_| {
            write_search_list(KEY_HISTORY, &[]);
            history.set(Vec::new());
        })
    };

    let on_submit = {
        let input = input.clone();
        let submitted_query = submitted_query.clone();
        let record = on_record.clone();
        Callback::from(move |event: SubmitEvent| {
            event.prevent_default();
            let q = input.trim().to_string();
            // Always update the submitted query (an empty submit clears the
            // grid, preserving pre-#257 behaviour), but only record non-empty
            // queries into search history.
            if !q.is_empty() {
                record.emit(q.clone());
            }
            submitted_query.set(q);
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
                        <button
                            type="button"
                            class="btn btn-ghost join-item"
                            onclick={on_save}
                            disabled={(*input).trim().is_empty()}
                            title="Save this search locally"
                        >{ "Save" }</button>
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
            {
                if !history.is_empty() || !saved.is_empty() {
                    html! {
                        <div class="flex flex-wrap items-start gap-x-6 gap-y-3">
                            if !history.is_empty() {
                                <div class="min-w-0">
                                    <div class="flex items-center gap-2 mb-1">
                                        <span class="text-xs font-semibold text-base-content/70 uppercase tracking-wide">{ "Recent" }</span>
                                        <button type="button" class="btn btn-xs btn-ghost text-base-content/60" onclick={on_clear_history.clone()}>{ "Clear history" }</button>
                                    </div>
                                    <div class="flex flex-wrap gap-2">
                                        { for history.iter().map(|q| html! {
                                            <button
                                                type="button"
                                                class="btn btn-xs btn-outline"
                                                title={format!("Run: {}", q)}
                                                onclick={on_run_search.reform({
                                                    let q = q.clone();
                                                    move |_: MouseEvent| q.clone()
                                                })}
                                            >{ q }</button>
                                        }) }
                                    </div>
                                </div>
                            }
                            if !saved.is_empty() {
                                <div class="min-w-0">
                                    <div class="mb-1">
                                        <span class="text-xs font-semibold text-base-content/70 uppercase tracking-wide">{ "Saved searches" }</span>
                                    </div>
                                    <div class="flex flex-wrap gap-2">
                                        { for saved.iter().map(|q| html! {
                                            <div class="join">
                                                <button
                                                    type="button"
                                                    class="btn btn-xs btn-outline join-item"
                                                    title={format!("Run: {}", q)}
                                                    onclick={on_run_search.reform({
                                                        let q = q.clone();
                                                        move |_: MouseEvent| q.clone()
                                                    })}
                                                >{ q }</button>
                                                <button
                                                    type="button"
                                                    class="btn btn-xs btn-outline btn-error join-item"
                                                    title="Remove saved search"
                                                    onclick={on_remove_saved.reform({
                                                        let q = q.clone();
                                                        move |_: MouseEvent| q.clone()
                                                    })}
                                                >{ "✕" }</button>
                                            </div>
                                        }) }
                                    </div>
                                </div>
                            }
                        </div>
                    }
                } else {
                    html! {}
                }
            }
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

#[cfg(test)]
mod tests {
    use super::{push_front_unique, push_unique};

    #[test]
    fn history_moves_matching_item_to_front() {
        let list = vec!["wolf".to_string(), "rating:s".to_string()];
        assert_eq!(
            push_front_unique(&list, "rating:s", 10),
            vec!["rating:s".to_string(), "wolf".to_string(),]
        );
    }

    #[test]
    fn history_dedupes_and_caps() {
        let list: Vec<String> = (0..10).map(|i| format!("tag{i}")).collect();
        let out = push_front_unique(&list, "tag0", 10);
        assert_eq!(out.len(), 10);
        assert_eq!(out[0], "tag0");
        // the original tag0 duplicate is dropped
        assert!(!out[1..].contains(&"tag0".to_string()));
    }

    #[test]
    fn history_caps_at_limit() {
        let list: Vec<String> = (0..10).map(|i| format!("tag{i}")).collect();
        let out = push_front_unique(&list, "fresh", 10);
        assert_eq!(out.len(), 10);
        assert_eq!(out[0], "fresh");
    }

    #[test]
    fn saved_stays_unchanged_for_duplicate() {
        let list = vec!["a".to_string(), "b".to_string()];
        assert_eq!(push_unique(&list, "a"), list);
    }

    #[test]
    fn saved_prepends_new_item() {
        let list = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            push_unique(&list, "c"),
            vec!["c".to_string(), "a".to_string(), "b".to_string()]
        );
    }
}
