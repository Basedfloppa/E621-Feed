use crate::models::*;
use yew::prelude::*;

/// A human-readable explanation of a post's score.
///
/// Two display modes:
/// - **Simple** (default): top-3 channels with an icon and a short "why"
///   sentence, e.g. "Because the tags match your profile + fresh artist".
/// - **Detailed**: all 12 channels with colour-coded progress bars.
#[derive(Properties, PartialEq, Clone)]
pub struct ScoringBreakdownProps {
    pub breakdown: ScoreBreakdown,
    #[prop_or(false)]
    pub detailed: bool,
}

#[function_component(ScoringBreakdown)]
pub fn scoring_breakdown(props: &ScoringBreakdownProps) -> Html {
    if props.detailed {
        render_detailed(&props.breakdown)
    } else {
        render_simple(&props.breakdown)
    }
}

// ── Helpers ──────────────────────────────────────────────────────

struct ChannelInfo {
    label: &'static str,
    icon: &'static str,
    value: f32,
    /// Short explanation prefix, e.g. "tags match your profile"
    why: &'static str,
}

fn all_channels(bd: &ScoreBreakdown) -> [ChannelInfo; 12] {
    [
        ChannelInfo {
            label: "Tag",
            icon: "🏷️",
            value: bd.tag_similarity,
            why: "tags match your profile",
        },
        ChannelInfo {
            label: "Quality",
            icon: "⭐",
            value: bd.quality_fit,
            why: "high-quality post",
        },
        ChannelInfo {
            label: "Recent",
            icon: "🕐",
            value: bd.recency_fit,
            why: "recently posted",
        },
        ChannelInfo {
            label: "Rating",
            icon: "🔞",
            value: bd.rating_fit,
            why: "rating you prefer",
        },
        ChannelInfo {
            label: "Media",
            icon: "🎞️",
            value: bd.media_fit,
            why: "media type you like",
        },
        ChannelInfo {
            label: "Popular",
            icon: "🔥",
            value: bd.popularity_fit,
            why: "popular with others",
        },
        ChannelInfo {
            label: "Interact",
            icon: "👆",
            value: bd.interaction_fit,
            why: "similar to posts you interacted with",
        },
        ChannelInfo {
            label: "Relation",
            icon: "🔗",
            value: bd.tag_relation_fit,
            why: "coherent tag combination",
        },
        ChannelInfo {
            label: "Uploader",
            icon: "📤",
            value: bd.uploader_fit,
            why: "uploader you tend to favourite",
        },
        ChannelInfo {
            label: "Exclusive",
            icon: "💎",
            value: bd.exclusivity_fit,
            why: "rare tag combination",
        },
        ChannelInfo {
            label: "Novel",
            icon: "✨",
            value: bd.novelty_fit,
            why: "fresh unfamiliar tags",
        },
        ChannelInfo {
            label: "Discover",
            icon: "🔍",
            value: bd.artist_discovery_fit,
            why: "new artist near your tastes",
        },
    ]
}

fn bar_colour(value: f32) -> &'static str {
    if value >= 0.15 {
        "bg-success"
    } else if value >= 0.05 {
        "bg-info"
    } else if value > 0.0 {
        "bg-warning"
    } else {
        "bg-base-300"
    }
}

/// Pick the top-K channels by absolute contribution (value).
fn top_k(chs: &[ChannelInfo; 12], k: usize) -> Vec<&ChannelInfo> {
    let mut ranked: Vec<&ChannelInfo> = chs.iter().collect();
    ranked.sort_by(|a, b| {
        b.value
            .partial_cmp(&a.value)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
        .into_iter()
        .take(k)
        .filter(|c| c.value > 0.0)
        .collect()
}

/// Build a human-readable "why" sentence from the top channels.
fn why_sentence(chs: &[&ChannelInfo]) -> String {
    if chs.is_empty() {
        return "No strong signals — score is near default.".to_string();
    }
    let parts: Vec<&str> = chs.iter().map(|c| c.why).collect();
    if parts.len() == 1 {
        format!("Because {}", parts[0])
    } else {
        format!(
            "Because {} + {}",
            parts[..parts.len() - 1].join(" + "),
            parts[parts.len() - 1]
        )
    }
}

// ── Simple mode ───────────────────────────────────────────────────

fn render_simple(bd: &ScoreBreakdown) -> Html {
    let chs = all_channels(bd);
    let tops = top_k(&chs, 3);
    let sentence = why_sentence(&tops);

    if tops.is_empty() {
        return html! {
            <div class="flex flex-col gap-1 px-2 py-1 text-center text-xs" aria-label="Why this post">
                <p class="text-base-content/40 text-xs italic">{ &sentence }</p>
            </div>
        };
    }

    let badges: Html = tops.iter().map(|c| html! {
        <span class="badge badge-ghost gap-1 text-xs" title={format!("{} = {:.4}", c.label, c.value)}>
            <span class="opacity-70">{ c.icon }</span>
            { format!("{:.2}", c.value) }
        </span>
    }).collect();

    html! {
        <div class="flex flex-col gap-1 px-2 py-1 text-center text-xs" aria-label="Why this post">
            <p class="text-base-content/80 text-xs leading-tight italic text-center">
                { &sentence }
            </p>
            <div class="flex flex-wrap justify-center gap-1">
                { badges }
            </div>
        </div>
    }
}

// ── Detailed mode ─────────────────────────────────────────────────

fn render_detailed(bd: &ScoreBreakdown) -> Html {
    let chs = all_channels(bd);

    html! {
        <div class="flex flex-col gap-1 p-2" aria-label="Score breakdown">
            { for chs.iter().map(|c| {
                let bar_pct = (c.value * 100.0).clamp(0.0, 100.0);
                let bar_style = format!("width: {:.0}%", bar_pct);
                let colour = bar_colour(c.value);
                html! {
                    <div class="flex items-center gap-2 text-xs">
                        <span class="w-16 shrink-0 text-base-content/70 truncate" title={c.label}>
                            { c.icon }
                            { " " }
                            { c.label }
                        </span>
                        <div class="flex-1 h-3 rounded bg-base-300 overflow-hidden">
                            <div class={classes!("h-full", "rounded", colour)}
                                 style={bar_style}
                                 title={format!("{} = {:.4}", c.label, c.value)}>
                            </div>
                        </div>
                        <span class="w-10 text-right tabular-nums text-base-content/60">
                            { format!("{:.2}", c.value) }
                        </span>
                    </div>
                }
            })}
        </div>
    }
}
