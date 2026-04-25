use std::cell::RefCell;
use std::rc::Rc;

use reqwasm::http::Request;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, MouseEvent as WebMouseEvent, MutationObserver,
    MutationObserverInit, js_sys,
};
use yew::prelude::*;

use crate::models::{
    TagRelationEdge, TagRelationGraphPayload, TagRelationNode, get_or_create_owner_token,
};
use crate::pages::UserInfo;

#[derive(Properties, PartialEq)]
pub struct TagRelationGraphCardProps {
    pub found_user: UseStateHandle<Option<UserInfo>>,
    pub api_base: String,
}

#[derive(Clone, Copy, Default)]
struct NodeState {
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    radius: f64,
}

#[derive(Default, Clone)]
struct LayoutState {
    nodes: Vec<NodeState>,
    width: f64,
    height: f64,
}

#[function_component(TagRelationGraphCard)]
pub fn tag_relation_graph_card(props: &TagRelationGraphCardProps) -> Html {
    let payload: UseStateHandle<Option<TagRelationGraphPayload>> = use_state(|| None);
    let loading = use_state(|| false);
    let error: UseStateHandle<Option<String>> = use_state(|| None);
    let top_n = use_state(|| 60usize);
    let min_cooc = use_state(|| 2i64);
    let theme_trigger = use_state(|| 0u32);
    let resize_trigger = use_state(|| 0u32);
    let hover_idx: UseStateHandle<Option<usize>> = use_state(|| None);

    let canvas_ref = use_node_ref();
    let layout_ref: Rc<RefCell<LayoutState>> = use_mut_ref(LayoutState::default);

    {
        let payload = payload.clone();
        let loading = loading.clone();
        let error = error.clone();
        let api_base = props.api_base.clone();
        let found_user = props.found_user.clone();
        let top_n_val = *top_n;
        let min_cooc_val = *min_cooc;

        use_effect_with(
            (
                (*found_user).clone(),
                top_n_val,
                min_cooc_val,
                api_base.clone(),
            ),
            move |(user, top_n_val, min_cooc_val, api_base)| {
                let Some(user) = user.clone() else {
                    payload.set(None);
                    return;
                };
                let Some(owner_token) = get_or_create_owner_token() else {
                    error.set(Some("Missing device token".to_string()));
                    return;
                };

                let url = format!(
                    "{}/account/{}/tag_relations?owner_token={}&top={}&min_cooc={}",
                    api_base,
                    user.id,
                    urlencoding::encode(&owner_token),
                    top_n_val,
                    min_cooc_val,
                );
                let payload = payload.clone();
                let loading = loading.clone();
                let error = error.clone();
                loading.set(true);
                error.set(None);

                wasm_bindgen_futures::spawn_local(async move {
                    match Request::get(&url).send().await {
                        Ok(resp) if resp.ok() => match resp.json::<TagRelationGraphPayload>().await
                        {
                            Ok(graph) => {
                                payload.set(Some(graph));
                            }
                            Err(e) => {
                                error.set(Some(format!("Failed to parse graph: {e}")));
                                payload.set(None);
                            }
                        },
                        Ok(resp) => {
                            let status = resp.status();
                            error.set(Some(format!("Graph fetch failed (status {status})")));
                            payload.set(None);
                        }
                        Err(e) => {
                            error.set(Some(format!("Network error: {e}")));
                            payload.set(None);
                        }
                    }
                    loading.set(false);
                });
            },
        );
    }

    {
        let theme_trigger = theme_trigger.clone();
        use_effect_with((), move |_| {
            let document = web_sys::window().unwrap().document().unwrap();
            let target = document.body().unwrap();
            let trigger = theme_trigger.clone();
            let callback = Closure::<dyn FnMut(js_sys::Array, _)>::new(
                move |mutations: js_sys::Array, _obs: MutationObserver| {
                    for i in 0..mutations.length() {
                        let mutation = mutations.get(i).dyn_into::<web_sys::MutationRecord>().ok();
                        if let Some(m) = mutation {
                            let attr = m.attribute_name();
                            if attr.as_deref() == Some("data-bs-theme")
                                || attr.as_deref() == Some("class")
                            {
                                trigger.set(*trigger + 1);
                                break;
                            }
                        }
                    }
                },
            );
            let observer = MutationObserver::new(callback.as_ref().unchecked_ref()).unwrap();
            let options = MutationObserverInit::new();
            options.set_attributes(true);
            observer.observe_with_options(&target, &options).unwrap();
            callback.forget();
            move || observer.disconnect()
        });
    }

    {
        let resize_trigger = resize_trigger.clone();
        use_effect_with((), move |_| {
            let closure = Closure::<dyn FnMut()>::new(move || {
                resize_trigger.set(*resize_trigger + 1);
            });
            let window = web_sys::window().unwrap();
            window
                .add_event_listener_with_callback("resize", closure.as_ref().unchecked_ref())
                .unwrap();
            move || {
                window
                    .remove_event_listener_with_callback(
                        "resize",
                        closure.as_ref().unchecked_ref(),
                    )
                    .unwrap();
            }
        });
    }

    {
        let canvas_ref = canvas_ref.clone();
        let payload = payload.clone();
        let layout_ref = layout_ref.clone();
        let hover_idx_render = hover_idx.clone();
        use_effect_with(
            (
                payload.clone(),
                *theme_trigger,
                *resize_trigger,
                *hover_idx_render,
            ),
            move |(payload, _, _, hover_idx_val)| {
                let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() else {
                    return;
                };
                if let Some(graph) = payload.as_ref() {
                    let logical_width = canvas.client_width().max(0) as f64;
                    if logical_width <= 0.0 {
                        return;
                    }
                    let logical_height = (logical_width * 0.66).clamp(360.0, 720.0);

                    let mut layout = layout_ref.borrow_mut();
                    let layout_dirty = layout.nodes.len() != graph.nodes.len()
                        || (layout.width - logical_width).abs() > 0.5
                        || (layout.height - logical_height).abs() > 0.5;
                    if layout_dirty {
                        *layout =
                            initial_layout(&graph.nodes, logical_width, logical_height);
                        run_simulation(&mut layout, &graph.edges, 350);
                    }
                    draw_graph(&canvas, &layout, graph, *hover_idx_val);
                } else {
                    let logical_width = canvas.client_width().max(0) as f64;
                    let logical_height = 360.0;
                    if logical_width > 0.0 {
                        let _ = clear_canvas(&canvas, logical_width, logical_height);
                    }
                }
            },
        );
    }

    let on_mouse_move = {
        let canvas_ref = canvas_ref.clone();
        let layout_ref = layout_ref.clone();
        let hover_idx = hover_idx.clone();
        Callback::from(move |evt: WebMouseEvent| {
            let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() else {
                return;
            };
            let element: &web_sys::Element = canvas.as_ref();
            let rect = element.get_bounding_client_rect();
            let x = evt.client_x() as f64 - rect.left();
            let y = evt.client_y() as f64 - rect.top();
            let layout = layout_ref.borrow();
            let mut found: Option<usize> = None;
            for (i, n) in layout.nodes.iter().enumerate() {
                let dx = x - n.x;
                let dy = y - n.y;
                if (dx * dx + dy * dy).sqrt() <= n.radius + 2.0 {
                    found = Some(i);
                    break;
                }
            }
            if *hover_idx != found {
                hover_idx.set(found);
            }
        })
    };

    let on_mouse_leave = {
        let hover_idx = hover_idx.clone();
        Callback::from(move |_: WebMouseEvent| {
            if hover_idx.is_some() {
                hover_idx.set(None);
            }
        })
    };

    let on_top_change = {
        let top_n = top_n.clone();
        Callback::from(move |e: Event| {
            let target: web_sys::HtmlInputElement = e.target_unchecked_into();
            if let Ok(v) = target.value().parse::<usize>() {
                top_n.set(v.clamp(5, 250));
            }
        })
    };

    let on_min_cooc_change = {
        let min_cooc = min_cooc.clone();
        Callback::from(move |e: Event| {
            let target: web_sys::HtmlInputElement = e.target_unchecked_into();
            if let Ok(v) = target.value().parse::<i64>() {
                min_cooc.set(v.max(1));
            }
        })
    };

    let hover_summary = (*hover_idx).and_then(|idx| {
        payload.as_ref().and_then(|graph| graph.nodes.get(idx).map(|n| (idx, n.clone())))
    });

    let has_graph = payload
        .as_ref()
        .map(|g| !g.nodes.is_empty())
        .unwrap_or(false);

    if props.found_user.is_none() {
        return html! {};
    }
    if !*loading && error.is_none() && !has_graph {
        return html! {};
    }

    let body = if let Some(err) = (*error).clone() {
        html! { <p class="text-danger mb-0">{err}</p> }
    } else {
        html! {
            <>
                <div class="row gx-2 align-items-center mb-3">
                    <div class="col-auto">
                        <label class="form-label small mb-0">{"Top tags"}</label>
                    </div>
                    <div class="col-12 col-sm-3">
                        <input
                            type="number"
                            class="form-control form-control-sm"
                            min="5"
                            max="250"
                            step="5"
                            value={top_n.to_string()}
                            onchange={on_top_change}
                        />
                    </div>
                    <div class="col-auto">
                        <label class="form-label small mb-0">{"Min co-occurrence"}</label>
                    </div>
                    <div class="col-12 col-sm-3">
                        <input
                            type="number"
                            class="form-control form-control-sm"
                            min="1"
                            step="1"
                            value={min_cooc.to_string()}
                            onchange={on_min_cooc_change}
                        />
                    </div>
                    if *loading {
                        <div class="col-auto">
                            <span class="spinner-border spinner-border-sm text-primary" role="status"/>
                        </div>
                    }
                </div>
                <div class="position-relative">
                    <canvas
                        ref={canvas_ref.clone()}
                        style="display: block; width: 100%; touch-action: none;"
                        onmousemove={on_mouse_move}
                        onmouseleave={on_mouse_leave}
                    />
                    if let Some((_, node)) = hover_summary.clone() {
                        <div class="position-absolute top-0 end-0 m-2 px-2 py-1 small rounded shadow-sm bg-body border">
                            <strong>{ node.name.clone() }</strong>
                            { format!(" · {} · {}×", node.group_type, node.count) }
                        </div>
                    }
                </div>
                <p class="small text-muted mt-2 mb-0">
                    {
                        match payload.as_ref() {
                            Some(g) => format!(
                                "{} nodes / {} edges · personal pairs from {} favourites · catalog has {} posts",
                                g.nodes.len(), g.edges.len(), g.account_post_count, g.catalog_post_count
                            ),
                            None => String::new(),
                        }
                    }
                </p>
                <div class="d-flex flex-wrap gap-2 mt-2 small">
                    <span class="badge text-bg-light">{ legend_chip("artist") }</span>
                    <span class="badge text-bg-light">{ legend_chip("character") }</span>
                    <span class="badge text-bg-light">{ legend_chip("copyright") }</span>
                    <span class="badge text-bg-light">{ legend_chip("species") }</span>
                    <span class="badge text-bg-light">{ legend_chip("general") }</span>
                    <span class="badge text-bg-light">{ legend_chip("lore") }</span>
                </div>
            </>
        }
    };

    html! {
        <div class="card mt-4">
            <div class="card-header bg-primary text-white d-flex justify-content-between align-items-center">
                <h5 class="mb-0">{"Tag Relation Graph"}</h5>
                <small class="opacity-75">{"hover a node for details"}</small>
            </div>
            <div class="card-body">
                { body }
            </div>
        </div>
    }
}

fn legend_chip(group: &str) -> String {
    let dot = match group {
        "artist" => "● artist",
        "character" => "● character",
        "copyright" => "● copyright",
        "species" => "● species",
        "general" => "● general",
        "lore" => "● lore",
        _ => "●",
    };
    dot.to_string()
}

fn initial_layout(nodes: &[TagRelationNode], width: f64, height: f64) -> LayoutState {
    let mut state = LayoutState {
        nodes: Vec::with_capacity(nodes.len()),
        width,
        height,
    };
    if nodes.is_empty() {
        return state;
    }

    let max_count = nodes.iter().map(|n| n.count).max().unwrap_or(1).max(1) as f64;
    let cx = width / 2.0;
    let cy = height / 2.0;
    let outer = (width.min(height) * 0.42).max(80.0);
    let len = nodes.len() as f64;

    for (i, n) in nodes.iter().enumerate() {
        let theta = (i as f64) * std::f64::consts::TAU / len.max(1.0);
        let normalized = (n.count as f64).max(1.0).ln() / max_count.max(1.0).ln().max(1e-3);
        let r = 4.0 + (normalized * 14.0).clamp(0.0, 14.0);
        state.nodes.push(NodeState {
            x: cx + outer * theta.cos(),
            y: cy + outer * theta.sin(),
            vx: 0.0,
            vy: 0.0,
            radius: r,
        });
    }
    state
}

fn run_simulation(layout: &mut LayoutState, edges: &[TagRelationEdge], iterations: usize) {
    if layout.nodes.len() < 2 {
        return;
    }

    let n = layout.nodes.len();
    let edge_max = edges
        .iter()
        .map(|e| e.user_cooc)
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    let mut adjusted_edges: Vec<(usize, usize, f64)> = Vec::with_capacity(edges.len());
    for e in edges {
        if e.source < n && e.target < n && e.source != e.target {
            let strength = ((e.user_cooc as f64).max(1.0).ln() + 1.0)
                / ((edge_max).max(1.0).ln() + 1.0);
            adjusted_edges.push((e.source, e.target, strength.clamp(0.05, 1.0)));
        }
    }

    let cx = layout.width / 2.0;
    let cy = layout.height / 2.0;
    let ideal = (layout.width.min(layout.height) / (n as f64).sqrt()).clamp(45.0, 110.0);
    let repulsion = 5500.0;
    let spring_k = 0.045;
    let center_pull = 0.0035;
    let damping = 0.82;
    let max_step = 18.0;

    for _ in 0..iterations {
        for ni in 0..n {
            let r1 = layout.nodes[ni].radius;
            let mut fx = 0.0;
            let mut fy = 0.0;
            for nj in 0..n {
                if ni == nj {
                    continue;
                }
                let r2 = layout.nodes[nj].radius;
                let dx = layout.nodes[ni].x - layout.nodes[nj].x;
                let dy = layout.nodes[ni].y - layout.nodes[nj].y;
                let dist_sq = (dx * dx + dy * dy).max(1.0);
                let dist = dist_sq.sqrt();
                let min_dist = r1 + r2 + 4.0;
                let scale = if dist < min_dist {
                    repulsion * 4.0 / dist_sq
                } else {
                    repulsion / dist_sq
                };
                fx += (dx / dist) * scale;
                fy += (dy / dist) * scale;
            }
            layout.nodes[ni].vx += fx;
            layout.nodes[ni].vy += fy;
        }

        for &(a, b, strength) in &adjusted_edges {
            let dx = layout.nodes[b].x - layout.nodes[a].x;
            let dy = layout.nodes[b].y - layout.nodes[a].y;
            let dist = (dx * dx + dy * dy).sqrt().max(1.0);
            let target_len = ideal / (0.5 + strength);
            let displacement = dist - target_len;
            let force = spring_k * displacement * (0.4 + strength);
            let fx = (dx / dist) * force;
            let fy = (dy / dist) * force;
            layout.nodes[a].vx += fx;
            layout.nodes[a].vy += fy;
            layout.nodes[b].vx -= fx;
            layout.nodes[b].vy -= fy;
        }

        for node in layout.nodes.iter_mut() {
            node.vx += (cx - node.x) * center_pull;
            node.vy += (cy - node.y) * center_pull;
            node.vx *= damping;
            node.vy *= damping;
            node.vx = node.vx.clamp(-max_step, max_step);
            node.vy = node.vy.clamp(-max_step, max_step);
            node.x += node.vx;
            node.y += node.vy;
            node.x = node.x.clamp(node.radius + 4.0, layout.width - node.radius - 4.0);
            node.y = node.y.clamp(node.radius + 4.0, layout.height - node.radius - 4.0);
        }
    }
}

fn clear_canvas(canvas: &HtmlCanvasElement, width: f64, height: f64) -> Option<()> {
    let dpr = web_sys::window()?.device_pixel_ratio();
    let pw = (width * dpr).round() as u32;
    let ph = (height * dpr).round() as u32;
    if canvas.width() != pw {
        canvas.set_width(pw);
    }
    if canvas.height() != ph {
        canvas.set_height(ph);
    }
    let ctx: CanvasRenderingContext2d = canvas.get_context("2d").ok()??.dyn_into().ok()?;
    ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0).ok()?;
    ctx.scale(dpr, dpr).ok()?;
    ctx.clear_rect(0.0, 0.0, width, height);
    Some(())
}

fn draw_graph(
    canvas: &HtmlCanvasElement,
    layout: &LayoutState,
    graph: &TagRelationGraphPayload,
    hover: Option<usize>,
) {
    let width = layout.width;
    let height = layout.height;
    if clear_canvas(canvas, width, height).is_none() {
        return;
    }
    let ctx: CanvasRenderingContext2d = match canvas.get_context("2d").ok().flatten() {
        Some(c) => match c.dyn_into() {
            Ok(c) => c,
            Err(_) => return,
        },
        None => return,
    };

    let el: &web_sys::Element = canvas.as_ref();
    let body_color = css_var(el, "--bs-body-color").unwrap_or_else(|| "#212529".into());
    let muted = css_var(el, "--bs-secondary").unwrap_or_else(|| "#6c757d".into());

    let palette = group_palette(el);

    let max_user_cooc = graph
        .edges
        .iter()
        .map(|e| e.user_cooc)
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    for edge in &graph.edges {
        if edge.source >= layout.nodes.len() || edge.target >= layout.nodes.len() {
            continue;
        }
        let a = layout.nodes[edge.source];
        let b = layout.nodes[edge.target];
        let strength = ((edge.user_cooc as f64).max(1.0).ln() + 1.0)
            / (max_user_cooc.max(1.0).ln() + 1.0);
        let line_w = (0.5 + strength * 3.5).clamp(0.5, 4.5);
        let alpha = (0.18 + strength * 0.55).clamp(0.18, 0.85);
        let highlight = match hover {
            Some(h) if h == edge.source || h == edge.target => true,
            _ => false,
        };
        let stroke = if highlight {
            with_alpha(&body_color, 0.95)
        } else {
            with_alpha(&muted, alpha as f32)
        };
        ctx.set_stroke_style_str(&stroke);
        ctx.set_line_width(if highlight { line_w + 0.7 } else { line_w });
        ctx.begin_path();
        ctx.move_to(a.x, a.y);
        ctx.line_to(b.x, b.y);
        ctx.stroke();
    }

    ctx.set_font("12px system-ui, -apple-system, Segoe UI, Roboto, sans-serif");
    ctx.set_text_baseline("middle");

    for (i, node) in graph.nodes.iter().enumerate() {
        let pos = layout.nodes[i];
        let is_hover = hover == Some(i);
        let fill = palette
            .iter()
            .find(|(k, _)| *k == node.group_type.as_str())
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| muted.clone());

        ctx.begin_path();
        ctx.arc(
            pos.x,
            pos.y,
            pos.radius,
            0.0,
            std::f64::consts::TAU,
        )
        .ok();
        ctx.set_fill_style_str(&fill);
        ctx.fill();

        ctx.set_line_width(if is_hover { 2.0 } else { 0.8 });
        ctx.set_stroke_style_str(&body_color);
        ctx.stroke();

        if is_hover || pos.radius >= 8.0 {
            ctx.set_text_align("left");
            ctx.set_fill_style_str(&body_color);
            let _ = ctx.fill_text(&node.name, pos.x + pos.radius + 4.0, pos.y);
        }
    }
}

fn group_palette(el: &web_sys::Element) -> Vec<(&'static str, String)> {
    vec![
        (
            "artist",
            css_var(el, "--bs-danger").unwrap_or_else(|| "#dc3545".into()),
        ),
        (
            "character",
            css_var(el, "--bs-warning").unwrap_or_else(|| "#ffc107".into()),
        ),
        (
            "copyright",
            css_var(el, "--bs-info").unwrap_or_else(|| "#0dcaf0".into()),
        ),
        (
            "species",
            css_var(el, "--bs-success").unwrap_or_else(|| "#198754".into()),
        ),
        (
            "general",
            css_var(el, "--bs-primary").unwrap_or_else(|| "#0d6efd".into()),
        ),
        (
            "lore",
            css_var(el, "--bs-secondary").unwrap_or_else(|| "#6c757d".into()),
        ),
    ]
}

fn css_var(el: &web_sys::Element, name: &str) -> Option<String> {
    let window = web_sys::window()?;
    let computed = window.get_computed_style(el).ok()??;
    computed
        .get_property_value(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn with_alpha(color: &str, alpha: f32) -> String {
    let trimmed = color.trim();
    if let Some(stripped) = trimmed.strip_prefix('#') {
        if let Some((r, g, b)) = parse_hex(stripped) {
            return format!("rgba({r},{g},{b},{:.3})", alpha.clamp(0.0, 1.0));
        }
    }
    if let Some(rest) = trimmed
        .strip_prefix("rgb(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return format!("rgba({rest},{:.3})", alpha.clamp(0.0, 1.0));
    }
    trimmed.to_string()
}

fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    match s.len() {
        3 => {
            let r = u8::from_str_radix(&s[0..1].repeat(2), 16).ok()?;
            let g = u8::from_str_radix(&s[1..2].repeat(2), 16).ok()?;
            let b = u8::from_str_radix(&s[2..3].repeat(2), 16).ok()?;
            Some((r, g, b))
        }
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some((r, g, b))
        }
        _ => None,
    }
}
