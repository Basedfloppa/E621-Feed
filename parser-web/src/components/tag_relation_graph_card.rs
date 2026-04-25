use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use reqwasm::http::Request;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, MouseEvent as WebMouseEvent, MutationObserver,
    MutationObserverInit, WheelEvent as WebWheelEvent, js_sys,
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
    /// Per-node community label (compact, contiguous ints). Drives the halo
    /// colour overlay so cluster structure is visible at a glance instead of
    /// requiring the user to mentally trace edges.
    communities: Vec<u32>,
    width: f64,
    height: f64,
}

#[derive(Clone, Copy, PartialEq)]
struct ViewTransform {
    offset_x: f64,
    offset_y: f64,
    scale: f64,
}

impl Default for ViewTransform {
    fn default() -> Self {
        Self {
            offset_x: 0.0,
            offset_y: 0.0,
            scale: 1.0,
        }
    }
}

impl ViewTransform {
    fn to_logical(&self, cx: f64, cy: f64) -> (f64, f64) {
        (
            (cx - self.offset_x) / self.scale,
            (cy - self.offset_y) / self.scale,
        )
    }
}

const ZOOM_MIN: f64 = 0.4;
const ZOOM_MAX: f64 = 6.0;
const ZOOM_STEP: f64 = 1.2;

/// Barnes-Hut θ. 0.9 is a good legibility/speed tradeoff for n ≤ 1000:
/// distant clusters use centre-of-mass, nearby pairs are exact.
const BH_THETA: f64 = 0.9;

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
    let view: UseStateHandle<ViewTransform> = use_state(ViewTransform::default);
    // Drag state lives in a ref because mousemove fires often and we don't
    // want to trigger a re-render on every pixel of motion.
    let drag_ref: Rc<RefCell<Option<(f64, f64, f64, f64)>>> = use_mut_ref(|| None);

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
        let view_render = *view;
        use_effect_with(
            (
                payload.clone(),
                *theme_trigger,
                *resize_trigger,
                *hover_idx_render,
                edges_per_tag_val,
                view_render,
            ),
            move |(payload, _, _, hover_idx_val, k_val, view_render)| {
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
                        // View transform is intentionally NOT reset here — a
                        // user pan/zoom should survive parameter tweaks (top_n,
                        // edges_per_tag, etc.), so they can iterate without
                        // losing their place. Use double-click or the reset
                        // button to recenter explicitly.
                    }
                    draw_graph(&canvas, &layout, graph, *hover_idx_val, *view_render);
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
        let view = view.clone();
        let drag_ref = drag_ref.clone();
        Callback::from(move |evt: WebMouseEvent| {
            let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() else {
                return;
            };
            let element: &web_sys::Element = canvas.as_ref();
            let rect = element.get_bounding_client_rect();
            let x = evt.client_x() as f64 - rect.left();
            let y = evt.client_y() as f64 - rect.top();

            // Drag → pan the viewport.
            if let Some((start_cx, start_cy, start_off_x, start_off_y)) =
                *drag_ref.borrow()
            {
                let new_off_x = start_off_x + (x - start_cx);
                let new_off_y = start_off_y + (y - start_cy);
                let cur = *view;
                if (cur.offset_x - new_off_x).abs() > 0.5
                    || (cur.offset_y - new_off_y).abs() > 0.5
                {
                    view.set(ViewTransform {
                        offset_x: new_off_x,
                        offset_y: new_off_y,
                        scale: cur.scale,
                    });
                }
                return;
            }

            // Hover hit-test in transformed (logical) space.
            let (lx, ly) = view.to_logical(x, y);
            let layout = layout_ref.borrow();
            let mut found: Option<usize> = None;
            // Account for the visual-only zoom: pick radius is in logical px.
            let pick_pad = (3.0 / view.scale.max(0.1)).clamp(1.0, 12.0);
            for (i, n) in layout.nodes.iter().enumerate() {
                let dx = lx - n.x;
                let dy = ly - n.y;
                if (dx * dx + dy * dy).sqrt() <= n.radius + pick_pad {
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
        let drag_ref = drag_ref.clone();
        Callback::from(move |_: WebMouseEvent| {
            if hover_idx.is_some() {
                hover_idx.set(None);
            }
            *drag_ref.borrow_mut() = None;
        })
    };

    let on_mouse_down = {
        let canvas_ref = canvas_ref.clone();
        let drag_ref = drag_ref.clone();
        let view = view.clone();
        Callback::from(move |evt: WebMouseEvent| {
            // Left button only.
            if evt.button() != 0 {
                return;
            }
            let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() else {
                return;
            };
            let element: &web_sys::Element = canvas.as_ref();
            let rect = element.get_bounding_client_rect();
            let x = evt.client_x() as f64 - rect.left();
            let y = evt.client_y() as f64 - rect.top();
            let cur = *view;
            *drag_ref.borrow_mut() = Some((x, y, cur.offset_x, cur.offset_y));
        })
    };

    let on_mouse_up = {
        let drag_ref = drag_ref.clone();
        Callback::from(move |_: WebMouseEvent| {
            *drag_ref.borrow_mut() = None;
        })
    };

    // Wheel listener has to be attached non-passively or `preventDefault()`
    // is silently ignored and the page scrolls under the cursor. Yew's
    // `onwheel` registers as passive in modern browsers, hence the manual
    // `addEventListener` below. Dep on `canvas_present` so we re-bind
    // whenever the canvas first mounts (i.e. once data arrives).
    {
        let canvas_ref_for_effect = canvas_ref.clone();
        let view = view.clone();
        let canvas_present = payload.is_some() || *loading;
        use_effect_with(canvas_present, move |_present| {
            let canvas_ref = canvas_ref_for_effect;
            let mut handle: Option<(HtmlCanvasElement, Closure<dyn FnMut(WebWheelEvent)>)> =
                None;

            if let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() {
                let canvas_for_cb = canvas.clone();
                let view_for_cb = view.clone();
                let cb = Closure::<dyn FnMut(WebWheelEvent)>::new(
                    move |evt: WebWheelEvent| {
                        evt.prevent_default();
                        let element: &web_sys::Element = canvas_for_cb.as_ref();
                        let rect = element.get_bounding_client_rect();
                        let x = evt.client_x() as f64 - rect.left();
                        let y = evt.client_y() as f64 - rect.top();
                        let cur = *view_for_cb;
                        let direction = if evt.delta_y() < 0.0 {
                            ZOOM_STEP
                        } else {
                            1.0 / ZOOM_STEP
                        };
                        let new_scale =
                            (cur.scale * direction).clamp(ZOOM_MIN, ZOOM_MAX);
                        if (new_scale - cur.scale).abs() < 1e-6 {
                            return;
                        }
                        let real = new_scale / cur.scale;
                        view_for_cb.set(ViewTransform {
                            offset_x: x - (x - cur.offset_x) * real,
                            offset_y: y - (y - cur.offset_y) * real,
                            scale: new_scale,
                        });
                    },
                );

                let opts = web_sys::AddEventListenerOptions::new();
                opts.set_passive(false);
                let _ = canvas
                    .add_event_listener_with_callback_and_add_event_listener_options(
                        "wheel",
                        cb.as_ref().unchecked_ref(),
                        &opts,
                    );

                handle = Some((canvas, cb));
            }

            move || {
                if let Some((canvas, cb)) = handle {
                    let _ = canvas.remove_event_listener_with_callback(
                        "wheel",
                        cb.as_ref().unchecked_ref(),
                    );
                    drop(cb);
                }
            }
        });
    }

    let on_dblclick = {
        let view = view.clone();
        Callback::from(move |_: WebMouseEvent| {
            view.set(ViewTransform::default());
        })
    };

    let on_zoom_in = {
        let canvas_ref = canvas_ref.clone();
        let view = view.clone();
        Callback::from(move |_: WebMouseEvent| {
            zoom_around_centre(&canvas_ref, &view, ZOOM_STEP);
        })
    };

    let on_zoom_out = {
        let canvas_ref = canvas_ref.clone();
        let view = view.clone();
        Callback::from(move |_: WebMouseEvent| {
            zoom_around_centre(&canvas_ref, &view, 1.0 / ZOOM_STEP);
        })
    };

    let on_zoom_reset = {
        let view = view.clone();
        Callback::from(move |_: WebMouseEvent| {
            view.set(ViewTransform::default());
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
                    <div class="col-auto ms-auto">
                        <div class="btn-group btn-group-sm" role="group" aria-label="Zoom controls">
                            <button type="button" class="btn btn-outline-secondary" title="Zoom out" onclick={on_zoom_out}>{"−"}</button>
                            <button type="button" class="btn btn-outline-secondary" title="Reset view (or double-click the graph)" onclick={on_zoom_reset}>{ format!("{:.0}%", view.scale * 100.0) }</button>
                            <button type="button" class="btn btn-outline-secondary" title="Zoom in" onclick={on_zoom_in}>{"+"}</button>
                        </div>
                    </div>
                    if *loading {
                        <div class="col-auto">
                            <span class="spinner-border spinner-border-sm text-primary" role="status"/>
                        </div>
                    }
                </div>
                <div class="position-relative" style="user-select: none;">
                    <canvas
                        ref={canvas_ref.clone()}
                        style={format!(
                            "display: block; width: 100%; touch-action: none; cursor: {};",
                            if drag_ref.borrow().is_some() { "grabbing" } else { "grab" }
                        )}
                        onmousemove={on_mouse_move}
                        onmouseleave={on_mouse_leave}
                        onmousedown={on_mouse_down}
                        onmouseup={on_mouse_up}
                        ondblclick={on_dblclick}
                    />
                    if let Some((_, node)) = hover_summary.clone() {
                        <div class="position-absolute top-0 end-0 m-2 px-2 py-1 small rounded shadow-sm bg-body border" style="pointer-events: none;">
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
                <small class="opacity-75">{"hover a node for details · halo colour = community"}</small>
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

/// Backbone selection has two phases that union together:
///   1. Maximum spanning tree by edge_score over the whole graph (Kruskal).
///      Guarantees connectedness across all nodes that have any edge.
///   2. Per-node top-K voting on top of the MST. An edge survives if the MST
///      kept it OR if either endpoint included it among its strongest K.
/// The MST guard prevents the per-node-top-K alone from leaving disconnected
/// components or stranded "satellite" nodes when one tag dominates.
fn select_backbone(graph: &TagRelationGraphPayload, k: usize) -> Vec<(usize, usize, usize, f64)> {
    let n = graph.nodes.len();
    if n == 0 || graph.edges.is_empty() {
        return Vec::new();
    }
    let k = k.max(1);
    let n_user = graph.account_post_count.max(1);

    let mut scored: Vec<f64> = Vec::with_capacity(graph.edges.len());
    let mut valid: Vec<bool> = Vec::with_capacity(graph.edges.len());
    let mut per_node: Vec<Vec<(f64, usize)>> = vec![Vec::new(); n];
    for (idx, e) in graph.edges.iter().enumerate() {
        if e.source >= n || e.target >= n || e.source == e.target {
            scored.push(f64::NEG_INFINITY);
            valid.push(false);
            continue;
        }
        let s = edge_score(e, &graph.nodes, n_user);
        scored.push(s);
        valid.push(true);
        per_node[e.source].push((s, idx));
        per_node[e.target].push((s, idx));
    }

    let mut keep = vec![false; graph.edges.len()];

    // Phase 1: Kruskal MST over decreasing score (i.e., maximum spanning tree).
    let mut order: Vec<usize> = (0..graph.edges.len()).filter(|&i| valid[i]).collect();
    order.sort_by(|&a, &b| {
        scored[b]
            .partial_cmp(&scored[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for idx in order {
        let e = &graph.edges[idx];
        let ra = find(&mut parent, e.source);
        let rb = find(&mut parent, e.target);
        if ra != rb {
            parent[ra] = rb;
            keep[idx] = true;
        }
    }

    // Phase 2: per-node top-K. Union with MST.
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
/// Includes an MST allowance (n-1 edges if connected) so the count matches the
/// real backbone post-MST union.
fn expected_backbone_len(graph: &TagRelationGraphPayload, k: usize) -> usize {
    let n = graph.nodes.len();
    if n == 0 {
        return 0;
    }
    let mut count = 0usize;
    let mut per_node = vec![0usize; n];
    for e in &graph.edges {
        if e.source >= n || e.target >= n {
            continue;
        }
        if per_node[e.source] < k || per_node[e.target] < k {
            count += 1;
            per_node[e.source] += 1;
            per_node[e.target] += 1;
        }
    }
    count + (n.saturating_sub(1))
}

fn simulation_iterations(n: usize) -> usize {
    // Hard cap; convergence detection in `run_simulation` typically stops
    // earlier on small graphs and runs longer on large ones.
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
        communities: Vec::new(),
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
        // Smooth radius scaling: 60-node graph → r_max ≈ 14, 200+ → r_max ≈ 10.
        // No more visible jump when the user nudges top_n past 120.
        let r_max = smooth_r_max(n);
        let r = 3.5 + (normalized * r_max).clamp(0.0, r_max);
        state.nodes.push(NodeState { x, y, radius: r });
    }

    // Compute communities once layout is set up. Label propagation over the
    // backbone — fast and good enough for the visual cue we're after.
    state.communities = label_propagation(state.nodes.len(), &state.edges, 12);

    state
}

#[inline]
fn smooth_r_max(n: usize) -> f64 {
    // Smoothstep from 14 (small graphs) down to 10 (dense), interpolated
    // across n ∈ [60, 200]. No discontinuity at n = 120.
    let t = ((n as f64 - 60.0) / (200.0 - 60.0)).clamp(0.0, 1.0);
    let s = t * t * (3.0 - 2.0 * t);
    14.0 + (10.0 - 14.0) * s
}

/// Label Propagation Algorithm: each node iteratively adopts the label of the
/// neighbour community with the largest summed edge weight. Converges in <15
/// iterations for typical tag graphs and produces compact, contiguous
/// community ids ready for a color palette lookup.
fn label_propagation(
    n_nodes: usize,
    edges: &[(usize, usize, usize, f64)],
    iterations: usize,
) -> Vec<u32> {
    if n_nodes == 0 {
        return Vec::new();
    }
    let mut labels: Vec<u32> = (0..n_nodes as u32).collect();
    let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n_nodes];
    // Edge weights enter the adjacency as a positive lift (PMI can be < 0
    // for anti-correlated pairs; clamping keeps anti-correlation from voting
    // *against* a community).
    let min_score = edges
        .iter()
        .map(|(_, _, _, s)| *s)
        .fold(f64::INFINITY, f64::min);
    let shift = if min_score.is_finite() && min_score < 0.0 {
        -min_score + 0.05
    } else {
        0.05
    };
    for &(a, b, _, score) in edges {
        if a >= n_nodes || b >= n_nodes || a == b {
            continue;
        }
        let w = (score + shift).max(0.05);
        adj[a].push((b, w));
        adj[b].push((a, w));
    }

    // Fixed deterministic visit order based on a hash spread of the index.
    let mut order: Vec<usize> = (0..n_nodes).collect();
    order.sort_by_key(|&i| (i.wrapping_mul(2_654_435_761usize)) as u32);

    for _ in 0..iterations {
        let mut changed = false;
        for &i in &order {
            if adj[i].is_empty() {
                continue;
            }
            let mut weights: HashMap<u32, f64> = HashMap::new();
            for &(j, w) in &adj[i] {
                *weights.entry(labels[j]).or_insert(0.0) += w;
            }
            // Tie-break: prefer the smaller label id for determinism.
            let new_label = weights
                .into_iter()
                .max_by(|a, b| {
                    a.1.partial_cmp(&b.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| b.0.cmp(&a.0))
                })
                .map(|(l, _)| l)
                .unwrap_or(labels[i]);
            if new_label != labels[i] {
                labels[i] = new_label;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Compact labels to dense [0..k) range so the palette index is small.
    let unique: BTreeSet<u32> = labels.iter().copied().collect();
    let map: HashMap<u32, u32> = unique
        .iter()
        .enumerate()
        .map(|(i, l)| (*l, i as u32))
        .collect();
    labels.iter_mut().for_each(|l| *l = *map.get(l).unwrap_or(l));
    labels
}

// =============== Barnes-Hut quadtree =====================================
// Flat-arena quadtree used for O(N log N) repulsion computation. Children are
// referenced by index; index 0 is reserved for the root, and `QT_NIL` marks
// "no child". The tree is rebuilt once per FR iteration.

const QT_NIL: usize = usize::MAX;

#[derive(Clone, Copy)]
struct QtNode {
    cx0: f64,
    cy0: f64,
    size: f64,
    mass: f64,
    com_x: f64,
    com_y: f64,
    children: [usize; 4],
    body: usize,
}

impl QtNode {
    fn empty(cx0: f64, cy0: f64, size: f64) -> Self {
        Self {
            cx0,
            cy0,
            size,
            mass: 0.0,
            com_x: 0.0,
            com_y: 0.0,
            children: [QT_NIL; 4],
            body: QT_NIL,
        }
    }
}

struct QuadTree {
    nodes: Vec<QtNode>,
}

impl QuadTree {
    fn new(min_x: f64, min_y: f64, size: f64) -> Self {
        Self {
            nodes: vec![QtNode::empty(min_x, min_y, size)],
        }
    }

    #[inline]
    fn quadrant(&self, n: usize, x: f64, y: f64) -> usize {
        let half = self.nodes[n].size * 0.5;
        let mid_x = self.nodes[n].cx0 + half;
        let mid_y = self.nodes[n].cy0 + half;
        let east = (x >= mid_x) as usize;
        let south = (y >= mid_y) as usize;
        // 0=NW 1=NE 2=SW 3=SE
        (south << 1) | east
    }

    fn ensure_child(&mut self, parent: usize, q: usize) -> usize {
        let existing = self.nodes[parent].children[q];
        if existing != QT_NIL {
            return existing;
        }
        let half = self.nodes[parent].size * 0.5;
        let cx0 = self.nodes[parent].cx0
            + if q & 1 == 1 { half } else { 0.0 };
        let cy0 = self.nodes[parent].cy0
            + if q & 2 == 2 { half } else { 0.0 };
        let new_idx = self.nodes.len();
        self.nodes.push(QtNode::empty(cx0, cy0, half));
        self.nodes[parent].children[q] = new_idx;
        new_idx
    }

    /// Insert a body. Stops splitting at a minimum cell size to avoid infinite
    /// recursion when two bodies coincide (which can happen on initial layout
    /// before forces have separated them).
    fn insert(&mut self, body_idx: usize, body_x: f64, body_y: f64, positions: &[(f64, f64)]) {
        const MIN_CELL: f64 = 0.5;
        let mut cur = 0usize;
        loop {
            let n = self.nodes[cur];
            if n.mass == 0.0 {
                let nm = &mut self.nodes[cur];
                nm.com_x = body_x;
                nm.com_y = body_y;
                nm.mass = 1.0;
                nm.body = body_idx;
                return;
            }

            if n.body != QT_NIL {
                // Leaf — split.
                let existing = n.body;
                let (ex, ey) = positions[existing];

                if self.nodes[cur].size <= MIN_CELL {
                    // Bottom of recursion: stack the new body's mass onto this
                    // leaf without splitting further. Geometric centre is the
                    // mean of the two — close enough for repulsion at this
                    // resolution.
                    let nm = &mut self.nodes[cur];
                    nm.com_x = (nm.com_x * nm.mass + body_x) / (nm.mass + 1.0);
                    nm.com_y = (nm.com_y * nm.mass + body_y) / (nm.mass + 1.0);
                    nm.mass += 1.0;
                    return;
                }

                // Mark as internal and migrate the existing body downward.
                let q_existing = self.quadrant(cur, ex, ey);
                let q_body = self.quadrant(cur, body_x, body_y);
                {
                    let nm = &mut self.nodes[cur];
                    nm.body = QT_NIL;
                    nm.com_x = (nm.com_x * nm.mass + body_x) / (nm.mass + 1.0);
                    nm.com_y = (nm.com_y * nm.mass + body_y) / (nm.mass + 1.0);
                    nm.mass += 1.0;
                }

                let c_e = self.ensure_child(cur, q_existing);
                {
                    let cn = &mut self.nodes[c_e];
                    cn.com_x = ex;
                    cn.com_y = ey;
                    cn.mass = 1.0;
                    cn.body = existing;
                }

                if q_existing == q_body {
                    // Same quadrant — descend into it and split further.
                    cur = c_e;
                    continue;
                } else {
                    let c_b = self.ensure_child(cur, q_body);
                    let cn = &mut self.nodes[c_b];
                    cn.com_x = body_x;
                    cn.com_y = body_y;
                    cn.mass = 1.0;
                    cn.body = body_idx;
                    return;
                }
            }

            // Internal: update COM and descend into appropriate quadrant.
            {
                let nm = &mut self.nodes[cur];
                nm.com_x = (nm.com_x * nm.mass + body_x) / (nm.mass + 1.0);
                nm.com_y = (nm.com_y * nm.mass + body_y) / (nm.mass + 1.0);
                nm.mass += 1.0;
            }
            let q = self.quadrant(cur, body_x, body_y);
            cur = self.ensure_child(cur, q);
        }
    }

    /// Accumulate repulsive force on a body at (px, py) from all other masses
    /// in the tree. Cells satisfying `s/d < theta` are treated as a single
    /// point at the centre of mass (Barnes-Hut approximation).
    fn repulse(&self, px: f64, py: f64, k_sq: f64, theta: f64, body_idx: usize) -> (f64, f64) {
        let mut fx = 0.0f64;
        let mut fy = 0.0f64;
        // Iterative depth-first traversal. Stack is bounded by depth ≈ log4(N).
        let mut stack: Vec<usize> = Vec::with_capacity(64);
        stack.push(0);
        while let Some(idx) = stack.pop() {
            let n = self.nodes[idx];
            if n.mass <= 0.0 {
                continue;
            }
            let dx = px - n.com_x;
            let dy = py - n.com_y;
            let dist_sq = (dx * dx + dy * dy).max(0.25);
            let dist = dist_sq.sqrt();

            let is_leaf = n.body != QT_NIL && n.children.iter().all(|&c| c == QT_NIL);
            if is_leaf {
                // Skip self when this is the singleton leaf for body_idx.
                // For stacked-body leaves (rare, only on bodies within 0.5px),
                // the contribution from "self" is at most 1/mass — close
                // enough; explicit core-overlap correction handles the rest.
                if n.body == body_idx && n.mass <= 1.5 {
                    continue;
                }
                let force = (k_sq * n.mass) / dist;
                fx += (dx / dist) * force;
                fy += (dy / dist) * force;
                continue;
            }

            // Internal cell: apply BH approximation if far enough.
            if n.size / dist < theta {
                let force = (k_sq * n.mass) / dist;
                fx += (dx / dist) * force;
                fy += (dy / dist) * force;
            } else {
                for &c in &n.children {
                    if c != QT_NIL {
                        stack.push(c);
                    }
                }
            }
        }
        (fx, fy)
    }
}

/// Fruchterman–Reingold over the backbone with Barnes-Hut repulsion. Stops
/// early when total node displacement falls below an epsilon (≈ 0.5 px per
/// node on average), capped by the iteration budget.
fn run_simulation(layout: &mut LayoutState, max_iterations: usize) {
    let n = layout.nodes.len();
    if n < 2 {
        return;
    }

    let area = layout.width * layout.height;
    let k = ((area / n as f64).sqrt() * 1.35).max(36.0);
    let k_sq = k * k;

    // Normalise edge scores into a (0.15..1.0) attraction multiplier so the
    // strongest pair-specific links pull harder than the weakest.
    let scores: Vec<f64> = layout.edges.iter().map(|(_, _, _, s)| *s).collect();
    let s_min = scores.iter().cloned().fold(f64::INFINITY, f64::min);
    let s_max = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = (s_max - s_min).max(1e-6);

    let initial_temp = layout.width.max(layout.height) * 0.20;
    let cooling = 0.992f64;
    let mut t = initial_temp;
    // Convergence epsilon: stop when total motion is under ~0.5px per node.
    let convergence_eps = (n as f64) * 0.5;

    let mut forces: Vec<(f64, f64)> = vec![(0.0, 0.0); n];
    let mut positions: Vec<(f64, f64)> = layout
        .nodes
        .iter()
        .map(|p| (p.x, p.y))
        .collect();
    let pad = 4.0_f64;

    for iter in 0..max_iterations {
        for f in forces.iter_mut() {
            *f = (0.0, 0.0);
        }
        for (i, p) in layout.nodes.iter().enumerate() {
            positions[i] = (p.x, p.y);
        }

        // Build quadtree over current positions. Pad bounds slightly so
        // boundary nodes are inside the root cell.
        let (mut min_x, mut min_y, mut max_x, mut max_y) =
            (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for &(x, y) in &positions {
            if x < min_x {
                min_x = x;
            }
            if y < min_y {
                min_y = y;
            }
            if x > max_x {
                max_x = x;
            }
            if y > max_y {
                max_y = y;
            }
        }
        let size = ((max_x - min_x).max(max_y - min_y)).max(1.0) + 8.0;
        let mut tree = QuadTree::new(min_x - 4.0, min_y - 4.0, size);
        for (i, &(x, y)) in positions.iter().enumerate() {
            tree.insert(i, x, y, &positions);
        }

        // Repulsion via Barnes-Hut. Plus a near-field core overlap correction
        // so nodes can't sit on top of each other regardless of theta.
        for i in 0..n {
            let (px, py) = positions[i];
            let (mut fx, mut fy) = tree.repulse(px, py, k_sq, BH_THETA, i);

            // Hard core overlap correction — quadratic in the overlap depth.
            // Cheap because it only kicks in when distance < r_i + r_j + pad.
            for j in 0..n {
                if i == j {
                    continue;
                }
                let dx = px - positions[j].0;
                let dy = py - positions[j].1;
                let dist_sq = dx * dx + dy * dy;
                let min_dist = layout.nodes[i].radius + layout.nodes[j].radius + pad;
                if dist_sq < min_dist * min_dist {
                    let dist = dist_sq.sqrt().max(0.5);
                    let core_boost = (min_dist - dist) * 8.0;
                    fx += (dx / dist) * core_boost;
                    fy += (dy / dist) * core_boost;
                }
            }

            forces[i].0 += fx;
            forces[i].1 += fy;
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

        // Integrate, capped by the current temperature, and tally motion.
        let mut total_motion = 0.0f64;
        for i in 0..n {
            let (fx, fy) = forces[i];
            let mag = (fx * fx + fy * fy).sqrt();
            if mag <= 1e-6 {
                continue;
            }
            let mv = mag.min(t);
            layout.nodes[i].x += (fx / mag) * mv;
            layout.nodes[i].y += (fy / mag) * mv;
            total_motion += mv;
        }

        t *= cooling;

        // Convergence: if both motion is small and we've burned enough
        // iterations to settle, exit early. The min-iter floor avoids
        // bailing out on the first pass before any forces have been felt.
        if iter > 60 && total_motion < convergence_eps {
            break;
        }
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
    view: ViewTransform,
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

    // The DPR scaling done in `clear_canvas` is already applied. Stack the
    // user pan/zoom on top so all coordinates below are still in logical
    // canvas space; the transform takes care of where they land.
    let _ = ctx.translate(view.offset_x, view.offset_y);
    let _ = ctx.scale(view.scale, view.scale);

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

    // Community palette: distinct hues spread by the golden ratio so adjacent
    // ids never collide visually. Singletons (community of size 1) inherit
    // the muted colour so they don't add visual noise.
    let max_community = layout.communities.iter().copied().max().unwrap_or(0);
    let mut community_size = vec![0u32; (max_community as usize) + 1];
    for &c in &layout.communities {
        community_size[c as usize] += 1;
    }
    let community_color = |c: u32| -> String {
        if (c as usize) < community_size.len() && community_size[c as usize] <= 1 {
            return muted.clone();
        }
        // Golden ratio conjugate spreads hues evenly around the wheel.
        let hue = ((c as f64) * 137.508_f64) % 360.0;
        format!("hsl({:.0}, 60%, 55%)", hue)
    };

    // Pass 1: nodes (halo + fill). Halo = community ring drawn slightly
    // larger than the fill — gives a visible cluster cue without obscuring
    // group_type colour.
    for (i, node) in graph.nodes.iter().enumerate() {
        let pos = layout.nodes[i];
        let is_hover = hover == Some(i);
        let fill = palette
            .iter()
            .find(|(k, _)| *k == node.group_type.as_str())
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| muted.clone());

        // Halo (community ring).
        if let Some(&community) = layout.communities.get(i) {
            let halo_color = community_color(community);
            ctx.begin_path();
            ctx.arc(pos.x, pos.y, pos.radius + 3.5, 0.0, std::f64::consts::TAU)
                .ok();
            ctx.set_stroke_style_str(&halo_color);
            ctx.set_line_width(if is_hover { 3.5 } else { 2.5 });
            ctx.stroke();
        }

        // Fill.
        ctx.begin_path();
        ctx.arc(pos.x, pos.y, pos.radius, 0.0, std::f64::consts::TAU)
            .ok();
        ctx.set_fill_style_str(&fill);
        ctx.fill();

        ctx.set_line_width(if is_hover { 2.0 } else { 0.8 });
        ctx.set_stroke_style_str(&body_color);
        ctx.stroke();
    }

    // Pass 2: labels with greedy collision avoidance. Sort by importance
    // (radius first, then count) so top-N tags claim screen real estate
    // before the long tail. Hovered node is forced on top regardless.
    let mut placed: Vec<(f64, f64, f64, f64)> = Vec::new();
    let approx_label_w = |name: &str| (name.chars().count() as f64) * 6.6 + 6.0;
    let label_h = 14.0;

    let mut order: Vec<usize> = (0..graph.nodes.len()).collect();
    order.sort_by(|&a, &b| {
        let ra = layout.nodes[a].radius;
        let rb = layout.nodes[b].radius;
        rb.partial_cmp(&ra)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| graph.nodes[b].count.cmp(&graph.nodes[a].count))
    });
    if let Some(h) = hover {
        if let Some(p) = order.iter().position(|&i| i == h) {
            order.remove(p);
            order.insert(0, h);
        }
    }

    ctx.set_text_align("left");
    ctx.set_fill_style_str(&body_color);
    for i in order {
        let pos = layout.nodes[i];
        let is_hover = hover == Some(i);
        let name = &graph.nodes[i].name;
        let lx = pos.x + pos.radius + 4.0;
        let ly = pos.y;
        let lw = approx_label_w(name);
        let lh = label_h;
        let bbox = (lx, ly - lh * 0.5, lx + lw, ly + lh * 0.5);

        // Always render the hovered label (and big-enough nodes), even if it
        // overlaps — visibility on hover is non-negotiable. For everyone else,
        // skip when the bbox collides with anything already placed.
        let force = is_hover || pos.radius >= 9.0;
        if !force {
            let collides = placed.iter().any(|&(ax0, ay0, ax1, ay1)| {
                bbox.0 < ax1 && bbox.2 > ax0 && bbox.1 < ay1 && bbox.3 > ay0
            });
            if collides {
                continue;
            }
        }

        let _ = ctx.fill_text(name, lx, ly);
        placed.push(bbox);
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

fn zoom_around_centre(
    canvas_ref: &NodeRef,
    view: &UseStateHandle<ViewTransform>,
    factor: f64,
) {
    let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() else {
        return;
    };
    let cx = (canvas.client_width().max(0) as f64) * 0.5;
    let cy = (canvas.client_height().max(0) as f64) * 0.5;
    let cur: ViewTransform = **view;
    let new_scale = (cur.scale * factor).clamp(ZOOM_MIN, ZOOM_MAX);
    if (new_scale - cur.scale).abs() < 1e-6 {
        return;
    }
    let real = new_scale / cur.scale;
    view.set(ViewTransform {
        offset_x: cx - (cx - cur.offset_x) * real,
        offset_y: cy - (cy - cur.offset_y) * real,
        scale: new_scale,
    });
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
