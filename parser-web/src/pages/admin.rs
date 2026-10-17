//! Operator console — `/admin` (TODO §3.2).
//!
//! The console is a view over the *instance*. There is no separate login screen,
//! because the `HttpOnly` owner cookie already *is* the identity — but the gate
//! is the administration grant, not the instance claim and not a link count:
//! `admin_entry_visible` hides the entry unless this browser linked
//! `[runtime].admin_user` with a key matching `[runtime].admin_api`, and
//! `GET /api/admin/overview` answers 403 otherwise. What the console is for is
//! *acting* on the instance — run `/process`, see its progress, unlink, and (the
//! first real consumer of the `BucketOverride` machinery) pin an account to an
//! A/B arm instead of living with the hash assignment.
//!
//! Gating happens twice, deliberately: the nav entry is hidden for a browser that
//! does not operate the instance (`admin_entry_visible`), and the page itself
//! explains the situation rather than rendering an empty table, so a bookmark or
//! a shared URL does not show a broken screen.

use gloo_timers::callback::Interval;
use reqwasm::http::Request;
use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlSelectElement, window};
use yew::prelude::*;

use super::catalog::human_bytes;
use crate::models::*;

/// How often the console re-reads the overview while a `/process` job runs.
/// A running job is the only thing on this page that changes on its own, and
/// it reports page progress, so a slow poll is enough to look live without
/// turning an idle console into a request generator.
const POLL_MS: u32 = 3000;

/// Accounts per request. Matches the server's default page size; the console
/// passes it explicitly so the "showing N of M" line cannot drift from what
/// was actually asked for.
const PAGE_LIMIT: i64 = 100;

/// The two asks, split by whether the administration tier is still open — the
/// same distinction the catalog makes.
///
/// The console is not behind the instance claim, and saying that it is would
/// send the reader to `[instance].owner_secret`, which does not open it. While
/// nobody operates the instance the credential is worth naming; once somebody
/// does, the link that would grant it is refused, so the reader is told who
/// holds it instead of being invited to try.
fn empty_admin_notice(admin_available: bool) -> (&'static str, &'static str) {
    if admin_available {
        (
            "This browser does not operate the instance",
            "The console shows instance state — the media queue, the job registry, the bucket \
             pins — so it takes the [runtime].admin_api key from config.toml, proved in the \
             owner claim on the settings page. The instance claim does not open it.",
        )
    } else {
        (
            "Another browser operates this instance",
            "The console takes [runtime].admin_user linked with the [runtime].admin_api key, and \
             that account is already linked under the current key.",
        )
    }
}

/// The operator reached the console and the instance has no accounts yet.
///
/// Deliberately a *different* card from [`empty_admin_notice`]: the console is
/// only reachable as an operator, so an empty table means an empty instance.
/// Telling the operator "you do not operate this" would be a lie, and telling
/// them to link what they already linked would be a loop.
///
/// No call to action. The account-linking form is not advertised anywhere — the
/// console is reachable only once that link exists, so a button here would be
/// unreachable for everyone who could still use it, and an advertisement to
/// everyone who cannot.
fn empty_instance_card() -> Html {
    html! {
        <>
            <h2 class="card-title text-xl">{ "No accounts on this instance yet" }</h2>
            <p class="text-sm text-base-content/70">
                { "The console covers every account on the instance. Accounts appear here as they \
                   are collected, with their library size and their /process state." }
            </p>
        </>
    }
}

/// Fire one owner action, report what the server said, and re-read the table.
///
/// Every button here has the same shape — a request scoped to one account, an
/// outcome worth reporting, a refreshed snapshot — so they share one
/// implementation and differ only in the request and the success wording.
/// `busy` holds the account whose row is waiting, so only that row's buttons
/// go quiet.
fn spawn_action(
    request: Request,
    account_id: i32,
    success: String,
    busy: UseStateHandle<Option<i32>>,
    message: UseStateHandle<String>,
    is_error: UseStateHandle<bool>,
    tick: UseStateHandle<u32>,
) {
    spawn_local(async move {
        busy.set(Some(account_id));
        message.set(String::new());
        is_error.set(false);
        let outcome = match request.send().await {
            Ok(resp) if resp.ok() => Ok(success),
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(humanize_error_body(status, &body))
            }
            Err(e) => Err(humanize_network_error(e)),
        };
        match outcome {
            Ok(text) => {
                message.set(text);
                is_error.set(false);
            }
            Err(text) => {
                message.set(text);
                is_error.set(true);
            }
        }
        busy.set(None);
        tick.set(tick.wrapping_add(1));
    });
}

#[function_component(AdminPage)]
pub fn admin_page() -> Html {
    let backend_url = read_config_from_head()
        .map(|cfg| cfg.backend_domain)
        .unwrap_or_default();

    // Same source the nav gate reads, so the card the page shows and the menu
    // entry it hides can never disagree about who operates the instance. Needed
    // separately from the overview because a 403 carries no body to read it from.
    let ownership = use_ownership(&backend_url);
    let admin_available = ownership.as_ref().is_some_and(|s| s.admin_available);

    let overview = use_state(|| Option::<AdminOverview>::None);
    let load_error = use_state(|| Option::<String>::None);
    // The API answers 403 without the instance claim. That is an expected state
    // with its own card, not an error banner: a visitor who does not own the
    // instance is being told how to, not shown a failure.
    let forbidden = use_state(|| false);
    let loaded = use_state(|| false);
    let message = use_state(String::new);
    let is_error = use_state(|| false);
    let busy = use_state(|| Option::<i32>::None);
    let mode = use_state(|| "auto".to_string());
    let offset = use_state(|| 0i64);
    // Bumped by every action and by the poller: the effect below re-reads the
    // overview whenever it changes.
    let tick = use_state(|| 0u32);

    {
        let overview = overview.clone();
        let load_error = load_error.clone();
        let forbidden = forbidden.clone();
        let loaded = loaded.clone();
        let backend_url = backend_url.clone();
        use_effect_with(
            (backend_url.clone(), *tick, *offset),
            move |(backend_url, _, offset)| {
                let backend_url = backend_url.clone();
                let offset = *offset;
                let forbidden = forbidden.clone();
                spawn_local(async move {
                    let url =
                        format!("{backend_url}/admin/overview?limit={PAGE_LIMIT}&offset={offset}");
                    match api_get(&url).send().await {
                        Ok(resp) if resp.ok() => match resp.json::<AdminOverview>().await {
                            Ok(fresh) => {
                                overview.set(Some(fresh));
                                load_error.set(None);
                                forbidden.set(false);
                            }
                            Err(_) => load_error.set(Some(
                                "The console data could not be read. Try again.".to_string(),
                            )),
                        },
                        Ok(resp) if resp.status() == 403 => {
                            forbidden.set(true);
                            load_error.set(None);
                        }
                        Ok(resp) => {
                            let status = resp.status();
                            let body = resp.text().await.unwrap_or_default();
                            load_error.set(Some(humanize_error_body(status, &body)));
                        }
                        Err(e) => load_error.set(Some(humanize_network_error(e))),
                    }
                    loaded.set(true);
                });
                || ()
            },
        );
    }

    // Poll only while something is actually running: an idle console that
    // re-reads every three seconds would be pure noise.
    {
        let tick = tick.clone();
        let polling = overview.as_ref().is_some_and(|o| o.totals.jobs_running > 0);
        use_effect_with(polling, move |polling| {
            let interval =
                polling.then(|| Interval::new(POLL_MS, move || tick.set(tick.wrapping_add(1))));
            move || drop(interval)
        });
    }

    let on_process = {
        let backend_url = backend_url.clone();
        let mode = mode.clone();
        let busy = busy.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let tick = tick.clone();
        Callback::from(move |account_id: i32| {
            let selected = (*mode).clone();
            let request = api_post(&format!(
                "{backend_url}/process/{account_id}?mode={selected}"
            ));
            spawn_action(
                request,
                account_id,
                format!("Processing is running for account {account_id}."),
                busy.clone(),
                message.clone(),
                is_error.clone(),
                tick.clone(),
            );
        })
    };

    let on_sync = {
        let backend_url = backend_url.clone();
        let busy = busy.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let tick = tick.clone();
        Callback::from(move |account_id: i32| {
            spawn_action(
                api_post(&format!("{backend_url}/account/{account_id}/sync")),
                account_id,
                format!("Sync finished for account {account_id}."),
                busy.clone(),
                message.clone(),
                is_error.clone(),
                tick.clone(),
            );
        })
    };

    let on_unlink = {
        let backend_url = backend_url.clone();
        let busy = busy.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let tick = tick.clone();
        Callback::from(move |account_id: i32| {
            // Unlinking is the one action here that destroys data, so it asks
            // first — and the wording had to be corrected after watching a real
            // unlink: dropping the *last* device link cascades to the account
            // row itself (`delete_device_link`), which takes its `accounts_post`
            // rows with it. An earlier draft promised "your saved posts stay in
            // the catalog", which is only true of posts another account also
            // saves. Understating a destructive action in a confirm dialog is
            // the failure mode this wording exists to avoid.
            let confirmed = window()
                .and_then(|w| {
                    w.confirm_with_message(&format!(
                        "Unlink account {account_id} from this browser?\n\n\
                         If no other device is linked to it, the account and its saved-post \
                         links are deleted from this instance, and its library has to be \
                         re-imported with /process. Links held by another device are unaffected."
                    ))
                    .ok()
                })
                .unwrap_or(false);
            if !confirmed {
                return;
            }
            let backend_url = backend_url.clone();
            let busy = busy.clone();
            let message = message.clone();
            let is_error = is_error.clone();
            let tick = tick.clone();
            spawn_local(async move {
                busy.set(Some(account_id));
                message.set(String::new());
                is_error.set(false);
                let outcome = match api_delete(&format!("{backend_url}/account/{account_id}"))
                    .send()
                    .await
                {
                    Ok(resp) if resp.ok() => Ok(format!(
                        "Account {account_id} is no longer linked to this browser."
                    )),
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        Err(humanize_error_body(status, &body))
                    }
                    Err(e) => Err(humanize_network_error(e)),
                };
                match outcome {
                    Ok(text) => {
                        message.set(text);
                        is_error.set(false);
                        // The nav entry, the catalog tab and the account
                        // selector are all derived from ownership, so they have
                        // to be told — otherwise the console keeps offering a
                        // tab for a library this browser just gave up.
                        dispatch_account_list_changed();
                    }
                    Err(text) => {
                        message.set(text);
                        is_error.set(true);
                    }
                }
                busy.set(None);
                tick.set(tick.wrapping_add(1));
            });
        })
    };

    let on_bucket = {
        let backend_url = backend_url.clone();
        let busy = busy.clone();
        let message = message.clone();
        let is_error = is_error.clone();
        let tick = tick.clone();
        Callback::from(move |(account_id, value): (i32, String)| {
            // The route answers with the account's bucket state, but the console
            // re-reads the whole overview instead of patching the single row:
            // the *effective* bucket also depends on which names are configured,
            // and a stale pin is exactly the case where the two disagree.
            let (request, success) = if value.is_empty() {
                (
                    api_delete(&format!(
                        "{backend_url}/account/{account_id}/experiment_bucket"
                    )),
                    format!("Bucket pin cleared for account {account_id}."),
                )
            } else {
                (
                    api_put(&format!(
                        "{backend_url}/account/{account_id}/experiment_bucket"
                    ))
                    .header("Content-Type", "application/json")
                    .body(serde_json::json!({ "bucket": value }).to_string()),
                    format!("Account {account_id} pinned to '{value}'."),
                )
            };
            spawn_action(
                request,
                account_id,
                success,
                busy.clone(),
                message.clone(),
                is_error.clone(),
                tick.clone(),
            );
        })
    };

    let on_mode_change = {
        let mode = mode.clone();
        Callback::from(move |e: Event| {
            mode.set(e.target_unchecked_into::<HtmlSelectElement>().value());
        })
    };

    let on_prev = {
        let offset = offset.clone();
        Callback::from(move |_: MouseEvent| offset.set((*offset - PAGE_LIMIT).max(0)))
    };
    let on_next = {
        let offset = offset.clone();
        Callback::from(move |_: MouseEvent| offset.set(*offset + PAGE_LIMIT))
    };

    let status_class = if *is_error {
        "alert alert-error text-sm py-2 mb-3"
    } else {
        "alert alert-success text-sm py-2 mb-3"
    };

    html! {
        <div id="admin-page" class="m-4">
            <h1 class="text-2xl font-semibold text-base-content mb-1">{ "Owner console" }</h1>
            <p class="text-sm text-base-content/70 mb-3">
                { "Everything here is instance state: the media queue, the job registry and the bucket pins. There is no separate admin login — the [runtime].admin_api key proved in Settings is the gate, and the cookie is the identity." }
            </p>

            if !message.is_empty() {
                <div role="alert" class={status_class}>{ (*message).clone() }</div>
            }
            if let Some(err) = &*load_error {
                <div role="alert" class="alert alert-error text-sm py-2 mb-3">{ err.clone() }</div>
            }

            if *forbidden {
                <div class="card bg-base-100 border border-base-300">
                    <div class="card-body text-base-content">
                        { empty_notice_card(admin_available) }
                    </div>
                </div>
            } else if let Some(o) = &*overview {
                if o.totals.accounts == 0 {
                    <div class="card bg-base-100 border border-base-300">
                        <div class="card-body text-base-content">
                            { empty_instance_card() }
                        </div>
                    </div>
                } else {
                    <div class="stats stats-vertical lg:stats-horizontal shadow bg-base-100 w-full mb-4">
                        <div class="stat">
                            <div class="stat-title">{ "Accounts" }</div>
                            <div class="stat-value text-2xl">{ o.totals.accounts }</div>
                            <div class="stat-desc">
                                { format!("every account on this instance ({})", o.instance_accounts) }
                            </div>
                        </div>
                        <div class="stat">
                            <div class="stat-title">{ "Saved posts" }</div>
                            <div class="stat-value text-2xl">{ o.totals.saved_posts }</div>
                            <div class="stat-desc">{ "distinct across the instance" }</div>
                        </div>
                        <div class="stat">
                            <div class="stat-title">{ "e621 keys stored" }</div>
                            <div class="stat-value text-2xl">{ o.totals.accounts_with_key }</div>
                            <div class="stat-desc">{ "key-backed direct sync" }</div>
                        </div>
                        <div class="stat">
                            <div class="stat-title">{ "Jobs running" }</div>
                            <div class="stat-value text-2xl">{ o.totals.jobs_running }</div>
                            <div class="stat-desc">{ "in this server process" }</div>
                        </div>
                    </div>

                    <div class="card bg-base-100 shadow mb-4">
                        <div class="card-body text-base-content">
                            <h2 class="card-title text-xl">{ "Media cache" }</h2>
                            <p class="text-sm text-base-content/70">
                                { "Counters are instance-wide: the store keeps one file per post and has no per-owner split yet (TODO §4.5)." }
                            </p>
                            <div class="flex flex-wrap gap-4 text-sm">
                                <span>{ format!("Stored: {}", o.media.stored) }</span>
                                <span>{ format!("Pending: {}", o.media.pending) }</span>
                                <span>{ format!("On disk: {}", human_bytes(o.media.bytes)) }</span>
                            </div>
                            <div class="flex flex-wrap gap-2">
                                <span class={classes!("badge", "badge-sm", if o.media.enabled { "badge-success" } else { "badge-ghost" })}>
                                    { if o.media.enabled { "Downloading originals" } else { "Originals off ([catalog] toggles)" } }
                                </span>
                                <span class={classes!("badge", "badge-sm", if o.media.paused { "badge-warning" } else { "badge-ghost" })}>
                                    { if o.media.paused { "Worker paused" } else { "Worker running" } }
                                </span>
                            </div>
                        </div>
                    </div>

                    <div class="card bg-base-100 shadow mb-4">
                        <div class="card-body text-base-content">
                            <h2 class="card-title text-xl">{ "Experiment buckets" }</h2>
                            if o.buckets.is_empty() {
                                <p class="text-sm text-base-content/70">
                                    { "No [buckets.*] sections in config.toml, so scoring runs on [priors] alone and there is nothing to assign. Add a bucket section to A/B a channel." }
                                </p>
                            } else {
                                <p class="text-sm text-base-content/70">
                                    { "An account with no pin is assigned by a hash of its id, which keeps the split stable across restarts. A pin overrides that for one account." }
                                </p>
                                <div class="flex flex-wrap gap-2">
                                    { for o.buckets.iter().map(|b| html! {
                                        <span class="badge badge-outline">{ b }</span>
                                    }) }
                                </div>
                            }
                        </div>
                    </div>

                    <div class="card bg-base-100 shadow">
                        <div class="card-body text-base-content">
                            <div class="flex flex-wrap items-center justify-between gap-2">
                                <h2 class="card-title text-xl">{ "Accounts" }</h2>
                                <div class="flex items-center gap-2">
                                    <label class="text-sm" for="admin-process-mode">{ "Process mode" }</label>
                                    <select id="admin-process-mode" class="select select-sm"
                                        onchange={on_mode_change}>
                                        { for PROCESS_MODES.iter().map(|(value, label)| html! {
                                            <option value={*value} selected={(*mode).as_str() == *value}>
                                                { *label }
                                            </option>
                                        }) }
                                    </select>
                                </div>
                            </div>

                            if o.accounts.is_empty() {
                                <p class="text-base-content/70 text-center my-6">
                                    { "No accounts on this page." }
                                </p>
                            } else {
                                <div class="overflow-x-auto">
                                    <table class="table table-sm">
                                        <thead>
                                            <tr>
                                                <th>{ "Account" }</th>
                                                <th>{ "Saved" }</th>
                                                <th>{ "e621 key" }</th>
                                                <th>{ "A/B bucket" }</th>
                                                <th>{ "Job" }</th>
                                                <th>{ "Actions" }</th>
                                            </tr>
                                        </thead>
                                        <tbody>
                                            { for o.accounts.iter().map(|a| {
                                                let running = a.job.as_ref().is_some_and(|j| j.phase == "running");
                                                let row_busy = *busy == Some(a.id);
                                                let account_id = a.id;
                                                let stale = bucket_pin_is_stale(a);
                                                let on_bucket_select = {
                                                    let on_bucket = on_bucket.clone();
                                                    Callback::from(move |e: Event| {
                                                        let value = e
                                                            .target_unchecked_into::<HtmlSelectElement>()
                                                            .value();
                                                        on_bucket.emit((account_id, value));
                                                    })
                                                };
                                                let on_process_row = {
                                                    let on_process = on_process.clone();
                                                    Callback::from(move |_: MouseEvent| on_process.emit(account_id))
                                                };
                                                let on_sync_row = {
                                                    let on_sync = on_sync.clone();
                                                    Callback::from(move |_: MouseEvent| on_sync.emit(account_id))
                                                };
                                                let on_unlink_row = {
                                                    let on_unlink = on_unlink.clone();
                                                    Callback::from(move |_: MouseEvent| on_unlink.emit(account_id))
                                                };
                                                html! {
                                                    <tr>
                                                        <td>
                                                            <span class="font-medium">{ &a.name }</span>
                                                            <span class="text-xs text-base-content/60">{ format!(" (ID: {})", a.id) }</span>
                                                        </td>
                                                        <td>{ a.saved_posts }</td>
                                                        <td>
                                                            if a.has_key {
                                                                <span class="badge badge-sm badge-outline">{ "stored" }</span>
                                                            } else {
                                                                <span class="text-xs text-base-content/50">{ "—" }</span>
                                                            }
                                                        </td>
                                                        <td>
                                                            <div class="flex flex-col gap-1">
                                                                <span class={classes!("text-xs", if stale { "text-warning" } else { "text-base-content/60" })}>
                                                                    { bucket_cell(a) }
                                                                </span>
                                                                if !o.buckets.is_empty() && a.addressable {
                                                                    // `id` + `aria-label`: a bare control with neither is
                                                                    // both unnamed for a screen reader and flagged by the
                                                                    // browser's own form-field audit, and there is one of
                                                                    // these per row.
                                                                    <select class="select select-xs" disabled={row_busy}
                                                                        id={format!("admin-bucket-{}", a.id)}
                                                                        aria-label={format!("A/B bucket for account {}", a.id)}
                                                                        onchange={on_bucket_select}>
                                                                        <option value="" selected={a.bucket_explicit.is_none()}>
                                                                            { "Hash (no pin)" }
                                                                        </option>
                                                                        if let Some(pinned) = a.bucket_explicit.as_deref()
                                                                            .filter(|p| !o.buckets.iter().any(|b| b == p))
                                                                        {
                                                                            // A pin naming a bucket this instance no longer
                                                                            // configures has no option in the list, and a
                                                                            // `<select>` with nothing selected displays its
                                                                            // *first* option — so without this the control
                                                                            // would read "Hash (no pin)" while the database
                                                                            // still holds the stale pin. The cell above says
                                                                            // so in words; this makes the control agree.
                                                                            <option value={pinned.to_string()} selected={true}>
                                                                                { format!("{pinned} (not configured)") }
                                                                            </option>
                                                                        }
                                                                        { for o.buckets.iter().map(|b| html! {
                                                                            <option value={b.clone()}
                                                                                selected={a.bucket_explicit.as_deref() == Some(b.as_str())}>
                                                                                { b }
                                                                            </option>
                                                                        }) }
                                                                    </select>
                                                                }
                                                            </div>
                                                        </td>
                                                        <td class="text-xs">{ job_cell(&a.job) }</td>
                                                        <td>
                                                            <div class="flex flex-wrap gap-1">
                                                                // Every one of these lands on a route that
                                                                // re-checks `account_in_scope`, so a row the
                                                                // console merely *shows* must not offer them —
                                                                // the control would be a guaranteed 404.
                                                                if a.addressable {
                                                                    <button type="button" class="btn btn-xs"
                                                                        disabled={row_busy || running}
                                                                        onclick={on_process_row}>
                                                                        { if running { "Running…" } else { "Process" } }
                                                                    </button>
                                                                    <button type="button" class="btn btn-xs btn-ghost"
                                                                        disabled={row_busy}
                                                                        onclick={on_sync_row}>
                                                                        { "Sync" }
                                                                    </button>
                                                                    <a class="btn btn-xs btn-ghost"
                                                                        href={format!("{}/account/{}/export", backend_url, a.id)}>
                                                                        { "Export" }
                                                                    </a>
                                                                }
                                                                if a.linked {
                                                                    <button type="button" class="btn btn-xs btn-ghost"
                                                                        disabled={row_busy}
                                                                        onclick={on_unlink_row}>
                                                                        { "Unlink" }
                                                                    </button>
                                                                } else if a.addressable {
                                                                    // The instance owner addresses accounts this
                                                                    // browser holds no link to, and unlinking removes
                                                                    // a link — so the action would be a guaranteed
                                                                    // 404. Saying why beats a dead button.
                                                                    <span class="text-xs text-base-content/60 self-center"
                                                                        title="This account is in scope through the instance claim, not through a link from this browser, so there is nothing to unlink.">
                                                                        { "no direct link" }
                                                                    </span>
                                                                } else {
                                                                    // Shown because this browser operates the
                                                                    // instance, not because it may act as the
                                                                    // account: operating the machine is not the same
                                                                    // as holding the libraries on it.
                                                                    <span class="text-xs text-base-content/60 self-center"
                                                                        title="Visible because this browser operates the instance. Acting as this account — syncing it or starting a run — needs a link to it, which this browser does not hold.">
                                                                        { "view only" }
                                                                    </span>
                                                                }
                                                            </div>
                                                        </td>
                                                    </tr>
                                                }
                                            }) }
                                        </tbody>
                                    </table>
                                </div>

                                <div class="flex flex-wrap items-center justify-between gap-2 mt-2">
                                    <span class="text-xs text-base-content/60">
                                        { format!(
                                            "Showing {}\u{2013}{} of {}",
                                            *offset + 1,
                                            *offset + o.accounts.len() as i64,
                                            o.totals.accounts,
                                        ) }
                                    </span>
                                    if o.totals.accounts > PAGE_LIMIT {
                                        <div class="join">
                                            <button type="button" class="btn btn-xs join-item"
                                                disabled={*offset == 0} onclick={on_prev}>
                                                { "Previous" }
                                            </button>
                                            <button type="button" class="btn btn-xs join-item"
                                                disabled={*offset + PAGE_LIMIT >= o.totals.accounts}
                                                onclick={on_next}>
                                                { "Next" }
                                            </button>
                                        </div>
                                    }
                                </div>
                            }
                        </div>
                    </div>
                }
            } else if !*loaded {
                <p class="text-base-content/70 text-center my-8">{ "Loading…" }</p>
            }
        </div>
    }
}

/// The "you do not operate this instance" card, kept as a function so the two
/// wordings can be pinned by a test without a DOM.
///
/// No action link at all — not to the account form, and not to the instance
/// claim. The account form is the credential, and the claim does not open the
/// console; offering either would advertise a door the reader cannot open.
fn empty_notice_card(admin_available: bool) -> Html {
    let (title, body) = empty_admin_notice(admin_available);
    html! {
        <>
            <h2 class="card-title text-xl">{ title }</h2>
            <p class="text-sm text-base-content/70">{ body }</p>
            <div class="card-actions justify-end mt-2">
                <a class="btn btn-sm btn-ghost" href="/">{ "Back to the feed" }</a>
            </div>
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same distinction the catalog draws: while the tier is open the credential
    /// has to be named, and once it is taken there is nothing to name — the reader
    /// is told who holds it instead of being invited to try.
    ///
    /// Neither wording may mention `owner_secret`. The claim does not open the
    /// console, so naming it would send the reader to the wrong form — which is
    /// exactly the conflation this tier was split out of.
    #[test]
    fn the_empty_notice_follows_whether_the_administration_tier_is_open() {
        let (offered_title, offered_body) = empty_admin_notice(true);
        let (taken_title, taken_body) = empty_admin_notice(false);

        assert_ne!(offered_title, taken_title);
        assert!(offered_title.contains("does not operate the instance"));
        assert!(taken_title.contains("Another browser operates"));
        assert!(offered_body.contains("admin_api"));
        // While the tier is open the reader is sent to the one place that can
        // grant it, rather than left to guess which page holds the form.
        assert!(offered_body.contains("owner claim"));
        assert!(offered_body.contains("settings page"));
        // Once the tier is taken the wording must not invite the reader to go and
        // link anything: that is the state where the server would refuse them.
        assert!(taken_body.contains("already linked"));
        assert!(!taken_body.contains("config.toml"));
        assert!(!offered_body.contains("owner_secret"));
        assert!(!taken_body.contains("owner_secret"));
    }
}
