use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use yew::{
    Callback, Html, Properties, UseStateHandle, classes, function_component, html, use_effect_with,
    use_state,
};

use crate::models::{ProcessJobPhase, ProcessJobState, api_get, api_post};

/// How often (ms) to poll `/process/<id>/status` while a job is running.
const STATUS_POLL_INTERVAL_MS: i32 = 5000;

/// UI strings and CSS classes that differ between full and incremental mode.
struct ModeUi {
    url_mode: &'static str,
    label_idle: &'static str,
    label_running: &'static str, // "Scanning" / "Updating"
    label_retry: &'static str,
    done_msg: &'static str,
    failed_msg: &'static str,
    class_idle: &'static str,
    class_progress: &'static str,
    tooltip_idle: &'static str,
    tooltip_running: &'static str,
    log_prefix: &'static str,
}

impl ModeUi {
    fn for_mode(mode: &str) -> Self {
        match mode {
            "incremental" => Self {
                url_mode: "incremental",
                label_idle: "Update favourites",
                label_running: "Updating",
                label_retry: "Retry update",
                done_msg: "Update complete",
                failed_msg: "Update failed",
                class_idle: "btn btn-outline btn-info btn-sm",
                class_progress: "progress-info",
                tooltip_idle: "Fetch only new favourites since last import (cheap — no full rebuild)",
                tooltip_running: "Incremental update in progress — status refreshes every 5s",
                log_prefix: "incremental",
            },
            _ => Self {
                url_mode: "full",
                label_idle: "Full re-analysis",
                label_running: "Scanning",
                label_retry: "Retry (full)",
                done_msg: "Analysis complete",
                failed_msg: "Analysis failed",
                class_idle: "btn btn-outline btn-success btn-sm",
                class_progress: "progress-warning",
                tooltip_idle: "Full re-download + rebuild of this account's tag profile",
                tooltip_running: "Full scan in progress — status refreshes every 5s",
                log_prefix: "re-analyze",
            },
        }
    }
}

/// Build a terminal Failed state for when the initial POST itself fails.
fn build_failed_state(account_id: i64, error: &str) -> ProcessJobState {
    ProcessJobState {
        account_id: account_id as i32,
        phase: ProcessJobPhase::Failed,
        pages_total: 0,
        pages_done: 0,
        error: Some(error.to_string()),
        started_at: String::new(),
        finished_at: None,
        phases: Vec::new(),
        elapsed_secs: 0.0,
    }
}

#[derive(Properties, PartialEq)]
pub struct ReanalyzeButtonProps {
    pub account_id: i64,
    pub api_base: String,
    /// `"full"` (default) or `"incremental"`. Controls the `/process` mode
    /// and all associated labels, CSS classes, and tooltips.
    #[prop_or(String::from("full"))]
    pub mode: String,
    /// Fired when the process completes (Done phase) or fails.
    /// The parent can use this to show a toast / refresh data.
    pub on_complete: Callback<Result<String, String>>,
    /// External signal: another operation (e.g. the other mode button) is
    /// running. When true, the button is disabled regardless of local state.
    #[prop_or(false)]
    pub blocked: bool,
    /// Signal to the parent when this button starts or stops running.
    /// `true` on start, `false` on completion / failure.
    #[prop_or_default]
    pub on_running: Callback<bool>,
    /// Additional CSS class(es) to append to the button element.
    #[prop_or_default]
    pub class: String,
}

/// Self-contained process-launch button with live status polling.
///
/// Supports two modes via the `mode` prop:
///   * `"full"` — `POST /process/<id>?mode=full` (teardown + rebuild)
///   * `"incremental"` — `POST /process/<id>?mode=incremental` (new favs only)
///
/// Shows a spinner + progress bar while the job runs, fires `on_complete`
/// with the outcome. Respects `blocked` (external lock between two mode
/// buttons on the same account) and emits `on_running` for the same purpose.
#[function_component(ReanalyzeButton)]
pub fn reanalyze_button(props: &ReanalyzeButtonProps) -> Html {
    let ui = ModeUi::for_mode(&props.mode);
    let job_status: UseStateHandle<Option<ProcessJobState>> = use_state(|| None);
    let in_flight = use_state(|| false);

    let on_click = {
        let api_base = props.api_base.clone();
        let account_id = props.account_id;
        let blocked = props.blocked;
        let url_mode = ui.url_mode;
        let job_status = job_status.clone();
        let in_flight = in_flight.clone();
        let on_complete = props.on_complete.clone();
        let on_running = props.on_running.clone();

        Callback::from(move |_| {
            if *in_flight || blocked {
                return;
            }
            in_flight.set(true);
            on_running.emit(true);

            let url = format!("{}/process/{}?mode={url_mode}", api_base, account_id);
            let job_status = job_status.clone();
            let in_flight = in_flight.clone();
            let on_complete = on_complete.clone();
            let on_running = on_running.clone();

            wasm_bindgen_futures::spawn_local(async move {
                match api_post(&url).send().await {
                    Ok(resp) if resp.ok() => match resp.json::<ProcessJobState>().await {
                        Ok(s) => {
                            job_status.set(Some(s));
                        }
                        Err(e) => {
                            job_status.set(Some(build_failed_state(
                                account_id,
                                &format!("Bad /process response: {e}"),
                            )));
                            in_flight.set(false);
                            on_running.emit(false);
                            on_complete.emit(Err(format!("Bad /process response: {e}")));
                        }
                    },
                    Ok(resp) => {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        let msg = format!("Server error {status}: {text}");
                        job_status.set(Some(build_failed_state(account_id, &msg)));
                        in_flight.set(false);
                        on_running.emit(false);
                        on_complete.emit(Err(msg));
                    }
                    Err(e) => {
                        let msg = format!("Network error: {e}");
                        job_status.set(Some(build_failed_state(account_id, &msg)));
                        in_flight.set(false);
                        on_running.emit(false);
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
        let done_msg = ui.done_msg;
        let failed_msg = ui.failed_msg;
        let log_prefix = ui.log_prefix;
        let job_status = job_status.clone();
        let in_flight = in_flight.clone();
        let on_complete = props.on_complete.clone();
        let on_running = props.on_running.clone();

        let phase = job_status
            .as_ref()
            .map(|s| s.phase.clone())
            .unwrap_or(ProcessJobPhase::Done);

        let ts = job_status
            .as_ref()
            .map(|s| s.started_at.clone())
            .unwrap_or_default();

        use_effect_with(
            (phase.clone(), account_id, ts),
            move |(phase, _account_id, _ts)| {
                let mut handle: Option<i32> = None;
                let mut _closure: Option<Closure<dyn FnMut()>> = None;

                if matches!(phase, ProcessJobPhase::Running) {
                    let url = format!("{}/process/{}/status", api_base, account_id);
                    let job_status = job_status.clone();
                    let in_flight = in_flight.clone();
                    let on_complete = on_complete.clone();
                    let on_running = on_running.clone();
                    let done_msg = done_msg;
                    let failed_msg = failed_msg;
                    let log_prefix = log_prefix;

                    let cb = Closure::<dyn FnMut()>::new(move || {
                        let url = url.clone();
                        let job_status = job_status.clone();
                        let in_flight = in_flight.clone();
                        let on_complete = on_complete.clone();
                        let on_running = on_running.clone();
                        let done_msg = done_msg;
                        let failed_msg = failed_msg;
                        let log_prefix = log_prefix;
                        wasm_bindgen_futures::spawn_local(async move {
                            match api_get(&url).send().await {
                                Ok(resp) if resp.ok() => {
                                    match resp.json::<Option<ProcessJobState>>().await {
                                        Ok(Some(s)) => {
                                            let is_terminal = matches!(
                                                s.phase,
                                                ProcessJobPhase::Done | ProcessJobPhase::Failed,
                                            );
                                            job_status.set(Some(s.clone()));
                                            if is_terminal {
                                                in_flight.set(false);
                                                on_running.emit(false);
                                                match s.phase {
                                                    ProcessJobPhase::Done => {
                                                        on_complete.emit(Ok(done_msg.to_string()));
                                                    }
                                                    ProcessJobPhase::Failed => {
                                                        let msg = s.error.unwrap_or_else(|| {
                                                            failed_msg.to_string()
                                                        });
                                                        on_complete.emit(Err(msg));
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                        Ok(None) => {
                                            job_status.set(Some(ProcessJobState {
                                                account_id: account_id as i32,
                                                phase: ProcessJobPhase::Done,
                                                pages_total: 0,
                                                pages_done: 0,
                                                error: None,
                                                started_at: String::new(),
                                                finished_at: None,
                                                phases: Vec::new(),
                                                elapsed_secs: 0.0,
                                            }));
                                            in_flight.set(false);
                                            on_running.emit(false);
                                            on_complete.emit(Ok(done_msg.to_string()));
                                        }
                                        Err(e) => {
                                            web_sys::console::warn_1(
                                                &format!("{log_prefix} status parse error: {e}")
                                                    .into(),
                                            );
                                        }
                                    }
                                }
                                Ok(resp) => {
                                    web_sys::console::warn_1(
                                        &format!("{log_prefix} status HTTP {}", resp.status())
                                            .into(),
                                    );
                                }
                                Err(e) => {
                                    web_sys::console::warn_1(
                                        &format!("{log_prefix} status network error: {e}").into(),
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
                        && let Some(w) = web_sys::window()
                    {
                        w.clear_interval_with_handle(h);
                    }
                    drop(_closure);
                }
            },
        );
    }

    let is_running = matches!(
        job_status.as_ref().map(|s| s.phase.clone()),
        Some(ProcessJobPhase::Running)
    );
    let is_failed = matches!(
        job_status.as_ref().map(|s| s.phase.clone()),
        Some(ProcessJobPhase::Failed)
    );

    let label_running = ui.label_running;
    let label_retry = ui.label_retry;
    let label_idle = ui.label_idle;
    let label = if is_running {
        match job_status.as_ref() {
            Some(s) if s.pages_total > 0 => {
                format!("{label_running} {}/{}", s.pages_done, s.pages_total)
            }
            _ => "Starting…".to_string(),
        }
    } else if is_failed {
        label_retry.to_string()
    } else {
        label_idle.to_string()
    };

    let progress_pct = job_status
        .as_ref()
        .filter(|s| s.pages_total > 0 && matches!(s.phase, ProcessJobPhase::Running))
        .map(|s| (s.pages_done as f32 / s.pages_total as f32 * 100.0).clamp(0.0, 100.0));

    let class_running = "btn btn-outline btn-warning btn-sm";
    let class_failed = "btn btn-outline btn-error btn-sm";
    let class_idle = ui.class_idle;
    let btn_class = if is_running {
        class_running
    } else if is_failed {
        class_failed
    } else {
        class_idle
    };

    let class_progress = ui.class_progress;
    let tooltip_idle = ui.tooltip_idle;
    let tooltip_running = ui.tooltip_running;

    html! {
        <div class="w-full">
            <button
                class={classes!(btn_class, props.class.clone())}
                onclick={on_click}
                disabled={is_running || *in_flight || props.blocked}
                title={
                    if props.blocked { "Another operation is in progress" }
                    else if is_running { tooltip_running }
                    else { tooltip_idle }
                }
            >
                if is_running {
                    <span>
                        <span class="loading loading-spinner loading-sm me-1" role="status" aria-hidden="true"></span>
                        { label.clone() }
                    </span>
                } else {
                    { label.clone() }
                }
            </button>
            if let Some(pct) = progress_pct {
                <progress
                    class={format!("progress mt-1 {class_progress}")}
                    style="height: 4px;"
                    value={format!("{pct:.0}")}
                    max="100"
                />
            }
        </div>
    }
}
