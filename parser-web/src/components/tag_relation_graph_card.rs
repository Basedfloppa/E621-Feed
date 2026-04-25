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
    read_config_from_head,
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
    /// Per-node community label (compact, contiguous ints). Drives the node
    /// fill colour so cluster structure is visible at a glance.
    communities: Vec<u32>,
    /// Cached edge score min/max so `draw_graph` doesn't recompute on every
    /// paint (drawn 60+ times/sec while panning).
    score_min: f64,
    score_max: f64,
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

const ZOOM_MIN: f64 = 0.5;
const ZOOM_MAX: f64 = 2.5;
const ZOOM_STEP: f64 = 1.1;

/// Barnes-Hut θ. 0.9 is a good legibility/speed tradeoff for n ≤ 1000:
/// distant clusters use centre-of-mass, nearby pairs are exact.
const BH_THETA: f64 = 0.9;

/// Click-vs-drag threshold (in pixels of cumulative cursor motion). Below
/// this, mouseup on a hovered node is treated as a click → open e621 search.
const CLICK_DRAG_THRESHOLD_PX: f64 = 4.0;

const STORAGE_KEY_TOP_N: &str = "tag_graph_top_n";
const STORAGE_KEY_MIN_COOC: &str = "tag_graph_min_cooc";
const STORAGE_KEY_EDGES_PER_TAG: &str = "tag_graph_edges_per_tag";

/// Produce a `(community_id, top_tag_name, node_count)` summary sorted by
/// size desc, singletons dropped, capped to keep the legend compact.
fn community_summary(
    payload: &UseStateHandle<Option<TagRelationGraphPayload>>,
    layout_ref: &Rc<RefCell<LayoutState>>,
) -> Vec<(u32, String, usize)> {
    let Some(graph) = payload.as_ref() else {
        return Vec::new();
    };
    let layout = layout_ref.borrow();
    let mut by_c: HashMap<u32, (String, i64, usize)> = HashMap::new();
    for (i, &c) in layout.communities.iter().enumerate() {
        if i >= graph.nodes.len() {
            continue;
        }
        let entry = by_c.entry(c).or_insert_with(|| (String::new(), 0, 0));
        entry.2 += 1;
        if graph.nodes[i].count > entry.1 {
            entry.1 = graph.nodes[i].count;
            entry.0 = graph.nodes[i].name.clone();
        }
    }
    let mut out: Vec<(u32, String, usize)> = by_c
        .into_iter()
        .filter(|(_, e)| e.2 > 1)
        .map(|(c, (name, _, size))| (c, name, size))
        .collect();
    out.sort_by(|a, b| b.2.cmp(&a.2));
    out.truncate(10);
    out
}

fn read_local<T: std::str::FromStr>(key: &str) -> Option<T> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(key).ok().flatten())
        .and_then(|v| v.parse::<T>().ok())
}

fn write_local(key: &str, value: &str) {
    if let Some(store) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = store.set_item(key, value);
    }
}

#[function_component(TagRelationGraphCard)]
pub fn tag_relation_graph_card(props: &TagRelationGraphCardProps) -> Html {
    let payload: UseStateHandle<Option<TagRelationGraphPayload>> = use_state(|| None);
    let loading = use_state(|| false);
    let error: UseStateHandle<Option<String>> = use_state(|| None);
    let top_n = use_state(|| read_local::<usize>(STORAGE_KEY_TOP_N).unwrap_or(60).clamp(5, 250));
    let min_cooc = use_state(|| read_local::<i64>(STORAGE_KEY_MIN_COOC).unwrap_or(3).max(1));
    let edges_per_tag =
        use_state(|| read_local::<usize>(STORAGE_KEY_EDGES_PER_TAG).unwrap_or(6).clamp(2, 20));
    let theme_trigger = use_state(|| 0u32);
    let resize_trigger = use_state(|| 0u32);
    let hover_idx: UseStateHandle<Option<usize>> = use_state(|| None);

    let canvas_ref = use_node_ref();
    let layout_ref: Rc<RefCell<LayoutState>> = use_mut_ref(LayoutState::default);
    let view: UseStateHandle<ViewTransform> = use_state(ViewTransform::default);
    // Pan/drag mutates `view_ref` directly and redraws via rAF, bypassing
    // Yew re-renders. Yew state catches up on drag-end.
    let view_ref: Rc<RefCell<ViewTransform>> = use_mut_ref(ViewTransform::default);
    let raf_id: Rc<RefCell<Option<i32>>> = use_mut_ref(|| None);
    // (start_cursor_x, start_cursor_y, start_offset_x, start_offset_y, total_dist)
    let drag_ref: Rc<RefCell<Option<(f64, f64, f64, f64, f64)>>> = use_mut_ref(|| None);
    // Reflected to Yew state only at drag start/end so the cursor flips, without
    // forcing a re-render per mousemove.
    let is_dragging = use_state(|| false);
    // Click-to-isolate community: when Some(c), other communities dim.
    let isolated_community: UseStateHandle<Option<u32>> = use_state(|| None);

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
        let top_n_val = *top_n;
        use_effect_with(top_n_val, move |v: &usize| {
            write_local(STORAGE_KEY_TOP_N, &v.to_string());
            || ()
        });
    }
    {
        let min_cooc_val = *min_cooc;
        use_effect_with(min_cooc_val, move |v: &i64| {
            write_local(STORAGE_KEY_MIN_COOC, &v.to_string());
            || ()
        });
    }
    {
        let edges_per_tag_val = *edges_per_tag;
        use_effect_with(edges_per_tag_val, move |v: &usize| {
            write_local(STORAGE_KEY_EDGES_PER_TAG, &v.to_string());
            || ()
        });
    }

    // Reset community isolation when the underlying graph changes, otherwise
    // a stale community-id from the previous topology could stay focused.
    {
        let isolated_community = isolated_community.clone();
        let nodes_len = payload.as_ref().map(|g| g.nodes.len()).unwrap_or(0);
        use_effect_with(nodes_len, move |_| {
            isolated_community.set(None);
            || ()
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
        let view_ref_for_effect = view_ref.clone();
        let edges_per_tag_val = *edges_per_tag;
        let view_render = *view;
        let iso_render = *isolated_community;
        // `view` is back in deps so zoom (Yew state path) repaints the canvas.
        // Drag pan still bypasses this effect via view_ref + rAF.
        use_effect_with(
            (
                payload.clone(),
                *theme_trigger,
                *resize_trigger,
                *hover_idx_render,
                edges_per_tag_val,
                view_render,
                iso_render,
            ),
            move |(payload, _, _, hover_idx_val, k_val, view_render, iso_val)| {
                let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() else {
                    return;
                };
                // Mirror Yew view → view_ref so drag-pan (rAF) sees latest scale/offset.
                *view_ref_for_effect.borrow_mut() = *view_render;
                let view = *view_render;

                if let Some(graph) = payload.as_ref() {
                    let logical_width = canvas.client_width().max(0) as f64;
                    if logical_width <= 0.0 {
                        return;
                    }
                    let logical_height = (logical_width * 0.85).clamp(480.0, 900.0);

                    let mut layout = layout_ref.borrow_mut();
                    let nodes_changed = layout.nodes.len() != graph.nodes.len()
                        || layout.edges.len() != expected_backbone_len(graph, *k_val);
                    let size_changed = (layout.width - logical_width).abs() > 0.5
                        || (layout.height - logical_height).abs() > 0.5;
                    if nodes_changed {
                        let backbone = select_backbone(graph, *k_val);
                        *layout = initial_layout(
                            &graph.nodes,
                            logical_width,
                            logical_height,
                            backbone,
                        );
                        run_simulation(&mut layout, simulation_iterations(graph.nodes.len()));
                        fit_to_viewport(&mut layout, 0.06);
                    } else if size_changed {
                        rescale_layout(&mut layout, logical_width, logical_height);
                    }
                    draw_graph(&canvas, &layout, graph, *hover_idx_val, *iso_val, view);
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
        let view_ref = view_ref.clone();
        let drag_ref = drag_ref.clone();
        let raf_id = raf_id.clone();
        let payload = payload.clone();
        let isolated_community = isolated_community.clone();
        Callback::from(move |evt: WebMouseEvent| {
            let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() else {
                return;
            };
            let element: &web_sys::Element = canvas.as_ref();
            let rect = element.get_bounding_client_rect();
            let x = evt.client_x() as f64 - rect.left();
            let y = evt.client_y() as f64 - rect.top();

            // Drag → pan via rAF without Yew re-render.
            let drag_active = drag_ref.borrow().is_some();
            if drag_active {
                let (new_off_x, new_off_y, dx, dy) = {
                    let mut d = drag_ref.borrow_mut();
                    let entry = d.as_mut().unwrap();
                    let (start_cx, start_cy, start_off_x, start_off_y, _) = *entry;
                    let new_off_x = start_off_x + (x - start_cx);
                    let new_off_y = start_off_y + (y - start_cy);
                    let cur = *view_ref.borrow();
                    let dx = (cur.offset_x - new_off_x).abs();
                    let dy = (cur.offset_y - new_off_y).abs();
                    entry.4 += (dx * dx + dy * dy).sqrt();
                    (new_off_x, new_off_y, dx, dy)
                };
                if dx > 0.5 || dy > 0.5 {
                    let scale = view_ref.borrow().scale;
                    *view_ref.borrow_mut() = ViewTransform {
                        offset_x: new_off_x,
                        offset_y: new_off_y,
                        scale,
                    };
                    schedule_redraw(
                        &canvas_ref,
                        &layout_ref,
                        &payload,
                        &hover_idx,
                        &isolated_community,
                        &view_ref,
                        &raf_id,
                    );
                }
                return;
            }

            // Hover hit-test in transformed (logical) space (uses view_ref so
            // it stays correct mid-drag flush, but drag isn't active here).
            let view = *view_ref.borrow();
            let (lx, ly) = view.to_logical(x, y);
            let layout = layout_ref.borrow();
            let mut found: Option<usize> = None;
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
        let is_dragging = is_dragging.clone();
        Callback::from(move |_: WebMouseEvent| {
            if hover_idx.is_some() {
                hover_idx.set(None);
            }
            let was_dragging = drag_ref.borrow().is_some();
            *drag_ref.borrow_mut() = None;
            if was_dragging {
                is_dragging.set(false);
            }
        })
    };

    let on_mouse_down = {
        let canvas_ref = canvas_ref.clone();
        let drag_ref = drag_ref.clone();
        let view_ref = view_ref.clone();
        let is_dragging = is_dragging.clone();
        Callback::from(move |evt: WebMouseEvent| {
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
            let cur = *view_ref.borrow();
            *drag_ref.borrow_mut() = Some((x, y, cur.offset_x, cur.offset_y, 0.0));
            is_dragging.set(true);
        })
    };

    let on_mouse_up = {
        let drag_ref = drag_ref.clone();
        let view_ref = view_ref.clone();
        let view = view.clone();
        let hover_idx = hover_idx.clone();
        let payload = payload.clone();
        let is_dragging = is_dragging.clone();
        Callback::from(move |evt: WebMouseEvent| {
            if evt.button() != 0 {
                return;
            }
            let drag_dist = drag_ref.borrow().map(|(_, _, _, _, d)| d).unwrap_or(f64::INFINITY);
            let was_dragging = drag_ref.borrow().is_some();
            *drag_ref.borrow_mut() = None;
            if was_dragging {
                is_dragging.set(false);
            }

            // Click (not drag) on a hovered node → open e621 search for that tag.
            if drag_dist < CLICK_DRAG_THRESHOLD_PX {
                if let (Some(idx), Some(graph), Some(cfg)) =
                    (*hover_idx, payload.as_ref(), read_config_from_head())
                {
                    if let Some(node) = graph.nodes.get(idx) {
                        let url = format!(
                            "{}/posts?tags={}",
                            cfg.posts_domain,
                            urlencoding::encode(&node.name),
                        );
                        if let Some(win) = web_sys::window() {
                            let _ = win.open_with_url_and_target(&url, "_blank");
                        }
                    }
                }
            }

            // Sync ref → Yew state once at drag end so toolbar/ effect see the final view.
            let final_view = *view_ref.borrow();
            if *view != final_view {
                view.set(final_view);
            }
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
                let view = view.clone();
                let cb = Closure::<dyn FnMut(WebWheelEvent)>::new(
                    move |evt: WebWheelEvent| {
                        evt.prevent_default();
                        let element: &web_sys::Element = canvas_for_cb.as_ref();
                        let rect = element.get_bounding_client_rect();
                        let x = evt.client_x() as f64 - rect.left();
                        let y = evt.client_y() as f64 - rect.top();
                        let cur = *view;
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
                        view.set(ViewTransform {
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
    let community_legend_data = community_summary(&payload, &layout_ref);
    let legend_html: Html = if community_legend_data.is_empty() {
        html! {}
    } else {
        let cur_iso = *isolated_community;
        let on_show_all = {
            let isolated_community = isolated_community.clone();
            Callback::from(move |_: WebMouseEvent| isolated_community.set(None))
        };
        let buttons: Html = community_legend_data
            .iter()
            .map(|(c, top_name, count)| {
                let isolated_community = isolated_community.clone();
                let target = *c;
                let onclick = Callback::from(move |_: WebMouseEvent| {
                    if *isolated_community == Some(target) {
                        isolated_community.set(None);
                    } else {
                        isolated_community.set(Some(target));
                    }
                });
                let active = cur_iso == Some(*c);
                let hue = (*c as f64 * 137.508_f64) % 360.0;
                let style = format!(
                    "border-left: 8px solid hsl({:.0}, 60%, 55%); padding-left: 0.5rem;",
                    hue
                );
                let cls = classes!(
                    "btn",
                    "btn-sm",
                    if active { "btn-secondary" } else { "btn-outline-secondary" }
                );
                let title = format!("{count} tags in this community");
                let label = format!("{top_name} · {count}");
                html! {
                    <button type="button" {onclick} class={cls} style={style} title={title}>
                        { label }
                    </button>
                }
            })
            .collect();
        let show_all = if cur_iso.is_some() {
            html! {
                <button type="button" class="btn btn-sm btn-outline-secondary" onclick={on_show_all}>
                    {"Show all"}
                </button>
            }
        } else {
            html! {}
        };
        html! {
            <div class="d-flex flex-wrap gap-2 mt-2 align-items-center">
                <small class="text-muted me-1">{"Communities:"}</small>
                { show_all }
                { buttons }
            </div>
        }
    };

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
                            if *is_dragging { "grabbing" } else { "grab" }
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
                    if matches!(payload.as_ref(), Some(g) if !g.nodes.is_empty() && g.edges.is_empty()) {
                        <div class="position-absolute top-50 start-50 translate-middle text-center text-muted small px-3 py-2 rounded bg-body-tertiary" style="pointer-events: none; max-width: 280px;">
                            {"No tag pairs co-occur often enough yet. Add more favourites or lower the "}
                            <em>{"min co-occurrence"}</em>
                            {" threshold above to see relationships."}
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
                <p class="small text-muted mt-1 mb-0">{"Node colour = detected community. Hover for tag info, click a node to open its e621 search, click a community below to isolate it."}</p>
                { legend_html }
            </>
        }
    };

    html! {
        <div class="card mt-4">
            <div class="card-header bg-primary text-white d-flex justify-content-between align-items-center">
                <h5 class="mb-0">{"Tag Relation Graph"}</h5>
                <small class="opacity-75">{"hover a node for details · colour = community"}</small>
            </div>
            <div class="card-body">
                { body }
            </div>
        </div>
    }
}

fn default_scoring() -> crate::models::TagRelationScoring {
    crate::models::TagRelationScoring {
        w_global: 0.4,
        w_personal: 0.6,
        pmi_scale: 5.0,
        cooc_ref: 20.0,
        user_cooc_ref: 5.0,
        min_cooc_global: 2,
        min_cooc_user: 1,
    }
}

fn resolve_scoring(graph: &TagRelationGraphPayload) -> crate::models::TagRelationScoring {
    let s = &graph.scoring;
    if s.pmi_scale <= 0.0 || (s.w_global <= 0.0 && s.w_personal <= 0.0) {
        default_scoring()
    } else {
        s.clone()
    }
}

fn pair_pmi_score(c: i64, m_a: f64, m_b: f64, n: f64, min_cooc: i64, scale: f64, cooc_ref: f64) -> f64 {
    if c < min_cooc || m_a <= 0.0 || m_b <= 0.0 || n <= 0.0 {
        return 0.0;
    }
    let denom = m_a * m_b / n;
    if denom <= 0.0 {
        return 0.0;
    }
    let lift = (c as f64) / denom;
    let raw_pmi = (lift.max(1e-6).ln() / scale.max(1e-3)).clamp(0.0, 1.0);
    let cooc_ref_log = ((cooc_ref + 1.0).ln()).max(1e-3);
    let conf = (((c as f64) + 1.0).ln() / cooc_ref_log).clamp(0.0, 1.0);
    raw_pmi * conf
}

fn edge_score(
    edge: &TagRelationEdge,
    nodes: &[TagRelationNode],
    n_user: i64,
    n_catalog: i64,
    scoring: &crate::models::TagRelationScoring,
) -> f64 {
    if edge.source >= nodes.len() || edge.target >= nodes.len() || edge.source == edge.target {
        return 0.0;
    }
    let m_a = nodes[edge.source].count.max(0) as f64;
    let m_b = nodes[edge.target].count.max(0) as f64;
    let nu = n_user.max(0) as f64;
    let nc = n_catalog.max(0) as f64;

    let user_score = pair_pmi_score(
        edge.user_cooc,
        m_a,
        m_b,
        nu,
        scoring.min_cooc_user,
        scoring.pmi_scale as f64,
        scoring.user_cooc_ref as f64,
    );

    let global_score = if edge.global_lift > 0.0 && edge.global_cooc >= scoring.min_cooc_global {
        let raw_pmi = ((edge.global_lift as f64).max(1e-6).ln() / (scoring.pmi_scale as f64).max(1e-3))
            .clamp(0.0, 1.0);
        let cooc_ref_log = ((scoring.cooc_ref as f64 + 1.0).ln()).max(1e-3);
        let conf = ((edge.global_cooc.max(0) as f64 + 1.0).ln() / cooc_ref_log).clamp(0.0, 1.0);
        let _ = nc; // n_catalog is captured via global_lift; keep param for parity.
        raw_pmi * conf
    } else {
        0.0
    };

    let w_g = scoring.w_global.max(0.0) as f64;
    let w_u = scoring.w_personal.max(0.0) as f64;
    let sum = (w_g + w_u).max(1e-6);
    (w_g * global_score + w_u * user_score) / sum
}

fn select_backbone(graph: &TagRelationGraphPayload, k: usize) -> Vec<(usize, usize, usize, f64)> {
    let n = graph.nodes.len();
    if n == 0 || graph.edges.is_empty() {
        return Vec::new();
    }
    let k = k.max(1);
    let n_user = graph.account_post_count;
    let n_catalog = graph.catalog_post_count;
    let scoring = resolve_scoring(graph);

    let mut scored: Vec<f64> = Vec::with_capacity(graph.edges.len());
    let mut valid: Vec<bool> = Vec::with_capacity(graph.edges.len());
    let mut per_node: Vec<Vec<(f64, usize)>> = vec![Vec::new(); n];
    for (idx, e) in graph.edges.iter().enumerate() {
        if e.source >= n || e.target >= n || e.source == e.target {
            scored.push(f64::NEG_INFINITY);
            valid.push(false);
            continue;
        }
        let s = edge_score(e, &graph.nodes, n_user, n_catalog, &scoring);
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
        score_min: 0.0,
        score_max: 1.0,
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
    for &(a, b, _, score) in edges {
        if a >= n_nodes || b >= n_nodes || a == b {
            continue;
        }
        if !score.is_finite() || score <= 0.0 {
            continue;
        }
        adj[a].push((b, score));
        adj[b].push((a, score));
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

    /// Visit every body that lies within `radius` of (px, py), passing each
    /// one's index and `(x, y)` to `f`. Used for the near-field core-overlap
    /// correction without paying the O(N²) all-pairs cost. Cells whose AABB
    /// can't intersect the query disc are skipped wholesale.
    fn query_neighbours<F: FnMut(usize, f64, f64)>(
        &self,
        px: f64,
        py: f64,
        radius: f64,
        positions: &[(f64, f64)],
        mut f: F,
    ) {
        let r_sq = radius * radius;
        let mut stack: Vec<usize> = Vec::with_capacity(64);
        stack.push(0);
        while let Some(idx) = stack.pop() {
            let n = self.nodes[idx];
            if n.mass <= 0.0 {
                continue;
            }
            // AABB-vs-disc rejection: nearest point on cell to (px, py).
            let nx = px.clamp(n.cx0, n.cx0 + n.size);
            let ny = py.clamp(n.cy0, n.cy0 + n.size);
            let dx = px - nx;
            let dy = py - ny;
            if dx * dx + dy * dy > r_sq {
                continue;
            }

            let is_leaf = n.body != QT_NIL && n.children.iter().all(|&c| c == QT_NIL);
            if is_leaf {
                let (bx, by) = positions[n.body];
                let ddx = bx - px;
                let ddy = by - py;
                if ddx * ddx + ddy * ddy <= r_sq {
                    f(n.body, bx, by);
                }
                continue;
            }
            for &c in &n.children {
                if c != QT_NIL {
                    stack.push(c);
                }
            }
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
        // so nodes can't sit on top of each other regardless of theta. The
        // overlap query uses the quadtree to visit only nodes inside the
        // candidate radius — O(log N) per body in the typical case versus the
        // O(N) all-pairs sweep this used to do.
        let max_radius = layout
            .nodes
            .iter()
            .map(|m| m.radius)
            .fold(0.0_f64, f64::max);
        for i in 0..n {
            let (px, py) = positions[i];
            let (mut fx, mut fy) = tree.repulse(px, py, k_sq, BH_THETA, i);

            let r_i = layout.nodes[i].radius;
            // Anything closer than r_i + r_max + pad could overlap.
            let query_r = r_i + max_radius + pad;
            tree.query_neighbours(px, py, query_r, &positions, |j, jx, jy| {
                if i == j {
                    return;
                }
                let dx = px - jx;
                let dy = py - jy;
                let dist_sq = dx * dx + dy * dy;
                let min_dist = r_i + layout.nodes[j].radius + pad;
                if dist_sq < min_dist * min_dist {
                    let dist = dist_sq.sqrt().max(0.5);
                    let core_boost = (min_dist - dist) * 8.0;
                    fx += (dx / dist) * core_boost;
                    fy += (dy / dist) * core_boost;
                }
            });

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

    // Cache the edge-score range so `draw_graph` doesn't recompute on every
    // paint (60+ paints/sec while panning).
    layout.score_min = if s_min.is_finite() { s_min } else { 0.0 };
    layout.score_max = if s_max.is_finite() { s_max } else { 1.0 };
}

/// Rescale already-simulated positions to a new canvas size — used when the
/// browser resizes but the topology hasn't changed. Avoids the full re-sim
/// (hundreds of ms for n>100) and just maps the existing aspect into the new
/// frame. Calls `fit_to_viewport` to re-centre.
fn rescale_layout(layout: &mut LayoutState, new_width: f64, new_height: f64) {
    if layout.width <= 0.0 || layout.height <= 0.0 {
        layout.width = new_width;
        layout.height = new_height;
        return;
    }
    let sx = new_width / layout.width;
    let sy = new_height / layout.height;
    for node in layout.nodes.iter_mut() {
        node.x *= sx;
        node.y *= sy;
    }
    layout.width = new_width;
    layout.height = new_height;
    fit_to_viewport(layout, 0.06);
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
    isolated_community: Option<u32>,
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

    // Cached score range from simulation — recomputing min/max on every paint
    // (60+/sec while panning) was a measurable cost.
    let s_min = layout.score_min;
    let span = (layout.score_max - layout.score_min).max(1e-6);

    // Isolation: focus on a single community by dimming everything else.
    let in_focus = |idx: usize| -> bool {
        match isolated_community {
            Some(c) => layout.communities.get(idx).copied() == Some(c),
            None => true,
        }
    };

    // ---- Pre-pass: dim out-of-focus edges + nodes (when isolating) -------
    if isolated_community.is_some() {
        let mut dim_edges: Vec<usize> = Vec::new();
        for (idx, &(src, tgt, _, _)) in layout.edges.iter().enumerate() {
            if src >= layout.nodes.len() || tgt >= layout.nodes.len() {
                continue;
            }
            if !(in_focus(src) && in_focus(tgt)) {
                dim_edges.push(idx);
            }
        }
        if !dim_edges.is_empty() {
            ctx.set_stroke_style_str(&with_alpha(&muted, 0.10));
            ctx.set_line_width(0.6);
            ctx.begin_path();
            for &idx in &dim_edges {
                let (src, tgt, _, _) = layout.edges[idx];
                let a = layout.nodes[src];
                let b = layout.nodes[tgt];
                ctx.move_to(a.x, a.y);
                ctx.line_to(b.x, b.y);
            }
            ctx.stroke();
        }
        ctx.set_fill_style_str(&with_alpha(&muted, 0.18));
        ctx.begin_path();
        for i in 0..layout.nodes.len().min(graph.nodes.len()) {
            if in_focus(i) {
                continue;
            }
            let pos = layout.nodes[i];
            ctx.move_to(pos.x + pos.radius, pos.y);
            ctx.arc(pos.x, pos.y, pos.radius, 0.0, std::f64::consts::TAU)
                .ok();
        }
        ctx.fill();
    }

    // ---- Pass 1: backbone edges (in-focus only) --------------------------
    // Quantise (alpha, line_width) into a small grid and stroke each bucket
    // as a single path.
    const ALPHA_BANDS: usize = 8;
    const WIDTH_BANDS: usize = 4;
    let total_buckets = ALPHA_BANDS * WIDTH_BANDS;
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); total_buckets];
    let mut highlight_edges: Vec<usize> = Vec::new();
    for (idx, &(src, tgt, _, score)) in layout.edges.iter().enumerate() {
        if src >= layout.nodes.len() || tgt >= layout.nodes.len() {
            continue;
        }
        if !(in_focus(src) && in_focus(tgt)) {
            continue;
        }
        let highlight = matches!(hover, Some(h) if h == src || h == tgt);
        if highlight {
            highlight_edges.push(idx);
            continue;
        }
        let strength = ((score - s_min) / span).clamp(0.0, 1.0);
        let alpha_b = ((strength * (ALPHA_BANDS as f64 - 1.0)).round() as usize)
            .min(ALPHA_BANDS - 1);
        let width_b = ((strength * (WIDTH_BANDS as f64 - 1.0)).round() as usize)
            .min(WIDTH_BANDS - 1);
        buckets[alpha_b * WIDTH_BANDS + width_b].push(idx);
    }
    for bucket_idx in 0..total_buckets {
        if buckets[bucket_idx].is_empty() {
            continue;
        }
        let alpha_b = bucket_idx / WIDTH_BANDS;
        let width_b = bucket_idx % WIDTH_BANDS;
        let alpha_norm = alpha_b as f64 / (ALPHA_BANDS as f64 - 1.0).max(1.0);
        let width_norm = width_b as f64 / (WIDTH_BANDS as f64 - 1.0).max(1.0);
        let alpha = (0.12 + alpha_norm * 0.55).clamp(0.12, 0.78);
        let line_w = (0.6 + width_norm * 3.0).clamp(0.6, 4.0);
        ctx.set_stroke_style_str(&with_alpha(&muted, alpha as f32));
        ctx.set_line_width(line_w);
        ctx.begin_path();
        for &idx in &buckets[bucket_idx] {
            let (src, tgt, _, _) = layout.edges[idx];
            let a = layout.nodes[src];
            let b = layout.nodes[tgt];
            ctx.move_to(a.x, a.y);
            ctx.line_to(b.x, b.y);
        }
        ctx.stroke();
    }

    // Highlighted edges drawn last (one path) so they always sit on top.
    if !highlight_edges.is_empty() {
        ctx.set_stroke_style_str(&with_alpha(&body_color, 0.95));
        ctx.set_line_width(2.6);
        ctx.begin_path();
        for &idx in &highlight_edges {
            let (src, tgt, _, _) = layout.edges[idx];
            let a = layout.nodes[src];
            let b = layout.nodes[tgt];
            ctx.move_to(a.x, a.y);
            ctx.line_to(b.x, b.y);
        }
        ctx.stroke();
    }

    // ---- Pass 2: nodes (community fill, batched per community) ------------
    // Coloured purely by community now — tag-type fill removed per UX request.
    // Singleton communities fall back to the muted colour so isolated nodes
    // don't add saturated noise.
    let max_community = layout.communities.iter().copied().max().unwrap_or(0);
    let mut community_size = vec![0u32; (max_community as usize) + 1];
    for &c in &layout.communities {
        community_size[c as usize] += 1;
    }
    let community_color = |c: u32| -> String {
        if (c as usize) < community_size.len() && community_size[c as usize] <= 1 {
            return muted.clone();
        }
        let hue = ((c as f64) * 137.508_f64) % 360.0;
        format!("hsl({:.0}, 60%, 55%)", hue)
    };

    // Bucket nodes by community so we stroke/fill each colour as one path.
    let bucket_count = (max_community as usize) + 1;
    let mut node_buckets: Vec<Vec<usize>> = vec![Vec::new(); bucket_count];
    for (i, &c) in layout.communities.iter().enumerate() {
        if i >= layout.nodes.len() {
            continue;
        }
        if hover == Some(i) {
            // Hover handled separately so the outline width is right.
            continue;
        }
        if !in_focus(i) {
            continue;
        }
        node_buckets[c as usize].push(i);
    }

    for (c_idx, ids) in node_buckets.iter().enumerate() {
        if ids.is_empty() {
            continue;
        }
        let color = community_color(c_idx as u32);
        ctx.set_fill_style_str(&color);
        ctx.begin_path();
        for &i in ids {
            let pos = layout.nodes[i];
            ctx.move_to(pos.x + pos.radius, pos.y);
            ctx.arc(pos.x, pos.y, pos.radius, 0.0, std::f64::consts::TAU)
                .ok();
        }
        ctx.fill();
    }

    // Outlines as one path per width tier (here: just the standard tier;
    // hovered node drawn last as a one-off below).
    ctx.set_line_width(0.8);
    ctx.set_stroke_style_str(&body_color);
    ctx.begin_path();
    for (i, _) in graph.nodes.iter().enumerate() {
        if hover == Some(i) || i >= layout.nodes.len() {
            continue;
        }
        if !in_focus(i) {
            continue;
        }
        let pos = layout.nodes[i];
        ctx.move_to(pos.x + pos.radius, pos.y);
        ctx.arc(pos.x, pos.y, pos.radius, 0.0, std::f64::consts::TAU)
            .ok();
    }
    ctx.stroke();

    // Hovered node: drawn last with a thicker outline.
    if let Some(h) = hover {
        if h < layout.nodes.len() {
            let pos = layout.nodes[h];
            let community = layout.communities.get(h).copied().unwrap_or(0);
            let color = community_color(community);
            ctx.set_fill_style_str(&color);
            ctx.begin_path();
            ctx.arc(pos.x, pos.y, pos.radius, 0.0, std::f64::consts::TAU)
                .ok();
            ctx.fill();
            ctx.set_line_width(2.0);
            ctx.set_stroke_style_str(&body_color);
            ctx.stroke();
        }
    }

    // ---- Pass 3: labels with greedy collision avoidance --------------------
    ctx.set_font("12px system-ui, -apple-system, Segoe UI, Roboto, sans-serif");
    ctx.set_text_baseline("middle");
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
        if !in_focus(i) && hover != Some(i) {
            continue;
        }
        let pos = layout.nodes[i];
        let is_hover = hover == Some(i);
        let name = &graph.nodes[i].name;
        let lx = pos.x + pos.radius + 4.0;
        let ly = pos.y;
        let lw = approx_label_w(name);
        let lh = label_h;
        let bbox = (lx, ly - lh * 0.5, lx + lw, ly + lh * 0.5);

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

/// Queue a single rAF redraw. If one is already pending, no-ops — the pending
/// frame will pick up whatever state is current when it fires.
fn schedule_redraw(
    canvas_ref: &NodeRef,
    layout_ref: &Rc<RefCell<LayoutState>>,
    payload: &UseStateHandle<Option<TagRelationGraphPayload>>,
    hover_idx: &UseStateHandle<Option<usize>>,
    isolated_community: &UseStateHandle<Option<u32>>,
    view_ref: &Rc<RefCell<ViewTransform>>,
    raf_id: &Rc<RefCell<Option<i32>>>,
) {
    if raf_id.borrow().is_some() {
        return;
    }
    let Some(win) = web_sys::window() else {
        return;
    };

    let canvas_ref = canvas_ref.clone();
    let layout_ref = layout_ref.clone();
    let payload = payload.clone();
    let hover_idx = hover_idx.clone();
    let isolated_community = isolated_community.clone();
    let view_ref = view_ref.clone();
    let raf_id_inner = raf_id.clone();

    let cb = Closure::<dyn FnMut(f64)>::new(move |_: f64| {
        *raf_id_inner.borrow_mut() = None;
        let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() else {
            return;
        };
        let Some(graph) = payload.as_ref() else {
            return;
        };
        let layout = layout_ref.borrow();
        if layout.nodes.is_empty() {
            return;
        }
        let view = *view_ref.borrow();
        draw_graph(&canvas, &layout, graph, *hover_idx, *isolated_community, view);
    });
    let id = win
        .request_animation_frame(cb.as_ref().unchecked_ref())
        .ok();
    *raf_id.borrow_mut() = id;
    // Closure leaks per scheduled frame (~100 bytes each). Coalescing through
    // `raf_id` caps this to one leak per frame fired, not per request.
    cb.forget();
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
