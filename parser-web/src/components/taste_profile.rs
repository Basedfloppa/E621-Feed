//! Taste Profile card — makes a single backend call and renders the
//! Taste Themes v3 community clusters (CORE + KINK) as a responsive grid
//! of blocks sized by tag count / importance.

use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use crate::components::{ErrorAlert, SkeletonLines};
use crate::models::{api_get, humanize_error_body, humanize_network_error};

// ── Taste Theme models (match backend TasteTheme / TasteThemeTag) ────

#[derive(serde::Deserialize, Clone, Debug, PartialEq, Default)]
pub struct TasteThemeTag {
    pub name: String,
    pub count: i64,
    pub centrality: f32,
}

#[derive(serde::Deserialize, Clone, Debug, PartialEq, Default)]
pub struct TasteTheme {
    pub name: String,
    #[serde(default)]
    pub core: Vec<TasteThemeTag>,
    #[serde(default)]
    pub kink: Vec<TasteThemeTag>,
    pub importance: f32,
    pub size: usize,
}

// ── Response model (matches backend TasteProfileResponse) ─────────

#[derive(serde::Deserialize, Clone, Debug, PartialEq, Default)]
pub struct TasteProfileResponse {
    #[serde(default)]
    pub themes: Vec<TasteTheme>,
}

// ── Component ────────────────────────────────────────────────────

#[derive(Properties, PartialEq)]
pub struct TasteProfileCardProps {
    pub found_user: UseStateHandle<Option<crate::pages::UserInfo>>,
    pub api_base: String,
}

#[function_component(TasteProfileCard)]
pub fn taste_profile_card(props: &TasteProfileCardProps) -> Html {
    let data: UseStateHandle<Option<TasteProfileResponse>> = use_state(|| None);
    let error: UseStateHandle<Option<String>> = use_state(|| None);
    let tick = use_state(|| 0u32);

    // Fetch taste profile in one call. `tick` is bumped by the ErrorAlert
    // Retry button to re-run the fetch after a failure.
    {
        let data = data.clone();
        let error = error.clone();
        let api_base = props.api_base.clone();
        let user = (*props.found_user).clone();
        let tick = *tick;

        use_effect_with((user, tick), move |(user, _tick)| {
            let u = match user.as_ref().cloned() {
                Some(u) => u,
                None => {
                    data.set(None);
                    error.set(None);
                    return;
                }
            };
            let url = format!(
                "{}/account/{}/taste-profile?top=250&min_cooc=1",
                api_base, u.id
            );
            spawn_local(async move {
                match api_get(&url).send().await {
                    Ok(resp) if resp.ok() => match resp.json::<TasteProfileResponse>().await {
                        Ok(body) => {
                            error.set(None);
                            data.set(Some(body));
                        }
                        Err(_) => {
                            error.set(Some(
                                "Your taste profile could not be read. Try again.".to_string(),
                            ));
                            data.set(None);
                        }
                    },
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        error.set(Some(humanize_error_body(status, &body)));
                        data.set(None);
                    }
                    Err(e) => {
                        error.set(Some(humanize_network_error(e)));
                        data.set(None);
                    }
                }
            });
        });
    }

    let on_retry = {
        let tick = tick.clone();
        Callback::from(move |_| tick.set(*tick + 1))
    };

    let Some(ref tp) = *data else {
        return html! {
            <div class="card bg-base-100 shadow mt-4">
                <div class="bg-primary text-primary-content p-4">
                    <h5 class="text-lg font-semibold">{"Your Taste Profile"}</h5>
                </div>
                <div class="card-body text-base-content">
                    {
                        if let Some(err) = &*error {
                            html! { <ErrorAlert message={err.clone()} on_retry={Some(on_retry.clone())} /> }
                        } else {
                            html! { <SkeletonLines /> }
                        }
                    }
                </div>
            </div>
        };
    };

    if tp.themes.is_empty() {
        return html! {};
    }

    html! {
        <div class="card bg-base-100 shadow mt-4">
            <div class="bg-primary text-primary-content p-4">
                <h5 class="text-lg font-semibold">{"Your Taste Profile"}</h5>
            </div>
            <div class="card-body text-base-content">
                <div class="mb-3 flex flex-wrap items-center justify-between gap-2">
                    <p class="text-xs text-base-content/60">
                        {format!("{} thematic clusters found", tp.themes.len())}
                    </p>
                    <div class="flex gap-2 text-xs" aria-label="Theme legend">
                        <span class="badge badge-primary badge-sm">{"CORE"}</span>
                        <span class="badge badge-secondary badge-sm">{"KINK"}</span>
                    </div>
                </div>
                { render_balanced_grid(&tp.themes) }
            </div>
        </div>
    }
}

/// Render themes as a responsive grid ordered by importance.
fn render_balanced_grid(themes: &[TasteTheme]) -> Html {
    let max_importance = themes
        .iter()
        .map(|theme| theme.importance)
        .fold(0.0f32, f32::max)
        .max(1.0);

    html! {
        <div class="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-3">
            { for themes.iter().map(|theme| render_theme_grid(theme, max_importance)) }
        </div>
    }
}

// ── Theme renderer ────────────────────────────────────────────────────

fn render_theme_grid(theme: &TasteTheme, max_importance: f32) -> Html {
    let imp_str = if theme.importance >= 100.0 {
        format!("{:.0}", theme.importance)
    } else {
        format!("{:.1}", theme.importance)
    };
    let importance_pct = ((theme.importance / max_importance) * 100.0).clamp(4.0, 100.0);
    let total_tags = theme.core.len() + theme.kink.len();

    html! {
        <div class="card card-border bg-base-100 shadow-sm h-full">
            <div class="card-body gap-2 p-3">
                <div class="flex items-start justify-between gap-2">
                    <h6 class="card-title text-sm leading-tight">{&theme.name}</h6>
                    <span class="text-[10px] text-base-content/50 shrink-0 whitespace-nowrap">
                        {imp_str}{" · "}{total_tags}{" tags"}
                    </span>
                </div>
                <progress
                    class="progress progress-info w-full"
                    value={importance_pct.to_string()}
                    max="100"
                    aria-label="Theme importance"
                />
                // CORE tags
            if !theme.core.is_empty() {
                <div class="flex flex-wrap gap-1 mb-1.5">
                    { for theme.core.iter().map(|t| {
                        // Scale badge size by count: larger count = larger badge
                        let badge_size = if t.count >= 200 { "badge-md" } else { "badge-sm" };
                        html! {
                            <span
                                class={classes!("badge", badge_size, "badge-primary", "gap-1")}
                                title={format!("{} · count: {} · centrality: {:.3}", t.name, t.count, t.centrality)}
                            >
                                {&t.name}
                                <span class="text-primary-content/60 text-[10px]">{t.count}</span>
                            </span>
                        }
                    }) }
                </div>
            }
            // KINK tags
            if !theme.kink.is_empty() {
                <div class="flex flex-wrap gap-1">
                    { for theme.kink.iter().map(|t| {
                        let badge_size = if t.count >= 150 { "badge-md" } else { "badge-sm" };
                        html! {
                            <span
                                class={classes!("badge", badge_size, "badge-secondary", "gap-1")}
                                title={format!("{} · count: {} · centrality: {:.3}", t.name, t.count, t.centrality)}
                            >
                                {&t.name}
                                <span class="text-secondary-content/60 text-[10px]">{t.count}</span>
                            </span>
                        }
                    }) }
                </div>
            }
                <div class="card-actions justify-end pt-1">
                    <span class="text-[10px] text-base-content/50">{"Hover tags for details"}</span>
                </div>
            </div>
        </div>
    }
}
