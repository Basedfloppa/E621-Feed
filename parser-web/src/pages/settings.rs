// pi-lens-ignore: E0432
use crate::components::{
    AccountKeyCard, IconSliders, PostCard, SavedAccountsSelect, SessionDevicesCard, StorageCard,
};
use crate::models::{
    Post, Rating, Score, Stats, Tags, api_get, api_patch, humanize_error_body,
    humanize_network_error, read_config_from_head,
};
use crate::pages::UserInfo;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use web_sys::{HtmlTextAreaElement, window};
use yew::prelude::*;

#[derive(Clone, PartialEq)]
struct DisplaySettings {
    show_rating: bool,
    show_affinity: bool,
    show_score: bool,
    show_post_number: bool,
    show_desc: bool,
    show_metadata: bool,
    show_breakdown: bool,
    show_detailed_breakdown: bool,
    score_results: bool,
    score_cutoff_pct: f32,
    grid: GridType,
}

/// Grid layout options (mirrors feed.rs). Persisted under `settings_grid_type`.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum GridType {
    #[default]
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

fn read_bool_local(key: &str, default: bool) -> bool {
    window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(key).ok().flatten())
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(default)
}

fn read_f32_local(key: &str, default: f32) -> f32 {
    window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(key).ok().flatten())
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(default)
}

fn write_local(key: &str, value: &str) {
    if let Some(storage) = window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(key, value);
    }
}

fn load_display_settings() -> DisplaySettings {
    DisplaySettings {
        show_rating: read_bool_local("settings_show_rating", true),
        show_affinity: read_bool_local("settings_show_affinity", false),
        show_score: read_bool_local("settings_show_score", true),
        show_post_number: read_bool_local("settings_show_post_number", true),
        show_desc: read_bool_local("settings_show_desc", true),
        show_metadata: read_bool_local("settings_show_metadata", false),
        show_breakdown: read_bool_local("settings_show_breakdown", false),
        show_detailed_breakdown: read_bool_local("settings_show_detailed_breakdown", false),
        score_results: read_bool_local("settings_score_results", false),
        score_cutoff_pct: read_f32_local("settings_score_cutoff_pct", 0.0),
        grid: GridType::from_storage(
            window()
                .and_then(|w| w.local_storage().ok().flatten())
                .and_then(|s| s.get_item("settings_grid_type").ok().flatten()),
        ),
    }
}

fn persist_display_settings(s: &DisplaySettings) {
    write_local("settings_show_rating", &s.show_rating.to_string());
    write_local("settings_show_affinity", &s.show_affinity.to_string());
    write_local("settings_show_score", &s.show_score.to_string());
    write_local("settings_show_post_number", &s.show_post_number.to_string());
    write_local("settings_show_desc", &s.show_desc.to_string());
    write_local("settings_show_metadata", &s.show_metadata.to_string());
    write_local("settings_show_breakdown", &s.show_breakdown.to_string());
    write_local(
        "settings_show_detailed_breakdown",
        &s.show_detailed_breakdown.to_string(),
    );
    write_local("settings_score_results", &s.score_results.to_string());
    write_local("settings_score_cutoff_pct", &s.score_cutoff_pct.to_string());
    write_local("settings_grid_type", s.grid.to_storage());
}

#[derive(Serialize, Deserialize, Clone)]
struct PreferredTag {
    tag: String,
    group: String,
    weight: f32,
}

#[derive(Serialize, Deserialize)]
struct AccountFeedSettings {
    blacklist: Option<String>,
    preferred_tags: Vec<PreferredTag>,
    experiment_bucket: Option<String>,
}

#[derive(Serialize)]
struct AccountFeedSettingsPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    blacklist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preferred_tags: Option<Vec<PreferredTag>>,
}

#[function_component(SettingsPage)]
pub fn settings_page() -> Html {
    let backend_url = read_config_from_head()
        .map(|cfg| cfg.backend_domain)
        .unwrap_or_default();

    let selected_user = use_state(|| Option::<UserInfo>::None);
    let accounts_loading = use_state(|| false);

    let server_loading = use_state(|| false);
    let server_message = use_state(String::new);
    let server_is_error = use_state(|| false);
    let blacklist_draft = use_state(String::new);
    let preferred_tags_draft = use_state(String::new);
    let experiment_bucket = use_state(|| Option::<String>::None);

    let display = use_state(load_display_settings);

    {
        let selected_user = selected_user.clone();
        let blacklist_draft = blacklist_draft.clone();
        let preferred_tags_draft = preferred_tags_draft.clone();
        let experiment_bucket = experiment_bucket.clone();
        let server_loading = server_loading.clone();
        let server_message = server_message.clone();
        let server_is_error = server_is_error.clone();
        let backend_url = backend_url.clone();

        use_effect_with(selected_user.clone(), move |user| {
            if let Some(user) = user.as_ref()
                && !backend_url.is_empty()
            {
                server_loading.set(true);
                let url = format!("{}/account/{}/feed_settings", backend_url, user.id);
                let blacklist_draft = blacklist_draft.clone();
                let preferred_tags_draft = preferred_tags_draft.clone();
                let experiment_bucket = experiment_bucket.clone();
                let server_loading = server_loading.clone();
                let server_message = server_message.clone();
                let server_is_error = server_is_error.clone();

                wasm_bindgen_futures::spawn_local(async move {
                    match api_get(&url).send().await {
                        Ok(resp) if resp.ok() => match resp.json::<AccountFeedSettings>().await {
                            Ok(s) => {
                                blacklist_draft.set(s.blacklist.unwrap_or_default());
                                let tags_text = s
                                    .preferred_tags
                                    .iter()
                                    .map(|t| format!("{}:{}:{}", t.tag, t.group, t.weight))
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                preferred_tags_draft.set(tags_text);
                                experiment_bucket.set(s.experiment_bucket.clone());
                                server_loading.set(false);
                            }
                            Err(_e) => {
                                server_message
                                    .set("Settings could not be read. Try again.".to_string());
                                server_is_error.set(true);
                                server_loading.set(false);
                            }
                        },
                        Ok(resp) => {
                            let status = resp.status();
                            let body = resp.text().await.unwrap_or_default();
                            server_message.set(humanize_error_body(status, &body));
                            server_is_error.set(true);
                            server_loading.set(false);
                        }
                        Err(e) => {
                            server_message.set(humanize_network_error(e));
                            server_is_error.set(true);
                            server_loading.set(false);
                        }
                    }
                });
            }
            || ()
        });
    }

    let on_save_server = {
        let selected_user = selected_user.clone();
        let blacklist_draft = blacklist_draft.clone();
        let preferred_tags_draft = preferred_tags_draft.clone();
        let experiment_bucket = experiment_bucket.clone();
        let server_loading = server_loading.clone();
        let server_message = server_message.clone();
        let server_is_error = server_is_error.clone();

        Callback::from(move |_| {
            let Some(user) = (*selected_user).clone() else {
                server_message.set("Select an account first.".to_string());
                server_is_error.set(true);
                return;
            };
            let Some(cfg) = read_config_from_head() else {
                return;
            };

            server_loading.set(true);
            server_message.set(String::new());
            server_is_error.set(false);

            let blacklist = Some((*blacklist_draft).clone());
            let tags_text = (*preferred_tags_draft).clone();
            let preferred_tags: Vec<PreferredTag> = tags_text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| {
                    let parts: Vec<&str> = l.split(':').collect();
                    if parts.len() == 3 {
                        let weight = parts[2].parse::<f32>().ok().unwrap_or(1.0).clamp(0.1, 10.0);
                        Some(PreferredTag {
                            tag: parts[0].trim().to_string(),
                            group: parts[1].trim().to_string(),
                            weight,
                        })
                    } else {
                        None
                    }
                })
                .collect();
            let preferred_tags = if preferred_tags.is_empty() {
                Some(Vec::new())
            } else {
                Some(preferred_tags)
            };

            let patch = AccountFeedSettingsPatch {
                blacklist,
                preferred_tags,
            };
            let body = serde_json::to_string(&patch).unwrap_or_default();

            let url = format!("{}/account/{}/feed_settings", cfg.backend_domain, user.id);
            let blacklist_draft = blacklist_draft.clone();
            let preferred_tags_draft = preferred_tags_draft.clone();
            let experiment_bucket = experiment_bucket.clone();
            let server_loading = server_loading.clone();
            let server_message = server_message.clone();
            let server_is_error = server_is_error.clone();

            wasm_bindgen_futures::spawn_local(async move {
                match api_patch(&url)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await
                {
                    Ok(resp) if resp.ok() => match resp.json::<AccountFeedSettings>().await {
                        Ok(s) => {
                            blacklist_draft.set(s.blacklist.unwrap_or_default());
                            let tags_text = s
                                .preferred_tags
                                .iter()
                                .map(|t| format!("{}:{}:{}", t.tag, t.group, t.weight))
                                .collect::<Vec<_>>()
                                .join("\n");
                            preferred_tags_draft.set(tags_text);
                            experiment_bucket.set(s.experiment_bucket.clone());
                            server_message.set("Settings saved.".to_string());
                            server_is_error.set(false);
                        }
                        Err(_e) => {
                            server_message
                                .set("Settings could not be read. Try again.".to_string());
                            server_is_error.set(true);
                        }
                    },
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        server_message.set(humanize_error_body(status, &body));
                        server_is_error.set(true);
                    }
                    Err(e) => {
                        server_message.set(humanize_network_error(e));
                        server_is_error.set(true);
                    }
                }
                server_loading.set(false);
            });
        })
    };

    let toggle_rating = {
        let display = display.clone();
        Callback::from(move |_| {
            let mut d = (*display).clone();
            d.show_rating = !d.show_rating;
            persist_display_settings(&d);
            display.set(d);
        })
    };
    let toggle_affinity = {
        let display = display.clone();
        Callback::from(move |_| {
            let mut d = (*display).clone();
            d.show_affinity = !d.show_affinity;
            persist_display_settings(&d);
            display.set(d);
        })
    };
    let toggle_score = {
        let display = display.clone();
        Callback::from(move |_| {
            let mut d = (*display).clone();
            d.show_score = !d.show_score;
            persist_display_settings(&d);
            display.set(d);
        })
    };
    let toggle_post_number = {
        let display = display.clone();
        Callback::from(move |_| {
            let mut d = (*display).clone();
            d.show_post_number = !d.show_post_number;
            persist_display_settings(&d);
            display.set(d);
        })
    };
    let toggle_desc = {
        let display = display.clone();
        Callback::from(move |_| {
            let mut d = (*display).clone();
            d.show_desc = !d.show_desc;
            persist_display_settings(&d);
            display.set(d);
        })
    };
    let toggle_metadata = {
        let display = display.clone();
        Callback::from(move |_| {
            let mut d = (*display).clone();
            d.show_metadata = !d.show_metadata;
            persist_display_settings(&d);
            display.set(d);
        })
    };
    let toggle_breakdown = {
        let display = display.clone();
        Callback::from(move |_| {
            let mut d = (*display).clone();
            d.show_breakdown = !d.show_breakdown;
            persist_display_settings(&d);
            display.set(d);
        })
    };
    let toggle_detailed_breakdown = {
        let display = display.clone();
        Callback::from(move |_| {
            let mut d = (*display).clone();
            d.show_detailed_breakdown = !d.show_detailed_breakdown;
            persist_display_settings(&d);
            display.set(d);
        })
    };
    let toggle_score_results = {
        let display = display.clone();
        Callback::from(move |_| {
            let mut d = (*display).clone();
            d.score_results = !d.score_results;
            persist_display_settings(&d);
            display.set(d);
        })
    };
    let set_cutoff = {
        let display = display.clone();
        Callback::from(move |pct: f32| {
            let mut d = (*display).clone();
            d.score_cutoff_pct = pct;
            persist_display_settings(&d);
            display.set(d);
        })
    };
    let set_grid = |g: GridType| {
        let display = display.clone();
        Callback::from(move |_: MouseEvent| {
            let mut d = (*display).clone();
            d.grid = g;
            persist_display_settings(&d);
            display.set(d);
        })
    };

    let on_blacklist_change = {
        let blacklist_draft = blacklist_draft.clone();
        Callback::from(move |e: Event| {
            let input: HtmlTextAreaElement = e.target_unchecked_into();
            blacklist_draft.set(input.value());
        })
    };
    let on_preferred_tags_change = {
        let preferred_tags_draft = preferred_tags_draft.clone();
        Callback::from(move |e: Event| {
            let input: HtmlTextAreaElement = e.target_unchecked_into();
            preferred_tags_draft.set(input.value());
        })
    };

    let message_class = if server_message.is_empty() {
        "hidden"
    } else if *server_is_error {
        "alert alert-error mt-3"
    } else {
        "alert alert-success mt-3"
    };

    let d = (*display).clone();

    // Static preview: build a few sample `Post`s and render them through the
    // real `PostCard` (so the preview matches exactly what the feed shows).
    // Sample posts carry NO file URLs, so `PostCard` renders its placeholder
    // thumbnail and makes no image request; `static_preview` + empty backend
    // suppress impression/interaction posts and click navigation.
    let make_sample = |id: i64, rating: Rating, artist: &[&str], general: &[&str], up: i64| {
        let p = Post {
            id,
            rating,
            uploader_name: Some("artist_handle".to_string()),
            tags: Tags {
                artist: artist.iter().map(|s| s.to_string()).collect(),
                general: general.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            stats: Stats {
                score: Score {
                    up,
                    down: up / 5,
                    total: up + up / 5,
                },
                fav_count: up * 3,
                ..Default::default()
            },
            ..Default::default()
        };
        Rc::new(p)
    };
    let sample_posts: Vec<(Rc<Post>, f32)> = vec![
        (
            make_sample(
                4_200_123,
                Rating::S,
                &["artist_a"],
                &["canine", "anthro", "fluffy"],
                214,
            ),
            0.92,
        ),
        (
            make_sample(
                4_199_902,
                Rating::Q,
                &["artist_b"],
                &["feral", "scaly", "night"],
                97,
            ),
            0.71,
        ),
        (
            make_sample(
                4_198_540,
                Rating::E,
                &["artist_c"],
                &["animated", "riding", "running"],
                33,
            ),
            0.45,
        ),
    ];

    html! {
        <div id="settings-page">
            <h1 class="text-2xl font-semibold text-base-content text-center mb-3">
                { "Settings" }
            </h1>
            <div class="flex justify-center">
                <div class="w-full max-w-3xl flex flex-col gap-6">

                    <div id="settings-server" class="card bg-base-100 shadow">
                        <div class="card-body text-base-content">
                            <h2 class="card-title text-xl">
                                <IconSliders /> { "Server Settings" }
                            </h2>
                            <p class="text-sm text-base-content/70 mb-2">
                                { "These settings are stored on the server and apply to all devices." }
                            </p>
                            <SavedAccountsSelect
                                selected_user={selected_user.clone()}
                                is_loading={accounts_loading}
                            />
                            if let Some(user) = &*selected_user {
                                <p class="text-sm text-base-content/70 mb-3">
                                    { format!("Account: {} (ID {})", user.name, user.id) }
                                </p>
                            }
                            if let Some(bucket) = &*experiment_bucket {
                                <p class="text-sm text-base-content/70 mb-3">
                                    { format!("Experiment bucket: {}", bucket) }
                                </p>
                            }
                            if selected_user.is_some() {
                                <fieldset class="fieldset w-full">
                                    <legend class="fieldset-legend">{ "Blacklist" }</legend>
                                    <textarea
                                        class="textarea w-full box-border"
                                        rows="5"
                                        value={(*blacklist_draft).clone()}
                                        onchange={on_blacklist_change}
                                        disabled={*server_loading}
                                        placeholder={"One tag per line, e.g.:\ngore\nyoung -rating:s\n-fav:yourname"}
                                    />
                                    <p class="text-xs text-base-content/70 mt-1">
                                        { "Leave empty to fall back to the server default blacklist." }
                                    </p>
                                </fieldset>
                                <fieldset class="fieldset w-full">
                                    <legend class="fieldset-legend">{ "Preferred Tags" }</legend>
                                    <textarea
                                        class="textarea w-full box-border"
                                        rows="4"
                                        value={(*preferred_tags_draft).clone()}
                                        onchange={on_preferred_tags_change}
                                        disabled={*server_loading}
                                        placeholder={"Format: tag:group:weight, e.g.:\nwolf:general:2.0\ncanine:species:1.5"}
                                    />
                                    <p class="text-xs text-base-content/70 mt-1">
                                        { "One per line: tag:group:weight (weight 0.1–10.0). Groups: general, artist, character, copyright, species, lore, meta." }
                                    </p>
                                </fieldset>
                                <button
                                    class="btn btn-primary"
                                    onclick={on_save_server}
                                    disabled={*server_loading}
                                >
                                    { if *server_loading { "Saving…" } else { "Save Server Settings" } }
                                </button>
                                <div class={message_class} role="alert">
                                    {&*server_message}
                                </div>
                            } else {
                                <p class="text-base-content/70 text-center py-4">
                                    { "Select an account to edit server settings." }
                                </p>
                            }
                        </div>
                    </div>

                    <StorageCard />

                    <SessionDevicesCard
                        backend_url={backend_url.clone()}
                        selected_account_id={(*selected_user).as_ref().map(|u| u.id)}
                    />

                    <AccountKeyCard
                        backend_url={backend_url.clone()}
                        selected_account_id={(*selected_user).as_ref().map(|u| u.id)}
                    />

                    <div id="settings-display" class="card bg-base-100 shadow">
                        <div class="card-body text-base-content">
                            <h2 class="card-title text-xl">
                                <IconSliders /> { "Display Settings" }
                            </h2>
                            <p class="text-sm text-base-content/70 mb-3">
                                { "These settings apply to all pages and are saved in your browser." }
                            </p>

                            <div class="mb-4">
                                <label class="label cursor-pointer justify-start gap-2 py-1">
                                    <input type="checkbox" class="toggle toggle-sm" checked={d.score_results}
                                        onchange={toggle_score_results} />
                                    <span class="label-text">{ "Score results" }</span>
                                </label>
                                <div class={classes!("join", "join-sm", if !d.score_results { "opacity-50" } else { "" })} role="group" aria-label="Score cutoff">
                                    { for [("Wide", 0.0f32), ("Balanced", 30.0), ("Strict", 60.0)].iter().map(|(label, cutoff)| {
                                        let active = (d.score_cutoff_pct - *cutoff).abs() < 0.1;
                                        let set_cutoff = set_cutoff.clone();
                                        let cutoff = *cutoff;
                                        html! {
                                            <button type="button" disabled={!d.score_results}
                                                class={classes!("btn", "btn-outline", "btn-xs", if active { "btn-active" } else { "" })}
                                                onclick={Callback::from(move |_| set_cutoff.emit(cutoff))}>
                                                { *label }
                                            </button>
                                        }
                                    }) }
                                </div>
                            </div>

                            <div class="mb-4">
                                <span class="text-xs text-base-content/70 block mb-1">{ "Grid density" }</span>
                                <div class="join join-sm" role="group" aria-label="Grid type">
                                    <button
                                        type="button"
                                        class={classes!("btn", "btn-outline", "btn-xs", if d.grid == GridType::Auto { "btn-active" } else { "" })}
                                        aria-pressed={(d.grid == GridType::Auto).to_string()}
                                        onclick={set_grid(GridType::Auto)}
                                    >{ "Auto" }</button>
                                    <button
                                        type="button"
                                        class={classes!("btn", "btn-outline", "btn-xs", if d.grid == GridType::Three { "btn-active" } else { "" })}
                                        aria-pressed={(d.grid == GridType::Three).to_string()}
                                        onclick={set_grid(GridType::Three)}
                                    >{ "3" }</button>
                                    <button
                                        type="button"
                                        class={classes!("btn", "btn-outline", "btn-xs", if d.grid == GridType::Two { "btn-active" } else { "" })}
                                        aria-pressed={(d.grid == GridType::Two).to_string()}
                                        onclick={set_grid(GridType::Two)}
                                    >{ "2" }</button>
                                    <button
                                        type="button"
                                        class={classes!("btn", "btn-outline", "btn-xs", if d.grid == GridType::One { "btn-active" } else { "" })}
                                        aria-pressed={(d.grid == GridType::One).to_string()}
                                        onclick={set_grid(GridType::One)}
                                    >{ "1" }</button>
                                </div>
                            </div>

                            <div class="divider my-2">{ "Badges" }</div>

                            <label class="label cursor-pointer justify-start gap-2 py-1">
                                <input type="checkbox" class="toggle toggle-sm" checked={d.show_rating}
                                    onchange={toggle_rating} />
                                <span class="label-text">{ "Rating badge" }</span>
                            </label>
                            <label class="label cursor-pointer justify-start gap-2 py-1">
                                <input type="checkbox" class="toggle toggle-sm" checked={d.show_affinity}
                                    disabled={!d.score_results}
                                    onchange={toggle_affinity} />
                                <span class="label-text">{ "Affinity score" }</span>
                            </label>
                            <label class="label cursor-pointer justify-start gap-2 py-1">
                                <input type="checkbox" class="toggle toggle-sm" checked={d.show_score}
                                    onchange={toggle_score} />
                                <span class="label-text">{ "Post score" }</span>
                            </label>
                            <label class="label cursor-pointer justify-start gap-2 py-1">
                                <input type="checkbox" class="toggle toggle-sm" checked={d.show_post_number}
                                    onchange={toggle_post_number} />
                                <span class="label-text">{ "Post number" }</span>
                            </label>

                            <div class="divider my-2">{ "Cards" }</div>

                            <label class="label cursor-pointer justify-start gap-2 py-1">
                                <input type="checkbox" class="toggle toggle-sm" checked={d.show_desc}
                                    onchange={toggle_desc} />
                                <span class="label-text">{ "Post text / tags" }</span>
                            </label>
                            <label class="label cursor-pointer justify-start gap-2 py-1">
                                <input type="checkbox" class="toggle toggle-sm" checked={d.show_metadata}
                                    onchange={toggle_metadata} />
                                <span class="label-text">{ "File metadata" }</span>
                            </label>
                            <label class="label cursor-pointer justify-start gap-2 py-1">
                                <input type="checkbox" class="toggle toggle-sm" checked={d.show_breakdown}
                                    disabled={!d.score_results}
                                    onchange={toggle_breakdown} />
                                <span class="label-text">{ "Score breakdown" }</span>
                            </label>
                            <label class="label cursor-pointer justify-start gap-2 py-1">
                                <input type="checkbox" class="toggle toggle-sm" checked={d.show_detailed_breakdown}
                                    disabled={!d.score_results}
                                    onchange={toggle_detailed_breakdown} />
                                <span class="label-text">{ "Detailed breakdown" }</span>
                            </label>
                        </div>
                    </div>

                    <div class="card bg-base-100 shadow">
                        <div class="card-body text-base-content">
                            <h3 class="card-title text-lg">{ "Preview" }</h3>
                            <p class="text-xs text-base-content/60 mb-2">
                                { "Example cards reflecting your display settings. Static — no data is fetched." }
                            </p>
                            <div class={format!("grid gap-3 {}", d.grid.grid_class())}>
                                { sample_posts.iter().enumerate().map(|(i, (post, affinity))| {
                                    html! {
                                        <PostCard
                                            post={post.clone()}
                                            affinity={*affinity}
                                            backend_url={AttrValue::from("")}
                                            account_id={0}
                                            session_id={AttrValue::default()}
                                            position={(i as i32) + 1}
                                            static_preview={true}
                                            show_rating={d.show_rating}
                                            show_affinity={d.show_affinity}
                                            show_score={d.show_score}
                                            show_post_number={d.show_post_number}
                                            show_desc={d.show_desc}
                                            show_metadata={d.show_metadata}
                                            show_breakdown={d.show_breakdown}
                                            show_detailed_breakdown={d.show_detailed_breakdown}
                                        />
                                    }
                                }).collect::<Html>() }
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        </div>
    }
}
