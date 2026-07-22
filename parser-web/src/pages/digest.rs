use serde::de::DeserializeOwned;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::RequestInit;
use web_sys::{Request, RequestMode, Response, window};
use yew::prelude::*;

use crate::components::*;
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
    fn to_storage(self) -> &'static str {
        match self {
            GridType::Auto => "auto",
            GridType::Three => "3",
            GridType::Two => "2",
            GridType::One => "1",
        }
    }
    fn col_class(self) -> &'static str {
        match self {
            GridType::Auto => {
                "col-xs-6 col-sm-5 col-md-4 col-lg-3 col-xl-2 col-xxl-1 d-flex justify-content-center"
            }
            GridType::Three => "col-4 d-flex justify-content-center",
            GridType::Two => "col-6 d-flex justify-content-center",
            GridType::One => "col-12 d-flex justify-content-center",
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

#[function_component(DigestPage)]
pub fn digest_page() -> Html {
    let posts = use_state(Vec::<ScoredPost>::new);
    let is_loading = use_state(|| false);
    let error = use_state(|| Option::<String>::None);
    let selected_user = use_state(|| Option::<UserInfo>::None);
    let is_full = use_state(|| false);

    // Display settings — persisted to localStorage under `digest_*` keys so
    // they survive reloads but stay isolated from the feed page's prefs.
    let hide_saved = use_state(|| read_bool_local("digest_hide_saved", false));
    let show_breakdown = use_state(|| read_bool_local("digest_show_breakdown", true));
    let show_desc = use_state(|| read_bool_local("digest_show_desc", true));
    let show_metadata = use_state(|| read_bool_local("digest_show_metadata", false));
    let grid = use_state(|| GridType::from_storage(read_local("digest_grid_type")));

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
    {
        let v = *show_breakdown;
        use_effect_with(v, move |a: &bool| {
            write_local("digest_show_breakdown", &a.to_string());
            || ()
        });
    }
    {
        let v = *show_desc;
        use_effect_with(v, move |a: &bool| {
            write_local("digest_show_desc", &a.to_string());
            || ()
        });
    }
    {
        let v = *show_metadata;
        use_effect_with(v, move |a: &bool| {
            write_local("digest_show_metadata", &a.to_string());
            || ()
        });
    }
    {
        let g = *grid;
        use_effect_with(g, move |g: &GridType| {
            write_local("digest_grid_type", g.to_storage());
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
                    Ok(resp) if resp.ok() => {
                        match resp.json::<Option<ProcessJobState>>().await {
                            Ok(s) => process_status.set(s),
                            Err(e) => {
                                web_sys::console::warn_1(
                                    &format!("digest process status parse: {e}").into(),
                                );
                                process_status.set(None);
                            }
                        }
                    }
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

                let posts = posts.clone();
                let is_loading = is_loading.clone();
                let error = error.clone();
                spawn_local(async move {
                    match fetch_json::<Vec<ScoredPost>>(&url).await {
                        Ok(result) => {
                            posts.set(result);
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
        let account_id = process_status
            .as_ref()
            .map(|s| s.account_id)
            .unwrap_or(0);
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
                                            let was_previously_running =
                                                process_status
                                                    .as_ref()
                                                    .map(|old| old.phase == ProcessJobPhase::Running)
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

    let on_toggle_full = {
        let is_full = is_full.clone();
        Callback::from(move |_: MouseEvent| {
            is_full.set(!*is_full);
        })
    };

    let on_toggle_hide_saved = {
        let hide_saved = hide_saved.clone();
        Callback::from(move |_: Event| {
            hide_saved.set(!*hide_saved);
        })
    };
    let on_toggle_show_breakdown = {
        let show_breakdown = show_breakdown.clone();
        Callback::from(move |_: Event| {
            show_breakdown.set(!*show_breakdown);
        })
    };
    let on_toggle_show_desc = {
        let show_desc = show_desc.clone();
        Callback::from(move |_: Event| {
            show_desc.set(!*show_desc);
        })
    };

    let set_grid = |g: GridType| {
        let grid = grid.clone();
        Callback::from(move |_: MouseEvent| grid.set(g))
    };

    let col_class = grid.col_class();

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
            <div class="alert alert-info d-flex align-items-center gap-2 flex-wrap mb-3" role="status">
                <span class="spinner-border spinner-border-sm" aria-hidden="true"></span>
                <span>{ &label }</span>
                if progress_pct > 0.0 {
                    <div class="flex-grow-1" style="min-width: 120px;">
                        <div class="progress" role="progressbar" style="height: 6px;"
                            aria-valuenow={format!("{:.0}", progress_pct)}
                            aria-valuemin="0" aria-valuemax="100">
                            <div class="progress-bar progress-bar-striped progress-bar-animated bg-info"
                                style={format!("width: {:.1}%", progress_pct)} />
                        </div>
                    </div>
                }
                if elapsed > 0.0 {
                    <small class="text-muted">{ format!("{:.0}s elapsed", elapsed) }</small>
                }
            </div>
        }
    } else {
        html! {}
    };

    html! {
        <div class="container-fluid mt-3">
            <div class="d-flex align-items-center gap-2 mb-3 flex-wrap">
                <h2 class="mb-0 me-auto">{ "Daily Digest" }</h2>
                <SavedAccountsSelect
                    selected_user={selected_user.clone()}
                    is_loading={is_loading.clone()}
                />
                <button
                    class="btn btn-outline-primary"
                    onclick={on_refresh}
                    disabled={*is_loading || process_is_running}
                >
                    <i class="bi bi-arrow-clockwise" aria-hidden="true"></i>
                    { " Refresh" }
                </button>
                <button
                    class={classes!(
                        "btn",
                        if *is_full { "btn-primary" } else { "btn-outline-secondary" },
                    )}
                    onclick={on_toggle_full}
                    disabled={*is_loading || process_is_running}
                >
                    { if *is_full { "Full digest" } else { "Quick digest" } }
                </button>

                <div class="dropdown">
                    <button
                        class="btn btn-outline-secondary dropdown-toggle"
                        type="button"
                        data-bs-toggle="dropdown"
                        data-bs-auto-close="outside"
                        aria-expanded="false"
                        aria-label="Display settings"
                    >
                        <i class="bi bi-sliders" aria-hidden="true"></i>
                        { " Display" }
                    </button>
                    <ul class="dropdown-menu dropdown-menu-end p-3" style="min-width: 260px;">
                        <li class="mb-2">
                            <div class="form-check form-switch">
                                <input
                                    class="form-check-input"
                                    type="checkbox"
                                    id="digest-hide-saved"
                                    checked={*hide_saved}
                                    onchange={on_toggle_hide_saved}
                                />
                                <label class="form-check-label" for="digest-hide-saved">
                                    { "Hide already-saved posts" }
                                </label>
                            </div>
                            <small class="text-muted d-block">
                                { "Filter out posts already in your favourites." }
                            </small>
                        </li>
                        <li class="mb-2">
                            <div class="form-check form-switch">
                                <input
                                    class="form-check-input"
                                    type="checkbox"
                                    id="digest-show-breakdown"
                                    checked={*show_breakdown}
                                    onchange={on_toggle_show_breakdown}
                                />
                                <label class="form-check-label" for="digest-show-breakdown">
                                    { "Show score breakdown" }
                                </label>
                            </div>
                        </li>
                        <li class="mb-2">
                            <div class="form-check form-switch">
                                <input
                                    class="form-check-input"
                                    type="checkbox"
                                    id="digest-show-desc"
                                    checked={*show_desc}
                                    onchange={on_toggle_show_desc}
                                />
                                <label class="form-check-label" for="digest-show-desc">
                                    { "Show post descriptions" }
                                </label>
                            </div>
                        </li>
                        <li class="mb-2">
                            <div class="form-check form-switch">
                                <input
                                    class="form-check-input"
                                    type="checkbox"
                                    id="digest-show-metadata"
                                    checked={*show_metadata}
                                    onchange={{
                                        let show_metadata = show_metadata.clone();
                                        Callback::from(move |_: Event| show_metadata.set(!*show_metadata))
                                    }}
                                />
                                <label class="form-check-label" for="digest-show-metadata">
                                    { "Show file metadata" }
                                </label>
                            </div>
                        </li>
                        <li><hr class="dropdown-divider" /></li>
                        <li>
                            <small class="text-muted d-block mb-1">{ "Grid density" }</small>
                            <div class="btn-group btn-group-sm w-100" role="group" aria-label="Grid type">
                                <button
                                    type="button"
                                    class={classes!("btn","btn-outline-secondary", if *grid == GridType::Auto { "active" } else { "" })}
                                    aria-pressed={(*grid == GridType::Auto).to_string()}
                                    onclick={set_grid(GridType::Auto)}
                                >{ "Auto" }</button>
                                <button
                                    type="button"
                                    class={classes!("btn","btn-outline-secondary", if *grid == GridType::Three { "active" } else { "" })}
                                    aria-pressed={(*grid == GridType::Three).to_string()}
                                    onclick={set_grid(GridType::Three)}
                                >{ "3" }</button>
                                <button
                                    type="button"
                                    class={classes!("btn","btn-outline-secondary", if *grid == GridType::Two { "active" } else { "" })}
                                    aria-pressed={(*grid == GridType::Two).to_string()}
                                    onclick={set_grid(GridType::Two)}
                                >{ "2" }</button>
                                <button
                                    type="button"
                                    class={classes!("btn","btn-outline-secondary", if *grid == GridType::One { "active" } else { "" })}
                                    aria-pressed={(*grid == GridType::One).to_string()}
                                    onclick={set_grid(GridType::One)}
                                >{ "1" }</button>
                            </div>
                        </li>
                    </ul>
                </div>
            </div>

            if let Some(ref e) = *error {
                <div class="alert alert-danger" role="alert">{ e }</div>
            }

            { process_banner }

            if *is_loading {
                <div class="d-flex justify-content-center my-5">
                    <div class="spinner-border" role="status">
                        <span class="visually-hidden">{ "Loading..." }</span>
                    </div>
                </div>
            }

            if !*is_loading && !process_is_running && posts.is_empty() && error.is_none() {
                <div class="alert alert-info" role="alert">
                    { "No posts yet. Select an account above and wait for the digest to load." }
                </div>
            }

            <div class="row g-2">
                { for posts.iter().enumerate().map(|(i, sp)| {
                    let Some(cfg) = read_config_from_head() else {
                        return html! {};
                    };
                    html! {
                        <div class={col_class}>
                            <PostCard
                                post={std::rc::Rc::new(sp.post.clone())}
                                affinity={sp.score}
                                backend_url={cfg.backend_domain.clone()}
                                account_id={selected_user.as_ref().map(|u| u.id as i32).unwrap_or(0)}
                                session_id={String::new()}
                                position={i as i32}
                                breakdown={sp.breakdown.clone()}
                                show_desc={*show_desc}
                                show_metadata={*show_metadata}
                                show_breakdown={*show_breakdown}
                            />
                        </div>
                    }
                }) }
            </div>
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
        .map_err(|e| format!("Fetch promise rejected: {e:?}"))?;

    let resp: Response = resp_value
        .dyn_into()
        .map_err(|_| "Failed to cast Response".to_string())?;

    if !resp.ok() {
        let status = resp.status();
        let status_text = resp.status_text();
        return Err(format!("HTTP {status}: {status_text}"));
    }

    let text_promise = resp
        .text()
        .map_err(|e| format!("Failed to get text promise: {e:?}"))?;
    let text = wasm_bindgen_futures::JsFuture::from(text_promise)
        .await
        .map_err(|e| format!("Text future rejected: {e:?}"))?
        .as_string()
        .ok_or("Response text is not a string".to_string())?;

    serde_json::from_str(&text).map_err(|e| format!("JSON parse error: {e}"))
}
