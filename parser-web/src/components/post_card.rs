use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::{Element, Node, ResizeObserver, ResizeObserverEntry, window};
use yew::prelude::*;

type OutsideMenuListener = (web_sys::Document, Closure<dyn FnMut(web_sys::MouseEvent)>);

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
    pub show_desc: bool,
    #[prop_or_default]
    pub show_metadata: bool,
    #[prop_or_default]
    pub show_breakdown: bool,
    /// Show the rating badge (S/Q/E with colour) on the post card.
    #[prop_or(true)]
    pub show_rating: bool,
    /// Show the affinity score badge on the post card.
    #[prop_or(true)]
    pub show_affinity: bool,
    /// Show the post score (upvote/downvote total) badge on the post card.
    #[prop_or(true)]
    pub show_score: bool,
    /// Show the post number / file-metadata badge on the post card.
    #[prop_or(true)]
    pub show_post_number: bool,
}

#[function_component(PostCard)]
pub fn post_card(props: &PostCardProps) -> Html {
    let post = &props.post;
    let show_desc = &props.show_desc;

    let root_ref = use_node_ref();
    let video_ref = use_node_ref();
    let negative_menu_ref = use_node_ref();
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

    // Close the negative-signal menu when a pointer action lands outside it.
    // `details` supplies the keyboard-friendly open/close behavior; this only
    // adds the expected popover dismissal behavior.
    {
        let negative_menu_ref = negative_menu_ref.clone();
        use_effect(move || {
            let mut listener: Option<OutsideMenuListener> = None;
            if let Some(document) = window().and_then(|window| window.document()) {
                let callback = Closure::wrap(Box::new(move |event: web_sys::MouseEvent| {
                    let Some(menu) = negative_menu_ref.cast::<Element>() else {
                        return;
                    };
                    let target = event
                        .target()
                        .and_then(|target| target.dyn_into::<Node>().ok());
                    let menu_node: Node = menu.clone().unchecked_into();
                    if target.is_none_or(|target| !menu_node.contains(Some(&target))) {
                        let _ = menu.remove_attribute("open");
                    }
                }) as Box<dyn FnMut(_)>);
                let _ = document.add_event_listener_with_callback(
                    "mousedown",
                    callback.as_ref().unchecked_ref(),
                );
                listener = Some((document, callback));
            }
            move || {
                if let Some((document, callback)) = listener {
                    let _ = document.remove_event_listener_with_callback(
                        "mousedown",
                        callback.as_ref().unchecked_ref(),
                    );
                }
            }
        });
    }

    // Animated post-card previews autoplay silently. Belt-and-suspenders
    // property writes cover browsers where Yew's boolean attribute is applied
    // after the media element has begun loading.
    {
        let video_ref = video_ref.clone();
        let is_video = matches!(
            post.files.meta.ext.as_deref(),
            Some("webm") | Some("mp4") | Some("WEBM") | Some("MP4")
        ) || post.files.meta.duration.unwrap_or(0.0) > 0.0;
        use_effect_with(post.id, move |_| {
            if is_video && let Some(el) = video_ref.cast::<web_sys::HtmlVideoElement>() {
                el.set_muted(true);
                el.set_default_muted(true);
                el.set_volume(0.0);
            }
            || ()
        });
    }

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

            if !*impression_logged && let Some(el) = root_ref.cast::<Element>() {
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
                                    let _ = win
                                        .set_timeout_with_callback_and_timeout_and_arguments_0(
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

    let stop_video = Callback::from(|event: Event| {
        if let Some(video) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::HtmlVideoElement>().ok())
        {
            video.set_muted(true);
            video.set_default_muted(true);
            video.set_volume(0.0);
        }
    });

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
    let score_detail = AttrValue::from(format!(
        "↑ {}   ↓ {}",
        post.stats.score.up, post.stats.score.down
    ));

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

    let on_like = {
        let backend_url = props.backend_url.to_string();
        let session_id = props.session_id.to_string();
        let account_id = props.account_id;
        let position = props.position;
        let post_id = post.id;
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            e.prevent_default();
            send_interaction(
                backend_url.clone(),
                FeedInteractionRequest {
                    account_id,
                    post_id,
                    event_type: FeedInteractionType::Like,
                    position,
                    session_id: session_id.clone(),
                },
            );
        })
    };

    let on_strong_like = {
        let backend_url = props.backend_url.to_string();
        let session_id = props.session_id.to_string();
        let account_id = props.account_id;
        let position = props.position;
        let post_id = post.id;
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            e.prevent_default();
            send_interaction(
                backend_url.clone(),
                FeedInteractionRequest {
                    account_id,
                    post_id,
                    event_type: FeedInteractionType::StrongLike,
                    position,
                    session_id: session_id.clone(),
                },
            );
        })
    };

    let on_unhide = {
        let hidden = hidden.clone();
        let backend_url = props.backend_url.to_string();
        let session_id = props.session_id.to_string();
        let account_id = props.account_id;
        let position = props.position;
        let post_id = post.id;
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            e.prevent_default();
            hidden.set(false);
            undo_interaction(
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
        "post-card",
        "card-compact",
        "overflow-hidden",
        "w-full",
        "relative",
        "border",
        "border-base-300",
        "shadow-sm",
        "break-inside-avoid",
        "mb-3"
    );
    if *hidden {
        root_classes.push("post-card-hidden");
    }

    // Post number badge with optional file metadata
    let badge_text = {
        let size_kb = post.files.meta.size / 1024;
        let size_str = if size_kb > 1024 {
            format!("{:.1}MB", size_kb as f64 / 1024.0)
        } else {
            format!("{}KB", size_kb)
        };
        let ext = post.files.meta.ext.as_deref().unwrap_or("").to_uppercase();
        let fav_part = format!("♥ {}", post.stats.fav_count);
        if props.show_metadata {
            format!("#{} — {} {} — {}", post.id, ext, size_str, fav_part)
        } else {
            format!("#{} — {}", post.id, fav_part)
        }
    };

    // Pre-compute footer content so we don't need let-bindings inside html!.
    let footer_content: Option<Html> = {
        let mut parts: Vec<Html> = Vec::new();
        if let Some(bd) = props.breakdown.as_ref().filter(|_| props.show_breakdown) {
            let chs: [(&str, &str, f32); 11] = [
                (
                    "Tag",
                    "Cosine similarity between this post's tags and your favourites (TF-IDF weighted).",
                    bd.tag_similarity,
                ),
                (
                    "Quality",
                    "How this post's score, favourites and comments compare to the typical post you like.",
                    bd.quality_fit,
                ),
                (
                    "Recent",
                    "How close this post's age is to the ages you usually engage with.",
                    bd.recency_fit,
                ),
                (
                    "Rating",
                    "Match between this post's rating (S/Q/E) and the rating mix of your favourites.",
                    bd.rating_fit,
                ),
                (
                    "Media",
                    "Match between this post's media type (image / gif / video) and your usual preference.",
                    bd.media_fit,
                ),
                (
                    "Popular",
                    "How this post's favourite count and duration compare to the norm in your profile.",
                    bd.popularity_fit,
                ),
                (
                    "Interact",
                    "Signal from your recent feed behaviour on this post's tags — impressions, opens, and hides.",
                    bd.interaction_fit,
                ),
                (
                    "Relation",
                    "How coherently this post's tags relate to each other — globally (PMI lift) and inside your own favourites (pair co-occurrence).",
                    bd.tag_relation_fit,
                ),
                (
                    "Uploader",
                    "How this post's uploader compares to the uploaders you tend to favourite.",
                    bd.uploader_fit,
                ),
                (
                    "Exclusive",
                    "How rare or unusual this post's tag combination is within your profile — favours distinctive picks.",
                    bd.exclusivity_fit,
                ),
                (
                    "Novel",
                    "How fresh or unfamiliar this post's tags are compared to what you've seen recently.",
                    bd.novelty_fit,
                ),
            ];
            parts.push(html! {
                <div class="p-2 flex flex-wrap justify-center gap-1" aria-label="Score breakdown">
                    { for chs.iter().map(|&(label, title, val)| html! {
                        <span class="badge badge-ghost truncate max-w-full" title={title}>
                            { format!("{} {:.2}", label, val) }
                        </span>
                    }) }
                </div>
            });
        }
        if *show_desc {
            parts.push(html! {
                <div class="p-2 text-center border-t border-base-300">
                    { if !post.tags.general.is_empty() {
                        html! { <p class="text-base-content/70 text-sm mb-0 break-words">{ tag_preview(&post.tags.general, preview_count) }</p> }
                    } else {
                        html! { <p class="text-base-content/70 text-sm mb-0">{ "—" }</p> }
                    }}
                </div>
            });
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.into_iter().collect())
        }
    };

    let inner: Html = html! {
        <>
            <div class="relative p-0 min-h-25">
                {
                    if let Some(url) = img_url {
                        let is_video = matches!(
                            post.files.meta.ext.as_deref(),
                            Some("webm") | Some("mp4") | Some("WEBM") | Some("MP4")
                        ) || post.files.meta.duration.unwrap_or(0.0) > 0.0;

                        if is_video {
                            if let Some(video_url) = &post.files.original.url {
                                html! {
                                    <video
                                        ref={video_ref.clone()}
                                        class="w-full object-cover"
                                        src={video_url.clone()}
                                        poster={url.clone()}
                                        autoplay={true}
                                        muted={true}
                                        defaultmuted={true}
                                        loop={true}
                                        playsinline={true}
                                        preload="none"
                                        onplay={stop_video.clone()}
                                    />
                                }
                            } else {
                                html! {
                                    <img
                                        class="w-full object-cover"
                                        src={url}
                                        alt={(*alt_text).clone()}
                                        loading="lazy"
                                        decoding="async"
                                    />
                                }
                            }
                        } else {
                            html! {
                                <img
                                    class="w-full object-cover"
                                    src={url}
                                    alt={(*alt_text).clone()}
                                    loading="lazy"
                                    decoding="async"
                                />
                            }
                        }
                    } else {
                        html! {
                            <div
                                class="bg-base-300 text-base-content flex items-center justify-center"
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
                    class="absolute inset-0 z-1"
                    onclick={on_link_click}
                    aria-label={format!(
                        "Open post {} on e621 (rating {}, score {}, affinity {:.2})",
                        post.id, rating_label, post.stats.score.total, &props.affinity
                    )}
                />

                if props.show_rating {
                    <span
                        class={classes!(rating_classes, "absolute", "top-0", "start-0", "m-2")}
                        title="Rating"
                        aria-label={format!("Rating {rating_label}")}
                    >
                        { rating_label }
                    </span>
                }

                <details
                    ref={negative_menu_ref.clone()}
                    class="dropdown dropdown-bottom absolute top-0 left-1/2 -translate-x-1/2 mt-2"
                    style="z-index: 2;"
                >
                    <summary
                        class="post-card-hide-btn btn btn-sm btn-neutral btn-circle list-none"
                        onmousedown={Callback::from(|e: MouseEvent| e.stop_propagation())}
                        title="Recommendation controls"
                        aria-label={format!("Recommendation controls for post {}", post.id)}
                    >
                        { "…" }
                    </summary>
                    <ul
                        class="dropdown-content menu rounded-box bg-base-100 shadow w-52 mt-1 p-2"
                        onmouseleave={{
                            let negative_menu_ref = negative_menu_ref.clone();
                            Callback::from(move |_event: MouseEvent| {
                                if let Some(menu) = negative_menu_ref.cast::<Element>() {
                                    let _ = menu.remove_attribute("open");
                                }
                            })
                        }}
                    >
                        <li>
                            <button type="button" onclick={on_like}>
                                <span>{ "Like" }</span>
                            </button>
                        </li>
                        <li>
                            <button type="button" onclick={on_strong_like}>
                                <span>{ "Strong like" }</span>
                            </button>
                        </li>
                        <li>
                            <button type="button" onclick={on_hide}>
                                <span>{ "Not interested" }</span>
                            </button>
                        </li>
                        <li class="menu-title text-xs normal-case whitespace-normal">
                            <span>{ "Like increases matching-tag affinity. Strong like counts three times. Not interested hides this post and reduces matching-tag affinity; it can be undone." }</span>
                        </li>
                    </ul>
                </details>

                if props.show_affinity {
                    <span
                        class={classes!("badge", "badge-ghost", "absolute", "top-0", "right-0", "m-2")}
                        title={"Overall recommendation score — blends tag similarity, quality, recency, rating, media, popularity, interaction, and tag-relation signals into a single affinity measure. Higher values indicate a stronger match with your personal preferences, but absolute scores shift with model tuning."}
                        aria-label={format!("Affinity {:.2}", props.affinity)}>
                        { format!("{:.2}",&props.affinity) }
                    </span>
                }

                if props.show_score {
                    <span
                        class={classes!("badge", "absolute", "bottom-0", "right-0", "m-2", if score_summary > 0 {"badge-success"} else {"badge-error"})}
                        title={score_detail}
                    >
                        { score_summary }
                    </span>
                }

                if props.show_post_number {
                    <span class="absolute bottom-0 left-0 m-2 badge badge-neutral text-neutral-content text-sm leading-none" style="font-size:0.65rem;">
                        { badge_text.clone() }
                    </span>
                }
            </div>

            // Card footer: score breakdown + tags, pushed to bottom so all
            // cards in a grid row have their footer starting at the same Y.
            {
                if let Some(footer) = footer_content {
                    html! { <div class="mt-auto bg-base-100 border-и border-base-300">{ footer }</div> }
                } else { html!{} }
            }
        </>
    };

    if *hidden {
        return html! {
            <div
                class={classes!("card", "min-h-25", "h-full", "post-card-hidden", "w-full", "p-3", "flex", "flex-col", "items-center", "justify-center", "text-center", "break-inside-avoid", "mb-3")}
                ref={root_ref}
                aria-label={format!("Post {} hidden", post.id)}
            >
                <span class="text-base-content/70 text-sm mb-2">{ format!("Hidden #{}", post.id) }</span>
                <button
                    type="button"
                    class="btn btn-sm btn-outline"
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

fn undo_interaction(backend_url: String, payload: FeedInteractionRequest) {
    spawn_local(async move {
        let Ok(body) = serde_json::to_string(&payload) else {
            return;
        };
        if let Err(err) = api_delete(&format!("{backend_url}/interaction"))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
        {
            web_sys::console::warn_1(&format!("failed to undo interaction: {err}").into());
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
    post.files
        .preview
        .jpg
        .as_deref()
        .or(post.files.preview.webp.as_deref())
}

fn sample_url(post: &Post) -> Option<&str> {
    if post.has.sample {
        post.files
            .sample
            .jpg
            .as_deref()
            .or(post.files.sample.webp.as_deref())
    } else {
        None
    }
}

fn fallback_image_url(post: &Post) -> String {
    if let Some(url) = preview_url(post)
        && is_supported_image(url)
    {
        return url.to_owned();
    }
    if let Some(url) = sample_url(post)
        && is_supported_image(url)
    {
        return url.to_owned();
    }
    if let Some(url) = post.files.original.url.as_deref()
        && is_supported_image(url)
    {
        return url.to_owned();
    }
    String::new()
}

fn preferred_image_url(post: &Post, required_width: i64) -> Option<AttrValue> {
    let mut candidates: Vec<(AttrValue, i64)> = Vec::new();

    if let Some(url) = preview_url(post)
        && is_supported_image(url)
    {
        candidates.push((AttrValue::from(url.to_owned()), post.files.preview.width));
    }
    if let Some(url) = sample_url(post)
        && is_supported_image(url)
    {
        candidates.push((AttrValue::from(url.to_owned()), post.files.sample.width));
    }
    if let Some(url) = post.files.original.url.as_deref()
        && is_supported_image(url)
    {
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
        Rating::S => ("S", classes!("badge", "badge-success")),
        Rating::Q => ("Q", classes!("badge", "badge-warning")),
        Rating::E => ("E", classes!("badge", "badge-error")),
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
