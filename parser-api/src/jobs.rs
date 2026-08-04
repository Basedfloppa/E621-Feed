//! Background-job tracking for long-running tasks (currently `/process`).
//!
//! State lives in-process: a `RwLock<HashMap<account_id, ProcessJobState>>`.
//! Survives across requests but not server restarts — that's intentional. If
//! a job dies along with the process, the user just retries.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rocket::serde::Serialize;
use schemars::JsonSchema;

/// A single recorded phase in a process pipeline (serializable version).
#[derive(Debug, Serialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct JobPhaseRecord {
    pub name: String,
    /// Elapsed milliseconds since the previous phase (or since start for the first).
    pub elapsed_ms: f64,
}

#[derive(Debug, Serialize, Clone, Copy, JsonSchema, PartialEq, Eq)]
#[serde(crate = "rocket::serde", rename_all = "snake_case")]
pub enum ProcessJobPhase {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Serialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct ProcessJobState {
    pub account_id: i32,
    pub phase: ProcessJobPhase,
    pub pages_total: i32,
    pub pages_done: i32,
    pub error: Option<String>,
    #[schemars(with = "String", description = "RFC3339 timestamp")]
    pub started_at: DateTime<Utc>,
    #[schemars(with = "Option<String>", description = "RFC3339 timestamp")]
    pub finished_at: Option<DateTime<Utc>>,
    /// Pipeline phase timing records (populated when `perf_metrics` is enabled).
    #[serde(default)]
    pub phases: Vec<JobPhaseRecord>,
    /// Total elapsed seconds since the process started.
    pub elapsed_secs: f64,
}

pub enum BeginResult {
    Started(ProcessJobState),
    AlreadyRunning(ProcessJobState),
}

static JOBS: OnceLock<RwLock<HashMap<i32, ProcessJobState>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<i32, ProcessJobState>> {
    JOBS.get_or_init(|| RwLock::new(HashMap::new()))
}

#[must_use]
pub fn get_state(account_id: i32) -> Option<ProcessJobState> {
    registry().read().ok()?.get(&account_id).cloned()
}

/// Whether any account currently has a running `/process` job. Used to
/// gate the one-shot cooccurrence backfill: its global pass shares the same
/// `tag_cooccurrence` increments as live ingest, so running both together
/// would double-count pairs.
#[must_use]
pub fn any_running() -> bool {
    registry()
        .read()
        .map(|m| m.values().any(|s| s.phase == ProcessJobPhase::Running))
        .unwrap_or(false)
}

#[must_use]
pub fn try_begin(account_id: i32) -> BeginResult {
    let mut map = registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = map.get(&account_id)
        && existing.phase == ProcessJobPhase::Running
    {
        return BeginResult::AlreadyRunning(existing.clone());
    }
    let state = ProcessJobState {
        account_id,
        phase: ProcessJobPhase::Running,
        pages_total: 0,
        pages_done: 0,
        error: None,
        started_at: Utc::now(),
        finished_at: None,
        phases: Vec::new(),
        elapsed_secs: 0.0,
    };
    map.insert(account_id, state.clone());
    BeginResult::Started(state)
}

pub fn set_pages_total(account_id: i32, total: i32) {
    if let Ok(mut map) = registry().write()
        && let Some(s) = map.get_mut(&account_id)
    {
        s.pages_total = total;
    }
}

pub fn record_page_done(account_id: i32) {
    if let Ok(mut map) = registry().write()
        && let Some(s) = map.get_mut(&account_id)
    {
        s.pages_done += 1;
    }
}

/// Drop old Done/Failed entries. Running jobs older than
/// `runtime.jobs_running_timeout_secs` are also evicted (guard against
/// zombie jobs whose tokio task was cancelled). Done/Failed retention comes
/// from `runtime.jobs_finished_retain_secs`. Returns `(before, after)` for
/// logging.
#[must_use]
pub fn prune_finished_jobs() -> (usize, usize) {
    let cfg = crate::models::cfg();
    let retain_secs = cfg.runtime.jobs_finished_retain_secs.max(0);
    let running_timeout_secs = cfg.runtime.jobs_running_timeout_secs.max(60);
    let mut map = registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = map.len();
    let cutoff = Utc::now() - ChronoDuration::seconds(retain_secs);
    let running_cutoff = Utc::now() - ChronoDuration::seconds(running_timeout_secs);
    map.retain(|_, s| {
        if matches!(s.phase, ProcessJobPhase::Running) {
            // Evict zombie Running jobs stuck past the timeout.
            return s.started_at > running_cutoff;
        }
        s.finished_at.is_none_or(|f| f > cutoff)
    });
    let after = map.len();
    (before, after)
}

/// Record a pipeline phase timing for a running job. Idempotent — silently
/// no-ops if the job doesn't exist or isn't running.
pub fn record_phase(account_id: i32, name: impl Into<String>, elapsed_ms: f64) {
    if let Ok(mut map) = registry().write()
        && let Some(s) = map.get_mut(&account_id)
        && s.phase == ProcessJobPhase::Running
    {
        s.phases.push(JobPhaseRecord {
            name: name.into(),
            elapsed_ms,
        });
        s.elapsed_secs = (Utc::now() - s.started_at).num_milliseconds() as f64 / 1000.0;
    }
}

pub fn finish(account_id: i32, result: Result<(), String>) {
    if let Ok(mut map) = registry().write()
        && let Some(s) = map.get_mut(&account_id)
    {
        match result {
            Ok(()) => {
                s.phase = ProcessJobPhase::Done;
                s.error = None;
            }
            Err(e) => {
                s.phase = ProcessJobPhase::Failed;
                s.error = Some(e);
            }
        }
        s.finished_at = Some(Utc::now());
    }
}
