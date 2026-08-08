use crate::components::icons::{
    IconClock, IconDiscover, IconExclusive, IconInteract, IconMedia, IconNovel, IconPopular,
    IconQuestion, IconRating, IconRelation, IconStar, IconTag, IconUploader,
};
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
    /// Backend-generated human-readable reasons ("New artist…", "Matches
    /// your … taste", …). Shown above the channel breakdown when present.
    #[prop_or_default]
    pub reasons: Vec<String>,
    #[prop_or(false)]
    pub detailed: bool,
}

#[function_component(ScoringBreakdown)]
pub fn scoring_breakdown(props: &ScoringBreakdownProps) -> Html {
    let reasons = &props.reasons;
    if props.detailed {
        render_detailed(&props.breakdown, reasons)
    } else {
        render_simple(&props.breakdown, reasons)
    }
}

// ── Helpers ──────────────────────────────────────────────────────

struct ChannelInfo {
    label: &'static str,
    icon: Html,
    value: f32,
    /// Short explanation prefix, e.g. "tags match your profile"
    why: &'static str,
}

fn all_channels(bd: &ScoreBreakdown) -> [ChannelInfo; 12] {
    [
        ChannelInfo {
            label: "Tag",
            icon: html! { <IconTag /> },
            value: bd.tag_similarity,
            why: "tags match your profile",
        },
        ChannelInfo {
            label: "Quality",
            icon: html! { <IconStar /> },
            value: bd.quality_fit,
            why: "high-quality post",
        },
        ChannelInfo {
            label: "Recent",
            icon: html! { <IconClock /> },
            value: bd.recency_fit,
            why: "recently posted",
        },
        ChannelInfo {
            label: "Rating",
            icon: html! { <IconRating /> },
            value: bd.rating_fit,
            why: "rating you prefer",
        },
        ChannelInfo {
            label: "Media",
            icon: html! { <IconMedia /> },
            value: bd.media_fit,
            why: "media type you like",
        },
        ChannelInfo {
            label: "Popular",
            icon: html! { <IconPopular /> },
            value: bd.popularity_fit,
            why: "popular with others",
        },
        ChannelInfo {
            label: "Interact",
            icon: html! { <IconInteract /> },
            value: bd.interaction_fit,
            why: "similar to posts you interacted with",
        },
        ChannelInfo {
            label: "Relation",
            icon: html! { <IconRelation /> },
            value: bd.tag_relation_fit,
            why: "coherent tag combination",
        },
        ChannelInfo {
            label: "Uploader",
            icon: html! { <IconUploader /> },
            value: bd.uploader_fit,
            why: "uploader you tend to favourite",
        },
        ChannelInfo {
            label: "Exclusive",
            icon: html! { <IconExclusive /> },
            value: bd.exclusivity_fit,
            why: "rare tag combination",
        },
        ChannelInfo {
            label: "Novel",
            icon: html! { <IconNovel /> },
            value: bd.novelty_fit,
            why: "fresh unfamiliar tags",
        },
        ChannelInfo {
            label: "Discover",
            icon: html! { <IconDiscover /> },
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

fn reasons_blocks(reasons: &[String]) -> Html {
    if reasons.is_empty() {
        return html! {};
    }
    html! {
        <div class="flex flex-wrap justify-center gap-1 px-1 pt-1">
            { for reasons.iter().map(|r| html! {
                <span class="inline-flex items-center gap-1 max-w-full rounded-full border border-primary/40 bg-primary/10 px-2.5 py-1 text-xs font-normal leading-snug text-primary">
                    <IconQuestion />
                    <span class="min-w-0 break-words">{ r.as_str() }</span>
                </span>
            }) }
        </div>
    }
}

fn render_simple(bd: &ScoreBreakdown, reasons: &[String]) -> Html {
    let chs = all_channels(bd);
    let tops = top_k(&chs, 3);
    let sentence = why_sentence(&tops);

    if tops.is_empty() && reasons.is_empty() {
        return html! {
            <div class="flex flex-col gap-1 px-2 py-1 text-center text-xs" aria-label="Why this post">
                <p class="text-base-content/40 text-xs italic">{ &sentence }</p>
            </div>
        };
    }

    let badges: Html = tops.iter().map(|c| html! {
        <span class="badge badge-ghost gap-1 text-xs" title={format!("{} = {:.4}", c.label, c.value)}>
            <span class="opacity-70">{ c.icon.clone() }</span>
            { format!("{:.2}", c.value) }
        </span>
    }).collect();

    // When the backend produced concrete reasons, they are the primary
    // explanation; the generi top-channel sentence is omitted to avoid
    // saying the same thing twice in different words.
    let primary: Html = if reasons.is_empty() {
        html! {
            <p class="text-base-content/80 text-xs leading-tight italic text-center">
                { &sentence }
            </p>
        }
    } else {
        reasons_blocks(reasons)
    };

    html! {
        <div class="flex flex-col gap-1 px-2 py-1 text-center text-xs" aria-label="Why this post">
            { primary }
            <div class="flex flex-wrap justify-center gap-1">
                { badges }
            </div>
        </div>
    }
}

// ── Detailed mode ─────────────────────────────────────────────────

fn render_detailed(bd: &ScoreBreakdown, reasons: &[String]) -> Html {
    let chs = all_channels(bd);

    html! {
        <div class="flex flex-col gap-1 p-2" aria-label="Score breakdown">
            { reasons_blocks(reasons) }
            { for chs.iter().map(|c| {
                let bar_pct = (c.value * 100.0).clamp(0.0, 100.0);
                let bar_style = format!("width: {:.0}%", bar_pct);
                let colour = bar_colour(c.value);
                html! {
                    <div class="flex items-center gap-2 text-xs">
                        <span class="w-16 shrink-0 text-base-content/70 truncate" title={c.label}>
                            { c.icon.clone() }
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
