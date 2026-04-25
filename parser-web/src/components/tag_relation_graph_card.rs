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
    radius: f64,
}

#[derive(Default, Clone)]
struct LayoutState {
    nodes: Vec<NodeState>,
    /// Backbone: for each kept edge, the original index in `payload.edges` plus
    /// the score that earned it the slot. Both layout and rendering use only
    /// these — full edge sets are visually opaque on dense graphs.
    edges: Vec<(usize, usize, usize, f64)>,
    width: f64,
    height: f64,
}

#[function_component(TagRelationGraphCard)]
pub fn tag_relation_graph_card(props: &TagRelationGraphCardProps) -> Html {
    let payload: UseStateHandle<Option<TagRelationGraphPayload>> = use_state(|| None);
    let loading = use_state(|| false);
    let error: UseStateHandle<Option<String>> = use_state(|| None);
    let top_n = use_state(|| 60usize);
    let min_cooc = use_state(|| 3i64);
    let edges_per_tag = use_state(|| 6usize);
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
        let edges_per_tag_val = *edges_per_tag;
        use_effect_with(
            (
                payload.clone(),
                *theme_trigger,
                *resize_trigger,
                *hover_idx_render,
                edges_per_tag_val,
            ),
            move |(payload, _, _, hover_idx_val, k_val)| {
                let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() else {
                    return;
                };
                if let Some(graph) = payload.as_ref() {
                    let logical_width = canvas.client_width().max(0) as f64;
                    if logical_width <= 0.0 {
                        return;
                    }
                    let logical_height = (logical_width * 0.85).clamp(480.0, 900.0);

                    let mut layout = layout_ref.borrow_mut();
                    let layout_dirty = layout.nodes.len() != graph.nodes.len()
                        || (layout.width - logical_width).abs() > 0.5
                        || (layout.height - logical_height).abs() > 0.5
                        || layout.edges.len() != expected_backbone_len(graph, *k_val);
                    if layout_dirty {
                        let backbone = select_backbone(graph, *k_val);
                        *layout = initial_layout(
                            &graph.nodes,
                            logical_width,
                            logical_height,
                            backbone,
                        );
                        run_simulation(&mut layout, simulation_iterations(graph.nodes.len()));
                        fit_to_viewport(&mut layout, 0.06);
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

    let on_edges_per_tag_change = {
        let edges_per_tag = edges_per_tag.clone();
        Callback::from(move |e: Event| {
            let target: web_sys::HtmlInputElement = e.target_unchecked_into();
            if let Ok(v) = target.value().parse::<usize>() {
                edges_per_tag.set(v.clamp(2, 20));
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
                    <div class="col-12 col-sm-2">
                        <input
                            type="number"
                            class="form-control form-control-sm"
                            min="1"
                            step="1"
                            value={min_cooc.to_string()}
                            onchange={on_min_cooc_change}
                        />
                    </div>
                    <div class="col-auto">
                        <label class="form-label small mb-0" title="Each tag keeps only its strongest links to other tags. Lower = clearer backbone, higher = denser graph.">{"Edges per tag"}</label>
                    </div>
                    <div class="col-12 col-sm-2">
                        <input
                            type="number"
                            class="form-control form-control-sm"
                            min="2"
                            max="20"
                            step="1"
                            value={edges_per_tag.to_string()}
                            onchange={on_edges_per_tag_change}
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

/// Personalized lift / pointwise mutual information for a tag pair, computed
/// from the user's own data: `user_cooc · N_user / (count_a · count_b)`.
/// Larger means "this pair appears together more than what either tag's
/// marginal frequency would predict" — exactly what we want to surface and
/// what dense raw co-occurrence counts hide.
fn user_pmi(edge: &TagRelationEdge, nodes: &[TagRelationNode], n_user: i64) -> f64 {
    if edge.source >= nodes.len() || edge.target >= nodes.len() {
        return f64::NEG_INFINITY;
    }
    let count_a = nodes[edge.source].count.max(1) as f64;
    let count_b = nodes[edge.target].count.max(1) as f64;
    let n_u = n_user.max(1) as f64;
    let lift = (edge.user_cooc.max(1) as f64) * n_u / (count_a * count_b);
    lift.max(1e-3).ln()
}

fn edge_score(edge: &TagRelationEdge, nodes: &[TagRelationNode], n_user: i64) -> f64 {
    let user_term = user_pmi(edge, nodes, n_user);
    // Blend a smaller global PMI so semantically coherent pairs (well-known
    // in the catalog) get a small lift even when the user's sample is thin.
    let global_term = if edge.global_lift > 0.0 {
        (edge.global_lift as f64).max(1e-3).ln()
    } else {
        0.0
    };
    user_term + 0.35 * global_term
}

/// Keep, for each node, its top-K incident edges by `edge_score`. Take the
/// union of those decisions: a node only votes in its own edges out, but as
/// long as either endpoint kept the edge it survives. Net effect: every node
/// has at least 1 link if any exist, and the densest "popular pairs with
/// popular" edges drop out unless they're also pair-specific.
fn select_backbone(graph: &TagRelationGraphPayload, k: usize) -> Vec<(usize, usize, usize, f64)> {
    let n = graph.nodes.len();
    if n == 0 || graph.edges.is_empty() {
        return Vec::new();
    }
    let k = k.max(1);
    let n_user = graph.account_post_count.max(1);

    let mut per_node: Vec<Vec<(f64, usize)>> = vec![Vec::new(); n];
    let mut scored: Vec<f64> = Vec::with_capacity(graph.edges.len());

    for (idx, e) in graph.edges.iter().enumerate() {
        if e.source >= n || e.target >= n || e.source == e.target {
            scored.push(f64::NEG_INFINITY);
            continue;
        }
        let s = edge_score(e, &graph.nodes, n_user);
        scored.push(s);
        per_node[e.source].push((s, idx));
        per_node[e.target].push((s, idx));
    }

    let mut keep = vec![false; graph.edges.len()];
    for adj in per_node.iter_mut() {
        adj.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        for (_, idx) in adj.iter().take(k) {
            keep[*idx] = true;
        }
    }

    let mut out = Vec::new();
    for (idx, e) in graph.edges.iter().enumerate() {
        if keep[idx] {
            out.push((e.source, e.target, idx, scored[idx]));
        }
    }
    out
}

/// Cheap upper bound used to detect "the user changed `edges_per_tag`" so the
/// layout cache invalidates without us having to recompute the backbone twice.
fn expected_backbone_len(graph: &TagRelationGraphPayload, k: usize) -> usize {
    let mut count = 0usize;
    let mut per_node = vec![0usize; graph.nodes.len()];
    for e in &graph.edges {
        if e.source >= graph.nodes.len() || e.target >= graph.nodes.len() {
            continue;
        }
        if per_node[e.source] < k || per_node[e.target] < k {
            count += 1;
            per_node[e.source] += 1;
            per_node[e.target] += 1;
        }
    }
    count
}

fn simulation_iterations(n: usize) -> usize {
    // Big graphs need more iterations to settle; tiny graphs converge fast.
    let base = 600;
    let extra = (n.saturating_sub(60) as f64 * 1.6).round() as usize;
    (base + extra).min(1500)
}

fn initial_layout(
    nodes: &[TagRelationNode],
    width: f64,
    height: f64,
    backbone: Vec<(usize, usize, usize, f64)>,
) -> LayoutState {
    let mut state = LayoutState {
        nodes: Vec::with_capacity(nodes.len()),
        edges: backbone,
        width,
        height,
    };
    if nodes.is_empty() {
        return state;
    }

    let max_count = nodes.iter().map(|n| n.count).max().unwrap_or(1).max(1) as f64;
    let n = nodes.len();

    // Grid + per-cell pseudo-random jitter. Scales cleanly to hundreds of
    // nodes — a single circle ran out of circumference around N≈80 and
    // stuffed every node on top of its neighbour.
    let aspect = width / height.max(1.0);
    let cols = ((n as f64 * aspect).sqrt().ceil() as usize).max(2);
    let rows = ((n + cols - 1) / cols).max(1);
    let inner_w = width * 0.86;
    let inner_h = height * 0.86;
    let dx = inner_w / cols as f64;
    let dy = inner_h / rows as f64;
    let off_x = (width - inner_w) * 0.5 + dx * 0.5;
    let off_y = (height - inner_h) * 0.5 + dy * 0.5;

    for (i, node) in nodes.iter().enumerate() {
        let col = i % cols;
        let row = i / cols;
        // Two independent hash-ish jitter axes — keeps the seed deterministic
        // (so re-renders don't reflow) without correlating x and y.
        let h1 = ((i as f64) * 12.9898).sin().fract().abs();
        let h2 = ((i as f64) * 78.233).sin().fract().abs();
        let jx = (h1 - 0.5) * dx * 0.6;
        let jy = (h2 - 0.5) * dy * 0.6;
        let x = off_x + col as f64 * dx + jx;
        let y = off_y + row as f64 * dy + jy;

        let normalized = (node.count as f64).max(1.0).ln() / max_count.max(1.0).ln().max(1e-3);
        // Shrink node radii a little when n is large so they fit.
        let r_max = if n > 120 { 10.0 } else { 14.0 };
        let r = 3.5 + (normalized * r_max).clamp(0.0, r_max);
        state.nodes.push(NodeState { x, y, radius: r });
    }
    state
}

/// Fruchterman–Reingold over the backbone only. Repulsion `k²/d` between
/// every node pair (still O(N²) but cheap for N≤300), attraction `d²/k` along
/// each backbone edge weighted by score percentile. No mid-simulation bounds
/// clamp — `fit_to_viewport` does the final framing, so freely-spreading
/// nodes can't get stuck against an edge.
fn run_simulation(layout: &mut LayoutState, iterations: usize) {
    let n = layout.nodes.len();
    if n < 2 {
        return;
    }

    let area = layout.width * layout.height;
    let k = ((area / n as f64).sqrt() * 1.35).max(36.0);

    // Normalise edge scores into a (0.15..1.0) attraction multiplier so the
    // strongest pair-specific links pull harder than the weakest.
    let scores: Vec<f64> = layout.edges.iter().map(|(_, _, _, s)| *s).collect();
    let s_min = scores.iter().cloned().fold(f64::INFINITY, f64::min);
    let s_max = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = (s_max - s_min).max(1e-6);

    let initial_temp = layout.width.max(layout.height) * 0.20;
    // 0.992^iterations ≈ 0.005 at 700, ≈ 0.0001 at 1200.
    let cooling = 0.992f64;
    let mut t = initial_temp;

    let mut forces: Vec<(f64, f64)> = vec![(0.0, 0.0); n];
    let pad = 4.0_f64;

    for _ in 0..iterations {
        for f in forces.iter_mut() {
            *f = (0.0, 0.0);
        }

        // Repulsion (every pair).
        for i in 0..n {
            for j in (i + 1)..n {
                let dx = layout.nodes[i].x - layout.nodes[j].x;
                let dy = layout.nodes[i].y - layout.nodes[j].y;
                let dist_sq = (dx * dx + dy * dy).max(0.25);
                let dist = dist_sq.sqrt();

                let min_dist = layout.nodes[i].radius + layout.nodes[j].radius + pad;
                let core_boost = if dist < min_dist {
                    (min_dist - dist) * 8.0
                } else {
                    0.0
                };
                let force = (k * k) / dist + core_boost;
                let fx = (dx / dist) * force;
                let fy = (dy / dist) * force;
                forces[i].0 += fx;
                forces[i].1 += fy;
                forces[j].0 -= fx;
                forces[j].1 -= fy;
            }
        }

        // Attraction (backbone edges only).
        for &(a, b, _, score) in &layout.edges {
            let w = (0.15 + 0.85 * ((score - s_min) / span)).clamp(0.15, 1.0);
            let dx = layout.nodes[a].x - layout.nodes[b].x;
            let dy = layout.nodes[a].y - layout.nodes[b].y;
            let dist = (dx * dx + dy * dy).sqrt().max(0.5);
            let force = (dist * dist / k) * w;
            let fx = (dx / dist) * force;
            let fy = (dy / dist) * force;
            forces[a].0 -= fx;
            forces[a].1 -= fy;
            forces[b].0 += fx;
            forces[b].1 += fy;
        }

        // Integrate, capped by the current temperature. No bounds clamp —
        // post-simulation `fit_to_viewport` handles the canvas mapping.
        for i in 0..n {
            let (fx, fy) = forces[i];
            let mag = (fx * fx + fy * fy).sqrt();
            if mag <= 1e-6 {
                continue;
            }
            let mv = mag.min(t);
            layout.nodes[i].x += (fx / mag) * mv;
            layout.nodes[i].y += (fy / mag) * mv;
        }

        t *= cooling;
    }
}

/// Re-centres and uniformly scales the laid-out nodes so the bounding box
/// fills the canvas with a small margin. Without this, FR converges to
/// whatever absolute size the forces happened to land on — usually a tight
/// blob in the middle.
fn fit_to_viewport(layout: &mut LayoutState, margin_frac: f64) {
    if layout.nodes.is_empty() {
        return;
    }
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for node in &layout.nodes {
        let r = node.radius;
        min_x = min_x.min(node.x - r);
        max_x = max_x.max(node.x + r);
        min_y = min_y.min(node.y - r);
        max_y = max_y.max(node.y + r);
    }
    let bb_w = (max_x - min_x).max(1.0);
    let bb_h = (max_y - min_y).max(1.0);

    let m = (margin_frac * layout.width.min(layout.height)).max(16.0);
    let target_w = (layout.width - 2.0 * m).max(1.0);
    let target_h = (layout.height - 2.0 * m).max(1.0);
    // Take the smaller scale so neither axis overflows.
    let scale = (target_w / bb_w).min(target_h / bb_h);

    let cx = (min_x + max_x) * 0.5;
    let cy = (min_y + max_y) * 0.5;
    let ncx = layout.width * 0.5;
    let ncy = layout.height * 0.5;
    for node in layout.nodes.iter_mut() {
        node.x = ncx + (node.x - cx) * scale;
        node.y = ncy + (node.y - cy) * scale;
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

    // Render only backbone edges. Thickness/alpha are tied to the backbone
    // score (PMI-flavoured), not raw co-occurrence count, so two pairs with
    // identical raw counts render differently when one is meaningful and the
    // other is "both tags are popular".
    let scores: Vec<f64> = layout.edges.iter().map(|(_, _, _, s)| *s).collect();
    let s_min = scores.iter().cloned().fold(f64::INFINITY, f64::min);
    let s_max = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = (s_max - s_min).max(1e-6);

    for &(src, tgt, _, score) in &layout.edges {
        if src >= layout.nodes.len() || tgt >= layout.nodes.len() {
            continue;
        }
        let a = layout.nodes[src];
        let b = layout.nodes[tgt];
        let strength = ((score - s_min) / span).clamp(0.0, 1.0);
        let line_w = (0.6 + strength * 3.0).clamp(0.6, 4.0);
        let alpha = (0.12 + strength * 0.55).clamp(0.12, 0.78);
        let highlight = matches!(hover, Some(h) if h == src || h == tgt);
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
