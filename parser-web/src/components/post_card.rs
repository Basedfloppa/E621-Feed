use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::{Element, ResizeObserver, ResizeObserverEntry, window};
use yew::prelude::*;

use crate::components::shared_observer;
use crate::models::*;

#[derive(Properties, PartialEq)]
pub struct PostCardProps {
    pub post: Rc<Post>,
    pub affinity: f32,
    pub backend_url: AttrValue,
    pub account_id: i32,
    pub session_id: AttrValue,
    pub position: i32,
    #[prop_or_default]
    pub breakdown: Option<ScoreBreakdown>,
    #[prop_or_default]
    pub alt: Option<AttrValue>,
    #[prop_or_default]
    pub show_desc: bool
}

#[function_component(PostCard)]
pub fn post_card(props: &PostCardProps) -> Html {
    let post = &props.post;
    let show_desc = &props.show_desc;

    let root_ref = use_node_ref();
    let impression_logged = use_state(|| false);
    let hidden = use_state(|| false);
    let card_width = use_state(|| 0.0f64);
    let current_img_url = {
        let url = fallback_image_url(post);
        let initial = if url.is_empty() {
            None
        } else {
            Some(AttrValue::from(url))
        };
        use_state(|| initial)
    };

    let ro_handle = use_mut_ref::<
        Option<(
            ResizeObserver,
            Closure<dyn FnMut(web_sys::js_sys::Array, ResizeObserver)>,
        )>,
        _,
    >(|| None);

    {
        let card_width = card_width.clone();
        let root_ref = root_ref.clone();
        let post = Rc::clone(post);
        let current_img_url = current_img_url.clone();
        let ro_handle = ro_handle.clone();

        use_effect_with(post.id, move |_pid| {
            let choose = {
                let card_width = card_width.clone();
                let root_ref = root_ref.clone();
                let post = Rc::clone(&post);
                let current_img_url = current_img_url.clone();

                move || {
                    let Some(win) = window() else {
                        return;
                    };
                    let dpr = win.device_pixel_ratio();

                    let required_css_px = root_ref
                        .cast::<Element>()
                        .map(|el| el.client_width() as f64)
                        .unwrap_or(0.0);

                    if (*card_width - required_css_px).abs() > f64::EPSILON {
                        card_width.set(required_css_px);
                    }

                    let required_device_px = (required_css_px * dpr).ceil() as i64;
                    let new_url = preferred_image_url(post.as_ref(), required_device_px);

                    if *current_img_url != new_url {
                        current_img_url.set(new_url);
                    }
                }
            };

            if let Some(el) = root_ref.cast::<Element>() {
                let cb = {
                    let current_img_url = current_img_url.clone();
                    let post = Rc::clone(&post);
                    let card_width = card_width.clone();
                    let root_ref = root_ref.clone();

                    Closure::wrap(Box::new(
                        move |entries: web_sys::js_sys::Array, _obs: ResizeObserver| {
                            if let Some(entry) = entries.get(0).dyn_ref::<ResizeObserverEntry>() {
                                let Some(win) = window() else {
                                    return;
                                };
                                let dpr = win.device_pixel_ratio();

                                let css_w = entry.content_rect().width();
                                if (*card_width - css_w).abs() > f64::EPSILON {
                                    card_width.set(css_w);
                                }
                                let required_device_px = (css_w * dpr).ceil() as i64;

                                let new_url =
                                    preferred_image_url(post.as_ref(), required_device_px);

                                if *current_img_url != new_url {
                                    current_img_url.set(new_url);
                                }
                            } else {
                                let Some(win) = window() else {
                                    return;
                                };
                                let dpr = win.device_pixel_ratio();
                                let required_css_px = root_ref
                                    .cast::<Element>()
                                    .map(|el| el.client_width() as f64)
                                    .unwrap_or(0.0);
                                if (*card_width - required_css_px).abs() > f64::EPSILON {
                                    card_width.set(required_css_px);
                                }
                                let required_device_px = (required_css_px * dpr).ceil() as i64;
                                let new_url =
                                    preferred_image_url(post.as_ref(), required_device_px);
                                if *current_img_url != new_url {
                                    current_img_url.set(new_url);
                                }
                            }
                        },
                    )
                        as Box<dyn FnMut(web_sys::js_sys::Array, ResizeObserver)>)
                };

                let ro = ResizeObserver::new(cb.as_ref().unchecked_ref())
                    .expect("create ResizeObserver");
                ro.observe(&el);

                *ro_handle.borrow_mut() = Some((ro, cb));

                choose();
            }

            move || {
                if let Some((ro, _cb)) = ro_handle.borrow_mut().take() {
                    ro.disconnect();
                }
            }
        });
    }

    {
        let root_ref = root_ref.clone();
        let backend_url = props.backend_url.to_string();
        let session_id = props.session_id.to_string();
        let account_id = props.account_id;
        let position = props.position;
        let post_id = post.id;
        let impression_logged = impression_logged.clone();

        use_effect_with(post.id, move |_| {
            let mut registration: Option<(Element, u64)> = None;

            if !*impression_logged
                && let Some(el) = root_ref.cast::<Element>() {
                    let is_visible = std::rc::Rc::new(std::cell::Cell::new(false));
                    let is_scheduled = std::rc::Rc::new(std::cell::Cell::new(false));

                    let on_entry: shared_observer::CardCallback = {
                        let is_visible = is_visible.clone();
                        let is_scheduled = is_scheduled.clone();
                        let impression_logged = impression_logged.clone();
                        let backend_url = backend_url.clone();
                        let session_id = session_id.clone();

                        Box::new(move |entry| {
                            if entry.intersection_ratio() >= 0.5 {
                                is_visible.set(true);
                                if !is_scheduled.get() && !*impression_logged {
                                    is_scheduled.set(true);

                                    let is_visible_timeout = is_visible.clone();
                                    let is_scheduled_timeout = is_scheduled.clone();
                                    let impression_logged = impression_logged.clone();
                                    let backend_url = backend_url.clone();
                                    let session_id = session_id.clone();

                                    let timeout_cb = Closure::once_into_js(move || {
                                        is_scheduled_timeout.set(false);
                                        if is_visible_timeout.get() && !*impression_logged {
                                            impression_logged.set(true);
                                            send_interaction(
                                                backend_url,
                                                FeedInteractionRequest {
                                                    account_id,
                                                    post_id,
                                                    event_type:
                                                        FeedInteractionType::QualifiedImpression,
                                                    position,
                                                    session_id,
                                                },
                                            );
                                        }
                                    });

                                    if let Some(win) = window() {
                                        let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(
                                            timeout_cb.as_ref().unchecked_ref(),
                                            800,
                                        );
                                    }
                                }
                            } else {
                                is_visible.set(false);
                            }
                        })
                    };

                    let id = shared_observer::observe(&el, on_entry);
                    registration = Some((el, id));
                }

            move || {
                if let Some((el, id)) = registration {
                    shared_observer::unobserve(&el, id);
                }
            }
        });
    }

    let img_url = (*current_img_url).clone();
    let preview_count = preview_tag_count(*card_width);

    let alt_text = {
        let post = Rc::clone(post);
        let alt = props.alt.clone();
        use_memo((post.id, alt.clone()), move |_| {
            if let Some(alt) = alt {
                alt
            } else {
                let mut parts: Vec<&str> = Vec::new();
                parts.extend(post.tags.general.iter().map(String::as_str));
                parts.extend(post.tags.character.iter().map(String::as_str));
                parts.extend(post.tags.artist.iter().map(String::as_str));
                if parts.is_empty() {
                    AttrValue::from(format!("Post #{}", post.id))
                } else {
                    AttrValue::from(parts.join(", "))
                }
            }
        })
    };

    let (rating_label, rating_classes) = rating_badge_classes(&post.rating);

    let score_summary = post.stats.score.total;
    let score_detail = AttrValue::from(format!("↑ {}   ↓ {}", post.stats.score.up, post.stats.score.down));

    let on_hide = {
        let backend_url = props.backend_url.to_string();
        let session_id = props.session_id.to_string();
        let account_id = props.account_id;
        let position = props.position;
        let post_id = post.id;
        let hidden = hidden.clone();
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            e.prevent_default();
            hidden.set(true);
            send_interaction(
                backend_url.clone(),
                FeedInteractionRequest {
                    account_id,
                    post_id,
                    event_type: FeedInteractionType::Hide,
                    position,
                    session_id: session_id.clone(),
                },
            );
        })
    };

    let on_unhide = {
        let hidden = hidden.clone();
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            e.prevent_default();
            hidden.set(false);
        })
    };

    let posts_domain = read_config_from_head().map(|c| c.posts_domain);
    let post_url = posts_domain
        .as_deref()
        .map(|domain| format!("{domain}/posts/{}", post.id))
        .unwrap_or_else(|| format!("/posts/{}", post.id));

    let on_link_click = {
        let backend_url = props.backend_url.to_string();
        let session_id = props.session_id.to_string();
        let account_id = props.account_id;
        let position = props.position;
        let post_id = post.id;
        Callback::from(move |_e: MouseEvent| {
            send_interaction(
                backend_url.clone(),
                FeedInteractionRequest {
                    account_id,
                    post_id,
                    event_type: FeedInteractionType::Open,
                    position,
                    session_id: session_id.clone(),
                },
            );
        })
    };

    let mut root_classes = classes!(
        "card",
        "h-100",
        "overflow-hidden",
        "w-100",
        "position-relative"
    );
    if *hidden {
        root_classes.push("post-card-hidden");
    }

    let inner: Html = html! {
        <>
            <div class="position-relative card-body p-0">
                {
                    if let Some(url) = img_url {
                        html! {
                            <img
                                class="card-img-top img-fluid"
                                src={url}
                                alt={(*alt_text).clone()}
                                loading="lazy"
                                decoding="async"
                            />
                        }
                    } else {
                        html! {
                            <div
                                class="bg-secondary text-white d-flex align-items-center justify-content-center"
                                style="aspect-ratio: 4 / 3;"
                                aria-label="No preview available"
                            >
                                { "No preview available" }
                            </div>
                        }
                    }
                }

                <a
                    href={post_url.clone()}
                    target="_blank"
                    rel="noopener noreferrer"
                    class="stretched-link"
                    onclick={on_link_click}
                    aria-label={format!(
                        "Open post {} on e621 (rating {}, score {}, affinity {:.2})",
                        post.id, rating_label, post.stats.score.total, &props.affinity
                    )}
                />

                <span
                    class={classes!(rating_classes, "position-absolute", "top-0", "start-0", "m-2")}
                    title="Rating"
                    aria-label={format!("Rating {rating_label}")}
                >
                    { rating_label }
                </span>

                <button
                    type="button"
                    class={classes!(
                        "post-card-hide-btn", "btn", "btn-sm", "btn-dark",
                        "rounded-circle", "position-absolute", "top-0", "start-50",
                        "translate-middle-x", "mt-2"
                    )}
                    style="z-index: 2;"
                    onmousedown={Callback::from(|e: MouseEvent| e.stop_propagation())}
                    onclick={on_hide}
                    title="Not interested"
                    aria-label={format!("Hide post {}", post.id)}
                >
                    { "×" }
                </button>

                <span
                    class={classes!("badge", "rounded","bg-secondary","position-absolute", "top-0", "end-0", "m-2")} >
                    { format!("{:.2}",&props.affinity) }
                </span>

                <span
                    class={classes!("badge", "position-absolute", "bottom-0", "end-0", "m-2", if score_summary > 0 {"bg-success"} else {"bg-danger"})}
                    title={score_detail}
                >
                    { score_summary }
                </span>
            </div>

            {
                if show_desc == &true {
                    html! {
                    <div class="card-body p-2 text-center align-items-end">
                        <h6 class="card-title mt-1">{ format!("#{}", post.id) }</h6>
                        {
                            if let Some(breakdown) = &props.breakdown {
                                html! {
                                    <div class="mb-1 d-flex flex-wrap justify-content-center gap-1" aria-label="Score breakdown">
                                        <span class="badge text-muted text-truncate mw-100" title="Cosine similarity between this post's tags and your favourites (TF-IDF weighted).">
                                            { format!("Tag {:.2}", breakdown.tag_similarity) }
                                        </span>
                                        <span class="badge text-muted text-truncate mw-100" title="How this post's score, favourites and comments compare to the typical post you like.">
                                            { format!("Quality {:.2}", breakdown.quality_fit) }
                                        </span>
                                        <span class="badge text-muted text-truncate mw-100" title="How close this post's age is to the ages you usually engage with.">
                                            { format!("Recent {:.2}", breakdown.recency_fit) }
                                        </span>
                                        <span class="badge text-muted text-truncate mw-100" title="Match between this post's rating (S/Q/E) and the rating mix of your favourites.">
                                            { format!("Rating {:.2}", breakdown.rating_fit) }
                                        </span>
                                        <span class="badge text-muted text-truncate mw-100" title="Match between this post's media type (image / gif / video) and your usual preference.">
                                            { format!("Media {:.2}", breakdown.media_fit) }
                                        </span>
                                        <span class="badge text-muted text-truncate mw-100" title="How this post's favourite count and duration compare to the norm in your profile.">
                                            { format!("Popular {:.2}", breakdown.popularity_fit) }
                                        </span>
                                        <span class="badge text-muted text-truncate mw-100" title="Signal from your recent feed behaviour on this post's tags — impressions, opens, and hides.">
                                            { format!("Interact {:.2}", breakdown.interaction_fit) }
                                        </span>
                                        <span class="badge text-muted text-truncate mw-100" title="How coherently this post's tags relate to each other — globally (PMI lift) and inside your own favourites (pair co-occurrence).">
                                            { format!("Relation {:.2}", breakdown.tag_relation_fit) }
                                        </span>
                                    </div>
                                }
                            } else {
                                html! {}
                            }
                        }

                        {
                            if !post.tags.general.is_empty() {
                                html! {
                                    <p class="card-text text-muted small mb-0 post-tags-preview">
                                        { tag_preview(&post.tags.general, preview_count) }
                                    </p>
                                }
                            } else {
                                html! { <p class="card-text text-muted small mb-0">{ "—" }</p> }
                            }
                        }
                    </div>
                    }
                }
                else {
                    html! {}
                }
            }
        </>
    };

    if *hidden {
        return html! {
            <div
                class={classes!("card", "h-100", "post-card-hidden", "w-100", "p-3", "d-flex", "flex-column", "align-items-center", "justify-content-center", "text-center")}
                ref={root_ref}
                aria-label={format!("Post {} hidden", post.id)}
            >
                <span class="text-muted small mb-2">{ format!("Hidden #{}", post.id) }</span>
                <button
                    type="button"
                    class="btn btn-sm btn-outline-secondary"
                    onclick={on_unhide}
                    aria-label="Undo hide"
                >
                    { "Undo" }
                </button>
            </div>
        };
    }

    html! {
        <article
            class={root_classes}
            ref={root_ref}
        >
            { inner }
        </article>
    }
}

fn send_interaction(backend_url: String, payload: FeedInteractionRequest) {
    spawn_local(async move {
        let body = match serde_json::to_string(&payload) {
            Ok(body) => body,
            Err(err) => {
                web_sys::console::warn_1(&format!("failed to encode interaction: {err}").into());
                return;
            }
        };

        if let Err(err) = api_post(&format!("{backend_url}/interaction"))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
        {
            web_sys::console::warn_1(&format!("failed to send interaction: {err}").into());
        }
    });
}

fn is_supported_image(url: &str) -> bool {
    const ALLOWED: [&str; 7] = [".gif", ".png", ".jpg", ".jpeg", ".webp", ".avif", ".apng"];

    // Require an absolute URL — e621 hands back relative paths such as
    // "/images/download-deleted-preview.png" / ".../download-preview.png"
    // for deleted or preview-less posts, and the browser would resolve
    // those against *our* origin (`/images/...` 404). Anything that
    // isn't an http(s) URL is unusable as a remote thumbnail.
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return false;
    }
    ALLOWED.iter().any(|ext| lower.ends_with(ext))
}

fn preview_url(post: &Post) -> Option<&str> {
    post.files.preview.jpg.as_deref()
        .or(post.files.preview.webp.as_deref())
}

fn sample_url(post: &Post) -> Option<&str> {
    if post.has.sample {
        post.files.sample.jpg.as_deref()
            .or(post.files.sample.webp.as_deref())
    } else {
        None
    }
}

fn fallback_image_url(post: &Post) -> String {
    if let Some(url) = preview_url(post)
        && is_supported_image(url) {
            return url.to_owned();
        }
    if let Some(url) = sample_url(post)
        && is_supported_image(url) {
            return url.to_owned();
        }
    if let Some(url) = post.files.original.url.as_deref()
        && is_supported_image(url) {
            return url.to_owned();
        }
    String::new()
}

fn preferred_image_url(post: &Post, required_width: i64) -> Option<AttrValue> {
    let mut candidates: Vec<(AttrValue, i64)> = Vec::new();

    if let Some(url) = preview_url(post)
        && is_supported_image(url) {
            candidates.push((AttrValue::from(url.to_owned()), post.files.preview.width));
        }
    if let Some(url) = sample_url(post)
        && is_supported_image(url) {
            candidates.push((AttrValue::from(url.to_owned()), post.files.sample.width));
        }
    if let Some(url) = post.files.original.url.as_deref()
        && is_supported_image(url) {
            candidates.push((AttrValue::from(url.to_owned()), post.files.original.width));
        }

    candidates.sort_by_key(|&(_, w)| w);
    if let Some((u, _)) = candidates.iter().find(|&&(_, w)| w >= required_width) {
        return Some(u.clone());
    }
    candidates.last().map(|(u, _)| u.clone())
}

fn rating_badge_classes(r: &Rating) -> (&'static str, Classes) {
    match r {
        Rating::S => ("S", classes!("badge", "bg-success")),
        Rating::Q => ("Q", classes!("badge", "bg-warning", "text-dark")),
        Rating::E => ("E", classes!("badge", "bg-danger")),
    }
}

fn tag_preview(tags: &[String], n: usize) -> String {
    tags.iter()
        .take(n)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

fn preview_tag_count(card_width: f64) -> usize {
    match card_width {
        w if w >= 520.0 => 8,
        w if w >= 420.0 => 6,
        w if w >= 320.0 => 5,
        w if w >= 260.0 => 4,
        w if w >= 210.0 => 3,
        _ => 2,
    }
}
