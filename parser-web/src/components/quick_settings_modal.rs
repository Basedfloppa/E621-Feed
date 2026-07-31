//! Quick display-settings modal opened from the header.
//!
//! Edits the same unified `settings_*` localStorage keys as the /settings
//! page, then dispatches a `settings-changed` CustomEvent so any page that
//! listens (feed, search, digest, trending, favorites) re-reads its display
//! config without a reload.

use wasm_bindgen::JsCast;
use web_sys::window;
use yew::prelude::*;

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
}

/// Mirror of the /settings display state, loaded from unified keys.
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

fn load() -> DisplaySettings {
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

fn persist(s: &DisplaySettings) {
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
    // Tell open pages to re-read their display config.
    if let Some(win) = window() {
        let _ = win.dispatch_event(
            &web_sys::CustomEvent::new("settings-changed").expect("create settings-changed event"),
        );
    }
}

#[derive(Properties, PartialEq)]
pub struct QuickSettingsModalProps {
    pub open: bool,
    pub on_close: Callback<()>,
}

/// Small modal exposing the shared display settings (score mode, badges,
/// cards, grid density) without navigating to /settings.
#[function_component(QuickSettingsModal)]
pub fn quick_settings_modal(props: &QuickSettingsModalProps) -> Html {
    // Hooks must run unconditionally — only gate the rendered output.
    let state = use_state(load);
    // Reload from localStorage whenever the modal is (re)opened.
    {
        let state = state.clone();
        let open = props.open;
        use_effect_with(open, move |_| {
            state.set(load());
            || ()
        });
    }
    if !props.open {
        return html! {};
    }
    let d = (*state).clone();

    let close = props.on_close.clone();
    let on_close_click = {
        let close = close.clone();
        Callback::from(move |_: MouseEvent| close.emit(()))
    };
    let stop = Callback::from(|e: MouseEvent| e.stop_propagation());
    let on_backdrop = {
        let close = close.clone();
        Callback::from(move |_: MouseEvent| close.emit(()))
    };

    let toggle = |key: &'static str| {
        let state = state.clone();
        Callback::from(move |_| {
            let mut s = (*state).clone();
            match key {
                "rating" => s.show_rating = !s.show_rating,
                "affinity" => s.show_affinity = !s.show_affinity,
                "score" => s.show_score = !s.show_score,
                "post_number" => s.show_post_number = !s.show_post_number,
                "desc" => s.show_desc = !s.show_desc,
                "metadata" => s.show_metadata = !s.show_metadata,
                "breakdown" => s.show_breakdown = !s.show_breakdown,
                "detailed_breakdown" => s.show_detailed_breakdown = !s.show_detailed_breakdown,
                "score_results" => s.score_results = !s.score_results,
                _ => {}
            }
            persist(&s);
            state.set(s);
        })
    };

    let set_cutoff = {
        let state = state.clone();
        Callback::from(move |pct: f32| {
            let mut s = (*state).clone();
            s.score_cutoff_pct = pct;
            persist(&s);
            state.set(s);
        })
    };

    let set_grid = |g: GridType| {
        let state = state.clone();
        Callback::from(move |_: MouseEvent| {
            let mut s = (*state).clone();
            s.grid = g;
            persist(&s);
            state.set(s);
        })
    };

    html! {
        <dialog class="modal modal-open" aria-modal="true" onclick={on_backdrop}>
            <div class="modal-box w-96 max-w-full" onclick={stop}>
                <button
                    type="button"
                    class="btn btn-sm btn-circle btn-ghost absolute right-2 top-2"
                    aria-label="Close quick settings"
                    onclick={on_close_click.clone()}
                >
                    { "✕" }
                </button>
                <h2 class="font-bold text-lg text-base-content mb-3">{ "Quick settings" }</h2>

                <div class="mb-3">
                    <label class="label cursor-pointer justify-start gap-2 py-1">
                        <input type="checkbox" class="toggle toggle-sm" checked={d.score_results}
                            onchange={toggle("score_results")} />
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
                    <label class="label cursor-pointer justify-start gap-2 py-1 mt-1">
                        <span class="label-text text-xs text-base-content/70">{ "Per-page cutoff (%)" }</span>
                        <input
                            type="number"
                            class="input input-bordered input-xs w-20"
                            value={d.score_cutoff_pct.to_string()}
                            step="5"
                            min="0"
                            max="95"
                            disabled={!d.score_results}
                            oninput={{
                                let set_cutoff = set_cutoff.clone();
                                Callback::from(move |e: InputEvent| {
                                    if let Some(target) = e.target()
                                        && let Ok(input) = target.dyn_into::<web_sys::HtmlInputElement>()
                                        && let Ok(v) = input.value().parse::<f32>()
                                    {
                                        set_cutoff.emit(v.clamp(0.0, 95.0));
                                    }
                                })
                            }}
                        />
                    </label>
                </div>

                <div class="mb-3">
                    <span class="text-xs text-base-content/70 block mb-1">{ "Grid density" }</span>
                    <div class="join join-sm" role="group" aria-label="Grid type">
                        <button type="button" class={classes!("btn", "btn-outline", "btn-xs", if d.grid == GridType::Auto { "btn-active" } else { "" })}
                            aria-pressed={(d.grid == GridType::Auto).to_string()}
                            onclick={set_grid(GridType::Auto)}>{ "Auto" }</button>
                        <button type="button" class={classes!("btn", "btn-outline", "btn-xs", if d.grid == GridType::Three { "btn-active" } else { "" })}
                            aria-pressed={(d.grid == GridType::Three).to_string()}
                            onclick={set_grid(GridType::Three)}>{ "3" }</button>
                        <button type="button" class={classes!("btn", "btn-outline", "btn-xs", if d.grid == GridType::Two { "btn-active" } else { "" })}
                            aria-pressed={(d.grid == GridType::Two).to_string()}
                            onclick={set_grid(GridType::Two)}>{ "2" }</button>
                        <button type="button" class={classes!("btn", "btn-outline", "btn-xs", if d.grid == GridType::One { "btn-active" } else { "" })}
                            aria-pressed={(d.grid == GridType::One).to_string()}
                            onclick={set_grid(GridType::One)}>{ "1" }</button>
                    </div>
                </div>

                <div class="divider my-2">{ "Badges" }</div>

                <label class="label cursor-pointer justify-start gap-2 py-1">
                    <input type="checkbox" class="toggle toggle-sm" checked={d.show_rating} onchange={toggle("rating")} />
                    <span class="label-text">{ "Rating badge" }</span>
                </label>
                <label class="label cursor-pointer justify-start gap-2 py-1">
                    <input type="checkbox" class="toggle toggle-sm" checked={d.show_affinity} disabled={!d.score_results}
                        onchange={toggle("affinity")} />
                    <span class="label-text">{ "Affinity score" }</span>
                </label>
                <label class="label cursor-pointer justify-start gap-2 py-1">
                    <input type="checkbox" class="toggle toggle-sm" checked={d.show_score} onchange={toggle("score")} />
                    <span class="label-text">{ "Post score" }</span>
                </label>
                <label class="label cursor-pointer justify-start gap-2 py-1">
                    <input type="checkbox" class="toggle toggle-sm" checked={d.show_post_number} onchange={toggle("post_number")} />
                    <span class="label-text">{ "Post number" }</span>
                </label>

                <div class="divider my-2">{ "Cards" }</div>

                <label class="label cursor-pointer justify-start gap-2 py-1">
                    <input type="checkbox" class="toggle toggle-sm" checked={d.show_desc} onchange={toggle("desc")} />
                    <span class="label-text">{ "Post text / tags" }</span>
                </label>
                <label class="label cursor-pointer justify-start gap-2 py-1">
                    <input type="checkbox" class="toggle toggle-sm" checked={d.show_metadata} onchange={toggle("metadata")} />
                    <span class="label-text">{ "File metadata" }</span>
                </label>
                <label class="label cursor-pointer justify-start gap-2 py-1">
                    <input type="checkbox" class="toggle toggle-sm" checked={d.show_breakdown} disabled={!d.score_results}
                        onchange={toggle("breakdown")} />
                    <span class="label-text">{ "Score breakdown" }</span>
                </label>
                <label class="label cursor-pointer justify-start gap-2 py-1">
                    <input type="checkbox" class="toggle toggle-sm" checked={d.show_detailed_breakdown} disabled={!d.score_results}
                        onchange={toggle("detailed_breakdown")} />
                    <span class="label-text">{ "Detailed breakdown" }</span>
                </label>

                <div class="modal-action">
                    <a href="/settings" class="btn btn-outline btn-sm" onclick={on_close_click.clone()}>
                        { "Open full settings" }
                    </a>
                    <button type="button" class="btn btn-primary btn-sm" onclick={on_close_click}>
                        { "Done" }
                    </button>
                </div>
            </div>
        </dialog>
    }
}
