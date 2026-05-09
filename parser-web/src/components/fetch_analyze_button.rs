use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use yew::{
    Callback, Html, MouseEvent, Properties, UseStateHandle, function_component, html,
    use_effect_with, use_state,
};

use crate::models::{
    ProcessJobPhase, ProcessJobState, api_get, api_post, humanize_error_body,
};
use crate::pages::{TagCount, UserInfo};

const STATUS_POLL_INTERVAL_MS: i32 = 1500;

#[derive(Properties, PartialEq)]
pub struct AnalyzeButtonProps {
    pub found_user: UseStateHandle<Option<UserInfo>>,
    pub error: UseStateHandle<Option<String>>,
    pub api_base: String,
    pub tag_count: UseStateHandle<Vec<TagCount>>,
    pub is_loading: UseStateHandle<bool>,
}

#[function_component(FetchAnalyzeButton)]
pub fn fetch_analyze_button(props: &AnalyzeButtonProps) -> Html {
    let is_fetching = use_state(|| false);
    let job_status: UseStateHandle<Option<ProcessJobState>> = use_state(|| None);

    let fetch_tags = make_fetch_tags(
        props.api_base.clone(),
        props.found_user.clone(),
        props.tag_count.clone(),
        props.error.clone(),
        is_fetching.clone(),
        props.is_loading.clone(),
    );

    // Bootstrap the existing job state when the user changes — so a refresh
    // mid-run still shows the running indicator and resumes polling.
    {
        let api_base = props.api_base.clone();
        let job_status = job_status.clone();
        let error = props.error.clone();
        use_effect_with((*props.found_user).clone(), move |user| {
            let Some(user) = user.clone() else {
                job_status.set(None);
                return;
            };
            let _ = error;
            let url = format!("{}/process/{}/status", api_base, user.id);
            let job_status = job_status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(resp) = api_get(&url).send().await {
                    if resp.ok() {
                        if let Ok(s) = resp.json::<Option<ProcessJobState>>().await {
                            job_status.set(s);
                        }
                    }
                }
            });
        });
    }

    let analyze_tags = {
        let api_base = props.api_base.clone();
        let found_user = props.found_user.clone();
        let job_status = job_status.clone();
        let is_loading = props.is_loading.clone();
        let error = props.error.clone();

        Callback::from(move |_| {
            let Some(user) = (*found_user).clone() else {
                error.set(Some("No user selected".to_string()));
                return;
            };

            is_loading.set(true);
            error.set(None);

            let url = format!("{}/process/{}", api_base, user.id);
            let job_status = job_status.clone();
            let is_loading = is_loading.clone();
            let error = error.clone();

            wasm_bindgen_futures::spawn_local(async move {
                match api_post(&url).send().await {
                    Ok(resp) if resp.ok() => match resp.json::<ProcessJobState>().await {
                        Ok(s) => {
                            // Optimistic running state until the first poll
                            // refreshes it; if the backend returned an
                            // existing state we just overwrite with that.
                            job_status.set(Some(s));
                            error.set(None);
                        }
                        Err(e) => {
                            error.set(Some(format!("Bad /process response: {e}")));
                            is_loading.set(false);
                        }
                    },
                    Ok(resp) => {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        web_sys::console::error_1(
                            &format!("/process failed (HTTP {status}): {text}").into(),
                        );
                        error.set(Some(humanize_error_body(status, &text)));
                        is_loading.set(false);
                    }
                    Err(e) => {
                        error.set(Some(format!("Processing error: {e}")));
                        is_loading.set(false);
                    }
                }
            });
        })
    };

    // Poll while a job is running. The cleanup closure clears the interval
    // both when the job finishes and when the component unmounts.
    {
        let api_base = props.api_base.clone();
        let found_user = props.found_user.clone();
        let job_status = job_status.clone();
        let is_loading = props.is_loading.clone();
        let error = props.error.clone();
        let fetch_tags_for_finish = fetch_tags.clone();

        let phase = job_status
            .as_ref()
            .map(|s| s.phase.clone())
            .unwrap_or(ProcessJobPhase::Done);
        let user_id = found_user.as_ref().map(|u| u.id);

        use_effect_with((phase, user_id), move |(phase, user_id)| {
            let mut handle: Option<i32> = None;
            let mut _closure: Option<Closure<dyn FnMut()>> = None;

            if matches!(phase, ProcessJobPhase::Running) {
                if let Some(uid) = user_id {
                    {
                        let url = format!("{}/process/{}/status", api_base, uid);
                        let job_status = job_status.clone();

                        let cb = Closure::<dyn FnMut()>::new(move || {
                            let url = url.clone();
                            let job_status = job_status.clone();
                            wasm_bindgen_futures::spawn_local(async move {
                                if let Ok(resp) = api_get(&url).send().await {
                                    if resp.ok() {
                                        if let Ok(s) =
                                            resp.json::<Option<ProcessJobState>>().await
                                        {
                                            job_status.set(s);
                                        }
                                    }
                                }
                            });
                        });

                        if let Some(window) = web_sys::window() {
                            if let Ok(h) = window
                                .set_interval_with_callback_and_timeout_and_arguments_0(
                                    cb.as_ref().unchecked_ref(),
                                    STATUS_POLL_INTERVAL_MS,
                                )
                            {
                                handle = Some(h);
                            }
                        }
                        _closure = Some(cb);
                    }
                }
            } else if matches!(phase, ProcessJobPhase::Done) {
                // Job completed since the previous render — refresh tag
                // counts and clear the loading state.
                let cb_evt = MouseEvent::new("click").ok();
                if let Some(evt) = cb_evt {
                    fetch_tags_for_finish.emit(evt);
                }
                is_loading.set(false);
                error.set(None);
            } else if matches!(phase, ProcessJobPhase::Failed) {
                if let Some(s) = job_status.as_ref() {
                    if let Some(err) = &s.error {
                        error.set(Some(err.clone()));
                    } else {
                        error.set(Some("Processing failed".to_string()));
                    }
                }
                is_loading.set(false);
            }

            move || {
                if let Some(h) = handle {
                    if let Some(w) = web_sys::window() {
                        w.clear_interval_with_handle(h);
                    }
                }
                drop(_closure);
            }
        });
    }

    let phase_running = matches!(
        job_status.as_ref().map(|s| s.phase.clone()),
        Some(ProcessJobPhase::Running)
    );
    let busy = phase_running || *is_fetching;

    let label = if phase_running {
        match job_status.as_ref() {
            Some(s) if s.pages_total > 0 => {
                format!("Analyzing {}/{}", s.pages_done, s.pages_total)
            }
            _ => "Starting…".to_string(),
        }
    } else if *is_fetching {
        "Loading…".to_string()
    } else if props.tag_count.is_empty() {
        "Analyze Tags".to_string()
    } else {
        "Re-analyze Tags".to_string()
    };

    let progress_pct = job_status
        .as_ref()
        .filter(|s| s.pages_total > 0 && phase_running)
        .map(|s| (s.pages_done as f32 / s.pages_total as f32 * 100.0).clamp(0.0, 100.0));

    let no_user = props.found_user.is_none();
    let disabled = *props.is_loading || no_user;
    let title = if no_user {
        "Pick or look up an account first"
    } else if *props.is_loading {
        "A request is already in flight"
    } else {
        ""
    };

    html! {
        <div class="d-grid gap-2 mb-4">
            <button
                class="btn btn-warning"
                onclick={analyze_tags}
                disabled={disabled}
                aria-disabled={disabled.to_string()}
                aria-busy={busy.to_string()}
                title={title}
            >
                { if busy {
                    html! {
                        <span>
                            <span class="spinner-border spinner-border-sm me-2" role="status" aria-hidden="true"></span>
                            { label.clone() }
                        </span>
                    }
                } else {
                    html! { { label.clone() } }
                }}
            </button>
            if let Some(pct) = progress_pct {
                <div class="progress" role="progressbar" aria-valuenow={format!("{pct:.0}")} aria-valuemin="0" aria-valuemax="100" style="height: 6px;">
                    <div class="progress-bar progress-bar-striped progress-bar-animated bg-warning" style={format!("width: {pct:.1}%")} />
                </div>
            }
            <small class="text-muted">
                { "Scans your favourites on e621 and builds a tag profile used for recommendations. Long-running accounts continue in the background even if you close this tab." }
            </small>
        </div>
    }
}

fn make_fetch_tags(
    api_base: String,
    found_user: UseStateHandle<Option<UserInfo>>,
    tag_count: UseStateHandle<Vec<TagCount>>,
    error: UseStateHandle<Option<String>>,
    is_fetching: UseStateHandle<bool>,
    is_loading: UseStateHandle<bool>,
) -> Callback<MouseEvent> {
    Callback::from(move |_| {
        if found_user.is_none() {
            error.set(Some("No user selected".to_string()));
            return;
        }

        let user_id = found_user.as_ref().unwrap().id;
        let url = format!("{}/account/{}/tag_counts", api_base, user_id);

        let tag_count = tag_count.clone();
        let error = error.clone();
        let is_fetching = is_fetching.clone();
        let is_loading = is_loading.clone();

        is_fetching.set(true);
        is_loading.set(true);

        wasm_bindgen_futures::spawn_local(async move {
            match api_get(&url).send().await {
                Ok(response) => {
                    if response.ok() {
                        match response.json::<Vec<TagCount>>().await {
                            Ok(counts) => {
                                tag_count.set(counts);
                                error.set(None);
                            }
                            Err(e) => {
                                error.set(Some(format!("Failed to parse tag data: {e}")));
                            }
                        }
                    } else if response.status() != 404 {
                        let status = response.status();
                        let text = response
                            .text()
                            .await
                            .unwrap_or_else(|_| "Unknown error".into());
                        error.set(Some(format!("Error {status}: {text}")));
                    }
                }
                Err(e) => {
                    error.set(Some(format!("Network error: {e}")));
                }
            }
            is_fetching.set(false);
            is_loading.set(false);
        });
    })
}
