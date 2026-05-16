//! SVG force-directed tag-relation graph.
//!
//! Previous iteration was a Canvas + Barnes-Hut Fruchterman–Reingold one-shot
//! simulation. That worked but was complex (chunked rAF runner, quadtree,
//! bucketed paint, DPR scaling) and had no live interactivity beyond pan/zoom
//! and hover. The new layout, inspired by `freenet/freenet-net-graph`, drops
//! all that for a simple SVG scene driven by three continuous forces:
//!
//! * **Pairwise repulsion** — inverse-square Coulomb-style push.
//! * **Edge attraction** — Hooke spring per backbone edge, with stronger
//!   personal-PMI links pulling harder than the weakest.
//! * **Mild centre gravity** — keeps the swarm in the viewport.
//!
//! Integration is plain Verlet with velocity damping at ~30 FPS. O(n²) per
//! tick is trivial in WASM at n ≤ 250 (the tag-graph `top_n` cap). Nodes can
//! be grabbed and dragged: the cursor pins one node's position while others
//! reshape around it in real time. SVG `viewBox` handles pan/zoom natively.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use gloo_timers::callback::Interval;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{MouseEvent as WebMouseEvent, WheelEvent as WebWheelEvent};
use yew::prelude::*;

use crate::models::{
    TagRelationEdge, TagRelationGraphPayload, TagRelationNode, api_get, read_config_from_head,
};
use crate::pages::UserInfo;

// =================== Coordinate space & physics tuning ====================

/// SVG `viewBox` width / height. Coordinate space is fixed at 1600×900 so
/// physics constants don't have to be retuned when the container resizes —
/// `preserveAspectRatio="xMidYMid meet"` scales the canvas to fit whatever
/// CSS sizes the SVG to. 16:9 aspect matches the typical card width better
/// than a square viewBox would.
const VIEWBOX_W: f64 = 1600.0;
const VIEWBOX_H: f64 = 900.0;
const CENTER_X: f64 = VIEWBOX_W / 2.0;
const CENTER_Y: f64 = VIEWBOX_H / 2.0;

/// Animation tick. 33 ms ≈ 30 FPS — same as freenet's default.
const TICK_MS: u32 = 33;
/// Inverse-square repulsion coefficient between every pair of nodes.
const K_REPEL: f64 = 3600.0;
/// Edge spring stiffness.
const K_EDGE: f64 = 0.014;
/// Natural resting length of every edge, in viewBox units.
const EDGE_REST_LENGTH: f64 = 110.0;
/// Linear pull toward the canvas centre.
const K_GRAVITY: f64 = 0.005;
/// Velocity damping per tick (0..1; closer to 1 = more inertia).
const DAMPING: f64 = 0.85;
/// Velocity cap so a runaway node can't fly across the canvas in one frame.
const MAX_SPEED: f64 = 22.0;
/// Higher cap during the warmup window so freshly-seeded clusters reach
/// their resting position quickly without visible churn.
const WARMUP_MAX_SPEED: f64 = 60.0;
/// Ticks of warmup-speed after a topology sync. Roughly 8 s at 30 FPS.
const WARMUP_TICKS: u32 = 240;
/// Floor on inter-node distance for the repulsion calculation. Prevents
/// `K_REPEL / d²` from blowing up when two nodes momentarily overlap.
const REPEL_MIN_DIST: f64 = 14.0;
/// Hard padding enforced *after* force integration: every pair is pushed
/// apart so their centres are at least `r_i + r_j + COLLISION_PAD` apart.
/// Repulsion alone can lose to combined spring/gravity forces in dense
/// hubs — this guarantees circles never visibly touch.
const COLLISION_PAD: f64 = 4.0;
/// Synchronous physics iterations run inside `sync()` before the first
/// visible frame. Combined with the community-aware seed below, this
/// lands the graph on a near-final layout the moment data arrives — no
/// visible "settling" churn on top of the user's first sight of the graph.
const PRESETTLE_STEPS: usize = 60;

const ZOOM_STEP: f64 = 1.15;
const MIN_SCALE: f64 = 0.1;
const MAX_SCALE: f64 = 2.0;

/// Click-vs-drag threshold (viewBox units of cumulative cursor motion).
/// Below this, mouseup on a held node is treated as a click → open e621
/// search for that tag instead of just releasing the pin.
const CLICK_DRAG_THRESHOLD: f64 = 6.0;

const STORAGE_KEY_TOP_N: &str = "tag_graph_top_n";
const STORAGE_KEY_MIN_COOC: &str = "tag_graph_min_cooc";
const STORAGE_KEY_EDGES_PER_TAG: &str = "tag_graph_edges_per_tag";

// =================== Layout state =========================================

#[derive(Properties, PartialEq)]
pub struct TagRelationGraphCardProps {
    pub found_user: UseStateHandle<Option<UserInfo>>,
    pub api_base: String,
}

/// Affine transform applied to the entire scene `<g>`. Pure visual — the
/// physics simulation runs in untransformed viewBox coords, so pan/zoom
/// never disturbs node positions or springs.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ViewState {
    tx: f64,
    ty: f64,
    scale: f64,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            tx: 0.0,
            ty: 0.0,
            scale: 1.0,
        }
    }
}

/// Cursor-drag bookkeeping for scene pan.
#[derive(Default)]
struct PanState {
    active: bool,
    last_client_x: f64,
    last_client_y: f64,
}

/// Active node-drag. While `Some`, the physics step pins the named node to
/// `(target_x, target_y)` and zeroes its velocity; surrounding nodes still
/// react to it via their normal repulsion/spring forces.
#[derive(Clone, Debug)]
struct PinnedDrag {
    node_idx: usize,
    target_x: f64,
    target_y: f64,
    /// Accumulated cursor distance since mousedown. Used to disambiguate
    /// click (small) from drag (large) on mouseup.
    distance: f64,
}

struct PhysNode {
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    /// Visual radius in viewBox units. Driven by `node.count` at sync time.
    radius: f64,
}

#[derive(Default)]
struct LayoutState {
    nodes: Vec<PhysNode>,
    /// Backbone: `(source_idx, target_idx, score)`. Built by `select_backbone`
    /// = MST ∪ per-node top-K. Both physics and rendering use only these.
    edges: Vec<(usize, usize, f64)>,
    /// Per-node community label, compact `[0, k)` ints. Drives fill hue.
    communities: Vec<u32>,
    /// Cached edge-score range so callers don't recompute on each render.
    score_min: f64,
    score_max: f64,
    /// Currently-dragged node, if any.
    pinned: Option<PinnedDrag>,
    /// Ticks since the last `sync()`. Gates the warmup-speed window.
    ticks_since_sync: u32,
    /// Inputs the current layout was built from. Compared on every effect
    /// re-run to decide whether to resync.
    built_for_node_count: usize,
    built_for_edges_per_tag: usize,
}

impl LayoutState {
    /// Wipe positions and rebuild from a freshly-fetched payload. Called when
    /// the node count or `edges_per_tag` changes — the backbone selection
    /// and community labels depend on both, so we re-do everything together.
    fn sync(&mut self, graph: &TagRelationGraphPayload, edges_per_tag: usize) {
        let backbone = select_backbone(graph, edges_per_tag);
        let n = graph.nodes.len();

        let mut s_min = f64::INFINITY;
        let mut s_max = f64::NEG_INFINITY;
        for &(_, _, _, s) in &backbone {
            if s < s_min {
                s_min = s;
            }
            if s > s_max {
                s_max = s;
            }
        }
        if !s_min.is_finite() {
            s_min = 0.0;
            s_max = 1.0;
        }
        if s_max <= s_min {
            s_max = s_min + 1e-3;
        }

        self.communities = label_propagation(n, &backbone, 12);
        self.edges = backbone.into_iter().map(|(a, b, _, s)| (a, b, s)).collect();
        self.score_min = s_min;
        self.score_max = s_max;
        self.built_for_node_count = n;
        self.built_for_edges_per_tag = edges_per_tag;
        self.pinned = None;
        self.ticks_since_sync = 0;

        // Uniform radius for every node — per UX request. Slight smoothstep
        // shrink as the graph density grows so a 250-tag layout doesn't
        // stuff oversized circles into a tight viewport.
        let radius = node_radius(n);

        // Group node indices by community so we can seed each community in
        // its own angular sector around the canvas. This is dramatically
        // closer to the eventual equilibrium than uniform random seeding
        // and lets the presettle loop finish in far fewer iterations.
        let n_communities = self
            .communities
            .iter()
            .copied()
            .max()
            .map(|c| c as usize + 1)
            .unwrap_or(0);
        let mut by_community: Vec<Vec<usize>> = vec![Vec::new(); n_communities];
        for (i, &c) in self.communities.iter().enumerate() {
            if (c as usize) < by_community.len() {
                by_community[c as usize].push(i);
            }
        }

        self.nodes.clear();
        self.nodes.resize_with(n, || PhysNode {
            x: CENTER_X,
            y: CENTER_Y,
            vx: 0.0,
            vy: 0.0,
            radius,
        });

        // Outer radius for community centres. Scales with how many
        // communities we have so big graphs still get well-separated
        // sectors and small ones don't fly to the edges.
        let outer_r = (260.0 + (n_communities as f64).sqrt() * 32.0).min(360.0);
        for (c_idx, ids) in by_community.iter().enumerate() {
            if ids.is_empty() {
                continue;
            }
            let nc = n_communities.max(1) as f64;
            let centre_angle = (c_idx as f64 / nc) * std::f64::consts::TAU;
            let cx = CENTER_X + outer_r * centre_angle.cos();
            let cy = CENTER_Y + outer_r * centre_angle.sin();
            // Inner radius grows with community size; floor at ~28 so even
            // pairs spread out a little instead of stacking on top of each
            // other and immediately ejecting under repulsion.
            let inner_r = 28.0 + (ids.len() as f64).sqrt() * 18.0;
            let count = ids.len().max(1) as f64;
            for (k, &i) in ids.iter().enumerate() {
                let mut h: u64 = 1469598103934665603;
                for b in graph.nodes[i].name.as_bytes() {
                    h = h.wrapping_mul(1099511628211);
                    h ^= *b as u64;
                }
                let jitter_r = 0.55 + ((h & 0xffff) as f64 / 0xffff as f64) * 0.45;
                let jitter_a = ((h >> 16) & 0xff) as f64 / 0xff as f64 * 0.6;
                let local_angle = (k as f64 / count) * std::f64::consts::TAU + jitter_a;
                let node = &mut self.nodes[i];
                node.x = cx + inner_r * jitter_r * local_angle.cos();
                node.y = cy + inner_r * jitter_r * local_angle.sin();
            }
        }

        // Presettle: run a batch of synchronous physics steps so the very
        // first frame the user sees is already close to equilibrium. Warmup
        // speed (gated by `ticks_since_sync < WARMUP_TICKS`) lets the
        // remaining clusters resolve within these iterations.
        for _ in 0..PRESETTLE_STEPS {
            self.step();
        }
    }

    fn step(&mut self) {
        let n = self.nodes.len();
        if n == 0 {
            return;
        }

        let positions: Vec<(f64, f64)> = self.nodes.iter().map(|p| (p.x, p.y)).collect();
        let radii: Vec<f64> = self.nodes.iter().map(|p| p.radius).collect();
        let mut forces: Vec<(f64, f64)> = vec![(0.0, 0.0); n];

        // Centre gravity. Linear pull keeps the swarm cohesive without
        // fighting the user when they pan the scene to look at a distant
        // cluster (we apply gravity in *world* space and the pan transform
        // is purely visual).
        for (i, &(x, y)) in positions.iter().enumerate() {
            forces[i].0 -= K_GRAVITY * (x - CENTER_X);
            forces[i].1 -= K_GRAVITY * (y - CENTER_Y);
        }

        // Pairwise repulsion. n² is fine at our scale (≤ 250 → ≤ ~31k pair
        // calcs at 30 Hz = ~940k ops/sec; trivial in WASM).
        for i in 0..n {
            for j in (i + 1)..n {
                let dx = positions[j].0 - positions[i].0;
                let dy = positions[j].1 - positions[i].1;
                // Floor uses each pair's combined radius so big nodes
                // never get sucked together by an artificially high force.
                let min_d = (radii[i] + radii[j] + REPEL_MIN_DIST).max(REPEL_MIN_DIST);
                let d = (dx * dx + dy * dy).sqrt().max(min_d);
                let mag = K_REPEL / (d * d);
                let ux = dx / d;
                let uy = dy / d;
                let fx = mag * ux;
                let fy = mag * uy;
                forces[i].0 -= fx;
                forces[i].1 -= fy;
                forces[j].0 += fx;
                forces[j].1 += fy;
            }
        }

        // Edge springs. High-score (strong PMI) edges pull harder so the
        // backbone topology surfaces visually even when many weaker edges
        // are also present.
        let span = (self.score_max - self.score_min).max(1e-6);
        for &(a, b, score) in &self.edges {
            if a >= n || b >= n {
                continue;
            }
            let w = (0.35 + 0.65 * ((score - self.score_min) / span)).clamp(0.35, 1.0);
            let dx = positions[b].0 - positions[a].0;
            let dy = positions[b].1 - positions[a].1;
            let d = (dx * dx + dy * dy).sqrt().max(0.01);
            let extension = d - EDGE_REST_LENGTH;
            let mag = K_EDGE * extension * w;
            let ux = dx / d;
            let uy = dy / d;
            let fx = mag * ux;
            let fy = mag * uy;
            forces[a].0 += fx;
            forces[a].1 += fy;
            forces[b].0 -= fx;
            forces[b].1 -= fy;
        }

        let pinned_idx = self.pinned.as_ref().map(|p| p.node_idx);
        let max_speed = if self.ticks_since_sync < WARMUP_TICKS {
            WARMUP_MAX_SPEED
        } else {
            MAX_SPEED
        };

        for i in 0..n {
            if Some(i) == pinned_idx {
                continue;
            }
            let (fx, fy) = forces[i];
            let node = &mut self.nodes[i];
            node.vx = (node.vx + fx) * DAMPING;
            node.vy = (node.vy + fy) * DAMPING;
            let speed = (node.vx * node.vx + node.vy * node.vy).sqrt();
            if speed > max_speed {
                node.vx *= max_speed / speed;
                node.vy *= max_speed / speed;
            }
            node.x += node.vx;
            node.y += node.vy;
        }

        // Snap the pinned node to its drag target *after* the force pass so
        // other nodes see it at the cursor position rather than at its
        // pre-step location.
        if let Some(p) = &self.pinned {
            if let Some(node) = self.nodes.get_mut(p.node_idx) {
                node.x = p.target_x;
                node.y = p.target_y;
                node.vx = 0.0;
                node.vy = 0.0;
            }
        }

        // Hard collision resolution. The continuous repulsion above keeps
        // most pairs apart on its own, but high-degree hubs in dense
        // graphs can have springs from many neighbours overpowering the
        // 1/d² push and squashing circles together visibly. A direct
        // position-based separation pass after integration guarantees
        // there's always a `COLLISION_PAD`-wide gap between any two
        // circles. The pinned node is treated as immovable so its
        // partner absorbs the full overlap displacement; otherwise both
        // sides move by half.
        for i in 0..n {
            for j in (i + 1)..n {
                let xi = self.nodes[i].x;
                let yi = self.nodes[i].y;
                let ri = self.nodes[i].radius;
                let xj = self.nodes[j].x;
                let yj = self.nodes[j].y;
                let rj = self.nodes[j].radius;
                let dx = xj - xi;
                let dy = yj - yi;
                let min_d = ri + rj + COLLISION_PAD;
                let d2 = dx * dx + dy * dy;
                if d2 >= min_d * min_d {
                    continue;
                }
                let d = d2.sqrt().max(0.001);
                let overlap = min_d - d;
                let ux = dx / d;
                let uy = dy / d;
                let (share_i, share_j) = if Some(i) == pinned_idx {
                    (0.0, 1.0)
                } else if Some(j) == pinned_idx {
                    (1.0, 0.0)
                } else {
                    (0.5, 0.5)
                };
                if share_i > 0.0 {
                    self.nodes[i].x -= ux * overlap * share_i;
                    self.nodes[i].y -= uy * overlap * share_i;
                }
                if share_j > 0.0 {
                    self.nodes[j].x += ux * overlap * share_j;
                    self.nodes[j].y += uy * overlap * share_j;
                }
            }
        }

        self.ticks_since_sync = self.ticks_since_sync.saturating_add(1);
    }
}

// =================== Coordinate transforms ================================

/// Convert client-space pixel coords into the SVG's `viewBox` units,
/// accounting for `xMidYMid meet` letterboxing. Returns the canvas centre
/// if the SVG isn't laid out yet.
fn client_to_viewbox(svg: &web_sys::Element, client_x: f64, client_y: f64) -> (f64, f64) {
    let rect = svg.get_bounding_client_rect();
    let bw = rect.width();
    let bh = rect.height();
    if bw <= 0.0 || bh <= 0.0 {
        return (CENTER_X, CENTER_Y);
    }
    let scale = (bw / VIEWBOX_W).min(bh / VIEWBOX_H);
    let off_x = (bw - VIEWBOX_W * scale) * 0.5;
    let off_y = (bh - VIEWBOX_H * scale) * 0.5;
    let vbx = (client_x - rect.left() - off_x) / scale;
    let vby = (client_y - rect.top() - off_y) / scale;
    (vbx, vby)
}

/// Multiplier that turns a one-pixel client-space delta into viewBox units.
/// Same letterbox math as `client_to_viewbox`, just the scale component —
/// used by pan so a 10 px drag moves the scene by exactly the same amount
/// regardless of zoom.
fn px_to_viewbox_scale(svg: &web_sys::Element) -> f64 {
    let rect = svg.get_bounding_client_rect();
    let bw = rect.width();
    let bh = rect.height();
    if bw <= 0.0 || bh <= 0.0 {
        return 1.0;
    }
    let scale = (bw / VIEWBOX_W).min(bh / VIEWBOX_H);
    if scale <= 0.0 { 1.0 } else { 1.0 / scale }
}

// =================== Persistence helpers ==================================

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

// =================== Community legend =====================================

/// `(community_id, top_tag_name, node_count)` sorted by size desc, singletons
/// dropped, capped to keep the legend compact.
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
    // Two-stage sort: pick the top 10 by size, then re-sort the survivors
    // alphabetically for display. HashMap iteration is non-deterministic,
    // so without the final alphabetic pass equal-sized communities would
    // visually swap positions on every physics tick — the legend flickers.
    out.sort_by(|a, b| b.2.cmp(&a.2));
    out.truncate(10);
    out.sort_by(|a, b| {
        a.1.to_ascii_lowercase()
            .cmp(&b.1.to_ascii_lowercase())
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

// =================== Component ============================================

#[function_component(TagRelationGraphCard)]
pub fn tag_relation_graph_card(props: &TagRelationGraphCardProps) -> Html {
    let payload: UseStateHandle<Option<TagRelationGraphPayload>> = use_state(|| None);
    let loading = use_state(|| false);
    let error: UseStateHandle<Option<String>> = use_state(|| None);
    let top_n = use_state(|| read_local::<usize>(STORAGE_KEY_TOP_N).unwrap_or(60).clamp(5, 250));
    let min_cooc = use_state(|| read_local::<i64>(STORAGE_KEY_MIN_COOC).unwrap_or(3).max(1));
    let edges_per_tag =
        use_state(|| read_local::<usize>(STORAGE_KEY_EDGES_PER_TAG).unwrap_or(6).clamp(2, 20));
    let hover_idx: UseStateHandle<Option<usize>> = use_state(|| None);
    let isolated_community: UseStateHandle<Option<u32>> = use_state(|| None);

    let svg_ref = use_node_ref();
    let layout: Rc<RefCell<LayoutState>> = use_mut_ref(LayoutState::default);
    // Re-render every physics tick. The component itself reads `layout` via
    // `Rc<RefCell>` so we don't actually need the tick value — bumping it
    // is just a signal to repaint the SVG.
    let tick = use_state(|| 0u64);

    let view = use_state(ViewState::default);
    // Mirror of `view` for long-lived `Closure`s (wheel + window mousemove /
    // mouseup). `UseStateHandle<T>` deref returns the value captured at the
    // moment the handle was cloned — a closure created once at mount and
    // never re-created would otherwise always see the initial `ViewState`,
    // breaking accumulated zoom (each wheel tick would multiply from 1.0 ×
    // ZOOM_STEP) and breaking drag math at any non-default scale (world-x
    // = (vb-x − tx) / scale uses a stale tx/scale). Yew Callbacks don't
    // have this problem because they're rebuilt every render. Synchronised
    // both on every render (line below) and inside the writing handlers
    // themselves so consecutive events within the same microtask see the
    // freshest value.
    let view_ref: Rc<RefCell<ViewState>> = use_mut_ref(ViewState::default);
    *view_ref.borrow_mut() = *view;
    let pan: Rc<RefCell<PanState>> = use_mut_ref(PanState::default);
    let is_dragging = use_state(|| false);

    // -------- Fetch tag-relation graph ------------------------------------
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

                let url = format!(
                    "{}/account/{}/tag_relations?top={}&min_cooc={}",
                    api_base, user.id, top_n_val, min_cooc_val,
                );
                let payload = payload.clone();
                let loading = loading.clone();
                let error = error.clone();
                loading.set(true);
                error.set(None);

                wasm_bindgen_futures::spawn_local(async move {
                    match api_get(&url).send().await {
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

    // -------- Persist settings to localStorage ----------------------------
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

    // -------- Sync layout from payload ------------------------------------
    // Compares (graph identity, edges_per_tag) against the layout's stored
    // inputs. Resyncs and clears isolation when either changes; otherwise
    // leaves the running simulation untouched (so a hover/zoom doesn't
    // trigger a reseed).
    {
        let layout = layout.clone();
        let isolated_community = isolated_community.clone();
        let edges_per_tag_val = *edges_per_tag;
        use_effect_with(
            (payload.clone(), edges_per_tag_val),
            move |(payload, k_val)| {
                if let Some(graph) = payload.as_ref() {
                    let mut needs_sync = false;
                    {
                        let l = layout.borrow();
                        if l.built_for_node_count != graph.nodes.len()
                            || l.built_for_edges_per_tag != *k_val
                        {
                            needs_sync = true;
                        }
                    }
                    if needs_sync {
                        layout.borrow_mut().sync(graph, *k_val);
                        isolated_community.set(None);
                    }
                } else {
                    let mut l = layout.borrow_mut();
                    l.nodes.clear();
                    l.edges.clear();
                    l.communities.clear();
                    l.pinned = None;
                    l.built_for_node_count = 0;
                    l.built_for_edges_per_tag = 0;
                }
                || ()
            },
        );
    }

    // -------- Continuous physics tick -------------------------------------
    {
        let layout = layout.clone();
        let tick = tick.clone();
        use_effect_with((), move |_| {
            let interval = Interval::new(TICK_MS, move || {
                layout.borrow_mut().step();
                tick.set(*tick + 1);
            });
            move || drop(interval)
        });
    }

    // -------- Non-passive wheel listener for cursor-anchored zoom ---------
    // Yew's `onwheel` registers as passive in modern browsers, which
    // silently no-ops `preventDefault()`. Without prevent_default, scrolling
    // the wheel over the graph also scrolls the page — disorienting.
    {
        let svg_ref_for_effect = svg_ref.clone();
        let view = view.clone();
        let view_ref = view_ref.clone();
        let canvas_present = payload.is_some() || *loading;
        use_effect_with(canvas_present, move |_present| {
            let mut handle: Option<(web_sys::Element, Closure<dyn FnMut(WebWheelEvent)>)> = None;

            if let Some(svg) = svg_ref_for_effect.cast::<web_sys::Element>() {
                let svg_for_cb = svg.clone();
                let view = view.clone();
                let view_ref = view_ref.clone();
                let cb = Closure::<dyn FnMut(WebWheelEvent)>::new(move |evt: WebWheelEvent| {
                    evt.prevent_default();
                    let direction = if evt.delta_y() < 0.0 {
                        ZOOM_STEP
                    } else {
                        1.0 / ZOOM_STEP
                    };
                    // Read through the mirror — `*view` would be the stale
                    // snapshot captured when this effect ran, so cumulative
                    // zoom would reset every event. See the `view_ref`
                    // declaration for the full story.
                    let cur = *view_ref.borrow();
                    let new_scale = (cur.scale * direction).clamp(MIN_SCALE, MAX_SCALE);
                    if (new_scale - cur.scale).abs() < 1e-6 {
                        return;
                    }
                    let (vbx, vby) = client_to_viewbox(
                        &svg_for_cb,
                        evt.client_x() as f64,
                        evt.client_y() as f64,
                    );
                    let f_eff = new_scale / cur.scale;
                    // Anchor world-space point under the cursor: solving
                    // `vb = new_tx + new_scale * world` for new_tx given the
                    // pre-zoom mapping `world = (vb - tx) / scale`.
                    let new_tx = vbx - f_eff * (vbx - cur.tx);
                    let new_ty = vby - f_eff * (vby - cur.ty);
                    let next = ViewState {
                        tx: new_tx,
                        ty: new_ty,
                        scale: new_scale,
                    };
                    // Write through to the mirror immediately so that two
                    // wheel events in the same microtask compound correctly
                    // even before Yew can re-render and re-sync.
                    *view_ref.borrow_mut() = next;
                    view.set(next);
                });

                let opts = web_sys::AddEventListenerOptions::new();
                opts.set_passive(false);
                let _ = svg.add_event_listener_with_callback_and_add_event_listener_options(
                    "wheel",
                    cb.as_ref().unchecked_ref(),
                    &opts,
                );
                handle = Some((svg, cb));
            }

            move || {
                if let Some((svg, cb)) = handle {
                    let _ = svg.remove_event_listener_with_callback(
                        "wheel",
                        cb.as_ref().unchecked_ref(),
                    );
                    drop(cb);
                }
            }
        });
    }

    // -------- SVG-level pan handlers --------------------------------------
    let on_svg_mousedown = {
        let pan = pan.clone();
        let is_dragging = is_dragging.clone();
        Callback::from(move |e: WebMouseEvent| {
            if e.button() != 0 {
                return;
            }
            e.prevent_default();
            let mut p = pan.borrow_mut();
            p.active = true;
            p.last_client_x = e.client_x() as f64;
            p.last_client_y = e.client_y() as f64;
            if !*is_dragging {
                is_dragging.set(true);
            }
        })
    };

    // Window-level mousemove + mouseup so that drag (both node-pin and
    // scene-pan) follows the cursor even when it leaves the SVG bounds.
    // SVG-only handlers stop firing the moment the pointer crosses out of
    // the element, which previously cut off any drag that wandered off
    // the canvas — the node would snap back, the pan would freeze. Now
    // the user can throw a node anywhere on the page and release it
    // wherever; on release the pin clears regardless of cursor location.
    {
        let pan = pan.clone();
        let layout = layout.clone();
        let view = view.clone();
        let view_ref = view_ref.clone();
        let svg_ref = svg_ref.clone();
        let is_dragging = is_dragging.clone();
        let payload = payload.clone();
        use_effect_with((), move |_| {
            let Some(window) = web_sys::window() else {
                return Box::new(|| {}) as Box<dyn FnOnce()>;
            };

            let mousemove_cb = {
                let pan = pan.clone();
                let layout = layout.clone();
                let view = view.clone();
                let view_ref = view_ref.clone();
                let svg_ref = svg_ref.clone();
                Closure::<dyn FnMut(WebMouseEvent)>::new(move |e: WebMouseEvent| {
                    let Some(svg) = svg_ref.cast::<web_sys::Element>() else {
                        return;
                    };
                    // Node drag takes priority: its mousedown set
                    // `layout.pinned`, and every mousemove updates the
                    // pin's target to follow the cursor in world coords.
                    // Read via `view_ref` so the world-space inverse uses
                    // the current zoom/pan — `*view` here is the snapshot
                    // captured at mount and is permanently stale.
                    let pinned_active = layout.borrow().pinned.is_some();
                    if pinned_active {
                        let (vbx, vby) = client_to_viewbox(
                            &svg,
                            e.client_x() as f64,
                            e.client_y() as f64,
                        );
                        let v = *view_ref.borrow();
                        let world_x = (vbx - v.tx) / v.scale;
                        let world_y = (vby - v.ty) / v.scale;
                        let mut l = layout.borrow_mut();
                        if let Some(p) = l.pinned.as_mut() {
                            let dx = world_x - p.target_x;
                            let dy = world_y - p.target_y;
                            p.distance += (dx * dx + dy * dy).sqrt();
                            p.target_x = world_x;
                            p.target_y = world_y;
                        }
                        return;
                    }

                    let (dx_px, dy_px) = {
                        let mut p = pan.borrow_mut();
                        if !p.active {
                            return;
                        }
                        let dx = e.client_x() as f64 - p.last_client_x;
                        let dy = e.client_y() as f64 - p.last_client_y;
                        p.last_client_x = e.client_x() as f64;
                        p.last_client_y = e.client_y() as f64;
                        (dx, dy)
                    };
                    let k = px_to_viewbox_scale(&svg);
                    let mut v = *view_ref.borrow();
                    v.tx += dx_px * k;
                    v.ty += dy_px * k;
                    *view_ref.borrow_mut() = v;
                    view.set(v);
                })
            };

            let mouseup_cb = {
                let pan = pan.clone();
                let layout = layout.clone();
                let is_dragging = is_dragging.clone();
                let payload = payload.clone();
                Closure::<dyn FnMut(WebMouseEvent)>::new(move |e: WebMouseEvent| {
                    if e.button() != 0 {
                        return;
                    }
                    let was_panning = pan.borrow().active;
                    pan.borrow_mut().active = false;

                    // Click vs drag on the pinned node: small total
                    // distance opens the tag's e621 search in a new tab;
                    // otherwise the node is just released to physics.
                    let pinned = layout.borrow().pinned.clone();
                    if let Some(p) = pinned {
                        if p.distance < CLICK_DRAG_THRESHOLD {
                            if let (Some(graph), Some(cfg)) =
                                (payload.as_ref(), read_config_from_head())
                            {
                                if let Some(node) = graph.nodes.get(p.node_idx) {
                                    let url = format!(
                                        "{}/posts?tags={}",
                                        cfg.posts_domain,
                                        urlencoding::encode(&node.name),
                                    );
                                    if let Some(win) = web_sys::window() {
                                        let _ =
                                            win.open_with_url_and_target(&url, "_blank");
                                    }
                                }
                            }
                        }
                        layout.borrow_mut().pinned = None;
                    }
                    if was_panning || *is_dragging {
                        is_dragging.set(false);
                    }
                })
            };

            let _ = window.add_event_listener_with_callback(
                "mousemove",
                mousemove_cb.as_ref().unchecked_ref(),
            );
            let _ = window.add_event_listener_with_callback(
                "mouseup",
                mouseup_cb.as_ref().unchecked_ref(),
            );

            let window_for_cleanup = window;
            Box::new(move || {
                let _ = window_for_cleanup.remove_event_listener_with_callback(
                    "mousemove",
                    mousemove_cb.as_ref().unchecked_ref(),
                );
                let _ = window_for_cleanup.remove_event_listener_with_callback(
                    "mouseup",
                    mouseup_cb.as_ref().unchecked_ref(),
                );
                drop(mousemove_cb);
                drop(mouseup_cb);
            }) as Box<dyn FnOnce()>
        });
    }

    let on_svg_mouseleave = {
        let hover_idx = hover_idx.clone();
        Callback::from(move |_: WebMouseEvent| {
            // Pan/pin state is *not* cleared here on purpose — that's
            // what enables dragging outside the SVG bounds. Window-level
            // mouseup is the single source of truth for ending a drag.
            // Hover is local to the SVG (no <circle> outside it), so we
            // can safely drop it on leave.
            if hover_idx.is_some() {
                hover_idx.set(None);
            }
        })
    };

    let on_dblclick = {
        let view = view.clone();
        Callback::from(move |_: WebMouseEvent| {
            view.set(ViewState::default());
        })
    };

    // -------- Render preparation ------------------------------------------
    let l = layout.borrow();
    let _ = *tick; // depend on tick so the component re-renders each frame
    let view_now = *view;
    let cur_hover = *hover_idx;
    let cur_iso = *isolated_community;

    // Hover focus set: when hovering a node, soften everything not directly
    // connected to it. Community-isolation: only nodes/edges in that
    // community render at full opacity. Both are independent; isolation
    // wins if active.
    let hover_neighbour_set: Option<std::collections::HashSet<usize>> =
        if let Some(h) = cur_hover {
            let mut set = std::collections::HashSet::with_capacity(8);
            set.insert(h);
            for &(a, b, _) in &l.edges {
                if a == h {
                    set.insert(b);
                }
                if b == h {
                    set.insert(a);
                }
            }
            Some(set)
        } else {
            None
        };

    // Out-of-context nodes (other communities while one is isolated, or
    // non-neighbours of a hovered node) stay rendered — they fade to this
    // opacity instead of dropping from the DOM. Keeping them in view
    // preserves spatial orientation so the user can still see how the
    // focused subset fits into the whole graph.
    const INACTIVE_OPACITY: f64 = 0.18;

    let node_focus_full = |idx: usize| -> bool {
        // Full opacity if: no hover-or-isolation context OR this node is in
        // the active context. Hover takes precedence over isolation because
        // hovering is the more recent user gesture.
        match (&hover_neighbour_set, cur_iso) {
            (Some(set), _) => set.contains(&idx),
            (None, Some(c)) => l.communities.get(idx).copied() == Some(c),
            (None, None) => true,
        }
    };

    let span = (l.score_max - l.score_min).max(1e-6);
    let max_community = l.communities.iter().copied().max().unwrap_or(0);
    let mut community_size = vec![0u32; (max_community as usize) + 1];
    for &c in &l.communities {
        community_size[c as usize] += 1;
    }
    // Singleton communities fall back to a muted grey so isolated nodes
    // don't add saturated noise to the palette.
    let community_color = |c: u32| -> String {
        if (c as usize) < community_size.len() && community_size[c as usize] <= 1 {
            return "#6b7280".to_string();
        }
        let hue = ((c as f64) * 137.508_f64) % 360.0;
        format!("hsl({:.0}, 60%, 55%)", hue)
    };

    // -------- Build SVG layers (edges → circles → labels) -----------------

    // Edges always render — out-of-context ones get a low-alpha "inactive"
    // pass instead of being culled, so the user keeps spatial context
    // when isolating a community or hovering a node.
    let edges_html: Vec<Html> = l
        .edges
        .iter()
        .filter_map(|&(a, b, score)| {
            if a >= l.nodes.len() || b >= l.nodes.len() {
                return None;
            }
            let strength = ((score - l.score_min) / span).clamp(0.0, 1.0);
            let in_hover = cur_hover.map(|h| h == a || h == b).unwrap_or(false);
            let endpoints_in_focus = node_focus_full(a) && node_focus_full(b);
            let width = if in_hover {
                2.6
            } else {
                0.6 + strength * 2.2
            };
            let opacity = if in_hover {
                0.95
            } else {
                let base = 0.15 + strength * 0.55;
                if endpoints_in_focus {
                    base
                } else {
                    (base * INACTIVE_OPACITY).max(0.04)
                }
            };
            let stroke = if in_hover {
                "var(--bs-body-color)"
            } else {
                "var(--bs-secondary)"
            };
            let na = &l.nodes[a];
            let nb = &l.nodes[b];
            Some(html! {
                <line
                    x1={format!("{:.2}", na.x)} y1={format!("{:.2}", na.y)}
                    x2={format!("{:.2}", nb.x)} y2={format!("{:.2}", nb.y)}
                    stroke={stroke}
                    stroke-width={format!("{:.2}", width)}
                    stroke-opacity={format!("{:.3}", opacity)}
                    stroke-linecap="round"
                />
            })
        })
        .collect();

    let graph_nodes_ref = payload.as_ref();

    let mut circles_html: Vec<Html> = Vec::with_capacity(l.nodes.len());
    let mut labels_html: Vec<Html> = Vec::with_capacity(l.nodes.len());

    for (i, n) in l.nodes.iter().enumerate() {
        let community = l.communities.get(i).copied().unwrap_or(0);
        let color = community_color(community);
        let is_hover = cur_hover == Some(i);
        let in_focus = node_focus_full(i);
        // Inactive nodes stay rendered at a low opacity instead of being
        // pulled from the DOM — this preserves spatial context when a
        // community is isolated or a node is hovered.
        let opacity: f64 = if in_focus { 1.0 } else { INACTIVE_OPACITY };
        let stroke_width = if is_hover { 2.4 } else { 0.9 };

        let on_node_mousedown = {
            let layout = layout.clone();
            let svg_ref = svg_ref.clone();
            let view_handle = view.clone();
            let is_dragging = is_dragging.clone();
            Callback::from(move |e: WebMouseEvent| {
                if e.button() != 0 {
                    return;
                }
                e.stop_propagation();
                e.prevent_default();
                let Some(svg) = svg_ref.cast::<web_sys::Element>() else {
                    return;
                };
                let (vbx, vby) =
                    client_to_viewbox(&svg, e.client_x() as f64, e.client_y() as f64);
                // Yew Callback — recreated each render — captures the
                // current `view` snapshot, so `*view_handle` is fresh here.
                // World-space inverse must use the *current* tx/ty/scale
                // or a drag started at non-default zoom would land the
                // node nowhere near the cursor.
                let v = *view_handle;
                let world_x = (vbx - v.tx) / v.scale;
                let world_y = (vby - v.ty) / v.scale;
                layout.borrow_mut().pinned = Some(PinnedDrag {
                    node_idx: i,
                    target_x: world_x,
                    target_y: world_y,
                    distance: 0.0,
                });
                if !*is_dragging {
                    is_dragging.set(true);
                }
            })
        };

        let on_node_mouseenter = {
            let hover_idx = hover_idx.clone();
            Callback::from(move |_: WebMouseEvent| {
                if *hover_idx != Some(i) {
                    hover_idx.set(Some(i));
                }
            })
        };

        let on_node_mouseleave = {
            let hover_idx = hover_idx.clone();
            Callback::from(move |_: WebMouseEvent| {
                if *hover_idx == Some(i) {
                    hover_idx.set(None);
                }
            })
        };

        // Halo for the currently-hovered circle so a mouse pointer over a
        // dense cluster has obvious feedback.
        let halo = if is_hover {
            Some(html! {
                <circle
                    cx={format!("{:.2}", n.x)} cy={format!("{:.2}", n.y)}
                    r={format!("{:.2}", n.radius + 6.0)}
                    fill="none"
                    stroke="var(--bs-body-color)"
                    stroke-width="1.4"
                    stroke-opacity="0.55"
                />
            })
        } else {
            None
        };

        let tag_label_for_title = graph_nodes_ref
            .and_then(|g| g.nodes.get(i))
            .map(|node| format!("{} · {} · {}×", node.name, node.group_type, node.count))
            .unwrap_or_default();

        circles_html.push(html! {
            <g key={i}>
                { halo }
                <circle
                    cx={format!("{:.2}", n.x)} cy={format!("{:.2}", n.y)}
                    r={format!("{:.2}", n.radius)}
                    fill={color.clone()}
                    fill-opacity={format!("{:.3}", opacity)}
                    stroke="var(--bs-body-color)"
                    stroke-width={format!("{:.2}", stroke_width)}
                    stroke-opacity={format!("{:.3}", opacity)}
                    style="cursor: pointer;"
                    onmousedown={on_node_mousedown}
                    onmouseenter={on_node_mouseenter}
                    onmouseleave={on_node_mouseleave}
                >
                    <title>{ tag_label_for_title }</title>
                </circle>
            </g>
        });

        // Every node gets a persistent label. The previous top-K gate left
        // many tags anonymous, and uniform radii removed the radius-based
        // signal that originally selected which tags were "important
        // enough" to label. Labels are rendered into a separate `Vec` and
        // emitted last in the SVG so they sit on top of every edge and
        // circle; a paint-order stroke + body-bg colour gives each glyph
        // a halo that keeps it legible over coloured nodes.
        let Some(node_data) = graph_nodes_ref.and_then(|g| g.nodes.get(i)) else {
            continue;
        };
        let (label_anchor, label_dx) = if n.x >= CENTER_X {
            ("start", n.radius + 4.0)
        } else {
            ("end", -(n.radius + 4.0))
        };
        let label_opacity: f64 = if is_hover {
            1.0
        } else if in_focus {
            0.92
        } else {
            INACTIVE_OPACITY
        };
        let weight = if is_hover { "700" } else { "500" };
        let trimmed = trim_label(&node_data.name);
        labels_html.push(html! {
            <text
                x={format!("{:.2}", n.x + label_dx)}
                y={format!("{:.2}", n.y + 4.0)}
                text-anchor={label_anchor}
                paint-order="stroke fill"
                stroke="var(--bs-body-bg)"
                stroke-width="3.2"
                stroke-linejoin="round"
                stroke-opacity={format!("{:.3}", label_opacity)}
                fill="var(--bs-body-color)"
                fill-opacity={format!("{:.3}", label_opacity)}
                font-size="11"
                font-weight={weight}
                style="pointer-events: none; user-select: none;"
            >
                { trimmed }
            </text>
        });
    }

    drop(l);

    // -------- Toolbar callbacks (controls + zoom) -------------------------
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

    let on_zoom_in = {
        let view = view.clone();
        Callback::from(move |_: WebMouseEvent| {
            let cur = *view;
            let new_scale = (cur.scale * ZOOM_STEP).clamp(MIN_SCALE, MAX_SCALE);
            if (new_scale - cur.scale).abs() < 1e-6 {
                return;
            }
            let f_eff = new_scale / cur.scale;
            let cx = CENTER_X;
            let cy = CENTER_Y;
            view.set(ViewState {
                tx: cx - f_eff * (cx - cur.tx),
                ty: cy - f_eff * (cy - cur.ty),
                scale: new_scale,
            });
        })
    };
    let on_zoom_out = {
        let view = view.clone();
        Callback::from(move |_: WebMouseEvent| {
            let cur = *view;
            let new_scale = (cur.scale / ZOOM_STEP).clamp(MIN_SCALE, MAX_SCALE);
            if (new_scale - cur.scale).abs() < 1e-6 {
                return;
            }
            let f_eff = new_scale / cur.scale;
            let cx = CENTER_X;
            let cy = CENTER_Y;
            view.set(ViewState {
                tx: cx - f_eff * (cx - cur.tx),
                ty: cy - f_eff * (cy - cur.ty),
                scale: new_scale,
            });
        })
    };
    let on_zoom_reset = {
        let view = view.clone();
        Callback::from(move |_: WebMouseEvent| view.set(ViewState::default()))
    };

    // -------- Hover summary + community legend ----------------------------
    let hover_summary = cur_hover.and_then(|idx| {
        payload
            .as_ref()
            .and_then(|graph| graph.nodes.get(idx).map(|n| (idx, n.clone())))
    });

    let community_legend_data = community_summary(&payload, &layout);
    let legend_html: Html = if community_legend_data.is_empty() {
        html! {}
    } else {
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
        let cursor = if *is_dragging { "grabbing" } else { "grab" };
        let transform = format!(
            "translate({:.2} {:.2}) scale({:.4})",
            view_now.tx, view_now.ty, view_now.scale
        );
        let view_dirty = view_now.tx != 0.0
            || view_now.ty != 0.0
            || (view_now.scale - 1.0).abs() > 1e-6;
        let svg_style = format!(
            "display: block; width: 100%; height: 100%; touch-action: none; user-select: none; cursor: {};",
            cursor
        );
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
                            <button type="button" class="btn btn-outline-secondary" title="Reset view (or double-click the graph)" onclick={on_zoom_reset.clone()}>{ format!("{:.0}%", view_now.scale * 100.0) }</button>
                            <button type="button" class="btn btn-outline-secondary" title="Zoom in" onclick={on_zoom_in}>{"+"}</button>
                        </div>
                    </div>
                    if *loading {
                        <div class="col-auto">
                            <span class="spinner-border spinner-border-sm text-primary" role="status"/>
                        </div>
                    }
                </div>
                <div class="position-relative" style="aspect-ratio: 16 / 9; min-height: 460px; max-height: 75vh; user-select: none;">
                    <svg
                        ref={svg_ref.clone()}
                        viewBox={format!("0 0 {VIEWBOX_W} {VIEWBOX_H}")}
                        preserveAspectRatio="xMidYMid meet"
                        style={svg_style}
                        onmousedown={on_svg_mousedown}
                        onmouseleave={on_svg_mouseleave}
                        ondblclick={on_dblclick}
                    >
                        <g transform={transform}>
                            { for edges_html }
                            { for circles_html }
                            { for labels_html }
                        </g>
                    </svg>
                    if let Some((_, node)) = hover_summary.clone() {
                        <div class="position-absolute top-0 end-0 m-2 px-2 py-1 small rounded shadow-sm bg-body border" style="pointer-events: none;">
                            <strong>{ node.name.clone() }</strong>
                            { format!(" · {} · {}×", node.group_type, node.count) }
                        </div>
                    }
                    if view_dirty {
                        <button type="button" class="btn btn-sm btn-outline-secondary position-absolute bottom-0 start-0 m-2" onclick={on_zoom_reset} title={format!("zoom {:.2}× — click to reset", view_now.scale)}>
                            { format!("⟲ reset ({:.1}×)", view_now.scale) }
                        </button>
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
                <p class="small text-muted mt-1 mb-0">{"Drag a node to reshape its neighbourhood · click a node to open its e621 search · drag empty space to pan · scroll to zoom · click a community below to isolate it."}</p>
                { legend_html }
            </>
        }
    };

    html! {
        <div class="card mt-4">
            <div class="card-header bg-primary text-white d-flex justify-content-between align-items-center">
                <h5 class="mb-0">{"Tag Relation Graph"}</h5>
                <small class="opacity-75">{"hover a node for details · drag to rearrange · colour = community"}</small>
            </div>
            <div class="card-body">
                { body }
            </div>
        </div>
    }
}

// =================== Tag-specific business logic (preserved) ==============
//
// The backbone selector and community detector below are kept verbatim from
// the previous implementation — they produce the meaningful structure that
// the SVG renderer above visualises. Touch carefully: the score function
// matches the server-side PMI formulation in `topology-contract`.

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

fn pair_pmi_score(
    c: i64,
    m_a: f64,
    m_b: f64,
    n: f64,
    min_cooc: i64,
    scale: f64,
    cooc_ref: f64,
) -> f64 {
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
        let _ = nc; // n_catalog captured via global_lift; param kept for parity.
        raw_pmi * conf
    } else {
        0.0
    };

    let w_g = scoring.w_global.max(0.0) as f64;
    let w_u = scoring.w_personal.max(0.0) as f64;
    let sum = (w_g + w_u).max(1e-6);
    (w_g * global_score + w_u * user_score) / sum
}

fn select_backbone(
    graph: &TagRelationGraphPayload,
    k: usize,
) -> Vec<(usize, usize, usize, f64)> {
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

    // Phase 1: Kruskal MST over decreasing score — a maximum spanning tree.
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

#[inline]
fn node_radius(n: usize) -> f64 {
    // Uniform radius for every node in the graph — count-based sizing was
    // dropped per UX request ("кружочки одного размера"). Smoothstep
    // shrinks the value as density grows so a 250-tag layout doesn't
    // stuff oversized circles into a tight viewport. n=40 → 11, n=250 → 6.5.
    let t = ((n as f64 - 40.0) / (250.0 - 40.0)).clamp(0.0, 1.0);
    let s = t * t * (3.0 - 2.0 * t);
    11.0 + (6.5 - 11.0) * s
}

/// Label Propagation Algorithm: each node iteratively adopts the label of
/// the neighbour community with the largest summed edge weight. Converges
/// in <15 iterations for typical tag graphs and produces compact, contiguous
/// community ids ready for a colour palette lookup.
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

    let unique: BTreeSet<u32> = labels.iter().copied().collect();
    let map: HashMap<u32, u32> = unique
        .iter()
        .enumerate()
        .map(|(i, l)| (*l, i as u32))
        .collect();
    labels.iter_mut().for_each(|l| *l = *map.get(l).unwrap_or(l));
    labels
}

fn trim_label(s: &str) -> String {
    let count = s.chars().count();
    if count > 24 {
        let mut iter = s.chars();
        let truncated: String = iter.by_ref().take(23).collect();
        format!("{truncated}…")
    } else {
        s.to_string()
    }
}
