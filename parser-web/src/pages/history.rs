use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use crate::components::*;
use crate::models::*;
use crate::pages::UserInfo;

/// Interaction-history page — shows which posts the selected account
/// opened / liked / hid, with an event-type filter.
#[function_component(HistoryPage)]
pub fn history_page() -> Html {
    let _settings_tick = use_settings_tick();
    let selected_user = use_state(|| Option::<UserInfo>::None);
    let is_loading = use_state(|| false);
    let error = use_state(|| Option::<String>::None);
    let entries = use_state(Vec::<InteractionHistoryEntry>::new);
    let filter = use_state(|| Option::<String>::None);

    // Fetch history whenever the user or the event filter changes.
    {
        let user = selected_user.clone();
        let filter = filter.clone();
        let entries = entries.clone();
        let is_loading = is_loading.clone();
        let error = error.clone();
        use_effect_with((user.clone(), filter.clone()), move |(user, filter)| {
            let Some(user) = user.as_ref() else {
                entries.set(Vec::new());
                return;
            };
            let Some(cfg) = read_config_from_head() else {
                error.set(Some("App configuration failed to load.".to_string()));
                return;
            };
            let mut url = format!("{}/account/{}/interactions", cfg.backend_domain, user.id);
            if let Some(f) = filter.as_ref() {
                url.push_str(&format!("?event={f}"));
            }
            is_loading.set(true);
            error.set(None);
            let entries = entries.clone();
            let is_loading = is_loading.clone();
            let error = error.clone();
            spawn_local(async move {
                match api_get(&url).send().await {
                    Ok(resp) if resp.ok() => {
                        match resp.json::<Vec<InteractionHistoryEntry>>().await {
                            Ok(items) => entries.set(items),
                            Err(e) => error.set(Some(format!("Failed to parse history: {e}"))),
                        }
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        error.set(Some(humanize_error_body(status, &body)));
                    }
                    Err(e) => error.set(Some(humanize_network_error(e))),
                }
                is_loading.set(false);
            });
        });
    }

    // Filter tabs.
    let on_select_filter = |f: Option<String>| {
        let filter = filter.clone();
        Callback::from(move |_: MouseEvent| filter.set(f.clone()))
    };

    let posts_domain = read_config_from_head()
        .map(|cfg| cfg.posts_domain)
        .unwrap_or_default();

    html! {
        <div class="m-4">
            <h1 class="text-2xl font-semibold text-base-content mb-3">{ "Interaction History" }</h1>
            <div class="flex flex-wrap gap-3 items-center mb-3">
                <div>
                    <SavedAccountsSelect
                        selected_user={selected_user.clone()}
                        is_loading={is_loading.clone()}
                    />
                </div>

                <div class="tabs tabs-boxed" role="tablist" aria-label="Event filter">
                    <button
                        type="button"
                        role="tab"
                        aria-selected={filter.is_none().to_string()}
                        class={classes!("tab", if filter.is_none() { "tab-active" } else { "" })}
                        onclick={on_select_filter(None)}
                    >{ "All" }</button>
                    { for ["open", "like", "strong_like", "hide"].iter().map(|ev| {
                        let label = match *ev {
                            "strong_like" => "Strong like".to_string(),
                            _ => {
                                let mut s = ev.to_string();
                                s = s[..1].to_uppercase() + &s[1..];
                                s
                            }
                        };
                        let active = filter.as_deref() == Some(*ev);
                        html! {
                            <button
                                type="button"
                                role="tab"
                                aria-selected={active.to_string()}
                                class={classes!("tab", if active { "tab-active" } else { "" })}
                                onclick={on_select_filter(Some((*ev).to_string()))}
                            >{ label }</button>
                        }
                    }) }
                </div>
            </div>

            if let Some(ref e) = *error {
                <div class="alert alert-error mb-3" role="alert" aria-live="polite">
                    <span>{ e }</span>
                    <button
                        type="button"
                        class="btn btn-sm btn-outline"
                        onclick={{
                            let user = selected_user.clone();
                            let filter = filter.clone();
                            Callback::from(move |_| {
                                // Re-trigger the fetch by bumping both states.
                                let _ = (user.as_ref(), filter.as_ref());
                            })
                        }}
                    >{ "Retry" }</button>
                </div>
            }

            if *is_loading {
                <div class="flex justify-center my-5">
                    <div class="loading loading-spinner loading-lg" role="status">
                        <span class="sr-only">{ "Loading..." }</span>
                    </div>
                </div>
            }

            if !*is_loading && !entries.is_empty() {
                <ul class="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-5 gap-3">
                    { for entries.iter().map(|entry| html! {
                        <li class="card post-card card-compact overflow-hidden w-full relative border border-base-300 shadow-sm break-inside-avoid">
                            <a
                                href={format!("{}/posts/{}", posts_domain, entry.post_id)}
                                target="_blank"
                                rel="noopener noreferrer"
                                class="block"
                            >
                                if let Some(ref post) = entry.post {
                                    if let Some(url) = post.files.preview.url.clone().or_else(|| post.files.sample.url.clone()) {
                                        <img
                                            src={url}
                                            alt={format!("Post {}", entry.post_id)}
                                            loading="lazy"
                                            class="w-full object-cover"
                                            style="aspect-ratio: 4 / 3;"
                                        />
                                    } else {
                                        <div class="w-full bg-base-300 flex items-center justify-center text-base-content/60" style="aspect-ratio: 4 / 3;">
                                            { format!("#{}", entry.post_id) }
                                        </div>
                                    }
                                } else {
                                    <div class="w-full bg-base-300 flex items-center justify-center text-base-content/60" style="aspect-ratio: 4 / 3;">
                                        { format!("#{}", entry.post_id) }
                                    </div>
                                }
                            </a>
                            <div class="p-2">
                                <div class="flex items-center justify-between">
                                    <span class={classes!("badge", event_badge_class(&entry.event_type))}>
                                        { event_label(&entry.event_type) }
                                    </span>
                                    <span class="text-xs text-base-content/60">
                                        { format_time(&entry.created_at) }
                                    </span>
                                </div>
                            </div>
                        </li>
                    }) }
                </ul>
            }

            if !*is_loading && entries.is_empty() && error.is_none() {
                <p class="text-base-content/70 text-center my-8">
                    {
                        if selected_user.is_some() {
                            "No interactions recorded for this account yet."
                        } else {
                            "Select an account to see its interaction history."
                        }
                    }
                </p>
            }
        </div>
    }
}

fn event_label(et: &FeedInteractionType) -> &'static str {
    match et {
        FeedInteractionType::QualifiedImpression => "Impression",
        FeedInteractionType::Open => "Open",
        FeedInteractionType::Like => "Like",
        FeedInteractionType::StrongLike => "Strong like",
        FeedInteractionType::Hide => "Hide",
    }
}

fn event_badge_class(et: &FeedInteractionType) -> &'static str {
    match et {
        FeedInteractionType::Open => "badge-info",
        FeedInteractionType::Like => "badge-success",
        FeedInteractionType::StrongLike => "badge-success badge-outline",
        FeedInteractionType::Hide => "badge-error",
        FeedInteractionType::QualifiedImpression => "badge-ghost",
    }
}

/// Compact relative/absolute timestamp from an RFC3339 string.
fn format_time(iso: &str) -> String {
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return iso.to_string();
    };
    dt.format("%Y-%m-%d %H:%M").to_string()
}
