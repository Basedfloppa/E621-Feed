use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use yew::prelude::*;
use yew_router::prelude::use_navigator;

use crate::Route;
use crate::components::*;
use crate::models::{ACCOUNT_LIST_CHANGED_EVENT, api_get, read_config_from_head};
use crate::pages::account::AccountPrefill;

/// MUST match `models::TagCount` on the backend (`parser-api/src/models/tags_info.rs`).
/// Serde ignores unknown fields on deserialize, so adding a field is safe;
/// removing or renaming one will silently produce `None`/default values.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TagCount {
    pub name: String,
    pub group_type: String,
    pub count: i64,
}

/// MUST match `TruncatedAccount` on the backend (`parser-api/src/models/users.rs`).
/// Same serde caveat as `TagCount`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserInfo {
    pub id: i64,
    pub name: String,
    pub blacklist: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagView {
    Chart,
    Graph,
    Profile,
}

#[function_component(HomePage)]
pub fn home_page() -> Html {
    let cfg = match read_config_from_head() {
        Some(c) => c,
        None => {
            return html! {
                <div class="mt-4">
                    <div class="alert alert-danger" role="alert">
                        { "App configuration failed to load. Please reload the page; if the problem persists, check that /static/config.js is reachable." }
                    </div>
                </div>
            };
        }
    };
    let selected_user: UseStateHandle<Option<UserInfo>> = use_state(|| None::<UserInfo>);
    let is_loading: UseStateHandle<bool> = use_state(|| false);
    let tag_counts: UseStateHandle<Vec<TagCount>> = use_state(Vec::<TagCount>::new);
    let error: UseStateHandle<Option<String>> = use_state(|| None::<String>);
    let canvas_ref = use_node_ref();
    let active_view: UseStateHandle<TagView> = use_state(|| TagView::Chart);
    let navigator = use_navigator();
    // Accounts with a running /process job (full or incremental).
    let process_running: UseStateHandle<HashSet<i64>> = use_state(HashSet::new);

    // Saved-accounts mirror for the "found in e621 but not saved on this
    // device" prompt below. `SavedAccountsSelect` maintains its own copy
    // and there's no shared store yet — duplicating the fetch is the
    // smallest change. The `ACCOUNT_LIST_CHANGED_EVENT` listener keeps
    // this view in sync after any creation/deletion in another tab/page.
    let saved_accounts: UseStateHandle<Vec<UserInfo>> = use_state(Vec::new);
    {
        let saved_accounts = saved_accounts.clone();
        let backend = cfg.backend_domain.clone();
        use_effect_with((), move |_| {
            let fetch = {
                let saved_accounts = saved_accounts.clone();
                let backend = backend.clone();
                move || {
                    let saved_accounts = saved_accounts.clone();
                    let url = format!("{}/accounts", backend);
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Ok(resp) = api_get(&url).send().await
                            && resp.ok()
                                && let Ok(accts) = resp.json::<Vec<UserInfo>>().await {
                                    saved_accounts.set(accts);
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

    // Drives the "create this account" prompt and gates the analyze /
    // tag-views section: a `selected_user` lifted out of search may
    // correspond to a real e621 account that just isn't saved locally
    // yet — analysing such an account would 404 in the backend, so
    // we surface the create flow first.
    let is_saved = selected_user
        .as_ref()
        .map(|u| saved_accounts.iter().any(|sa| sa.id == u.id))
        .unwrap_or(false);

    // Toast-like message handler for ReanalyzeButton completion events.
    // We re-use the existing `error` state to show the outcome.
    let on_reanalyze_msg = {
        let error = error.clone();
        Callback::from(move |result: Result<String, String>| {
            match result {
                Ok(msg) => error.set(Some(msg)),
                Err(e) => error.set(Some(e)),
            }
        })
    };

    // Click → navigate to `/account` with the looked-up id and name as
    // query params so the form arrives pre-filled. Uses
    // `push_with_query` (yew_router soft nav) rather than a hard `<a>`
    // jump so the SPA state (saved-accounts cache, session cookie, …)
    // doesn't get rebuilt for one click.
    let on_create_account = {
        let navigator = navigator.clone();
        let selected_user = selected_user.clone();
        Callback::from(move |_: MouseEvent| {
            let Some(user) = (*selected_user).clone() else {
                return;
            };
            if let Some(nav) = navigator.as_ref() {
                let _ = nav.push_with_query(
                    &Route::Account,
                    &AccountPrefill {
                        id: user.id.to_string(),
                        name: user.name.clone(),
                    },
                );
            }
        })
    };

    html! {
        <div>
            <div class="flex justify-center">
                <div class="w-full max-w-3xl">
                    <div class="card bg-base-100 shadow-sm">
                        <div class="card-body text-base-content">
                            <h1 class="card-title text-2xl text-center">{"e621 Tag Analyzer"}</h1>

                            <div id="home-account">
                                <SavedAccountsSelect
                                    selected_user={selected_user.clone()}
                                    is_loading={is_loading.clone()}
                                />

                                <UserSearchForm
                                    found_user={selected_user.clone()}
                                    error={error.clone()}
                                    api_base={cfg.backend_domain.clone()}
                                    is_loading={is_loading.clone()}
                                />
                            </div>

                            <UserInfoAlert
                                user={selected_user.clone()}
                                error={error.clone()}
                            />

                            // Saved accounts quick-actions — show a
                            // compact list of every saved account with
                            // a one-click Re-analyze button so the user
                            // doesn't have to switch to /account to
                            // trigger an update.
                            if !saved_accounts.is_empty() {
                                <div class="mb-3">
                                    <details open={saved_accounts.len() <= 3}>
                                        <summary class="text-base-content/70 text-sm mb-2 cursor-pointer">
                                            { format!("Saved accounts ({} total)", saved_accounts.len()) }
                                        </summary>
                                        <ul class="flex flex-col">
                                            {for saved_accounts.iter().map(|acc| {
                                                let acc_id = acc.id;
                                                let name = acc.name.clone();
                                                let on_msg = on_reanalyze_msg.clone();
                                                let process_running = process_running.clone();
                                                html! {
                                                    <li class="flex justify-between items-center py-1 px-2">
                                                        <span class="text-sm">
                                                            <strong>{ &name }</strong>
                                                            { format!(" (ID {})", acc.id) }
                                                        </span>
                                                        <div class="join join-sm" role="group">
                                                            <ReanalyzeButton
                                                                account_id={acc_id}
                                                                api_base={cfg.backend_domain.clone()}
                                                                on_complete={on_msg.clone()}
                                                                blocked={process_running.contains(&acc.id)}
                                                                on_running={{
                                                                    let process_running = process_running.clone();
                                                                    Callback::from(move |running: bool| {
                                                                        let mut set = (*process_running).clone();
                                                                        if running { set.insert(acc_id); }
                                                                        else { set.remove(&acc_id); }
                                                                        process_running.set(set);
                                                                    })
                                                                }}
                                                            />
                                                            <ReanalyzeButton
                                                                mode="incremental"
                                                                account_id={acc_id}
                                                                api_base={cfg.backend_domain.clone()}
                                                                on_complete={on_msg}
                                                                blocked={process_running.contains(&acc.id)}
                                                                on_running={{
                                                                    let process_running = process_running.clone();
                                                                    Callback::from(move |running: bool| {
                                                                        let mut set = (*process_running).clone();
                                                                        if running { set.insert(acc_id); }
                                                                        else { set.remove(&acc_id); }
                                                                        process_running.set(set);
                                                                    })
                                                                }}
                                                            />
                                                        </div>
                                                    </li>
                                                }
                                            })}
                                        </ul>
                                    </details>
                                </div>
                            }

                            // "Looked up but not saved" prompt. The
                            // search routes fall back to an e621 lookup
                            // when the account isn't in our DB, so a
                            // hit here can be either a saved account
                            // (analyzing works) or just an e621 lookup
                            // (analyzing would 404 because the backend
                            // requires a device-scoped row). We surface
                            // the create flow before the user clicks
                            // analyze and hits a confusing error.
                            if selected_user.is_some() && !is_saved {
                                <div class="alert alert-warning flex flex-wrap justify-between items-center gap-2 mb-3">
                                    <span class="flex-1">
                                        { "This account isn't saved on this device yet. Add it to your account list before analysing." }
                                    </span>
                                    <button
                                        type="button"
                                        class="btn btn-sm btn-primary"
                                        onclick={on_create_account}
                                    >
                                        { "Create this account" }
                                    </button>
                                </div>
                            }

                            // The analyze section only renders for an
                            // account that's actually persisted —
                            // `is_saved` implies `selected_user.is_some()`
                            // so the previous "user selected at all"
                            // gate is now strictly stronger.
                            if is_saved {
                                <div id="home-analyzer">
                                    <FetchAnalyzeButton
                                        tag_count={tag_counts.clone()}
                                        found_user={selected_user.clone()}
                                        error={error.clone()}
                                        api_base={cfg.backend_domain.clone()}
                                        is_loading={is_loading.clone()}
                                    />
                                </div>
                            }
                        <div class="text-center text-base-content/60 text-sm mt-4">
                            {"Your data stays on this server. Your e621 favourites and profile are never shared with third parties."}
                        </div>
                        </div>
                    </div>
                </div>
            </div>
            {
                // The view switcher and the chart/graph cards both hang
                // off an analysed account, so they live under the same
                // `is_saved` gate as the analyze button. An unsaved e621
                // lookup would otherwise expose buttons whose target
                // requests (`/account/<id>/tag_relations`, the
                // tag-counts fetch) return 404 for an account that
                // isn't device-scoped to this owner_token.
                if is_saved {
                    let chart_active = matches!(*active_view, TagView::Chart);
                    let graph_active = matches!(*active_view, TagView::Graph);
                    let profile_active = matches!(*active_view, TagView::Profile);
                    let on_chart = {
                        let active_view = active_view.clone();
                        Callback::from(move |_| active_view.set(TagView::Chart))
                    };
                    let on_graph = {
                        let active_view = active_view.clone();
                        Callback::from(move |_| active_view.set(TagView::Graph))
                    };
                    let on_profile = {
                        let active_view = active_view.clone();
                        Callback::from(move |_| active_view.set(TagView::Profile))
                    };
                    html! {
                        <>
                            <div class="mt-3">
                                <div class="flex justify-center">
                                    <div class="join" role="group" aria-label="Tag visualisation switcher">
                                        <button
                                            type="button"
                                            class={classes!("btn", "btn-outline", chart_active.then_some("btn-active"))}
                                            aria-pressed={chart_active.to_string()}
                                            onclick={on_chart}
                                        >
                                            { "Tag list" }
                                        </button>
                                        <button
                                            type="button"
                                            class={classes!("btn", "btn-outline", graph_active.then_some("btn-active"))}
                                            aria-pressed={graph_active.to_string()}
                                            onclick={on_graph}
                                        >
                                            { "Relation graph" }
                                        </button>
                                        <button
                                            type="button"
                                            class={classes!("btn", "btn-outline", profile_active.then_some("btn-active"))}
                                            aria-pressed={profile_active.to_string()}
                                            onclick={on_profile}
                                        >
                                            { "Taste Profile" }
                                        </button>
                                    </div>
                                </div>
                            </div>
                            <div class="container-fluid mt-3 px-3 px-md-4">
                                {
                                    match *active_view {
                                        TagView::Chart => html! {
                                            <TagChartCard
                                                canvas_ref={canvas_ref.clone()}
                                                tag_counts={tag_counts.clone()}
                                            />
                                        },
                                        TagView::Graph => html! {
                                            <TagRelationGraphCard
                                                found_user={selected_user.clone()}
                                                api_base={cfg.backend_domain.clone()}
                                            />
                                        },
                                        TagView::Profile => html! {
                                            <TasteProfileCard
                                                found_user={selected_user.clone()}
                                                api_base={cfg.backend_domain.clone()}
                                            />
                                        },
                                    }
                                }
                            </div>
                        </>
                    }
                } else {
                    html! {}
                }
            }
        </div>
    }
}
