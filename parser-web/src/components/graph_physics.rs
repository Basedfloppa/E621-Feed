//! Physics simulation for the force-directed tag-relation graph.
//!
//! Contains all constants, the [`LayoutState`] with Verlet integration,
//! [`PhysNode`], [`PinnedDrag`], and coordinate helpers.
//! Yew component / rendering live in [`super::tag_relation_graph_card`].

// =================== Coordinate space & physics tuning ====================

/// SVG `viewBox` width / height. Coordinate space is fixed at 1600×900 so
/// physics constants don't have to be retuned when the container resizes —
/// `preserveAspectRatio="xMidYMid meet"` scales the canvas to fit whatever
/// CSS sizes the SVG to. 16:9 aspect matches the typical card width better
/// than a square viewBox would.
pub const VIEWBOX_W: f64 = 1600.0;
pub const VIEWBOX_H: f64 = 900.0;
pub const CENTER_X: f64 = VIEWBOX_W / 2.0;
pub const CENTER_Y: f64 = VIEWBOX_H / 2.0;

/// Animation tick. 33 ms ≈ 30 FPS — same as freenet's default.
pub const TICK_MS: u32 = 33;
/// Inverse-square repulsion coefficient between every pair of nodes.
pub const K_REPEL: f64 = 3600.0;
/// Edge spring stiffness.
pub const K_EDGE: f64 = 0.014;
/// Natural resting length of every edge, in viewBox units.
pub const EDGE_REST_LENGTH: f64 = 110.0;
/// Linear pull toward the canvas centre.
pub const K_GRAVITY: f64 = 0.005;
/// Velocity damping per tick (0..1; closer to 1 = more inertia).
pub const DAMPING: f64 = 0.85;
/// Velocity cap so a runaway node can't fly across the canvas in one frame.
pub const MAX_SPEED: f64 = 22.0;
/// Higher cap during the warmup window so freshly-seeded clusters reach
/// their resting position quickly without visible churn.
pub const WARMUP_MAX_SPEED: f64 = 60.0;
/// Ticks of warmup-speed after a topology sync. Roughly 8 s at 30 FPS.
pub const WARMUP_TICKS: u32 = 240;
/// Floor on inter-node distance for the repulsion calculation. Prevents
/// `K_REPEL / d²` from blowing up when two nodes momentarily overlap.
pub const REPEL_MIN_DIST: f64 = 14.0;
/// Hard padding enforced *after* force integration: every pair is pushed
/// apart so their centres are at least `r_i + r_j + COLLISION_PAD` apart.
/// Repulsion alone can lose to combined spring/gravity forces in dense
/// hubs — this guarantees circles never visibly touch.
pub const COLLISION_PAD: f64 = 4.0;
/// Synchronous physics iterations run inside `sync()` before the first
/// visible frame. Combined with the community-aware seed below, this
/// lands the graph on a near-final layout the moment data arrives — no
/// visible "settling" churn on top of the user's first sight of the graph.
pub const PRESETTLE_STEPS: usize = 60;

pub const ZOOM_STEP: f64 = 1.15;
pub const MIN_SCALE: f64 = 0.1;
pub const MAX_SCALE: f64 = 2.0;

/// Click-vs-drag threshold (viewBox units of cumulative cursor motion).
/// Below this, mouseup on a held node is treated as a click → open e621
/// search for that tag instead of just releasing the pin.
pub const CLICK_DRAG_THRESHOLD: f64 = 6.0;

// =================== Physics types ==========================================

/// Active node-drag. While `Some`, the physics step pins the named node to
/// `(target_x, target_y)` and zeroes its velocity; surrounding nodes still
/// react to it via their normal repulsion/spring forces.
#[derive(Clone, Debug)]
pub struct PinnedDrag {
    pub node_idx: usize,
    pub target_x: f64,
    pub target_y: f64,
    /// Accumulated cursor distance since mousedown. Used to disambiguate
    /// click (small) from drag (large) on mouseup.
    pub distance: f64,
}

pub struct PhysNode {
    pub x: f64,
    pub y: f64,
    pub vx: f64,
    pub vy: f64,
    /// Visual radius in viewBox units. Driven by `node.count` at sync time.
    pub radius: f64,
}

#[derive(Default)]
pub struct LayoutState {
    pub nodes: Vec<PhysNode>,
    /// Backbone: `(source_idx, target_idx, score)`. Built by `select_backbone`
    /// = MST ∪ per-node top-K. Both physics and rendering use only these.
    pub edges: Vec<(usize, usize, f64)>,
    /// Per-node community label, compact `[0, k)` ints. Drives fill hue.
    pub communities: Vec<u32>,
    /// Cached edge-score range so callers don't recompute on each render.
    pub score_min: f64,
    pub score_max: f64,
    /// Currently-dragged node, if any.
    pub pinned: Option<PinnedDrag>,
    /// Ticks since the last `sync()`. Gates the warmup-speed window.
    pub ticks_since_sync: u32,
    /// Inputs the current layout was built from. Compared on every effect
    /// re-run to decide whether to resync.
    pub built_for_node_count: usize,
    pub built_for_edges_per_tag: usize,
}

impl LayoutState {
    /// Run one physics tick: compute forces (gravity, repulsion, edge
    /// springs), apply Verlet integration with velocity damping, snap the
    /// pinned node, and resolve hard collisions.
    pub fn step(&mut self) {
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

        for (i, &(fx, fy)) in forces.iter().enumerate() {
            if Some(i) == pinned_idx {
                continue;
            }

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
        if let Some(p) = &self.pinned
            && let Some(node) = self.nodes.get_mut(p.node_idx) {
                node.x = p.target_x;
                node.y = p.target_y;
                node.vx = 0.0;
                node.vy = 0.0;
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
pub fn client_to_viewbox(svg: &web_sys::Element, client_x: f64, client_y: f64) -> (f64, f64) {
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
pub fn px_to_viewbox_scale(svg: &web_sys::Element) -> f64 {
    let rect = svg.get_bounding_client_rect();
    let bw = rect.width();
    let bh = rect.height();
    if bw <= 0.0 || bh <= 0.0 {
        return 1.0;
    }
    let scale = (bw / VIEWBOX_W).min(bh / VIEWBOX_H);
    if scale <= 0.0 { 1.0 } else { 1.0 / scale }
}

#[inline]
pub fn node_radius(n: usize) -> f64 {
    // Uniform radius for every node in the graph — count-based sizing was
    // dropped per UX request ("кружочки одного размера"). Smoothstep
    // shrinks the value as density grows so a 250-tag layout doesn't
    // stuff oversized circles into a tight viewport. n=40 → 11, n=250 → 6.5.
    let t = ((n as f64 - 40.0) / (250.0 - 40.0)).clamp(0.0, 1.0);
    let s = t * t * (3.0 - 2.0 * t);
    11.0 + (6.5 - 11.0) * s
}
