use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use yew::{
    Callback, Html, Properties, UseStateHandle, function_component, html, use_effect_with,
    use_state,
};

use crate::models::{
    ProcessJobPhase, ProcessJobState, api_get, api_post,
};

/// How often (ms) to poll `/process/<id>/status` while a job is running.
/// Shorter than the home-page poll (60s) because the user is actively
/// watching this button — they want to see progress move.
const STATUS_POLL_INTERVAL_MS: i32 = 5000;

#[derive(Properties, PartialEq)]
pub struct ReanalyzeButtonProps {
    pub account_id: i64,
    pub api_base: String,
    /// Fired when a re-analyse completes (Done phase) or fails.
    /// The parent can use this to show a toast / refresh data.
    pub on_complete: Callback<Result<String, String>>,
}

/// A self-contained "Re-analyse" button with live status polling.
/// Shows a spinner + progress bar while the job runs, then fires
/// `on_complete` with the outcome.
#[function_component(ReanalyzeButton)]
pub fn reanalyze_button(props: &ReanalyzeButtonProps) -> Html {
    let job_status: UseStateHandle<Option<ProcessJobState>> = use_state(|| None);
    let in_flight = use_state(|| false);

    let on_click = {
        let api_base = props.api_base.clone();
        let account_id = props.account_id;
        let job_status = job_status.clone();
        let in_flight = in_flight.clone();
        let on_complete = props.on_complete.clone();

        Callback::from(move |_| {
            if *in_flight {
                return;
            }
            in_flight.set(true);

            let url = format!("{}/process/{}?mode=full", api_base, account_id);
            let job_status = job_status.clone();
            let in_flight = in_flight.clone();
            let on_complete = on_complete.clone();

            wasm_bindgen_futures::spawn_local(async move {
                match api_post(&url).send().await {
                    Ok(resp) if resp.ok() => {
                        match resp.json::<ProcessJobState>().await {
                            Ok(s) => {
                                job_status.set(Some(s));
                            }
                            Err(e) => {
                                job_status.set(Some(ProcessJobState {
                                    account_id: account_id as i32,
                                    phase: ProcessJobPhase::Failed,
                                    pages_total: 0,
                                    pages_done: 0,
                                    error: Some(format!("Bad /process response: {e}")),
                                    started_at: String::new(),
                                    finished_at: None,
                                    phases: Vec::new(),
                                    elapsed_secs: 0.0,
                                }));
                                in_flight.set(false);
                                on_complete.emit(Err(format!("Bad /process response: {e}")));
                            }
                        }
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        let msg = format!("Server error {status}: {text}");
                        job_status.set(Some(ProcessJobState {
                            account_id: account_id as i32,
                            phase: ProcessJobPhase::Failed,
                            pages_total: 0,
                            pages_done: 0,
                            error: Some(msg.clone()),
                            started_at: String::new(),
                            finished_at: None,
                            phases: Vec::new(),
                            elapsed_secs: 0.0,
                        }));
                        in_flight.set(false);
                        on_complete.emit(Err(msg));
                    }
                    Err(e) => {
                        let msg = format!("Network error: {e}");
                        job_status.set(Some(ProcessJobState {
                            account_id: account_id as i32,
                            phase: ProcessJobPhase::Failed,
                            pages_total: 0,
                            pages_done: 0,
                            error: Some(msg.clone()),
                            started_at: String::new(),
                            finished_at: None,
                            phases: Vec::new(),
                            elapsed_secs: 0.0,
                        }));
                        in_flight.set(false);
                        on_complete.emit(Err(msg));
                    }
                }
            });
        })
    };

    // Poll while a job is Running. Cleans up the interval when the
    // component unmounts or the phase changes from Running.
    {
        let api_base = props.api_base.clone();
        let account_id = props.account_id;
        let job_status = job_status.clone();
        let in_flight = in_flight.clone();
        let on_complete = props.on_complete.clone();

        let phase = job_status
            .as_ref()
            .map(|s| s.phase.clone())
            .unwrap_or(ProcessJobPhase::Done);

        let ts = job_status
            .as_ref()
            .map(|s| s.started_at.clone())
            .unwrap_or_default();

        use_effect_with((phase.clone(), account_id, ts), move |(phase, _account_id, _ts)| {
            let mut handle: Option<i32> = None;
            let mut _closure: Option<Closure<dyn FnMut()>> = None;

            if matches!(phase, ProcessJobPhase::Running) {
                let url = format!("{}/process/{}/status", api_base, account_id);
                let job_status = job_status.clone();
                let in_flight = in_flight.clone();
                let on_complete = on_complete.clone();

                let cb = Closure::<dyn FnMut()>::new(move || {
                    let url = url.clone();
                    let job_status = job_status.clone();
                    let in_flight = in_flight.clone();
                    let on_complete = on_complete.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        match api_get(&url).send().await {
                            Ok(resp) if resp.ok() => {
                                match resp.json::<Option<ProcessJobState>>().await {
                                    Ok(Some(s)) => {
                                        let is_terminal = matches!(s.phase, ProcessJobPhase::Done | ProcessJobPhase::Failed);
                                        job_status.set(Some(s.clone()));
                                        if is_terminal {
                                            in_flight.set(false);
                                            match s.phase {
                                                ProcessJobPhase::Done => {
                                                    on_complete.emit(Ok("Analysis complete".to_string()));
                                                }
                                                ProcessJobPhase::Failed => {
                                                    let msg = s.error.unwrap_or_else(|| "Analysis failed".to_string());
                                                    on_complete.emit(Err(msg));
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    Ok(None) => {
                                        // Job disappeared from registry — treat as Done.
                                        job_status.set(Some(ProcessJobState {
                                            account_id: account_id as i32,
                                            phase: ProcessJobPhase::Done,
                                            pages_total: 0, pages_done: 0,
                                            error: None,
                                            started_at: String::new(),
                                            finished_at: None,
                                            phases: Vec::new(),
                                            elapsed_secs: 0.0,
                                        }));
                                        in_flight.set(false);
                                        on_complete.emit(Ok("Analysis complete".to_string()));
                                    }
                                    Err(e) => {
                                        web_sys::console::warn_1(
                                            &format!("re-analyze status parse error: {e}").into(),
                                        );
                                    }
                                }
                            }
                            Ok(resp) => {
                                web_sys::console::warn_1(
                                    &format!("re-analyze status HTTP {}", resp.status()).into(),
                                );
                            }
                            Err(e) => {
                                web_sys::console::warn_1(
                                    &format!("re-analyze status network error: {e}").into(),
                                );
                            }
                        }
                    });
                });

                if let Some(window) = web_sys::window()
                    && let Ok(h) = window
                        .set_interval_with_callback_and_timeout_and_arguments_0(
                            cb.as_ref().unchecked_ref(),
                            STATUS_POLL_INTERVAL_MS,
                        )
                    {
                        handle = Some(h);
                        _closure = Some(cb);
                    }
            }

            move || {
                if let Some(h) = handle
                    && let Some(w) = web_sys::window() {
                        w.clear_interval_with_handle(h);
                    }
                drop(_closure);
            }
        });
    }

    let is_running = matches!(
        job_status.as_ref().map(|s| s.phase.clone()),
        Some(ProcessJobPhase::Running)
    );
    let is_failed = matches!(
        job_status.as_ref().map(|s| s.phase.clone()),
        Some(ProcessJobPhase::Failed)
    );

    let label = if is_running {
        match job_status.as_ref() {
            Some(s) if s.pages_total > 0 => {
                format!("Analyzing {}/{}", s.pages_done, s.pages_total)
            }
            _ => "Starting…".to_string(),
        }
    } else if is_failed {
        "Retry".to_string()
    } else {
        "Re-analyze".to_string()
    };

    let progress_pct = job_status
        .as_ref()
        .filter(|s| s.pages_total > 0 && matches!(s.phase, ProcessJobPhase::Running))
        .map(|s| (s.pages_done as f32 / s.pages_total as f32 * 100.0).clamp(0.0, 100.0));

    let btn_class = if is_running {
        "btn btn-outline-warning btn-sm"
    } else if is_failed {
        "btn btn-outline-danger btn-sm"
    } else {
        "btn btn-outline-success btn-sm"
    };

    html! {
        <div>
            <button
                class={btn_class}
                onclick={on_click}
                disabled={is_running || *in_flight}
                title={if is_running { "Analysis in progress — status refreshes every 5s" } else { "Re-analyse this account's favourites to refresh recommendations" }}
            >
                if is_running {
                    <span>
                        <span class="spinner-border spinner-border-sm me-1" role="status" aria-hidden="true"></span>
                        { label.clone() }
                    </span>
                } else {
                    { label.clone() }
                }
            </button>
            if let Some(pct) = progress_pct {
                <div
                    class="progress mt-1"
                    role="progressbar"
                    aria-valuenow={format!("{pct:.0}")}
                    aria-valuemin="0"
                    aria-valuemax="100"
                    style="height: 4px;"
                >
                    <div
                        class="progress-bar progress-bar-striped progress-bar-animated bg-warning"
                        style={format!("width: {pct:.1}%")}
                    />
                </div>
            }
        </div>
    }
}
