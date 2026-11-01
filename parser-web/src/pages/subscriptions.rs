//! Watchlist: tags the account follows, with the new-post counter and the
//! posts that arrived since the mark.
//!
//! The page is deliberately read-mostly: the only writes are "add", "mark
//! read" and "remove". Nothing here re-ranks the feed — a subscription is a
//! filter over the local catalog, not a scoring signal.

use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlInputElement, SubmitEvent};
use yew::prelude::*;

use crate::components::*;
use crate::models::*;
use crate::pages::UserInfo;

fn backend() -> Option<String> {
    read_config_from_head().map(|cfg| cfg.backend_domain)
}

#[function_component(SubscriptionsPage)]
pub fn subscriptions_page() -> Html {
    let _settings_tick = use_settings_tick();
    let selected_user = use_state(|| Option::<UserInfo>::None);
    let is_loading = use_state(|| false);
    let error = use_state(|| Option::<String>::None);
    let subs = use_state(Vec::<Subscription>::new);
    let new_posts = use_state(Vec::<ScoredPost>::new);
    let draft = use_state(String::new);
    let busy = use_state(|| false);
    let tick = use_state(|| 0u32);

    {
        let user = selected_user.clone();
        let subs = subs.clone();
        let new_posts = new_posts.clone();
        let is_loading = is_loading.clone();
        let error = error.clone();
        let tick_val = *tick;
        use_effect_with((user.clone(), tick_val), move |(user, _)| {
            let Some(user) = user.as_ref() else {
                subs.set(Vec::new());
                new_posts.set(Vec::new());
                return;
            };
            let Some(base) = backend() else {
                error.set(Some("App configuration failed to load.".to_string()));
                return;
            };
            let list_url = format!("{base}/account/{}/subscriptions", user.id);
            let new_url = format!("{base}/account/{}/subscriptions/new?limit=12", user.id);
            is_loading.set(true);
            error.set(None);
            let subs = subs.clone();
            let new_posts = new_posts.clone();
            let is_loading = is_loading.clone();
            let error = error.clone();
            spawn_local(async move {
                match api_get(&list_url).send().await {
                    Ok(resp) if resp.ok() => match resp.json::<Vec<Subscription>>().await {
                        Ok(items) => {
                            let has_new = items.iter().any(|s| s.new_count > 0);
                            subs.set(items);
                            if has_new {
                                match api_get(&new_url).send().await {
                                    Ok(r) if r.ok() => {
                                        if let Ok(posts) = r.json::<Vec<ScoredPost>>().await {
                                            new_posts.set(posts);
                                        }
                                    }
                                    _ => new_posts.set(Vec::new()),
                                }
                            } else {
                                new_posts.set(Vec::new());
                            }
                        }
                        Err(_) => error.set(Some("Watchlist could not be read.".to_string())),
                    },
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

    let on_input = {
        let draft = draft.clone();
        Callback::from(move |e: InputEvent| {
            draft.set(e.target_unchecked_into::<HtmlInputElement>().value());
        })
    };

    let on_add = {
        let draft = draft.clone();
        let busy = busy.clone();
        let error = error.clone();
        let tick = tick.clone();
        let user = selected_user.clone();
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            let (Some(user), Some(base)) = (user.as_ref(), backend()) else {
                return;
            };
            let tag = draft.trim().to_string();
            if tag.is_empty() {
                return;
            }
            let url = format!("{base}/account/{}/subscriptions", user.id);
            busy.set(true);
            error.set(None);
            let draft = draft.clone();
            let busy = busy.clone();
            let error = error.clone();
            let tick = tick.clone();
            spawn_local(async move {
                let body = serde_json::json!({ "tag": tag }).to_string();
                let response = api_post(&url)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await;
                match response {
                    Ok(resp) if resp.ok() => {
                        draft.set(String::new());
                        tick.set(*tick + 1);
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        error.set(Some(humanize_error_body(status, &body)));
                    }
                    Err(e) => error.set(Some(humanize_network_error(e))),
                }
                busy.set(false);
            });
        })
    };

    let mark_seen = |tag_id: Option<i64>| {
        let tick = tick.clone();
        let error = error.clone();
        let busy = busy.clone();
        let user = selected_user.clone();
        Callback::from(move |_: MouseEvent| {
            let (Some(user), Some(base)) = (user.as_ref(), backend()) else {
                return;
            };
            let url = format!("{base}/account/{}/subscriptions/seen", user.id);
            busy.set(true);
            let tick = tick.clone();
            let error = error.clone();
            let busy = busy.clone();
            spawn_local(async move {
                let body = serde_json::json!({ "tag_id": tag_id }).to_string();
                let response = api_post(&url)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await;
                match response {
                    Ok(resp) if resp.ok() => tick.set(*tick + 1),
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        error.set(Some(humanize_error_body(status, &body)));
                    }
                    Err(e) => error.set(Some(humanize_network_error(e))),
                }
                busy.set(false);
            });
        })
    };

    let remove = |tag_id: i64| {
        let tick = tick.clone();
        let error = error.clone();
        let user = selected_user.clone();
        Callback::from(move |_: MouseEvent| {
            let (Some(user), Some(base)) = (user.as_ref(), backend()) else {
                return;
            };
            let url = format!("{base}/account/{}/subscriptions/{}", user.id, tag_id);
            let tick = tick.clone();
            let error = error.clone();
            spawn_local(async move {
                match api_delete(&url).send().await {
                    Ok(resp) if resp.ok() => tick.set(*tick + 1),
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        error.set(Some(humanize_error_body(status, &body)));
                    }
                    Err(e) => error.set(Some(humanize_network_error(e))),
                }
            });
        })
    };

    let posts_domain = read_config_from_head()
        .map(|cfg| cfg.posts_domain)
        .unwrap_or_else(|| "https://e621.net".to_string());
    let total_new: i64 = subs.iter().map(|s| s.new_count).sum();

    html! {
        <div class="m-4">
            <h1 class="text-2xl font-semibold text-base-content mb-1">{ "Watchlist" }</h1>
            <p class="text-sm opacity-70 mb-3">
                { "Follow a tag; the counter shows posts that arrived after you last marked the tag read." }
            </p>
            <div class="flex flex-wrap gap-3 items-center mb-3">
                <SavedAccountsSelect
                    selected_user={selected_user.clone()}
                    is_loading={is_loading.clone()}
                />
                if selected_user.is_some() {
                    <form class="flex-1 min-w-56 max-w-xl" onsubmit={on_add.clone()}>
                        <div class="join w-full">
                            <input
                                id="watchlist-tag"
                                class="input input-sm join-item w-full"
                                type="text"
                                placeholder="tag or artist, e.g. felino"
                                value={(*draft).clone()}
                                oninput={on_input}
                            />
                            <button class="btn btn-sm btn-primary join-item" type="submit" disabled={*busy}>
                                { "Watch" }
                            </button>
                        </div>
                    </form>
                }
            </div>

            if let Some(ref e) = *error {
                <ErrorAlert message={(*e).clone()} on_retry={Some(Callback::from({
                    let tick = tick.clone();
                    move |_| tick.set(*tick + 1)
                }))} />
            }

            if *is_loading {
                <span class="loading loading-spinner loading-sm" />
            } else if selected_user.is_some() && subs.is_empty() {
                <div class="alert alert-info">
                    { "No tags watched yet. Add one above — for example an artist you follow." }
                </div>
            } else if !subs.is_empty() {
                <div class="flex items-center gap-2 mb-2">
                    <span class="badge badge-neutral">{ format!("{} watched", subs.len()) }</span>
                    if total_new > 0 {
                        <span class="badge badge-primary">{ format!("{total_new} new") }</span>
                        <button type="button" class="btn btn-xs btn-ghost" onclick={mark_seen(None)}>
                            { "Mark all read" }
                        </button>
                    }
                </div>
                <div class="overflow-x-auto">
                    <table class="table table-sm">
                        <thead>
                            <tr>
                                <th>{ "Tag" }</th>
                                <th>{ "Group" }</th>
                                <th>{ "New" }</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            { for subs.iter().map(|s| {
                                let tag_id = s.tag_id;
                                html! {
                                    <tr>
                                        <td class="font-mono">{ &s.tag_name }</td>
                                        <td><span class="badge badge-ghost badge-sm">{ &s.group_type }</span></td>
                                        <td>
                                            if s.new_count > 0 {
                                                <span class="badge badge-primary badge-sm">{ s.new_count }</span>
                                            } else {
                                                <span class="opacity-50">{ "—" }</span>
                                            }
                                        </td>
                                        <td class="text-right">
                                            if s.new_count > 0 {
                                                <button type="button" class="btn btn-xs btn-ghost"
                                                    onclick={mark_seen(Some(tag_id))}>
                                                    { "Mark read" }
                                                </button>
                                            }
                                            <button type="button" class="btn btn-xs btn-ghost text-error"
                                                onclick={remove(tag_id)}>
                                                { "Remove" }
                                            </button>
                                        </td>
                                    </tr>
                                }
                            }) }
                        </tbody>
                    </table>
                </div>

                if !new_posts.is_empty() {
                    <h2 class="text-lg font-semibold mt-6 mb-2">{ "New since last visit" }</h2>
                    <div class="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-6 gap-3">
                        { for new_posts.iter().map(|sp| {
                            let p = &sp.post;
                            let href = format!("{}/posts/{}", posts_domain, p.id);
                            let preview = p.files.preview.url.clone().unwrap_or_default();
                            html! {
                                <a class="block rounded-lg overflow-hidden bg-base-300" href={href}
                                   target="_blank" rel="noopener noreferrer">
                                    <img class="w-full h-40 object-cover" src={preview} alt={format!("post {}", p.id)} loading="lazy" />
                                    <div class="p-1 text-xs flex justify-between">
                                        <span>{ format!("#{}", p.id) }</span>
                                        <span>{ format!("♥ {}", p.stats.fav_count) }</span>
                                    </div>
                                </a>
                            }
                        }) }
                    </div>
                }
            }
        </div>
    }
}
