use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::{Element, HtmlSelectElement, Node, ResizeObserver, ResizeObserverEntry, window};
use yew::prelude::*;

type OutsideMenuListener = (web_sys::Document, Closure<dyn FnMut(web_sys::MouseEvent)>);

#[derive(Clone, PartialEq)]
struct PendingBlacklistRule {
    rule: String,
    label: String,
}

use crate::components::post_grid::best_dimensions;
use crate::components::post_viewer::open_post_viewer;
use crate::components::{ConfirmModal, shared_observer};
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
    /// Human-readable "why this post" reasons (from the backend).
    #[prop_or_default]
    pub reasons: Vec<String>,
    #[prop_or_default]
    pub alt: Option<AttrValue>,
    #[prop_or_default]
    pub show_desc: bool,
    #[prop_or_default]
    pub show_metadata: bool,
    #[prop_or_default]
    pub show_breakdown: bool,
    #[prop_or_default]
    pub show_detailed_breakdown: bool,
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
    let video_failed = use_state(|| false);
    let pending_blacklist_rule: UseStateHandle<Option<PendingBlacklistRule>> = use_state(|| None);
    let tag_picker_open = use_state(|| false);
    let selected_tag = use_state(String::new);
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
        // Only treat as video for actual video containers.
        // GIF files may have a duration set by the API but
        // cannot be played through a <video> element.
        let is_video = matches!(
            post.files.meta.ext.as_deref(),
            Some("webm") | Some("mp4") | Some("WEBM") | Some("MP4")
        ) || (post.files.meta.duration.unwrap_or(0.0) > 0.0
            && !matches!(post.files.meta.ext.as_deref(), Some("gif") | Some("GIF")));
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

    let on_image_error = {
        let post = Rc::clone(post);
        let current_img_url = current_img_url.clone();
        Callback::from(move |_event: Event| {
            let current = (*current_img_url)
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            current_img_url.set(next_image_url(post.as_ref(), &current).map(AttrValue::from));
        })
    };
    let on_video_error = {
        let video_failed = video_failed.clone();
        Callback::from(move |_event: Event| video_failed.set(true))
    };
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

    let mut blockable_tags: Vec<(String, String)> = Vec::new();
    for (group, tags) in [
        ("General", &post.tags.general),
        ("Artist", &post.tags.artist),
        ("Character", &post.tags.character),
        ("Species", &post.tags.species),
        ("Copyright", &post.tags.copyright),
        ("Meta", &post.tags.meta),
    ] {
        for tag in tags {
            blockable_tags.push((tag.clone(), group.to_string()));
        }
    }
    blockable_tags.sort_by(|a, b| a.0.cmp(&b.0));
    blockable_tags.dedup_by(|a, b| a.0 == b.0);
    let artist_to_block = post.tags.artist.first().cloned();
    let media_rule = match post
        .files
        .meta
        .ext
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp4") | Some("webm") => ("video", "Block video"),
        Some("gif") => ("animated", "Block animated media"),
        _ => ("-animated", "Block static images"),
    };
    let block_callback = |rule: String, label: String| {
        let pending = pending_blacklist_rule.clone();
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            e.prevent_default();
            pending.set(Some(PendingBlacklistRule {
                rule: rule.clone(),
                label: label.clone(),
            }));
        })
    };
    let on_open_tag_picker = {
        let open = tag_picker_open.clone();
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            e.prevent_default();
            open.set(true);
        })
    };
    let on_select_tag = {
        let selected = selected_tag.clone();
        Callback::from(move |e: Event| {
            selected.set(e.target_unchecked_into::<HtmlSelectElement>().value())
        })
    };
    let on_block_artist = artist_to_block
        .as_ref()
        .map(|tag| block_callback(tag.clone(), format!("Block artist {tag}")));
    let on_block_uploader = block_callback(
        format!("uploader:!{}", post.uploader_id),
        format!(
            "Block uploads by {}",
            post.uploader_name.as_deref().unwrap_or("this uploader")
        ),
    );
    let on_block_rating = block_callback(
        format!("rating:{}", rating_label.to_ascii_lowercase()),
        format!("Block {}-rated posts", rating_label),
    );
    let on_block_media = block_callback(media_rule.0.to_string(), media_rule.1.to_string());

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
        let post_clone = post.clone();
        Callback::from(move |e: MouseEvent| {
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
            // Plain primary click opens the in-app full viewer; modifier or
            // middle clicks fall through to the external e621 link.
            let modifier = e.meta_key() || e.ctrl_key() || e.shift_key() || e.alt_key();
            if !modifier {
                e.prevent_default();
                open_post_viewer((*post_clone).clone(), account_id);
            }
        })
    };

    // Middle-click / mouse-wheel click also counts as an open, but
    // `onclick` only fires for primary button. `auxclick` fires for
    // all non-primary buttons; we only care about button === 1 (middle).
    let on_aux_link_click = {
        let backend_url = props.backend_url.to_string();
        let session_id = props.session_id.to_string();
        let account_id = props.account_id;
        let position = props.position;
        let post_id = post.id;
        let post_clone = post.clone();
        Callback::from(move |e: MouseEvent| {
            if e.button() == 1 {
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
                // Middle click just opens the post in the in-app viewer,
                // like a plain primary click.
                e.prevent_default();
                open_post_viewer((*post_clone).clone(), account_id);
            }
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
            parts.push(html! {
                <div class="p-2">
                    <crate::components::scoring_breakdown::ScoringBreakdown
                        breakdown={bd.clone()}
                        reasons={props.reasons.clone()}
                        detailed={props.show_detailed_breakdown}
                    />
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

    // Temporary loading reserve. No permanent min-height — small
    // media shouldn't leave an empty box. Instead, reserve height while the
    // media is still loading: `aspect-ratio` from the known preview
    // dimensions (with the legacy 300px as a floor), released as soon as the
    // image/video fires a load event (or errors). Cards without a preview
    // image skip the reserve — their 4/3 fallback handles the height.
    let media_ready = use_state(|| img_url.is_none());
    {
        let media_ready = media_ready.clone();
        let url = img_url.clone();
        use_effect_with(url, move |url| {
            // Re-arm the reserve whenever the media URL changes (resolution
            // switch or error-fallback advance): the freshly-rendered element
            // starts loading from scratch and must reserve height again.
            media_ready.set(url.is_none());
            || ()
        });
    }
    let mark_media_ready = {
        let media_ready = media_ready.clone();
        Callback::from(move |_e: Event| media_ready.set(true))
    };
    let media_loading_style = if *media_ready {
        String::new()
    } else {
        let (w, h) = best_dimensions(&post.files);
        format!("aspect-ratio: {w} / {h}; min-height: 300px;")
    };

    let inner: Html = html! {
        <>
            <div class="relative p-0" style={media_loading_style}>
                {
                    if let Some(url) = img_url {
                        let is_video = matches!(
                            post.files.meta.ext.as_deref(),
                            Some("webm") | Some("mp4") | Some("WEBM") | Some("MP4")
                        ) || (post.files.meta.duration.unwrap_or(0.0) > 0.0
                            && !matches!(
                                post.files.meta.ext.as_deref(),
                                Some("gif") | Some("GIF")
                            ));

                        // Belt: never try to play a GIF through <video> even if the
                // API sets a duration — browsers reject it.
                let is_actually_video = is_video
                    && !matches!(
                        post.files.original.url.as_deref(),
                        Some(u) if u.ends_with(".gif") || u.ends_with(".GIF")
                    );
                        if is_actually_video && !*video_failed {
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
                                        onloadeddata={mark_media_ready.clone()}
                                        onloadedmetadata={mark_media_ready.clone()}
                                        onerror={on_video_error.clone()}
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
                                        onload={mark_media_ready.clone()}
                                        onerror={on_image_error.clone()}
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
                                    onload={mark_media_ready.clone()}
                                    onerror={on_image_error.clone()}
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
                    onauxclick={on_aux_link_click}
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
                            <span>{ "Permanent blacklist" }</span>
                        </li>
                        if !blockable_tags.is_empty() {
                            <li><button type="button" onclick={on_open_tag_picker}>{ "Block a tag…" }</button></li>
                        }
                        if let Some(on_block_artist) = on_block_artist {
                            <li><button type="button" onclick={on_block_artist}>{ format!("Block artist {}", artist_to_block.unwrap_or_default()) }</button></li>
                        }
                        <li><button type="button" onclick={on_block_uploader}>{ "Block uploader" }</button></li>
                        <li><button type="button" onclick={on_block_rating}>{ format!("Block rating {}", rating_label) }</button></li>
                        <li><button type="button" onclick={on_block_media}>{ media_rule.1 }</button></li>
                        <li class="menu-title text-xs normal-case whitespace-normal">
                            <span>{ "Blacklist rules apply to future Feed and Search results. You will confirm the exact e621 rule before it is saved." }</span>
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

    let pending_rule = (*pending_blacklist_rule).clone();
    let on_confirm_tag_picker = {
        let open = tag_picker_open.clone();
        let selected = selected_tag.clone();
        let pending = pending_blacklist_rule.clone();
        let fallback = blockable_tags
            .first()
            .map(|(tag, _)| tag.clone())
            .unwrap_or_default();
        Callback::from(move |_| {
            let tag = if selected.is_empty() {
                fallback.clone()
            } else {
                (*selected).clone()
            };
            if !tag.is_empty() {
                pending.set(Some(PendingBlacklistRule {
                    label: format!("Block tag {tag}"),
                    rule: tag,
                }));
            }
            open.set(false);
        })
    };
    let on_cancel_tag_picker = {
        let open = tag_picker_open.clone();
        Callback::from(move |_| open.set(false))
    };
    let on_confirm_blacklist = {
        let pending = pending_blacklist_rule.clone();
        let backend_url = props.backend_url.to_string();
        let account_id = props.account_id;
        Callback::from(move |_| {
            if let Some(rule) = (*pending).clone() {
                append_blacklist_rule(backend_url.clone(), account_id, rule.rule);
            }
            pending.set(None);
        })
    };
    let on_cancel_blacklist = {
        let pending = pending_blacklist_rule.clone();
        Callback::from(move |_| pending.set(None))
    };

    html! {
        <>
            <article class={root_classes} ref={root_ref}>{ inner }</article>
            if *tag_picker_open {
                <ConfirmModal
                    open={true}
                    title={"Block a tag"}
                    confirm_label={"Continue"}
                    on_confirm={on_confirm_tag_picker}
                    on_cancel={on_cancel_tag_picker}
                >
                    <label class="label" for="post-card-block-tag"><span class="label-text">{ "Choose a tag from this post" }</span></label>
                    <select id="post-card-block-tag" class="select select-bordered w-full" onchange={on_select_tag}>
                        { for blockable_tags.iter().map(|(tag, group)| html! { <option value={tag.clone()}>{ format!("[{group}] {tag}") }</option> }) }
                    </select>
                </ConfirmModal>
            }
            if let Some(rule) = pending_rule {
                <ConfirmModal
                    open={true}
                    title={rule.label.clone()}
                    confirm_label={"Add blacklist rule"}
                    destructive={true}
                    on_confirm={on_confirm_blacklist}
                    on_cancel={on_cancel_blacklist}
                >
                    <p>{ "This permanently adds the following e621 blacklist rule to this account:" }</p>
                    <code class="mt-2 block rounded bg-base-200 p-2 break-all">{ rule.rule }</code>
                    <p class="mt-2 text-sm text-base-content/70">{ "It will filter future Feed and Search results." }</p>
                </ConfirmModal>
            }
        </>
    }
}

fn append_blacklist_rule(backend_url: String, account_id: i32, rule: String) {
    spawn_local(async move {
        let url = format!("{backend_url}/account/{account_id}/blacklist");
        #[derive(serde::Deserialize)]
        struct BlacklistResponse {
            blacklist: Option<String>,
        }
        let current = match api_get(&url).send().await {
            Ok(response) if response.ok() => response
                .json::<BlacklistResponse>()
                .await
                .ok()
                .and_then(|value| value.blacklist)
                .unwrap_or_default(),
            Ok(response) => {
                web_sys::console::warn_1(
                    &format!("failed to load blacklist: {}", response.status()).into(),
                );
                return;
            }
            Err(error) => {
                web_sys::console::warn_1(&format!("failed to load blacklist: {error}").into());
                return;
            }
        };
        if current
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case(&rule))
        {
            return;
        }
        let next = if current.trim().is_empty() {
            rule
        } else {
            format!("{current}\n{rule}")
        };
        let body = serde_json::json!({ "blacklist": next }).to_string();
        if let Err(error) = api_patch(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
        {
            web_sys::console::warn_1(&format!("failed to update blacklist: {error}").into());
        }
    });
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
        .url
        .as_deref()
        .or(post.files.preview.alt.as_deref())
        .or(post.files.preview.jpg.as_deref())
        .or(post.files.preview.webp.as_deref())
}

fn sample_url(post: &Post) -> Option<&str> {
    post.files
        .sample
        .url
        .as_deref()
        .or(post.files.sample.alt.as_deref())
        .or(post.files.sample.jpg.as_deref())
        .or(post.files.sample.webp.as_deref())
}

fn image_urls(post: &Post) -> Vec<String> {
    [
        post.files.preview.url.as_deref(),
        post.files.preview.alt.as_deref(),
        post.files.preview.jpg.as_deref(),
        post.files.preview.webp.as_deref(),
        post.files.sample.url.as_deref(),
        post.files.sample.alt.as_deref(),
        post.files.sample.jpg.as_deref(),
        post.files.sample.webp.as_deref(),
        post.files.original.url.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|url| is_supported_image(url))
    .fold(Vec::new(), |mut urls, url| {
        if !urls.iter().any(|candidate| candidate == url) {
            urls.push(url.to_owned());
        }
        urls
    })
}

fn fallback_image_url(post: &Post) -> String {
    image_urls(post).into_iter().next().unwrap_or_default()
}

/// Return the next smaller/safer image source after a load failure. The final
/// candidate is always the original file when it is browser-displayable.
fn next_image_url(post: &Post, failed_url: &str) -> Option<String> {
    let urls = image_urls(post);
    urls.iter()
        .position(|url| url == failed_url)
        .and_then(|index| urls.get(index + 1).cloned())
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
