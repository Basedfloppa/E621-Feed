//! Full post viewer — a global full-screen overlay opened from any post card.
//!
//! Shows media with controls (image/video, full-res toggle, prev/next),
//! keyboard + mobile navigation, full tags by category, artist/uploader
//! links, description, sources, parent/children info, comments (from the
//! backend `/posts/<id>/comments`) and similar posts (reuses the existing
//! `/posts/<id>/similar` endpoint).

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlInputElement, HtmlVideoElement, InputEvent, KeyboardEvent, TouchEvent, window};
use yew::prelude::*;

use crate::components::error_alert::ErrorAlert;
use crate::models::*;

// ── Global trigger: any post card can open the viewer without threading a
//    callback through every grid. Single-threaded WASM → thread_local is safe.
thread_local! {
    static VIEWER_CB: RefCell<Option<Callback<(Post, i32)>>> = const { RefCell::new(None) };
}

/// Register the viewer host's callback (called once from `PostViewerHost`).
pub fn register_post_viewer(cb: Callback<(Post, i32)>) {
    VIEWER_CB.with(|cell| {
        *cell.borrow_mut() = Some(cb);
    });
}

/// Open the full post viewer for `post` in the given `account_id` context.
pub fn open_post_viewer(post: Post, account_id: i32) {
    VIEWER_CB.with(|cell| {
        if let Some(cb) = cell.borrow().as_ref() {
            cb.emit((post, account_id));
        }
    });
}

/// Clear the registered viewer callback (host unmount hygiene).
pub fn clear_post_viewer() {
    VIEWER_CB.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// Rendered once in `App`; listens for `open_post_viewer` and shows the overlay.
#[function_component(PostViewerHost)]
pub fn post_viewer_host() -> Html {
    let open = use_state(|| Option::<(Post, i32)>::None);
    {
        let open = open.clone();
        use_effect_with((), move |_| {
            register_post_viewer(Callback::from(move |value: (Post, i32)| {
                open.set(Some(value))
            }));
            || clear_post_viewer()
        });
    }
    html! {
        if let Some((post, account_id)) = (*open).clone() {
            <PostViewer
                post={post}
                account_id={account_id}
                on_close={Callback::from(move |_| open.set(None))}
            />
        }
    }
}

#[derive(Properties, PartialEq)]
pub struct PostViewerProps {
    pub post: Post,
    pub account_id: i32,
    pub on_close: Callback<()>,
}

/// Strip the light bbcode e621 uses in comments (`[i]…[/i]`, hex `&#47;`…),
/// so bodies render as readable plain text.
fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

/// Turn an e621 bbcode link target into a safe absolute URL. Only http(s) and
/// relative `/path` targets are expanded; anything else is made a harmless
/// relative link (so `javascript:` etc. can never become an active scheme).
fn link_target(path: &str, domain: &str) -> String {
    let p = path.trim();
    if p.starts_with("http://") || p.starts_with("https://") {
        p.to_string()
    } else if let Some(rest) = p.strip_prefix('/') {
        format!("{domain}/{rest}")
    } else {
        format!("{domain}/{p}")
    }
}

/// Convert e621 bbcode into safe, escaped HTML for display: all text is
/// escaped, recognised tags become matching HTML, and `"label":path` becomes
/// a clickable link. Newlines become `<br>` outside `<code>` blocks.
fn bbcode_to_html(body: &str, domain: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    let mut in_code = false;
    while i < chars.len() {
        // e621 inline link: "label":path
        if chars[i] == '"' {
            let after = &chars[i + 1..];
            if let Some(close_off) = after.iter().position(|&c| c == '"') {
                let close_i = i + 1 + close_off;
                if close_i + 1 < chars.len() && chars[close_i + 1] == ':' {
                    let label: String = chars[i + 1..close_i].iter().collect();
                    let mut j = close_i + 2;
                    let mut path = String::new();
                    while j < chars.len() && !chars[j].is_whitespace() && chars[j] != ']' {
                        path.push(chars[j]);
                        j += 1;
                    }
                    let href = escape_attr(&link_target(&path, domain));
                    out.push_str(&format!(
                        "<a class=\"text-primary underline break-all\" href=\"{href}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a>",
                        escape_text(&label)
                    ));
                    i = j;
                    continue;
                }
            }
            out.push_str("&quot;");
            i += 1;
            continue;
        }
        if chars[i] == '[' {
            if let Some(close_off) = chars[i..].iter().position(|&c| c == ']') {
                let tag: String = chars[i + 1..i + close_off].iter().collect();
                match tag.to_lowercase().as_str() {
                    "b" => out.push_str("<b>"),
                    "/b" => out.push_str("</b>"),
                    "i" => out.push_str("<i>"),
                    "/i" => out.push_str("</i>"),
                    "u" => out.push_str("<u>"),
                    "/u" => out.push_str("</u>"),
                    "s" | "strike" => out.push_str("<s>"),
                    "/s" | "/strike" => out.push_str("</s>"),
                    "code" => {
                        out.push_str("<code class=\"rounded bg-base-200 px-1\">");
                        in_code = true;
                    }
                    "/code" => {
                        out.push_str("</code>");
                        in_code = false;
                    }
                    "quote" => {
                        out.push_str("<blockquote class=\"border-l-2 border-base-300 pl-2 my-1\">")
                    }
                    "/quote" => out.push_str("</blockquote>"),
                    "spoiler" => out.push_str("<span class=\"text-base-content/40\">"),
                    "/spoiler" => out.push_str("</span>"),
                    "center" => out.push_str("<div class=\"text-center\">"),
                    "/center" => out.push_str("</div>"),
                    _ => {} // drop unknown bbcode tags
                }
                i += close_off + 1;
                continue;
            }
            out.push('[');
            i += 1;
            continue;
        }
        if chars[i] == '\n' {
            if !in_code {
                out.push_str("<br>");
            }
            i += 1;
            continue;
        }
        match chars[i] {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(chars[i]),
        }
        i += 1;
    }
    out
}

fn viewer_media_url(post: &Post, full_res: bool) -> String {
    if is_video(post) {
        // e621's sample/preview for video posts is a static jpg thumbnail that
        // the <video> element cannot decode; always load the actual video file.
        post.files.original.url.clone().unwrap_or_default()
    } else if full_res {
        post.files.original.url.clone().unwrap_or_default()
    } else {
        post.files
            .sample
            .url
            .clone()
            .or_else(|| post.files.original.url.clone())
            .unwrap_or_default()
    }
}

fn is_video(post: &Post) -> bool {
    matches!(
        post.files.meta.ext.as_deref(),
        Some("webm") | Some("mp4") | Some("WEBM") | Some("MP4")
    )
}

fn rating_label(r: &Rating) -> &'static str {
    match r {
        Rating::S => "S",
        Rating::Q => "Q",
        Rating::E => "E",
    }
}

fn rating_badge_class(r: &Rating) -> &'static str {
    match r {
        Rating::S => "badge-success",
        Rating::Q => "badge-warning",
        Rating::E => "badge-error",
    }
}

fn tag_color(group: &str) -> &'static str {
    match group {
        "Artist" => "badge-primary",
        "Character" => "badge-secondary",
        "Species" => "badge-info",
        "Copyright" => "badge-warning",
        "Meta" => "badge-success",
        "General" => "badge-accent",
        "Invalid" => "badge-error",
        "Lore" => "badge-info",
        "Contributor" => "text-white bg-teal-600 border-teal-600",
        _ => "badge-accent",
    }
}

/// Normal (non-hover) pill styling for a tag link in its category colour.
fn tag_outline(group: &str) -> &'static str {
    match group {
        "Artist" => "border-primary text-primary",
        "Character" => "border-secondary text-secondary",
        "Species" | "Lore" => "border-info text-info",
        "Copyright" => "border-warning text-warning",
        "Meta" => "border-success text-success",
        "General" => "border-accent text-accent",
        "Invalid" => "border-error text-error",
        "Contributor" => "border-teal-500 text-teal-600",
        _ => "border-neutral text-neutral",
    }
}

/// Hover state for a tag pill: fill with the category colour and switch to its
/// contrast text, like the filled category label. Pure Tailwind utilities (no
/// daisyUI `.badge` layer, which masks plain `background-color` on the hover).
fn tag_hover(group: &str) -> &'static str {
    match group {
        "Artist" => "hover:bg-primary hover:text-primary-content hover:border-primary",
        "Character" => "hover:bg-secondary hover:text-secondary-content hover:border-secondary",
        "Species" | "Lore" => "hover:bg-info hover:text-info-content hover:border-info",
        "Copyright" => "hover:bg-warning hover:text-warning-content hover:border-warning",
        "Meta" => "hover:bg-success hover:text-success-content hover:border-success",
        "General" => "hover:bg-accent hover:text-accent-content hover:border-accent",
        "Invalid" => "hover:bg-error hover:text-error-content hover:border-error",
        "Contributor" => "hover:bg-teal-500 hover:text-white hover:border-teal-500",
        _ => "hover:bg-neutral hover:text-neutral-content hover:border-neutral",
    }
}

/// A stable per-user text colour for comment nicknames, picked by hashing the name.
fn creator_color(name: &str) -> &'static str {
    const COLORS: [&str; 6] = [
        "text-primary",
        "text-secondary",
        "text-accent",
        "text-info",
        "text-success",
        "text-warning",
    ];
    let h = name.bytes().fold(0x517cc1b727220a95u64, |a, b| {
        (a ^ u64::from(b)).wrapping_mul(0x517cc1b727220a95)
    });
    COLORS[(h as usize) % COLORS.len()]
}

fn fmt_time(secs: f64) -> String {
    let s = if secs.is_finite() && secs >= 0.0 {
        secs as u64
    } else {
        0
    };
    format!("{}:{:02}", s / 60, s % 60)
}

fn icon_play() -> Html {
    html! {
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="pointer-events-none w-4 h-4">
            <path stroke-linecap="round" stroke-linejoin="round" d="M5.25 5.653c0-.856.917-1.398 1.667-.986l11.54 6.347a1.125 1.125 0 0 1 0 1.972l-11.54 6.347a1.125 1.125 0 0 1-1.667-.986V5.653Z" />
        </svg>
    }
}

fn icon_pause() -> Html {
    html! {
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="pointer-events-none w-4 h-4">
            <path stroke-linecap="round" stroke-linejoin="round" d="M5.25 7.5A2.25 2.25 0 0 1 7.5 5.25h0a2.25 2.25 0 0 1 2.25 2.25v9A2.25 2.25 0 0 1 7.5 18.75h0A2.25 2.25 0 0 1 5.25 16.5v-9Z" />
            <path stroke-linecap="round" stroke-linejoin="round" d="M12.75 7.5A2.25 2.25 0 0 1 15 5.25h0a2.25 2.25 0 0 1 2.25 2.25v9A2.25 2.25 0 0 1 15 18.75h0a2.25 2.25 0 0 1-2.25-2.25v-9Z" />
        </svg>
    }
}

fn icon_volume_high() -> Html {
    html! {
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="pointer-events-none w-4 h-4">
            <path stroke-linecap="round" stroke-linejoin="round" d="M19.114 5.636a9 9 0 0 1 0 12.728M16.463 8.288a5.25 5.25 0 0 1 0 7.424M6.75 8.25l4.72-4.72a.75.75 0 0 1 1.28.53v15.88a.75.75 0 0 1-1.28.53l-4.72-4.72H4.51c-.88 0-1.704-.507-1.938-1.354A9.009 9.009 0 0 1 2.25 12c0-.83.112-1.633.322-2.396C2.806 8.756 3.63 8.25 4.51 8.25H6.75Z" />
        </svg>
    }
}

fn icon_volume_mute() -> Html {
    html! {
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="pointer-events-none w-4 h-4">
            <path stroke-linecap="round" stroke-linejoin="round" d="M17.25 9.75 19.5 12m0 0 2.25 2.25M19.5 12l2.25-2.25M19.5 12l-2.25 2.25m-10.5-6 4.72-4.72a.75.75 0 0 1 1.28.53v15.88a.75.75 0 0 1-1.28.53l-4.72-4.72H4.51c-.88 0-1.704-.507-1.938-1.354A9.009 9.009 0 0 1 2.25 12c0-.83.112-1.633.322-2.396C2.806 8.756 3.63 8.25 4.51 8.25H6.75Z" />
        </svg>
    }
}

fn icon_fullscreen() -> Html {
    html! {
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="pointer-events-none w-4 h-4">
            <path stroke-linecap="round" stroke-linejoin="round" d="M3.75 9.75v-6h6M21 14.25v6h-6M14.25 3.75h6v6M9.75 20.25h-6v-6" />
        </svg>
    }
}

fn icon_panel_collapse() -> Html {
    html! {
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="pointer-events-none w-4 h-4">
            <path stroke-linecap="round" stroke-linejoin="round" d="M3.75 6.75h10.5M3.75 12h10.5M3.75 17.25h10.5" />
            <path stroke-linecap="round" stroke-linejoin="round" d="m19.5 8.25-3.75 3.75 3.75 3.75" />
        </svg>
    }
}

fn icon_panel_expand() -> Html {
    html! {
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="pointer-events-none w-4 h-4">
            <path stroke-linecap="round" stroke-linejoin="round" d="M20.25 6.75H9.75M20.25 12H9.75M20.25 17.25H9.75" />
            <path stroke-linecap="round" stroke-linejoin="round" d="m4.5 8.25 3.75 3.75L4.5 15.75" />
        </svg>
    }
}

/// Smallest available thumbnail URL for a post. Walks up preview → sample →
/// original (each preferring the primary `url` then legacy `jpg`/`webp`
/// fields) and filters out empty strings and video files (mp4/webm), which
/// an `<img>` cannot render. Video posts therefore use the static preview
/// frame instead of falling through to the video file itself.
fn post_thumb_url(post: &Post) -> String {
    let pick = |url: &Option<String>, jpg: &Option<String>, webp: &Option<String>| {
        url.clone()
            .filter(|u| !u.is_empty())
            .or_else(|| jpg.clone().filter(|u| !u.is_empty()))
            .or_else(|| webp.clone().filter(|u| !u.is_empty()))
    };
    let image_only = |u: Option<String>| {
        u.filter(|u| {
            let ext = u.rsplit('.').next().unwrap_or("");
            !matches!(ext, "mp4" | "webm" | "avi" | "mov" | "m4v")
        })
    };
    image_only(pick(
        &post.files.preview.url,
        &post.files.preview.jpg,
        &post.files.preview.webp,
    ))
    .or_else(|| {
        image_only(pick(
            &post.files.sample.url,
            &post.files.sample.jpg,
            &post.files.sample.webp,
        ))
    })
    .or_else(|| image_only(post.files.original.url.clone()))
    .unwrap_or_default()
}

/// A small clickable thumbnail that opens the given post in the viewer.
/// Pass `current_id = Some(id)` to highlight the post currently on screen.
fn post_thumb_button(post: &Post, open_post: &Callback<Post>, current_id: Option<i64>) -> Html {
    let post = post.clone();
    let url = post_thumb_url(&post);
    let id = post.id;
    let is_cur = current_id == Some(id);
    let open_post = open_post.clone();
    let frame = if is_cur {
        "border-primary cursor-default"
    } else {
        "border-base-300 hover:border-primary"
    };
    html! {
        <button
            type="button"
            class={classes!("relative", "w-16", "h-16", "rounded", "overflow-hidden", "border", "shrink-0", frame)}
            title={format!("Open post {}", id)}
            onclick={Callback::from(move |_: MouseEvent| open_post.emit(post.clone()))}
        >
            {
                if url.is_empty() {
                    html! { <span class="flex items-center justify-center w-full h-full text-xs text-base-content/50">{ format!("#{id}") }</span> }
                } else {
                    html! { <img class="w-full h-full object-cover" src={url} alt={format!("Post {}", id)} loading="lazy" /> }
                }
            }
        </button>
    }
}

#[function_component(PostViewer)]
pub fn post_viewer(props: &PostViewerProps) -> Html {
    let backend_url = read_config_from_head()
        .map(|c| c.backend_domain)
        .unwrap_or_default();
    let posts_domain = read_config_from_head()
        .map(|c| c.posts_domain)
        .unwrap_or_default();

    let current = use_state(|| Rc::new(props.post.clone()));
    let full_res = use_state(|| false);
    let idx = use_state(|| 0usize);
    let fs_active = use_state(|| false);
    let side_open = use_state(|| true);

    let comments = use_state(|| Option::<Vec<Comment>>::None);
    let comments_loading = use_state(|| false);
    let comments_error = use_state(|| Option::<String>::None);
    let comments_tick = use_state(|| 0u32);

    let similar = use_state(|| Option::<Vec<ScoredPost>>::None);
    let similar_loading = use_state(|| false);
    let similar_error = use_state(|| Option::<String>::None);
    let similar_tick = use_state(|| 0u32);
    let similar_requested = use_state(|| false);
    let pool_posts = use_state(|| Option::<Vec<Post>>::None);
    let pool_loading = use_state(|| false);
    let pool_error = use_state(|| Option::<String>::None);
    let rel_parent = use_state(|| Option::<Post>::None);
    let rel_children = use_state(Vec::<Post>::new);
    let rel_loading = use_state(|| false);

    let video_ref = use_node_ref();
    let media_box = use_node_ref();
    let zoom = use_state(|| 1.0f64);
    let pan_x = use_state(|| 0.0f64);
    let pan_y = use_state(|| 0.0f64);
    let playing = use_state(|| false);
    let muted = use_state(|| false);
    let vtime = use_state(|| 0.0f64);
    let vdur = use_state(|| 0.0f64);
    let vol = use_state(|| 1.0f64);

    // Lock page scroll while the viewer is open, restore on close.
    {
        use_effect_with((), move |_| {
            let body = window().and_then(|w| w.document()).and_then(|d| d.body());
            if let Some(b) = &body {
                let _ = b.style().set_property("overflow", "hidden");
            }
            move || {
                if let Some(b) = &body {
                    let _ = b.style().set_property("overflow", "");
                }
            }
        });
    }

    // Reset media UI when the displayed post changes.
    {
        let zoom = zoom.clone();
        let pan_x = pan_x.clone();
        let pan_y = pan_y.clone();
        let playing = playing.clone();
        let muted = muted.clone();
        let vtime = vtime.clone();
        let vdur = vdur.clone();
        let vol = vol.clone();
        let id = current.id;
        use_effect_with(id, move |_| {
            zoom.set(1.0);
            pan_x.set(0.0);
            pan_y.set(0.0);
            playing.set(false);
            muted.set(false);
            vtime.set(0.0);
            vdur.set(0.0);
            vol.set(1.0);
            || ()
        });
    }

    let cur_id = current.id;
    let cur = (*current).clone();

    // Comment count: prefer the actually-loaded (visible) comments so the
    // badge and the list always agree; fall back to the post's feed count
    // while the comments are still loading.
    let comment_count_label = match &*comments {
        Some(list) => format!("{}", list.iter().filter(|c| !c.is_hidden).count()),
        None => cur.stats.comment_count.to_string(),
    };

    // Reset per-post UI when the displayed post changes. Similar is loaded
    // on demand (user clicks "Load similar"), so reset the request flag.
    {
        let similar_requested = similar_requested.clone();
        let pool_posts = pool_posts.clone();
        let pool_loading = pool_loading.clone();
        let pool_error = pool_error.clone();
        let cur_id = current.id;
        use_effect_with(cur_id, move |_| {
            similar_requested.set(false);
            pool_posts.set(None);
            pool_loading.set(false);
            pool_error.set(None);
            || ()
        });
    }

    // Some post sources (search/feed grids) can return posts without the pool
    // or parent/child fields. When that data is missing, refresh the displayed
    // post from the single-post endpoint so navigation always appears.
    {
        let current = current.clone();
        let backend = backend_url.clone();
        let id = current.id;
        let need = current.pools.is_empty()
            && current.relationships.parent_id.is_none()
            && current.relationships.children.is_empty();
        use_effect_with((id, need), move |_| {
            if need {
                let backend = backend.clone();
                let id = id;
                spawn_local(async move {
                    let url = format!("{backend}/posts/{id}");
                    if let Ok(r) = api_get(&url).send().await
                        && r.ok()
                        && let Ok(p) = r.json::<Post>().await
                    {
                        current.set(Rc::new(p));
                    }
                });
            }
            || ()
        });
    }

    // -------- Fetch comments ---------------------------------------------
    {
        let id = cur_id;
        let backend = backend_url.clone();
        let comments = comments.clone();
        let comments_loading = comments_loading.clone();
        let comments_error = comments_error.clone();
        let tick = *comments_tick;
        use_effect_with((id, tick), move |_| {
            comments.set(None);
            comments_loading.set(true);
            comments_error.set(None);
            let url = format!("{backend}/posts/{id}/comments?limit=50");
            spawn_local(async move {
                match api_get(&url).send().await {
                    Ok(r) if r.ok() => match r.json::<Vec<Comment>>().await {
                        Ok(list) => comments.set(Some(list)),
                        Err(_) => comments_error
                            .set(Some("Comments could not be read. Try again.".to_string())),
                    },
                    Ok(r) => {
                        let status = r.status();
                        let body = r.text().await.unwrap_or_default();
                        comments_error.set(Some(humanize_error_body(status, &body)));
                    }
                    Err(e) => comments_error.set(Some(humanize_network_error(e))),
                }
                comments_loading.set(false);
            });
        });
    }

    // -------- Fetch similar (only when an account context exists) ---------
    {
        let id = cur_id;
        let acc = props.account_id;
        let backend = backend_url.clone();
        let similar = similar.clone();
        let similar_loading = similar_loading.clone();
        let similar_error = similar_error.clone();
        let idx = idx.clone();
        let tick = *similar_tick;
        let requested = *similar_requested;
        use_effect_with((id, acc, tick, requested), move |_| {
            if acc <= 0 {
                similar.set(None);
                return;
            }
            if !requested {
                similar.set(None);
                similar_loading.set(false);
                similar_error.set(None);
                return;
            }
            similar.set(None);
            similar_loading.set(true);
            similar_error.set(None);
            idx.set(0);
            let url = format!("{backend}/posts/{id}/similar?account_id={acc}&limit=12");
            spawn_local(async move {
                match api_get(&url).send().await {
                    Ok(r) if r.ok() => match r.json::<Vec<ScoredPost>>().await {
                        Ok(list) => similar.set(Some(list)),
                        Err(_) => {
                            similar_error.set(Some("Similar posts could not be read.".to_string()))
                        }
                    },
                    Ok(r) => {
                        let status = r.status();
                        let body = r.text().await.unwrap_or_default();
                        similar_error.set(Some(humanize_error_body(status, &body)));
                    }
                    Err(e) => similar_error.set(Some(humanize_network_error(e))),
                }
                similar_loading.set(false);
            });
        });
    }

    // -------- Keyboard: Esc closes ---------------------------------------
    {
        let on_close = props.on_close.clone();
        use_effect_with(cur_id, move |_| {
            let cb = {
                let on_close = on_close.clone();
                Closure::wrap(Box::new(move |e: KeyboardEvent| {
                    if e.key().as_str() == "Escape" {
                        on_close.emit(());
                    }
                }) as Box<dyn FnMut(KeyboardEvent)>)
            };
            let handle = window().map(|w| {
                let _ = w.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
                (w, cb)
            });
            move || {
                if let Some((w, cb)) = handle {
                    let _ = w.remove_event_listener_with_callback(
                        "keydown",
                        cb.as_ref().unchecked_ref(),
                    );
                }
            }
        });
    }

    // Navigation within the loaded similar list.
    let go = {
        let similar = similar.clone();
        let idx = idx.clone();
        let current = current.clone();
        Callback::from(move |delta: isize| {
            let sim = (*similar).clone().unwrap_or_default();
            if sim.is_empty() {
                return;
            }
            let n = sim.len();
            let cur_id = current.id;
            // Locate the current post within the similar list so prev/next
            // stay aligned and never skip an item. If the current post is the
            // original (not in the list), step onto the appropriate end.
            let next = match sim.iter().position(|sp| sp.post.id == cur_id) {
                Some(i) => ((i as isize + delta).rem_euclid(n as isize)) as usize,
                None => {
                    if delta > 0 {
                        0
                    } else {
                        n - 1
                    }
                }
            };
            idx.set(next);
            current.set(Rc::new(sim[next].post.clone()));
        })
    };

    let open_post = {
        let similar = similar.clone();
        let idx = idx.clone();
        let current = current.clone();
        Callback::from(move |p: Post| {
            if let Some(sim) = (*similar).as_ref()
                && let Some(i) = sim.iter().position(|sp| sp.post.id == p.id)
            {
                idx.set(i);
            }
            current.set(Rc::new(p));
        })
    };

    // ---- Pool navigation (step through the pool the post belongs to) -----
    let pool_id = cur.pools.first().copied();
    let pool_step = {
        let pool_posts = pool_posts.clone();
        let open_post = open_post.clone();
        let cur_id = current.id;
        Callback::from(move |delta: isize| {
            let Some(list) = (*pool_posts).clone() else {
                return;
            };
            if let Some(i) = list.iter().position(|p| p.id == cur_id) {
                let n = list.len();
                let ni = ((i as isize + delta).rem_euclid(n as isize)) as usize;
                open_post.emit(list[ni].clone());
            }
        })
    };

    // Preload the pool's posts when the post belongs to one, so the thumbnail
    // strip renders and prev/next stepping is instant.
    {
        let backend = backend_url.clone();
        let pool_posts = pool_posts.clone();
        let pool_loading = pool_loading.clone();
        let pool_error = pool_error.clone();
        let pid = pool_id;
        use_effect_with((cur_id, pid), move |_| {
            if let Some(pid) = pid {
                pool_posts.set(None);
                pool_loading.set(true);
                pool_error.set(None);
                let backend = backend.clone();
                let pool_posts = pool_posts.clone();
                let pool_loading = pool_loading.clone();
                let pool_error = pool_error.clone();
                let url = format!("{backend}/pools/{pid}/posts");
                spawn_local(async move {
                    match api_get(&url).send().await {
                        Ok(r) if r.ok() => match r.json::<Vec<Post>>().await {
                            Ok(list) => {
                                pool_loading.set(false);
                                pool_posts.set(Some(list));
                            }
                            Err(_) => {
                                pool_loading.set(false);
                                pool_error.set(Some("Pool could not be read.".to_string()));
                            }
                        },
                        Ok(r) => {
                            pool_loading.set(false);
                            let st = r.status();
                            let body = r.text().await.unwrap_or_default();
                            pool_error.set(Some(humanize_error_body(st, &body)));
                        }
                        Err(e) => {
                            pool_loading.set(false);
                            pool_error.set(Some(humanize_network_error(e)));
                        }
                    }
                });
            }
            || ()
        });
    }

    // Parent / children thumbnails: fetch those posts so their previews render.
    {
        let backend = backend_url.clone();
        let parent = cur.relationships.parent_id;
        let children = cur.relationships.children.clone();
        let rel_parent = rel_parent.clone();
        let rel_children = rel_children.clone();
        let rel_loading = rel_loading.clone();
        use_effect_with((cur_id, parent, children.clone()), move |_| {
            rel_parent.set(None);
            rel_children.set(Vec::new());
            let ids: Vec<i64> = parent.iter().chain(children.iter()).copied().collect();
            if !ids.is_empty() {
                rel_loading.set(true);
                let backend = backend.clone();
                let rel_parent = rel_parent.clone();
                let rel_children = rel_children.clone();
                let rel_loading = rel_loading.clone();
                let parent = parent;
                spawn_local(async move {
                    let mut fetched: Vec<Post> = Vec::new();
                    for id in ids {
                        let url = format!("{backend}/posts/{id}");
                        if let Ok(r) = api_get(&url).send().await
                            && r.ok()
                            && let Ok(p) = r.json::<Post>().await
                        {
                            fetched.push(p);
                        }
                    }
                    rel_loading.set(false);
                    rel_parent
                        .set(parent.and_then(|pi| fetched.iter().find(|p| p.id == pi).cloned()));
                    rel_children.set(
                        fetched
                            .into_iter()
                            .filter(|p| parent != Some(p.id))
                            .collect(),
                    );
                });
            }
            || ()
        });
    }

    let comments_retry = {
        let comments_tick = comments_tick.clone();
        Callback::from(move |_| comments_tick.set(*comments_tick + 1))
    };
    let load_similar = {
        let similar_requested = similar_requested.clone();
        Callback::from(move |_: MouseEvent| similar_requested.set(true))
    };
    let similar_retry = {
        let similar_tick = similar_tick.clone();
        Callback::from(move |_| similar_tick.set(*similar_tick + 1))
    };

    let media_url = viewer_media_url(&cur, *full_res);
    let media_is_video = is_video(&cur);
    let is_gif = cur
        .files
        .meta
        .ext
        .as_deref()
        .map(|e| e.eq_ignore_ascii_case("gif"))
        .unwrap_or(false);

    // Touch swipe on the media area for mobile prev/next.
    let media_touch_start = use_state(|| 0.0f64);
    let on_media_touchstart = {
        let media_touch_start = media_touch_start.clone();
        Callback::from(move |e: TouchEvent| {
            if let Some(t) = e.touches().get(0) {
                media_touch_start.set(t.client_x() as f64);
            }
        })
    };
    let on_media_touchend = {
        let media_touch_start = media_touch_start.clone();
        let go = go.clone();
        Callback::from(move |e: TouchEvent| {
            let start = *media_touch_start;
            if let Some(t) = e.changed_touches().get(0) {
                let dx = t.client_x() as f64 - start;
                if dx < -50.0 {
                    go.emit(1);
                } else if dx > 50.0 {
                    go.emit(-1);
                }
            }
        })
    };

    // -------- Static-image / GIF zoom + pan ------------------------------
    let drag = use_mut_ref(|| None::<(f64, f64, f64, f64)>);
    let zoom_in = {
        let zoom = zoom.clone();
        Callback::from(move |_: MouseEvent| zoom.set((*zoom * 1.25).min(6.0)))
    };
    let zoom_out = {
        let zoom = zoom.clone();
        Callback::from(move |_: MouseEvent| zoom.set((*zoom / 1.25).max(0.2)))
    };
    let zoom_reset = {
        let zoom = zoom.clone();
        let pan_x = pan_x.clone();
        let pan_y = pan_y.clone();
        Callback::from(move |_: MouseEvent| {
            zoom.set(1.0);
            pan_x.set(0.0);
            pan_y.set(0.0);
        })
    };
    let toggle_zoom = {
        let zoom = zoom.clone();
        let pan_x = pan_x.clone();
        let pan_y = pan_y.clone();
        Callback::from(move |_: MouseEvent| {
            if *zoom > 1.0 {
                zoom.set(1.0);
                pan_x.set(0.0);
                pan_y.set(0.0);
            } else {
                zoom.set(2.0);
            }
        })
    };
    let on_media_mousedown = {
        let drag = drag.clone();
        let zoom = zoom.clone();
        let pan_x = pan_x.clone();
        let pan_y = pan_y.clone();
        Callback::from(move |e: MouseEvent| {
            if *zoom > 1.0 {
                e.prevent_default();
                *drag.borrow_mut() =
                    Some((e.client_x() as f64, e.client_y() as f64, *pan_x, *pan_y));
            }
        })
    };
    let on_media_mousemove = {
        let drag = drag.clone();
        let pan_x = pan_x.clone();
        let pan_y = pan_y.clone();
        Callback::from(move |e: MouseEvent| {
            if let Some((sx, sy, px, py)) = *drag.borrow() {
                pan_x.set(px + (e.client_x() as f64 - sx));
                pan_y.set(py + (e.client_y() as f64 - sy));
            }
        })
    };
    let on_media_mouseup = {
        let drag = drag.clone();
        Callback::from(move |_: MouseEvent| *drag.borrow_mut() = None)
    };
    let on_media_mouseleave = on_media_mouseup.clone();

    // -------- Video player controls --------------------------------------
    let on_toggle_play = {
        let video_ref = video_ref.clone();
        Callback::from(move |_: MouseEvent| {
            if let Some(v) = video_ref.cast::<HtmlVideoElement>() {
                if v.paused() {
                    let _ = v.play();
                } else {
                    let _ = v.pause();
                }
            }
        })
    };
    let on_seek = {
        let video_ref = video_ref.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(v) = video_ref.cast::<HtmlVideoElement>() {
                let t = e
                    .target_unchecked_into::<HtmlInputElement>()
                    .value()
                    .parse::<f64>()
                    .unwrap_or(0.0);
                v.set_current_time(t);
            }
        })
    };
    let on_toggle_mute = {
        let video_ref = video_ref.clone();
        let muted = muted.clone();
        Callback::from(move |_: MouseEvent| {
            if let Some(v) = video_ref.cast::<HtmlVideoElement>() {
                let m = !v.muted();
                v.set_muted(m);
                muted.set(m);
            }
        })
    };
    let on_volume_input = {
        let video_ref = video_ref.clone();
        let muted = muted.clone();
        let vol = vol.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(v) = video_ref.cast::<HtmlVideoElement>() {
                let val = e
                    .target_unchecked_into::<HtmlInputElement>()
                    .value()
                    .parse::<f64>()
                    .unwrap_or(1.0)
                    .clamp(0.0, 1.0);
                v.set_volume(val);
                v.set_muted(val <= 0.0);
                muted.set(val <= 0.0);
                vol.set(val);
            }
        })
    };
    let on_fullscreen = {
        let media_box = media_box.clone();
        Callback::from(move |_: MouseEvent| {
            let doc = window().and_then(|w| w.document());
            if doc
                .as_ref()
                .is_some_and(|d| d.fullscreen_element().is_some())
            {
                if let Some(d) = doc {
                    d.exit_fullscreen();
                }
            } else if let Some(el) = media_box.cast::<web_sys::Element>() {
                let _ = el.request_fullscreen();
            }
        })
    };
    // Track browser fullscreen state so the media can stretch to fill the screen.
    {
        let fs_active = fs_active.clone();
        use_effect_with((), move |_| {
            let fs_active = fs_active.clone();
            let on_change = Rc::new(Closure::<dyn FnMut(Event)>::wrap(Box::new(
                move |_: Event| {
                    let active = window()
                        .and_then(|w| w.document())
                        .is_some_and(|d| d.fullscreen_element().is_some());
                    fs_active.set(active);
                },
            )));
            let doc = window().and_then(|w| w.document());
            let added = doc.as_ref().map(|d| {
                d.add_event_listener_with_callback(
                    "fullscreenchange",
                    (*on_change).as_ref().unchecked_ref(),
                )
            });
            move || {
                if let (Some(d), Some(Ok(()))) = (doc.as_ref(), added) {
                    let _ = d.remove_event_listener_with_callback(
                        "fullscreenchange",
                        (*on_change).as_ref().unchecked_ref(),
                    );
                }
            }
        });
    }
    let on_video_loaded = {
        let video_ref = video_ref.clone();
        let vdur = vdur.clone();
        let vtime = vtime.clone();
        Callback::from(move |_: Event| {
            if let Some(v) = video_ref.cast::<HtmlVideoElement>() {
                vdur.set(v.duration());
                vtime.set(v.current_time());
            }
        })
    };
    let on_video_time = {
        let video_ref = video_ref.clone();
        let vtime = vtime.clone();
        Callback::from(move |_: Event| {
            if let Some(v) = video_ref.cast::<HtmlVideoElement>() {
                vtime.set(v.current_time());
            }
        })
    };
    let on_video_play = {
        let playing = playing.clone();
        Callback::from(move |_: Event| playing.set(true))
    };
    let on_video_pause = {
        let playing = playing.clone();
        Callback::from(move |_: Event| playing.set(false))
    };
    let on_video_volume = {
        let video_ref = video_ref.clone();
        let muted = muted.clone();
        let vol = vol.clone();
        Callback::from(move |_: Event| {
            if let Some(v) = video_ref.cast::<HtmlVideoElement>() {
                muted.set(v.muted());
                vol.set(v.volume());
            }
        })
    };

    // Click-to-close on the backdrop; stop propagation on interactive/panels.
    let stop_click = Callback::from(|e: MouseEvent| e.stop_propagation());
    let on_backdrop = props.on_close.reform(|_: MouseEvent| ());

    let artist_tags: Vec<String> = cur.tags.artist.clone();
    let artist_name = artist_tags.first().cloned();

    let tag_groups: Vec<(&'static str, Vec<String>)> = vec![
        ("General", cur.tags.general.clone()),
        ("Artist", cur.tags.artist.clone()),
        ("Character", cur.tags.character.clone()),
        ("Species", cur.tags.species.clone()),
        ("Copyright", cur.tags.copyright.clone()),
        ("Meta", cur.tags.meta.clone()),
        ("Lore", cur.tags.lore.clone()),
        ("Invalid", cur.tags.invalid.clone()),
        ("Contributor", cur.tags.contributor.clone()),
    ]
    .into_iter()
    .filter(|(_, tags)| !tags.is_empty())
    .collect();

    let e621_post_url = format!("{posts_domain}/posts/{}", cur.id);

    let mut prev_enabled_class = "btn btn-ghost btn-circle btn-lg";
    let mut next_enabled_class = "btn btn-ghost btn-circle btn-lg";
    if similar.is_none() || similar.as_ref().is_some_and(|s| s.is_empty()) {
        prev_enabled_class = "btn btn-ghost btn-circle btn-lg btn-disabled";
        next_enabled_class = "btn btn-ghost btn-circle btn-lg btn-disabled";
    }

    html! {
        <div class="fixed inset-0 z-50 bg-black/90 text-base-content" role="dialog" aria-modal="true" aria-label={format!("Post viewer {}", cur.id)} onclick={on_backdrop.clone()}>
            <div class="flex h-full flex-col md:flex-row">
                // ---- Media column ----
                <div
                    ref={media_box.clone()}
                    class="relative flex-1 flex items-center justify-center overflow-hidden select-none bg-black"
                    ontouchstart={on_media_touchstart}
                    ontouchend={on_media_touchend}
                    onmousedown={on_media_mousedown}
                    onmousemove={on_media_mousemove}
                    onmouseup={on_media_mouseup}
                    onmouseleave={on_media_mouseleave}
                    style="min-height: 40vh;"
                >
                    <button
                        type="button"
                        class={classes!("absolute", "left-1", "top-1/2", "-translate-y-1/2", "z-10", prev_enabled_class)}
                        onclick={{ let go=go.clone(); Callback::from(move |e: MouseEvent| { e.stop_propagation(); go.emit(-1); }) }}
                        title="Previous post"
                        aria-label="Previous"
                    >{ "‹" }</button>
                    <button
                        type="button"
                        class={classes!("absolute", "right-1", "top-1/2", "-translate-y-1/2", "z-10", next_enabled_class)}
                        onclick={{ let go=go.clone(); Callback::from(move |e: MouseEvent| { e.stop_propagation(); go.emit(1); }) }}
                        title="Next post"
                        aria-label="Next"
                    >{ "›" }</button>
                    {
                        if !*side_open {
                            html! {
                                <button type="button" class="btn btn-sm btn-circle btn-primary absolute top-3 right-3 z-20 shadow-lg" title="Show info panel" onclick={{ let side_open=side_open.clone(); Callback::from(move |e: MouseEvent| { e.stop_propagation(); side_open.set(true); }) }} aria-label="Show panel">{ icon_panel_expand() }</button>
                            }
                        } else { html!{} }
                    }

                    {
                        if media_url.is_empty() {
                            html! { <div class="text-base-content/60 text-lg">{ "No media available" }</div> }
                        } else if media_is_video && !is_gif {
                            html! {
                                <div class="relative h-full w-full flex items-center justify-center overflow-hidden">
                                    <video
                                        onclick={stop_click.clone()}
                                        ref={video_ref.clone()}
                                        class={classes!("max-h-full", "max-w-full", "object-contain", if *fs_active { vec!["w-full", "h-full", "max-h-none", "max-w-none", "object-fill"] } else { Vec::new() })}
                                        src={media_url}
                                        autoplay={true}
                                        muted={true}
                                        playsinline={true}
                                        onloadedmetadata={on_video_loaded.clone()}
                                        ontimeupdate={on_video_time.clone()}
                                        onplay={on_video_play.clone()}
                                        onpause={on_video_pause.clone()}
                                        onvolumechange={on_video_volume.clone()}
                                    />
                                    <div class="absolute bottom-2 left-1/2 -translate-x-1/2 w-[min(94%,620px)] flex items-center gap-2 rounded-xl bg-black/75 px-3 py-1.5 text-base-content" onclick={stop_click.clone()}>
                                        <button type="button" class="btn btn-xs btn-circle btn-primary" title="Play / Pause" onclick={on_toggle_play.clone()} aria-label="Play/Pause">{ if *playing { icon_pause() } else { icon_play() } }</button>
                                        <span class="text-xs tabular-nums whitespace-nowrap">{ fmt_time(*vtime) }</span>
                                        <input type="range" class="range range-primary range-xs flex-1 min-w-20" min="0" max={vdur.to_string()} value={vtime.to_string()} oninput={on_seek.clone()} aria-label="Seek" title="Seek" />
                                        <span class="text-xs tabular-nums whitespace-nowrap">{ fmt_time(*vdur) }</span>
                                        <button type="button" class="btn btn-xs btn-circle btn-ghost" title="Mute / Unmute" onclick={on_toggle_mute.clone()} aria-label="Mute">{ if *muted { icon_volume_mute() } else { icon_volume_high() } }</button>
                                        <input type="range" class="range range-xs w-16" min="0" max="1" step="0.05" value={vol.to_string()} oninput={on_volume_input.clone()} aria-label="Volume" title="Volume" />
                                        <button type="button" class="btn btn-xs btn-circle btn-ghost" title={if *fs_active { "Exit fullscreen" } else { "Fullscreen" }} onclick={on_fullscreen.clone()} aria-label="Fullscreen">{ icon_fullscreen() }</button>
                                    </div>
                                </div>
                            }
                        } else {
                            let cursor = if *zoom > 1.0 { "grab" } else { "zoom-in" };
                            let img_style = if *fs_active {
                                format!("width:100%; height:100%; max-width:none; max-height:none; object-fit:fill; transform: translate({}px,{}px) scale({}); cursor: {};", *pan_x, *pan_y, *zoom, cursor)
                            } else if (*zoom - 1.0).abs() > 0.001 {
                                format!("max-height:100%; max-width:100%; object-fit:contain; transform: translate({}px,{}px) scale({}); cursor: {};", *pan_x, *pan_y, *zoom, cursor)
                            } else {
                                format!("max-height:100%; max-width:100%; object-fit:contain; cursor: {};", cursor)
                            };
                            html! {
                                <div class="relative h-full w-full flex items-center justify-center overflow-hidden">
                                    <img class="select-none" draggable="false" onclick={stop_click.clone()} style={img_style} ondblclick={toggle_zoom.clone()} src={media_url} alt={format!("Post {}", cur.id)} />
                                    <div class="absolute bottom-2 left-1/2 -translate-x-1/2 flex items-center gap-1 rounded-full bg-black/70 px-3 py-1 text-base-content" onclick={stop_click.clone()}>
                                        <button type="button" class="btn btn-xs btn-circle btn-ghost" title="Zoom out" onclick={zoom_out.clone()} aria-label="Zoom out">{ "−" }</button>
                                        <span class="text-xs tabular-nums w-12 text-center">{ format!("{:.0}%", (*zoom) * 100.0) }</span>
                                        <button type="button" class="btn btn-xs btn-circle btn-ghost" title="Zoom in" onclick={zoom_in.clone()} aria-label="Zoom in">{ "+" }</button>
                                        <button type="button" class="btn btn-xs btn-circle btn-ghost" title="Reset zoom" onclick={zoom_reset.clone()} aria-label="Reset zoom">{ "⟲" }</button>
                                        <button type="button" class="btn btn-xs btn-circle btn-ghost" title={if *fs_active { "Exit fullscreen" } else { "Fullscreen" }} onclick={on_fullscreen.clone()} aria-label="Fullscreen">{ icon_fullscreen() }</button>
                                    </div>
                                </div>
                            }
                        }
                    }
                </div>

                // ---- Info column ----
                <div class={classes!("w-full", "md:w-[420px]", "shrink-0", "bg-base-100", "overflow-y-auto", if *side_open { "" } else { "hidden" })} onclick={stop_click.clone()}>
                    <div class="p-4 space-y-4">
                        <div class="sticky top-0 z-10 -mx-4 px-4 pb-2 pt-1 bg-base-100/95 backdrop-blur-sm border-b border-base-300 flex items-start justify-between gap-2 relative">
                            <div class="flex flex-wrap items-center gap-1.5 min-w-0">
                                <span class="badge badge-lg">{ format!("#{}", cur.id) }</span>
                                <span class={classes!("badge", rating_badge_class(&cur.rating))}>{ rating_label(&cur.rating) }</span>
                            </div>
                            <div class="flex flex-wrap items-center gap-1.5 justify-end pe-20">
                                <span class={classes!("badge", if cur.stats.score.total >= 0 { "badge-success" } else { "badge-error" })} title="Score">{ format!("▲ {}", cur.stats.score.total) }</span>
                                <span class="badge badge-secondary" title="Favorites">{ format!("♥ {}", cur.stats.fav_count) }</span>
                                <span class="badge badge-info" title="Comments">{ format!("💬 {}", comment_count_label) }</span>
                            </div>
                            <button type="button" class="btn btn-sm btn-circle btn-ghost absolute end-12 top-1/2 -translate-y-1/2" title="Hide info panel" onclick={{ let side_open=side_open.clone(); Callback::from(move |e: MouseEvent| { e.stop_propagation(); side_open.set(false); }) }} aria-label="Hide panel">{ icon_panel_collapse() }</button>
                            <button type="button" class="btn btn-sm btn-circle btn-ghost absolute end-2 top-1/2 -translate-y-1/2" title="Close" onclick={props.on_close.reform(|_: MouseEvent| ())} aria-label="Close">{"✕"}</button>
                        </div>

                        <div class="flex flex-wrap items-center gap-1.5">
                            <a class="btn btn-xs btn-primary" href={e621_post_url.clone()} target="_blank" rel="noopener noreferrer">{ "Open on e621 ↗" }</a>
                            <div class="w-px h-4 bg-base-300"></div>
                            {
                                if *full_res {
                                    html! { <button type="button" class="btn btn-xs btn-ghost" onclick={{ let full_res=full_res.clone(); Callback::from(move |_: MouseEvent| full_res.set(false)) }}>{ "Sample" }</button> }
                                } else {
                                    html! { <button type="button" class="btn btn-xs btn-ghost" onclick={{ let full_res=full_res.clone(); Callback::from(move |_: MouseEvent| full_res.set(true)) }}>{ "Full res" }</button> }
                                }
                            }
                            if let Some(a) = artist_name.clone() {
                                <a class="btn btn-xs btn-ghost" href={format!("{posts_domain}/artists/show_or_new?name={a}")} target="_blank" rel="noopener noreferrer">{ format!("Artist: {}", a) }</a>
                            }
                            {
                                if let Some(u) = cur.uploader_name.clone() {
                                    html! { <a class="btn btn-xs btn-ghost" href={format!("{posts_domain}/users/{}", cur.uploader_id)} target="_blank" rel="noopener noreferrer">{ format!("Uploader: {}", u) }</a> }
                                } else { html!{} }
                            }
                        </div>

                        // ---- Description ----
                        if let Some(desc) = &cur.description {
                            if !desc.trim().is_empty() {
                                <div class="border-t border-base-300 pt-3">
                                    <p class="text-sm text-base-content/90 whitespace-pre-wrap break-words">{ desc }</p>
                                </div>
                            }
                        }

                        // ---- Parent / Children ----
                        {
                            if cur.relationships.parent_id.is_some() || !cur.relationships.children.is_empty() {
                                html! {
                                    <div class="border-t border-base-300 pt-3">
                                        <div class="text-[0.65rem] uppercase tracking-widest text-base-content/45 font-semibold mb-1.5">{ "Parent / Children" }</div>
                                        if *rel_loading {
                                            <div class="flex items-center gap-1.5"><span class="loading loading-spinner loading-sm"></span></div>
                                        } else {
                                            <div class="flex flex-wrap gap-1.5">
                                                if let Some(parent_post) = &*rel_parent {
                                                    { post_thumb_button(parent_post, &open_post, None) }
                                                }
                                                { for rel_children.iter().map(|c| post_thumb_button(c, &open_post, None)) }
                                            </div>
                                        }
                                    </div>
                                }
                            } else { html!{} }
                        }

                        // ---- Pool ----
                        {
                            if let Some(pid) = pool_id {
                                html! {
                                    <div class="border-t border-base-300 pt-3">
                                        <div class="flex items-center justify-between">
                                            <div class="text-[0.65rem] uppercase tracking-widest text-base-content/45 font-semibold">{ format!("Pool #{}", pid) }</div>
                                            <div class="flex items-center gap-1">
                                                <button type="button" class="btn btn-xs btn-circle btn-ghost" title="Previous in pool" onclick={{ let pool_step=pool_step.clone(); Callback::from(move |_: MouseEvent| pool_step.emit(-1)) }}>{ "‹" }</button>
                                                if *pool_loading { <span class="loading loading-spinner loading-xs"></span> }
                                                <button type="button" class="btn btn-xs btn-circle btn-ghost" title="Next in pool" onclick={{ let pool_step=pool_step.clone(); Callback::from(move |_: MouseEvent| pool_step.emit(1)) }}>{ "›" }</button>
                                            </div>
                                        </div>
                                        {
                                            if let Some(err) = &*pool_error {
                                                html! { <div class="text-xs text-error mt-1">{ err }</div> }
                                            } else if let Some(list) = &*pool_posts {
                                                html! {
                                                    <div class="flex gap-1.5 mt-1.5 overflow-x-auto pb-0.5">
                                                        { for list.iter().map(|p| post_thumb_button(p, &open_post, Some(cur.id))) }
                                                    </div>
                                                }
                                            } else { html!{} }
                                        }
                                    </div>
                                }
                            } else { html!{} }
                        }

                        // ---- Sources ----
                        if !cur.sources.is_empty() {
                            <div class="border-t border-base-300 pt-3">
                                <div class="text-[0.65rem] uppercase tracking-widest text-base-content/45 font-semibold mb-1.5">{ "Sources" }</div>
                                <ul class="space-y-1">
                                    { for cur.sources.iter().take(5).map(|s| html! {
                                        <li class="truncate"><a class="text-primary text-xs break-all" href={s.clone()} target="_blank" rel="noopener noreferrer">{ s }</a></li>
                                    }) }
                                </ul>
                            </div>
                        }

                    // ---- Full tags ----
                    <div class="border-t border-base-300 pt-3">
                        <div class="text-[0.65rem] uppercase tracking-widest text-base-content/45 font-semibold mb-1.5">{ "Tags" }</div>
                        { for tag_groups.iter().filter(|(_, tags)| !tags.is_empty()).map(|(group, tags)| html! {
                            <div class="mb-2">
                                <span class={classes!("badge", "badge-sm", "me-1", tag_color(group))}>{ group }</span>
                                { for tags.iter().map(|t| html! {
                                    <a class={classes!("inline-block", "rounded-full", "border", "px-2", "py-0.5", "text-xs", "me-1", "mb-1", "transition-colors", tag_outline(group), tag_hover(group))} href={format!("{posts_domain}/posts?tags={}", t)} target="_blank" rel="noopener noreferrer">{ t }</a>
                                }) }
                            </div>
                        }) }
                    </div>

                    // ---- Comments ----
                    <div class="border-t border-base-300 pt-3">
                        <div class="text-[0.65rem] uppercase tracking-widest text-base-content/45 font-semibold mb-1.5">{ format!("Comments ({comment_count_label})") }</div>
                        {
                            if let Some(err) = &*comments_error {
                                html! { <ErrorAlert message={err.clone()} on_retry={Some(comments_retry.clone())} /> }
                            } else if *comments_loading {
                                html! { <div class="flex justify-center py-4"><span class="loading loading-spinner loading-md text-primary"></span></div> }
                            } else if let Some(list) = &*comments {
                                if list.is_empty() {
                                    html! { <p class="text-sm text-base-content/50">{ "No comments yet." }</p> }
                                } else {
                                    html! {
                                        <ul class="space-y-3 max-h-64 overflow-y-auto">
                                            { for list.iter().filter(|c| !c.is_hidden).map(|c| html! {
                                                <li class="text-sm">
                                                    {{
                                                        let name = c.creator_name.clone().unwrap_or_else(|| "anonymous".to_string());
                                                        html! {
                                                            <span class={classes!("font-semibold", "me-2", creator_color(&name))}>{ name }</span>
                                                        }
                                                    }}
                                                    <span class="text-xs text-base-content/50">{ &c.created_at }</span>
                                                    <p class="text-base-content/90 break-words mt-0.5">{ Html::from_html_unchecked(yew::AttrValue::from(bbcode_to_html(&c.body, &posts_domain))) }</p>
                                                </li>
                                            }) }
                                        </ul>
                                    }
                                }
                            } else { html!{} }
                        }
                    </div>

                    // ---- Similar ----
                    <div class="border-t border-base-300 pt-3">
                        <div class="text-[0.65rem] uppercase tracking-widest text-base-content/45 font-semibold mb-1.5">{ "Similar" }</div>
                        {
                            if props.account_id <= 0 {
                                html! { <p class="text-sm text-base-content/50">{ "Select an account to see similar posts." }</p> }
                            } else if !*similar_requested {
                                html! {
                                    <button type="button" class="btn btn-xs btn-outline" title="Load similar posts on demand" onclick={load_similar.clone()}>{ "Load similar posts" }</button>
                                }
                            } else if let Some(err) = &*similar_error {
                                html! { <ErrorAlert message={err.clone()} on_retry={Some(similar_retry.clone())} /> }
                            } else if *similar_loading {
                                html! { <div class="flex justify-center py-4"><span class="loading loading-spinner loading-md text-primary"></span></div> }
                            } else if let Some(list) = &*similar {
                                if list.is_empty() {
                                    html! { <p class="text-sm text-base-content/50">{ "No similar posts found." }</p> }
                                } else {
                                    html! {
                                        <div class="grid grid-cols-3 gap-2">
                                            { for list.iter().map(|sp| html! {
                                                <button type="button" class="block w-full rounded overflow-hidden border border-base-300 hover:border-primary" onclick={{
                                                    let open_post = open_post.clone();
                                                    let p = sp.post.clone();
                                                    Callback::from(move |_: MouseEvent| open_post.emit(p.clone()))
                                                }}>
                                                    {{
                                                        let url = post_thumb_url(&sp.post);
                                                        if !url.is_empty() {
                                                            html! { <img class="w-full object-cover" style="aspect-ratio: 4/3;" src={url.clone()} alt={format!("Post {}", sp.post.id)} loading="lazy" /> }
                                                        } else {
                                                            html! { <div class="w-full bg-base-300 flex items-center justify-center text-xs" style="aspect-ratio: 4/3;">{ format!("#{}", sp.post.id) }</div> }
                                                        }
                                                    }}
                                                </button>
                                            }) }
                                        </div>
                                    }
                                }
                            } else { html!{} }
                        }
                    </div>
                </div>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::bbcode_to_html;

    #[test]
    fn bbcode_quote_link_and_bold() {
        let html = bbcode_to_html(
            "[quote]\"xx\":/users/5 said hi [b]bold[/b][/quote]",
            "https://e621.net",
        );
        assert!(html.contains("<blockquote"), "missing blockquote: {html}");
        assert!(html.contains("</blockquote>"), "missing close: {html}");
        assert!(
            html.contains("href=\"https://e621.net/users/5\""),
            "missing link: {html}"
        );
        assert!(html.contains("<b>bold</b>"), "missing bold: {html}");
        assert!(html.contains('>'), "unexpected escaped link: {html}");
    }

    #[test]
    fn bbcode_escapes_literal_text() {
        let html = bbcode_to_html("a < b & c > d\nline2\nline3", "https://e621.net");
        assert!(
            html.contains("a &lt; b &amp; c &gt; d<br>line2<br>line3"),
            "got: {html}"
        );
    }

    #[test]
    fn bbcode_drops_unknown_tags() {
        assert_eq!(bbcode_to_html("[nonsense]text[/nonsense]", "x"), "text");
    }

    #[test]
    fn bbcode_code_preserves_newlines() {
        let html = bbcode_to_html("[code]line1\nline2[/code]", "x");
        assert!(html.contains("<code"));
        // no <br> inserted inside <code>
        assert!(
            !html.contains("line1<br>"),
            "newline leaked into code: {html}"
        );
    }

    #[test]
    fn bbcode_link_target_never_javascript() {
        let html = bbcode_to_html("\"x\":javascript:alert(1)", "https://e621.net");
        // The href must not carry a javascript: scheme (it stays a relative path
        // under the configured domain), so it can never execute.
        assert!(
            !html.contains("href=\"javascript"),
            "unsafe scheme leaked: {html}"
        );
        assert!(
            html.contains("https://e621.net/"),
            "expected safe relative link: {html}"
        );
    }
}
