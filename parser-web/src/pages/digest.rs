use serde::de::DeserializeOwned;
use std::collections::HashSet;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::RequestInit;
use web_sys::{Request, RequestMode, Response, window};
use yew::prelude::*;

use crate::components::{
    IconArrowClockwise, SavedAccountsSelect, render_post_grid, render_post_grid_skeleton,
    use_settings_tick,
};
use crate::models::*;
use crate::pages::UserInfo;

// Mirrors `feed::GridType` — kept local so the digest page can carry its
// own remembered density without coupling the two pages' settings.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GridType {
    Auto,
    Three,
    Two,
    One,
}

impl GridType {
    fn from_storage(s: Option<String>) -> Self {
        match s.as_deref() {
            Some("3") => GridType::Three,
            Some("2") => GridType::Two,
            Some("1") => GridType::One,
            _ => GridType::Auto,
        }
    }
    fn grid_class(self) -> &'static str {
        match self {
            GridType::Auto => {
                "grid grid-cols-1 sm:grid-cols-2 md:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5 gap-3"
            }
            GridType::Three => "grid grid-cols-2 sm:grid-cols-3 gap-3",
            GridType::Two => "grid grid-cols-2 gap-3",
            GridType::One => "grid grid-cols-1 gap-3",
        }
    }
}

fn read_local(key: &str) -> Option<String> {
    window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(key).ok().flatten())
}

fn read_bool_local(key: &str, default_val: bool) -> bool {
    read_local(key)
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(default_val)
}

fn write_local(key: &str, val: &str) {
    if let Some(s) = window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = s.set_item(key, val);
    }
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

#[function_component(DigestPage)]
pub fn digest_page() -> Html {
    let _settings_tick = use_settings_tick();
    let posts = use_state(Vec::<ScoredPost>::new);
    let is_loading = use_state(|| false);
    let error = use_state(|| Option::<String>::None);
    let selected_user = use_state(|| Option::<UserInfo>::None);
    let is_full = use_state(|| false);

    // Display settings — display toggles read unified settings_show_* keys
    // (falling back to old `digest_*` keys); hide_saved stays digest-local.
    let hide_saved = use_state(|| read_bool_local("digest_hide_saved", true));
    let show_breakdown = read_display_setting("breakdown", "digest_show_breakdown", true);
    let show_detailed_breakdown = read_display_setting(
        "detailed_breakdown",
        "digest_show_detailed_breakdown",
        false,
    );
    let show_desc = read_display_setting("desc", "digest_show_desc", true);
    let show_metadata = read_display_setting("metadata", "digest_show_metadata", false);
    let show_rating = read_display_setting("rating", "digest_show_rating", true);
    let show_affinity = read_display_setting("affinity", "digest_show_affinity", true);
    let show_score = read_display_setting("score", "digest_show_score", true);
    let show_post_number = read_display_setting("post_number", "digest_show_post_number", true);
    let grid = {
        let storage = || window().and_then(|w| w.local_storage().ok().flatten());
        let stored = storage()
            .and_then(|s| s.get_item("settings_grid_type").ok().flatten())
            .or_else(|| storage().and_then(|s| s.get_item("digest_grid_type").ok().flatten()));
        GridType::from_storage(stored)
    };

    // Bumped by the Refresh button to force a re-fetch even when no input
    // changed (e.g. the cache TTL hasn't expired but the user wants fresh
    // server-side regeneration to retry).
    let refresh_tick = use_state(|| 0u32);

    // Tracks whether a /process job is running for the selected account.
    // When set, the digest page shows a progress indicator and disables
    // controls (Refresh, mode toggle) until the job completes.
    let process_status = use_state(|| Option::<ProcessJobState>::None);

    // Persist each setting on change. Effects only fire when the dep value
    // differs from the previous render, so this is cheap.
    {
        let v = *hide_saved;
        use_effect_with(v, move |a: &bool| {
            write_local("digest_hide_saved", &a.to_string());
            || ()
        });
    }
    // Bootstrap the process status when the selected account changes.
    // The digest requires a processed profile, so we show a progress
    // indicator when /process is still running.
    {
        let api_base = (*selected_user).clone().map(|u| (u.id, u));
        let api_base_for_effect = api_base.clone();
        let process_status = process_status.clone();
        use_effect_with(api_base_for_effect, move |val| {
            let Some((account_id, _)) = val else {
                process_status.set(None);
                return;
            };
            let Some(cfg) = read_config_from_head() else {
                return;
            };
            let url = format!("{}/process/{}/status", cfg.backend_domain, account_id);
            let process_status = process_status.clone();
            spawn_local(async move {
                match api_get(&url).send().await {
                    Ok(resp) if resp.ok() => match resp.json::<Option<ProcessJobState>>().await {
                        Ok(s) => process_status.set(s),
                        Err(e) => {
                            web_sys::console::warn_1(
                                &format!("digest process status parse: {e}").into(),
                            );
                            process_status.set(None);
                        }
                    },
                    Ok(_) => process_status.set(None),
                    Err(e) => {
                        web_sys::console::warn_1(
                            &format!("digest process status network: {e}").into(),
                        );
                    }
                }
            });
        });
    }

    // Single source of truth for fetching: any change to user / mode /
    // hide_saved or a manual refresh tick triggers a re-fetch. This also
    // fixes the previous stale-capture bug where toggling Full digest read
    // the pre-toggle value of `is_full` from the callback closure.
    {
        let posts = posts.clone();
        let is_loading = is_loading.clone();
        let error = error.clone();
        let user = (*selected_user).clone();
        let full_val = *is_full;
        let hide_saved_val = *hide_saved;
        let tick = *refresh_tick;
        use_effect_with(
            (user, full_val, hide_saved_val, tick),
            move |(user, full, hide_saved, _tick)| {
                let Some(user) = user.clone() else {
                    return;
                };
                let Some(cfg) = read_config_from_head() else {
                    error.set(Some("App configuration failed to load.".to_string()));
                    return;
                };

                let mut params: Vec<&'static str> = Vec::new();
                if *full {
                    params.push("full=true");
                }
                if *hide_saved {
                    params.push("exclude_saved=true");
                }
                let qs = if params.is_empty() {
                    String::new()
                } else {
                    format!("?{}", params.join("&"))
                };
                let url = format!("{}/digest/{}{}", cfg.backend_domain, user.id, qs);

                // Clear any stale process-running message now that we're
                // fetching fresh data (the user may have waited for the job
                // to finish). Keep the process_status so the next poll can
                // still detect a new run triggered by another tab.
                is_loading.set(true);
                error.set(None);
                posts.set(Vec::new()); // hide old content while loading new

                let posts = posts.clone();
                let is_loading = is_loading.clone();
                let error = error.clone();
                spawn_local(async move {
                    match fetch_json::<Vec<ScoredPost>>(&url).await {
                        Ok(result) => {
                            // Deduplicate by post ID — the backend
                            // stratified_sample can still yield duplicates
                            // between the top/mid/trending/exploration
                            // strata.
                            let mut seen = HashSet::new();
                            let deduped: Vec<ScoredPost> = result
                                .into_iter()
                                .filter(|sp| seen.insert(sp.post.id))
                                .collect();
                            posts.set(deduped);
                            is_loading.set(false);
                        }
                        Err(e) => {
                            error.set(Some(e));
                            is_loading.set(false);
                        }
                    }
                });
            },
        );
    }

    // Poll while a /process job is Running. When it transitions to Done,
    // bump the refresh tick so the digest fetch reruns automatically.
    {
        let _api_base = (*selected_user).clone().map(|u| (u.id, u));
        let refresh_tick = refresh_tick.clone();
        let process_status = process_status.clone();
        let phase = process_status
            .as_ref()
            .map(|s| s.phase.clone())
            .unwrap_or(ProcessJobPhase::Done);
        let account_id = process_status.as_ref().map(|s| s.account_id).unwrap_or(0);
        let started_at = process_status
            .as_ref()
            .map(|s| s.started_at.clone())
            .unwrap_or_default();

        use_effect_with(
            (phase.clone(), account_id, started_at),
            move |(phase, account_id, _started_at)| {
                let mut handle: Option<i32> = None;
                let mut _closure: Option<Closure<dyn FnMut()>> = None;

                if matches!(phase, ProcessJobPhase::Running) && *account_id > 0 {
                    if let Some(cfg) = read_config_from_head() {
                        let url = format!("{}/process/{}/status", cfg.backend_domain, account_id);
                        let process_status = process_status.clone();
                        let refresh_tick = refresh_tick.clone();

                        let cb = Closure::<dyn FnMut()>::new(move || {
                            let url = url.clone();
                            let process_status = process_status.clone();
                            let refresh_tick = refresh_tick.clone();
                            spawn_local(async move {
                                match api_get(&url).send().await {
                                    Ok(resp) if resp.ok() => {
                                        match resp.json::<Option<ProcessJobState>>().await {
                                            Ok(Some(s)) => {
                                                let was_done = matches!(
                                                    s.phase,
                                                    ProcessJobPhase::Done | ProcessJobPhase::Failed,
                                                );
                                                let was_previously_running = process_status
                                                    .as_ref()
                                                    .map(|old| {
                                                        old.phase == ProcessJobPhase::Running
                                                    })
                                                    .unwrap_or(false);
                                                process_status.set(Some(s));
                                                // Auto-refresh digest when a
                                                // running job finishes.
                                                if was_done && was_previously_running {
                                                    refresh_tick.set(*refresh_tick + 1);
                                                }
                                            }
                                            Ok(None) => {
                                                process_status.set(None);
                                            }
                                            Err(e) => {
                                                web_sys::console::warn_1(
                                                    &format!("digest status parse: {e}").into(),
                                                );
                                            }
                                        }
                                    }
                                    Ok(resp) => {
                                        web_sys::console::warn_1(
                                            &format!("digest status HTTP {}", resp.status()).into(),
                                        );
                                    }
                                    Err(e) => {
                                        web_sys::console::warn_1(
                                            &format!("digest status network: {e}").into(),
                                        );
                                    }
                                }
                            });
                        });

                        if let Some(window) = web_sys::window()
                            && let Ok(h) = window
                                .set_interval_with_callback_and_timeout_and_arguments_0(
                                    cb.as_ref().unchecked_ref(),
                                    5000,
                                )
                        {
                            handle = Some(h);
                            _closure = Some(cb);
                        }
                    } else {
                        web_sys::console::warn_1(&"digest poll: no config".into());
                    }
                }

                move || {
                    if let Some(h) = handle
                        && let Some(w) = web_sys::window()
                    {
                        w.clear_interval_with_handle(h);
                    }
                    drop(_closure);
                }
            },
        );
    }

    let on_refresh = {
        let refresh_tick = refresh_tick.clone();
        Callback::from(move |_: MouseEvent| {
            refresh_tick.set(*refresh_tick + 1);
        })
    };

    let process_is_running = matches!(
        process_status.as_ref().map(|s| s.phase.clone()),
        Some(ProcessJobPhase::Running)
    );

    // Explicit tab selection for Quick/Full digest — clearer than a
    // single toggle button whose label flips.
    let on_select_quick = {
        let is_full = is_full.clone();
        Callback::from(move |_: MouseEvent| is_full.set(false))
    };
    let on_select_full = {
        let is_full = is_full.clone();
        Callback::from(move |_: MouseEvent| is_full.set(true))
    };

    let on_toggle_hide_saved = {
        let hide_saved = hide_saved.clone();
        Callback::from(move |_: Event| {
            hide_saved.set(!*hide_saved);
        })
    };

    // Compute the process-running banner outside the html! macro so we
    // can use let bindings without tripping the macro's element parser.
    let process_banner = if process_is_running {
        let (label, progress_pct, elapsed) = match process_status.as_ref() {
            Some(ps) => {
                let label = if ps.pages_total > 0 {
                    format!("Processing favourites {}/{}", ps.pages_done, ps.pages_total)
                } else {
                    "Starting analysis…".to_string()
                };
                let pct = if ps.pages_total > 0 {
                    (ps.pages_done as f32 / ps.pages_total as f32 * 100.0).clamp(0.0, 100.0)
                } else {
                    0.0
                };
                let elapsed = ps.elapsed_secs;
                (label, pct, elapsed)
            }
            None => ("Processing…".to_string(), 0.0, 0.0),
        };
        html! {
            <div class="alert alert-info flex flex-wrap items-center gap-2 mb-3" role="status">
                <span class="loading loading-spinner loading-sm" aria-hidden="true"></span>
                <span>{ &label }</span>
                if progress_pct > 0.0 {
                    <div class="flex-1" style="min-width: 120px;">
                        <progress class="progress progress-info" style="height: 6px;"
                            value={format!("{:.0}", progress_pct)}
                            max="100" />
                    </div>
                }
                if elapsed > 0.0 {
                    <span class="text-xs text-base-content/70">{ format!("{:.0}s elapsed", elapsed) }</span>
                }
            </div>
        }
    } else {
        html! {}
    };

    let backend_url = read_config_from_head()
        .map(|cfg| cfg.backend_domain)
        .unwrap_or_default();
    let account_id = selected_user
        .as_ref()
        .map(|u| u.id as i32)
        .unwrap_or_default();
    let posts_grid_html = render_post_grid(
        &posts,
        grid.grid_class(),
        &backend_url,
        account_id,
        "",
        0,
        show_rating,
        show_affinity,
        show_score,
        show_post_number,
        show_desc,
        show_metadata,
        show_breakdown,
        show_detailed_breakdown,
    );

    html! {
        <div class="container-fluid">
            <div class="flex flex-wrap items-center gap-2 mb-3">
                <h2 class="text-2xl font-semibold text-base-content mb-0 me-auto">{ "Daily Digest" }</h2>
                <SavedAccountsSelect
                    selected_user={selected_user.clone()}
                    is_loading={is_loading.clone()}
                />
                <button
                    class="btn btn-outline"
                    onclick={on_refresh}
                    disabled={*is_loading || process_is_running}
                >
                    <IconArrowClockwise />
                    { " Refresh" }
                </button>

                <div class="join" role="tablist" aria-label="Digest mode">
                    <button
                        type="button"
                        role="tab"
                        aria-selected={(!*is_full).to_string()}
                        class={classes!(
                            "btn", "btn-sm", "join-item",
                            if !*is_full { "btn-primary" } else { "btn-outline" },
                        )}
                        onclick={on_select_quick}
                        disabled={*is_loading || process_is_running}
                    >{ "Quick digest" }</button>
                    <button
                        type="button"
                        role="tab"
                        aria-selected={(*is_full).to_string()}
                        class={classes!(
                            "btn", "btn-sm", "join-item",
                            if *is_full { "btn-primary" } else { "btn-outline" },
                        )}
                        onclick={on_select_full}
                        disabled={*is_loading || process_is_running}
                    >{ "Full digest" }</button>
                </div>

                <label class="label cursor-pointer gap-2">
                    <span class="text-sm text-base-content">{ "Hide saved" }</span>
                    <input
                        type="checkbox"
                        class="toggle toggle-sm"
                        checked={*hide_saved}
                        onchange={on_toggle_hide_saved}
                        disabled={*is_loading || process_is_running}
                        aria-label="Hide already-saved posts"
                    />
                </label>
            </div>

            {
                if selected_user.is_some() {
                    let mode_hint = if *is_full {
                        "Full digest: personalized scoring over the whole catalog (slower)".to_string()
                    } else {
                        "Quick digest: stratified picks — top, trending, exploration, wildcard, recent".to_string()
                    };
                    html! {
                        <p class="text-xs text-base-content/60 -mt-2 mb-3">{ mode_hint }</p>
                    }
                } else { html!{} }
            }

            if let Some(ref e) = *error {
                <div class="alert alert-error flex justify-between items-center" role="alert" aria-live="polite">
                    <span>{ e }</span>
                    <button
                        type="button"
                        class="btn btn-sm btn-outline"
                        onclick={{
                            let refresh_tick = refresh_tick.clone();
                            Callback::from(move |_| refresh_tick.set(*refresh_tick + 1))
                        }}
                    >{ "Retry" }</button>
                </div>
            }

            { process_banner }

            {
                if *is_loading && posts.is_empty() {
                    render_post_grid_skeleton(grid.grid_class())
                } else { html!{} }
            }

            if !*is_loading && !process_is_running && posts.is_empty() && error.is_none() {
                <div class="alert alert-info" role="alert">
                    { "No posts yet. Select an account above and wait for the digest to load." }
                </div>
            }

            { posts_grid_html }
        </div>
    }
}

async fn fetch_json<T: DeserializeOwned>(url: &str) -> Result<T, String> {
    let window = window().ok_or("No window available".to_string())?;

    let opts = RequestInit::new();
    opts.set_method("GET");
    opts.set_mode(RequestMode::Cors);
    opts.set_credentials(web_sys::RequestCredentials::Include);

    let request = Request::new_with_str_and_init(url, &opts)
        .map_err(|e| format!("Failed to create request: {e:?}"))?;

    let resp_value = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| humanize_network_error(format!("{e:?}")))?;

    let resp: Response = resp_value
        .dyn_into()
        .map_err(|_| "Failed to cast Response".to_string())?;

    let status = resp.status();
    let text_promise = resp
        .text()
        .map_err(|e| humanize_network_error(format!("{e:?}")))?;
    let text = wasm_bindgen_futures::JsFuture::from(text_promise)
        .await
        .map_err(|e| humanize_network_error(format!("{e:?}")))?
        .as_string()
        .ok_or("Response text is not a string".to_string())?;

    if !resp.ok() {
        return Err(humanize_error_body(status, &text));
    }

    serde_json::from_str(&text)
        .map_err(|_| "The server returned invalid data. Please try again.".to_string())
}
