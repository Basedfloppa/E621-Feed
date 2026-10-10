//! Analytic fit of the twelve `mix_*` blend weights (backlog TODO §2.7).
//!
//! The grid's coordinate search moves one knob at a time over a fixed probe
//! matrix, so it cannot exploit the structure of the blend: the twelve `mix_*`
//! weights enter the score **linearly**. After one full scoring pass the
//! calibrate [`ScoreCache`] holds the per-candidate channel matrix, and the
//! weights can be solved from that matrix directly — with no further scoring.
//!
//! ## Objective
//!
//! Per account, with candidates `i` and `u_i = blend(w, channels_i)`:
//!
//! ```text
//! L(w) = Σ_accounts Σ_{p : p < n_test} [ u_p − logsumexp_i(u_i) ]
//! ```
//!
//! a listwise softmax / Plackett–Luce surrogate: it maximises the likelihood
//! of the favourites under a softmax over the account's candidates. The
//! argument for global optimality is concavity — `u` is linear in `w` and
//! `logsumexp` is convex, so the composition is concave — and the only
//! non-linearities are the monotone `clamp(0,1)` and the logistic sharpening,
//! both of which preserve the ordering and are handled by the (sub)gradients
//! below. `w` lives on the probability simplex `w ≥ 0, Σw = 1`, which is
//! exactly what `MixWeights::from_priors` already imposes on the priors, so it
//! is not an extra modelling assumption.
//!
//! The simplex also kills the scale degeneracy: before the clamp, multiplying
//! all twelve weights by a constant changes nothing — which is why the grid's
//! *absolute* probe offsets were measured in a meaningless unit (`mix_quality
//! = 0.017` and `mix_sim = 0.603` differ by 35×, and no single step size
//! suits both ends).
//!
//! ## Solver
//!
//! Exponentiated gradient / mirror descent on the *two-sided* simplex: the
//! weights are split into `w = plus - minus`, both halves updated as
//! `h_c ← h_c·exp(±η·g_c)` and renormalised. Positivity of each half holds by
//! construction, so a weight of either sign is reachable; normalisation is
//! free, and no new dependency is needed. `η` is normalised by `‖g‖∞` (so the
//! update is scale-free across iterations) and halved until the objective
//! strictly improves — a backtracking line search that makes the iteration
//! **monotone**, which the solver test asserts and which no global Lipschitz
//! constant is needed for.
//!
//! ## Identifiability
//!
//! A channel whose value is constant across all candidates *within every
//! account* carries no per-candidate information: within an account it can only
//! shift every score by a common amount, and the ranking is monotone in that
//! shift. Its gradient is nevertheless **not literally zero** once the
//! `clamp(0,1)` is active: a common shift moves different candidates onto or
//! off the plateau, so `∂u_i/∂w_c` differs per candidate and the
//! `n_test·K − n_test·K` cancellation of the linear case no longer holds. That
//! residual component measures the *shape* of the score distribution, not any
//! association between the channel and the favourites — it is the direction
//! that sharpens or flattens the softmax, and every account contributes the
//! same kind of term.
//!
//! The solver therefore **projects it out** (zeroes that gradient component).
//! Without the projection a channel the data cannot rank would be inflated and
//! reported as though it had been shown to matter, which is the misreport this
//! section exists to prevent. The exact re-score is unaffected: it always runs
//! on the full blend, so if such a direction did carry ranking value the gate
//! would still be the arbiter.
//!
//! Such channels are listed separately — "the data cannot rank this" is a
//! different statement from "this is unimportant". `mix_interaction` is the
//! expected instance offline: the synthetic split has no `feed_interactions`
//! rows, so the channel (and its veto) are constant zero.
//!
//! ## Acceptance
//!
//! The surrogate is not NDCG@20, so the fitted vector is only a *candidate*.
//! It is re-scored exactly from the cache through the production
//! [`blend_score`] and judged by [`crate::grid::gate_probe`] — the same paired
//! 1-sided 95% t-test a grid probe must pass.

use std::fs;
use std::path::Path;
use std::time::Instant;

use e621_account_parser_api::models::cfg;
use e621_account_parser_api::utils::{
    BLEND_CHANNEL_COUNT, BLEND_CHANNEL_NAMES, Priors, blend_score, normalised_mix_weights,
};

use crate::cache::{ChannelScores, ScoreCache};
use crate::grid::gate_probe;
use crate::metrics::{Metrics, mrr_pub, ndcg_at_k_pub, recall_at_k_pub};

/// A weight vector in [`BLEND_CHANNEL_NAMES`] order.
type Weights = [f32; BLEND_CHANNEL_COUNT];
/// The objective's gradient; f64 because it accumulates over ~10^5 candidates
/// and the per-candidate terms differ in the bits that decide a weight.
type Grad = [f64; BLEND_CHANNEL_COUNT];

/// Smallest value either half of the two-sided simplex may take.
///
/// Exponentiated-gradient updates are multiplicative, so an exact zero is an
/// absorbing state: a half starting at 0.0 can never grow back, even when the
/// gradient says it should. That applies to `minus` as much as to `plus`, which
/// is why both are floored — otherwise no channel could ever acquire a negative
/// weight, and the model now permits them.
///
/// The floor is bounded from *below* by the arithmetic, not by taste. The
/// objective is evaluated through the production `f32` blend, and a step out of
/// the floor moves a weight by about `floor·(e − 1/e) ≈ 2.35·floor`, so the
/// blend moves by roughly that times a channel value in `[0, 1]`. One ULP at a
/// blend of 0.3 is already ~3e-8: a floor of 1e-12 — the value the one-sided
/// solver used — produces a perturbation the objective literally cannot
/// represent, which is why the first version of this fix still left a channel
/// pinned at zero. 1e-6 clears that resolution by ~30× while contributing at
/// most 1e-6 to a blend whose output is clamped to `[0, 1]`.
const WEIGHT_FLOOR: f32 = 1e-6;

/// Iteration budget. Each iteration is one gradient pass plus the accepted
/// objective evaluation.
///
/// Raising this from 300 did **not** change the outcome on the real cache: with
/// 2000 available the solve still stopped after 300 accepted steps, because the
/// line search could no longer find an improving one while `|∇L|∞` sat at
/// ~6.8e3. A large gradient next to a failed step is the signature of the
/// `clamp(0, 1)` kink, not of an exhausted budget — the objective is only
/// piecewise smooth. The larger budget is therefore headroom, not a fix;
/// [`FitOutcome::budget_exhausted`] is what reports when it actually binds.
const MAX_ITERS: usize = 2000;
/// Initial log-step. 1.0 moves the largest coordinate by a factor of e, which
/// the line search shrinks immediately if it overshoots.
const ETA0: f64 = 1.0;
/// Maximum halvings per iteration before declaring convergence.
const MAX_BACKTRACK: u32 = 40;
const MIN_ETA: f64 = 1e-9;
/// Relative improvement below which the iteration stops. Well above f64's
/// resolution at the objective's magnitude (~1e5) and well below any change
/// that moves a ranking.
const REL_TOL: f64 = 1e-12;

/// Result of one fit, including enough history for the report and the tests.
pub(crate) struct FitOutcome {
    pub(crate) weights: Weights,
    /// Objective after the last accepted step.
    pub(crate) objective: f64,
    /// Objective at the initial (incumbent-derived) weights.
    pub(crate) initial_objective: f64,
    /// One entry per accepted step; monotonically non-decreasing by
    /// construction (steps are only accepted on strict improvement).
    pub(crate) history: Vec<f64>,
    /// Channels constant within every account; zero gradient, not fitted.
    pub(crate) unidentifiable: Vec<usize>,
    /// `max_c |∂L/∂w_c|` at the returned weights.
    pub(crate) gradient_inf: f64,
    pub(crate) iterations: usize,
    /// True when the loop stopped on [`MAX_ITERS`] rather than because the line
    /// search could no longer improve. A budget-limited vector is not an
    /// optimum, and the report says so instead of presenting it as one.
    pub(crate) budget_exhausted: bool,
}

// ---- scoring path ----------------------------------------------------------

/// The fit's per-candidate score: **the production blend**, not a copy.
///
/// `rescore` and the objective both go through here, and `blend_score` is the
/// same function `ScoringContext::final_blend` calls (the anti-drift test in
/// this module pins that), so a fitted vector means the same thing offline as
/// it does in production.
#[inline]
fn candidate_score(w: &Weights, ch: &ChannelScores, temp: f32, penalty: f32) -> f32 {
    blend_score(*w, ch.values(), temp, penalty, ch.veto)
}

/// `∂u_i/∂w_c` for one candidate, in f64.
///
/// The chain through the blend's monotone pieces:
///
/// * `clamp(0,1)` on `raw` — subgradient **1 strictly inside** the interval and
///   **0 where clamped**. Zero is the right value where the clamp is active:
///   `raw ≥ 1` is insensitive to every weight (the score sits at the plateau),
///   and `raw ≤ 0` is insensitive to a decrease, so reporting a non-zero
///   derivative would push the solver along a direction the objective cannot
///   reward. The clamp *is* active on real data, because the channels are not
///   themselves bounded to `[0,1]`.
/// * `sigmoid((x−0.5)·T)` — derivative `T·σ·(1−σ)`, skipped when the
///   temperature is the no-op default (`≤ 1e-3`).
/// * the veto factor `1 − penalty` — a positive constant multiplier.
#[inline]
fn candidate_grad(w: &Weights, ch: &ChannelScores, temp: f32, penalty: f32) -> Grad {
    let s = ch.values();
    let mut raw = 0.0f64;
    for (wc, sc) in w.iter().zip(s.iter()) {
        raw += f64::from(*wc) * f64::from(*sc);
    }
    let mut d = if raw > 0.0 && raw < 1.0 { 1.0 } else { 0.0 };
    if d > 0.0 && temp > 1e-3 {
        let t = f64::from(temp);
        let z = (raw - 0.5) * t;
        let sig = 1.0 / (1.0 + (-z).exp());
        d *= t * sig * (1.0 - sig);
    }
    if ch.veto {
        d *= 1.0 - f64::from(penalty.clamp(0.0, 1.0));
    }
    let mut g = [0.0f64; BLEND_CHANNEL_COUNT];
    for (gc, sc) in g.iter_mut().zip(s.iter()) {
        *gc = d * f64::from(*sc);
    }
    g
}

/// Score one account's candidates into `u` (reused across accounts) and return
/// `(Σ_{p<n_test} u_p, max u, Σ exp(u − max))`.
///
/// Non-finite scores are folded to 0: a NaN channel would otherwise poison the
/// entire `logsumexp` (and with it every weight) from a single bad candidate.
/// The exact re-score and the gate still see the real value, so a cache that
/// produces NaN is rejected there rather than silently optimised.
fn fill_account_scores(
    acc: &crate::cache::AccountChannelCache,
    w: &Weights,
    temp: f32,
    penalty: f32,
    u: &mut Vec<f32>,
) -> (f64, f64, f64) {
    let mut pos_sum = 0.0f64;
    let mut max = f64::NEG_INFINITY;
    for (i, ch) in acc.channels.iter().enumerate() {
        let v = candidate_score(w, ch, temp, penalty);
        let v = if v.is_finite() { v } else { 0.0 };
        u.push(v);
        if i < acc.n_test {
            pos_sum += f64::from(v);
        }
        if f64::from(v) > max {
            max = f64::from(v);
        }
    }
    let mut sum_exp = 0.0f64;
    for &v in u.iter() {
        sum_exp += (f64::from(v) - max).exp();
    }
    (pos_sum, max, sum_exp)
}

/// `L(w)` — see the module docs.
pub(crate) fn objective(cache: &ScoreCache, w: &Weights, temp: f32, penalty: f32) -> f64 {
    let mut obj = 0.0f64;
    let mut u: Vec<f32> = Vec::new();
    for acc in &cache.accounts {
        if acc.n_test == 0 || acc.channels.is_empty() {
            continue;
        }
        u.clear();
        let (pos_sum, max, sum_exp) = fill_account_scores(acc, w, temp, penalty, &mut u);
        obj += pos_sum - acc.n_test as f64 * (max + sum_exp.ln());
    }
    obj
}

/// `L(w)` and `∂L/∂w`, with the unidentifiable components projected out.
///
/// `∂L/∂w_c = Σ_p ∂u_p/∂w_c − n_test · Σ_i softmax_i · ∂u_i/∂w_c`; the first
/// term says "raise the favourites", the second "lower everything the current
/// weights rank highly", and their balance is what makes a channel important
/// only when it *discriminates*. Channels listed in `unidentifiable` have their
/// component zeroed — see the module docs for why the raw component is not
/// zero there and why it is not evidence anyway.
fn objective_and_grad(
    cache: &ScoreCache,
    w: &Weights,
    temp: f32,
    penalty: f32,
    unidentifiable: &[usize],
) -> (f64, Grad) {
    let mut obj = 0.0f64;
    let mut grad = [0.0f64; BLEND_CHANNEL_COUNT];
    let mut u: Vec<f32> = Vec::new();
    for acc in &cache.accounts {
        if acc.n_test == 0 || acc.channels.is_empty() {
            continue;
        }
        u.clear();
        let (pos_sum, max, sum_exp) = fill_account_scores(acc, w, temp, penalty, &mut u);
        obj += pos_sum - acc.n_test as f64 * (max + sum_exp.ln());

        let mut pos_grad = [0.0f64; BLEND_CHANNEL_COUNT];
        let mut mix_grad = [0.0f64; BLEND_CHANNEL_COUNT];
        for (i, ch) in acc.channels.iter().enumerate() {
            let weight = (f64::from(u[i]) - max).exp() / sum_exp;
            let g = candidate_grad(w, ch, temp, penalty);
            for c in 0..BLEND_CHANNEL_COUNT {
                mix_grad[c] += weight * g[c];
                if i < acc.n_test {
                    pos_grad[c] += g[c];
                }
            }
        }
        for c in 0..BLEND_CHANNEL_COUNT {
            grad[c] += pos_grad[c] - acc.n_test as f64 * mix_grad[c];
        }
    }
    for &c in unidentifiable {
        grad[c] = 0.0;
    }
    (obj, grad)
}

// ---- two-sided simplex -----------------------------------------------------

/// The weights split into their positive and negative parts: `w = plus - minus`.
///
/// Blend weights may be negative — a channel can carry anti-signal — and
/// exponentiated gradient needs a positive orthant, so the solver runs on the
/// *two-sided* simplex `{plus, minus ≥ 0, Σ(plus + minus) = 1}` and joins the
/// halves to score. From a non-negative start `minus` is pinned at the floor by
/// its own update, so the search degenerates to the plain simplex: the
/// generalisation costs nothing until a weight actually wants to go negative.
struct Parts {
    plus: [f64; BLEND_CHANNEL_COUNT],
    minus: [f64; BLEND_CHANNEL_COUNT],
}

impl Parts {
    /// Join back to the weights the blend actually consumes.
    fn joined(&self) -> Weights {
        let mut w = [0.0f32; BLEND_CHANNEL_COUNT];
        for (o, (a, b)) in w.iter_mut().zip(self.plus.iter().zip(self.minus.iter())) {
            *o = (a - b) as f32;
        }
        w
    }

    /// Project onto `{plus, minus ≥ WEIGHT_FLOOR, Σ(plus + minus) = 1}`.
    ///
    /// Both halves are floored, not only the positive one: a half pinned at
    /// exact zero is an absorbing state for a multiplicative update, so a
    /// channel starting at zero could never acquire a negative part — which is
    /// the direction the data has been asking these channels for.
    fn renormalise(&mut self) {
        let floor = f64::from(WEIGHT_FLOOR);
        let mut sum = 0.0f64;
        for c in 0..BLEND_CHANNEL_COUNT {
            self.plus[c] = self.plus[c].max(floor);
            self.minus[c] = self.minus[c].max(floor);
            sum += self.plus[c] + self.minus[c];
        }
        // Flooring both halves makes `sum ≥ 2·N·floor > 0`, so there is no
        // degenerate branch to guard here.
        for c in 0..BLEND_CHANNEL_COUNT {
            self.plus[c] = (self.plus[c] / sum).max(floor);
            self.minus[c] = (self.minus[c] / sum).max(floor);
        }
    }
}

/// Solver start: the incumbent weights.
///
/// Warm-starting from the incumbent (rather than the simplex centre) makes the
/// reported improvement directly comparable to the shipped priors.
/// `normalised_mix_weights` already returns an L1-normalised vector — possibly
/// with negative components — so the only case needing a substitute is the
/// all-zero block, which carries no direction at all.
fn initial_weights(p: &Priors) -> Weights {
    let w = normalised_mix_weights(p);
    if w.iter().all(|&x| x == 0.0) {
        return [1.0 / BLEND_CHANNEL_COUNT as f32; BLEND_CHANNEL_COUNT];
    }
    w
}

/// Split weights onto the two-sided simplex.
fn split(w: &Weights) -> Parts {
    let mut parts = Parts {
        plus: [0.0; BLEND_CHANNEL_COUNT],
        minus: [0.0; BLEND_CHANNEL_COUNT],
    };
    for (c, &x) in w.iter().enumerate() {
        let x = f64::from(x);
        parts.plus[c] = x.max(0.0);
        parts.minus[c] = (-x).max(0.0);
    }
    parts.renormalise();
    parts
}

/// One exponentiated-gradient step on the two-sided simplex.
///
/// The objective's gradient in `w_c` is `+g_c` in `plus_c` and `-g_c` in
/// `minus_c`, so one exponent drives the two halves in opposite directions.
fn eg_step(parts: &Parts, g: &Grad, grad_inf: f64, eta: f64) -> Parts {
    let mut next = Parts {
        plus: [0.0; BLEND_CHANNEL_COUNT],
        minus: [0.0; BLEND_CHANNEL_COUNT],
    };
    for (c, &gc) in g.iter().enumerate() {
        // `grad_inf > 0` is checked by the caller; dividing by it makes `eta`
        // a step in *log-weight* units, independent of how many candidates the
        // gradient happened to accumulate over.
        let step = eta * gc / grad_inf;
        next.plus[c] = parts.plus[c] * step.exp();
        next.minus[c] = parts.minus[c] * (-step).exp();
    }
    next.renormalise();
    next
}

// ---- identifiability -------------------------------------------------------

/// Channels whose value never changes across the candidates of any account.
///
/// Checked structurally rather than by watching the gradient: the structural
/// rule is what the projection needs (it is exact and `w`-independent), while
/// "gradient stayed tiny" would conflate an unidentifiable channel with an
/// identifiable one whose gradient merely happens to be small at the current
/// weights. See the module docs for why the raw gradient of such a channel is
/// not literally zero once the clamp is active.
pub(crate) fn unidentifiable_channels(cache: &ScoreCache) -> Vec<usize> {
    let mut varied = [false; BLEND_CHANNEL_COUNT];
    for acc in &cache.accounts {
        if acc.channels.len() < 2 || varied.iter().all(|&v| v) {
            continue;
        }
        let first = acc.channels[0].values();
        for (c, v) in varied.iter_mut().enumerate() {
            if !*v && acc.channels.iter().any(|ch| ch.values()[c] != first[c]) {
                *v = true;
            }
        }
    }
    (0..BLEND_CHANNEL_COUNT).filter(|&c| !varied[c]).collect()
}

// ---- solver ----------------------------------------------------------------

/// Maximise [`objective`] over the two-sided simplex by exponentiated gradient
/// with a backtracking line search.
///
/// Deterministic and single-threaded on purpose: the gradient is an f64
/// reduction over every candidate, and the result is a persisted config
/// recommendation — reproducibility is worth more here than the wall clock,
/// and the whole solve is a few seconds against the ~60s scoring pass that
/// produced the cache.
pub(crate) fn solve(cache: &ScoreCache, init: &Weights, temp: f32, penalty: f32) -> FitOutcome {
    let unidentifiable = unidentifiable_channels(cache);
    let mut parts = split(init);
    let mut w = parts.joined();
    let (mut obj, _) = objective_and_grad(cache, &w, temp, penalty, &unidentifiable);
    let initial_objective = obj;
    let mut history = vec![obj];
    let mut eta = ETA0;
    let mut gradient_inf = 0.0f64;
    let mut iterations = 0usize;
    // Cleared by either natural stopping condition below; left set if the loop
    // simply runs out of iterations.
    let mut budget_exhausted = true;

    for it in 0..MAX_ITERS {
        let (obj_now, g) = objective_and_grad(cache, &w, temp, penalty, &unidentifiable);
        // `w` only ever changes to an accepted (improving) step, so the
        // recomputed objective must match the tracked one up to rounding.
        debug_assert!(
            obj_now >= obj - 1e-6 * obj.abs().max(1.0),
            "objective regressed without an accepted step: {obj_now} < {obj}"
        );
        obj = obj_now;
        gradient_inf = g.iter().fold(0.0f64, |m, x| m.max(x.abs()));
        iterations = it;
        if gradient_inf == 0.0 {
            budget_exhausted = false;
            break;
        }

        let tol = REL_TOL * obj.abs().max(1.0);
        let mut accepted = false;
        for _ in 0..MAX_BACKTRACK {
            if eta < MIN_ETA {
                break;
            }
            let cand = eg_step(&parts, &g, gradient_inf, eta);
            let cand_w = cand.joined();
            let cand_obj = objective(cache, &cand_w, temp, penalty);
            if cand_obj > obj + tol {
                parts = cand;
                w = cand_w;
                obj = cand_obj;
                history.push(obj);
                accepted = true;
                eta = (eta * 1.5).min(ETA0);
                break;
            }
            eta *= 0.5;
        }
        iterations = it + 1;
        if !accepted {
            budget_exhausted = false;
            break;
        }
    }

    FitOutcome {
        weights: w,
        objective: obj,
        initial_objective,
        history,
        unidentifiable,
        gradient_inf,
        iterations,
        budget_exhausted,
    }
}

// ---- exact re-scoring ------------------------------------------------------

/// Rank every account under `w` and recompute the production metrics exactly.
///
/// This is the acceptance path, so it uses the real [`blend_score`] rather
/// than the surrogate. The ordering is built and broken the same way as
/// [`crate::cache::score_with_cache`] — positives first, then negatives, then a
/// stable descending sort by score — because ties must resolve identically or
/// the "re-scored NDCG equals the eval baseline" agreement is only approximate.
///
/// Post ids are not stored in the cache (relevance is positional and binary),
/// so the ranked tuples carry the candidate index in their `id` slot; every
/// metric here reads only the score and the `is_positive` flag.
pub(crate) fn rescore(
    cache: &ScoreCache,
    priors: &Priors,
    w: &Weights,
    top_k_ndcg: usize,
    top_k_recall: usize,
) -> Metrics {
    let temp = priors.score_temperature;
    let penalty = priors.strong_negative_penalty;
    let mut totals = Metrics::default();
    let n = cache.accounts.len();
    totals.ndcg_per_account.reserve(n);
    totals.recall_per_account.reserve(n);
    totals.mrr_per_account.reserve(n);

    for acc in &cache.accounts {
        let mut scored: Vec<(i64, f32, bool)> = acc
            .channels
            .iter()
            .enumerate()
            .map(|(i, ch)| {
                (
                    i as i64,
                    candidate_score(w, ch, temp, penalty),
                    i < acc.n_test,
                )
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let ndcg = ndcg_at_k_pub(&scored, top_k_ndcg);
        let recall = recall_at_k_pub(&scored, top_k_recall, acc.n_test);
        let mrr = mrr_pub(&scored);
        totals.ndcg_at_k += ndcg;
        totals.recall_at_k += recall;
        totals.mrr += mrr;
        totals.n_accounts += 1;
        totals.ndcg_per_account.push(ndcg);
        totals.recall_per_account.push(recall);
        totals.mrr_per_account.push(mrr);
    }
    totals
}

/// Largest absolute per-account difference; `None` when the vectors differ in
/// length (nothing to align).
fn max_abs_per_account_diff(a: &[f64], b: &[f64]) -> Option<f64> {
    if a.len() != b.len() {
        return None;
    }
    Some(
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f64, f64::max),
    )
}

// ---- reference vectors -----------------------------------------------------

/// A weight vector from a previous run, loaded from one of the
/// `calibration_results/grid_*.toml` files (or from any file carrying a
/// `[priors]` section) so it can be re-scored against the **same** channel
/// matrix as the incumbent.
///
/// This is the only way to answer "what does the calibrated vector cost
/// relative to the current one?" — the comparison cannot be made across runs,
/// because the candidate set is sampled from a live database and two
/// invocations do not share a dataset (see `docs/calibration.md`). Re-scoring
/// a persisted file inside one run puts both vectors on one matrix, where the
/// paired gate is a measurement rather than a coincidence.
#[derive(Debug)]
pub(crate) struct ReferenceVector {
    /// Path as the operator wrote it, for the report header.
    label: String,
    priors: Priors,
}

/// The `[priors]` section alone. The grid result files are exactly that plus
/// comments; `config.toml` carries far more that this comparison must ignore.
#[derive(serde::Deserialize)]
struct PriorsSection {
    priors: Priors,
}

impl ReferenceVector {
    /// Read `path`, requiring a `[priors]` section and a complete `mix_*` block.
    ///
    /// Both requirements are refusals rather than warnings on purpose. `Priors`
    /// is `#[serde(default)]`, so a partial block parses happily and silently
    /// substitutes the built-in default for every omitted weight — which would
    /// report on a vector that is not in the file. An incomplete reference is
    /// worse than no reference, because it looks like an answer.
    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        let text = fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;

        let value: toml::Value = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        let table = value
            .as_table()
            .and_then(|t| t.get("priors"))
            .and_then(toml::Value::as_table);
        let Some(table) = table else {
            anyhow::bail!(
                "{}: no [priors] section — point this at a calibration_results/grid_*.toml \
                 or at a config.toml",
                path.display()
            );
        };

        let missing = missing_mix_weights(table);
        if !missing.is_empty() {
            anyhow::bail!(
                "{}: [priors] omits {} — a partial block would take the built-in default for \
                 the omitted weights, so the re-score would not be the vector this file \
                 describes",
                path.display(),
                missing.join(", ")
            );
        }

        let doc: PriorsSection = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing [priors] in {}: {e}", path.display()))?;
        Ok(Self {
            label: path.display().to_string(),
            priors: doc.priors,
        })
    }
}

/// The `mix_*` keys a `[priors]` table does not carry.
fn missing_mix_weights(table: &toml::value::Table) -> Vec<String> {
    BLEND_CHANNEL_NAMES
        .iter()
        .map(|name| format!("mix_{name}"))
        .filter(|key| !table.contains_key(key))
        .collect()
}

/// Fields other than `mix_*` and `now` where `reference` disagrees with `live`.
///
/// Only the mix weights can be re-scored from a cached matrix: every other knob
/// is already baked into the channel values, which were computed once under
/// `live`. So a difference here does not invalidate the comparison, but it does
/// mean the file was written against a different channel matrix and the
/// re-score only approximates it — worth printing rather than discovering later.
///
/// Serialised rather than hand-listed: `Priors` grows fields often, and a
/// hand-maintained diff would quietly stop covering the new ones.
fn non_mix_differences(live: &Priors, reference: &Priors) -> Vec<String> {
    let (Ok(live), Ok(reference)) = (serde_json::to_value(live), serde_json::to_value(reference))
    else {
        return Vec::new();
    };
    let (Some(live), Some(reference)) = (live.as_object(), reference.as_object()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for (key, a) in live {
        // `now` is the evaluation clock, not a knob; the file's value is
        // whatever `Priors::default()` produced when it was parsed here.
        if key == "now" || key.starts_with("mix_") {
            continue;
        }
        let Some(b) = reference.get(key) else {
            continue;
        };
        let same = match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => {
                // The result files round to two or three decimals, so an exact
                // compare would report every knob the file merely rounded.
                (x - y).abs() <= 1e-3 * x.abs().max(y.abs()).max(1.0)
            }
            _ => a == b,
        };
        if !same {
            out.push(format!("{key}: live={a}, file={b}"));
        }
    }
    out.sort();
    out
}

// ---- report ----------------------------------------------------------------

/// Solve the mix weights from `cache` and report the exact before/after
/// metrics, the fitted shares and the paired gate verdict.
///
/// `baseline` is the `eval` pass's averaged metrics — produced by
/// `score_with_cache` from this very cache. The re-score check below recomputes
/// the same numbers from the cache and flags any divergence, which is the only
/// signal that the cache and the metric path have drifted apart.
///
/// `diversify` is the eval's MMR flag. The fit optimises the raw blend and the
/// acceptance re-score ranks by blend score alone, so with MMR on the eval
/// baseline is ordered differently and the two are not directly comparable —
/// the report says so instead of letting the divergence check carry the blame.
///
/// `reference` is a persisted vector to re-score beside the incumbent (see
/// [`ReferenceVector`]). It shares this function's matrix, clock and blend
/// parameters, which is the entire point: three vectors, one measurement.
pub(crate) fn run_fit_and_report(
    cache: &ScoreCache,
    baseline: &Metrics,
    diversify: bool,
    reference: Option<&ReferenceVector>,
) {
    let cfg_arc = cfg();
    let top_k_ndcg = cfg_arc.backtest.top_k_ndcg;
    let top_k_recall = cfg_arc.backtest.top_k_recall;
    let priors = cfg_arc.priors.clone();
    let temp = priors.score_temperature;
    let penalty = priors.strong_negative_penalty;

    let n_accounts = cache.accounts.len();
    let n_cand: usize = cache.accounts.iter().map(|a| a.channels.len()).sum();
    let n_pos: usize = cache.accounts.iter().map(|a| a.n_test).sum();
    if n_pos == 0 || n_cand == 0 {
        eprintln!("[fit] cache has no positives/candidates — nothing to fit");
        return;
    }
    eprintln!(
        "[fit] solving 12 mix weights from {n_accounts} accounts / {n_cand} candidates / \
         {n_pos} positives (temp={temp}, penalty={penalty})"
    );
    if diversify {
        eprintln!(
            "[fit] NOTE: MMR diversify is on. The fit and its acceptance re-score rank by the \
             raw blend only, so the 'eval baseline' below is MMR-ordered and will differ; the \
             gate still compares raw-order incumbent vs raw-order candidate."
        );
    }

    // Incumbent, exactly as the scorer derives it (for the re-score check),
    // and the floored solver start (for the warm start).
    let incumbent = normalised_mix_weights(&priors);
    let init = initial_weights(&priors);

    let t_solve = Instant::now();
    let outcome = solve(cache, &init, temp, penalty);
    let solve_secs = t_solve.elapsed().as_secs_f32();

    let unidentifiable = &outcome.unidentifiable;
    if unidentifiable.is_empty() {
        eprintln!("[fit] identifiability: all 12 channels vary within at least one account");
    } else {
        let names: Vec<&str> = unidentifiable
            .iter()
            .map(|&c| BLEND_CHANNEL_NAMES[c])
            .collect();
        eprintln!(
            "[fit] identifiability: NOT identifiable offline — {} (constant within every \
             account ⇒ no per-candidate information; gradient projected out)",
            names.join(", ")
        );
        eprintln!(
            "[fit]   reported separately: 'the data cannot rank this channel', not 'it is \
             unimportant'"
        );
    }

    eprintln!(
        "[fit] objective L: first={:.6} last={:.6} (Δ={:+.6}) in {} accepted step(s) / {} \
         iteration(s), |∇L|∞={:.3e}",
        outcome.initial_objective,
        outcome.objective,
        outcome.objective - outcome.initial_objective,
        outcome.history.len() - 1,
        outcome.iterations,
        outcome.gradient_inf
    );
    if outcome.budget_exhausted {
        eprintln!(
            "[fit]   NOTE: the {MAX_ITERS}-iteration budget ran out before the line search stopped"
        );
        eprintln!(
            "[fit]     improving — the vector below is NOT a converged optimum, and |∇L|∞ above"
        );
        eprintln!("[fit]     is still large.");
    }
    if outcome.history.len() > 1 {
        let first = outcome.history[0];
        let last = outcome.history[outcome.history.len() - 1];
        eprintln!("[fit]   objective at the first iteration: {first:.6}");
        eprintln!("[fit]   objective at the last iteration:  {last:.6}");
    }

    // Weights: the solver parameterises on the two-sided simplex, so its vector
    // *is* the L1-normalised share — "raw" and "share" coincide by construction.
    eprintln!(
        "[fit] fitted weights: the solver's parameter IS the L1-normalised share (Σ|w|=1), so"
    );
    eprintln!(
        "[fit]   the `raw mix_*` values and the shares are the same numbers; a negative value"
    );
    eprintln!(
        "[fit]   means the channel is subtracted. from_priors renormalises any scaling of the"
    );
    eprintln!("[fit]   block to itself, so this is scale-free.");
    eprintln!(
        "[fit]   {:<18} {:>12} {:>12} {:>7}",
        "channel", "incumbent", "fitted", "Δ"
    );
    for (c, name) in BLEND_CHANNEL_NAMES.iter().enumerate() {
        let mark = if unidentifiable.contains(&c) {
            "*"
        } else {
            " "
        };
        eprintln!(
            "[fit]  {mark}{name:<18} {:>12.6} {:>12.6} {:>+7.4}",
            incumbent[c],
            outcome.weights[c],
            outcome.weights[c] - incumbent[c]
        );
    }
    if !unidentifiable.is_empty() {
        eprintln!(
            "[fit]   * unidentifiable offline — the share is whatever renormalisation left, not \
             fitted evidence"
        );
    }

    // Exact acceptance path.
    let t_rescore = Instant::now();
    let inc_metrics = rescore(cache, &priors, &incumbent, top_k_ndcg, top_k_recall);
    let cand_metrics = rescore(cache, &priors, &outcome.weights, top_k_ndcg, top_k_recall);
    let rescore_secs = t_rescore.elapsed().as_secs_f32();
    let inc = inc_metrics.average();
    let cand = cand_metrics.average();

    match max_abs_per_account_diff(&inc_metrics.ndcg_per_account, &baseline.ndcg_per_account) {
        Some(d) if d <= 1e-9 => eprintln!(
            "[fit] re-score check vs the eval baseline: identical per-account NDCG (max |Δ|={d:.1e})"
        ),
        Some(d) => eprintln!(
            "[fit] WARNING: re-scored incumbent differs from the eval baseline by up to {d:.3e} \
             per-account NDCG — cache and metric path have drifted"
        ),
        None => eprintln!(
            "[fit] WARNING: re-scored incumbent has {} accounts, eval baseline has {}",
            inc_metrics.n_accounts, baseline.n_accounts
        ),
    }

    eprintln!(
        "[fit] incumbent NDCG@{top_k_ndcg}={:.4} Recall@{top_k_recall}={:.4} MRR={:.4}",
        inc.ndcg_at_k, inc.recall_at_k, inc.mrr
    );
    eprintln!(
        "[fit] fitted    NDCG@{top_k_ndcg}={:.4} Recall@{top_k_recall}={:.4} MRR={:.4}",
        cand.ndcg_at_k, cand.recall_at_k, cand.mrr
    );

    // Same gate as a grid probe.
    let gate = gate_probe(&cand, inc.ndcg_at_k, &inc.ndcg_per_account);
    eprintln!(
        "[fit] paired delta={:+.6} SE={:.6} z=t(df={})={:.4} threshold={:.6} paired={}",
        gate.delta,
        gate.se,
        cand.n_accounts.saturating_sub(1),
        gate.z,
        gate.threshold(),
        gate.paired
    );
    eprintln!(
        "[fit] verdict: {} (surrogate ≠ NDCG, so the fitted vector is only a candidate)",
        if gate.accept { "ACCEPT" } else { "REJECT" }
    );
    eprintln!("[fit] timings: solve={solve_secs:.2}s rescore={rescore_secs:.2}s");

    if gate.accept {
        eprintln!("[fit] candidate [priors] block:");
        for (c, name) in BLEND_CHANNEL_NAMES.iter().enumerate() {
            eprintln!("mix_{name} = {:.6}", outcome.weights[c]);
        }
    }

    if let Some(reference) = reference {
        report_reference(cache, &incumbent, &inc, reference);
    }
}

/// Re-score a persisted vector against the same matrix and judge it with the
/// same paired gate the fitted vector gets.
///
/// Separate from [`run_fit_and_report`] because it answers a different
/// question: not "can a solver beat what we ship?" but "what would the vector
/// in this file have scored, measured on the matrix we just built?" That second
/// question is only answerable here — which is why the file cannot simply be
/// re-run and the two metric lines compared.
fn report_reference(
    cache: &ScoreCache,
    incumbent: &Weights,
    inc: &Metrics,
    reference: &ReferenceVector,
) {
    // Re-read rather than thread five more parameters through: every function
    // in this module resolves the live config the same way, and the caller's
    // `priors` is this same value.
    let cfg_arc = cfg();
    let live = cfg_arc.priors.clone();
    let top_k_ndcg = cfg_arc.backtest.top_k_ndcg;
    let top_k_recall = cfg_arc.backtest.top_k_recall;
    let temp = live.score_temperature;
    let penalty = live.strong_negative_penalty;

    let w = normalised_mix_weights(&reference.priors);
    eprintln!("[fit] reference vector: {}", reference.label);

    let diffs = non_mix_differences(&live, &reference.priors);
    if diffs.is_empty() {
        eprintln!(
            "[fit]   non-mix [priors] fields agree with the live config, so the cached matrix \
             is the one this file was written against"
        );
    } else {
        eprintln!(
            "[fit]   WARNING: {} non-mix field(s) differ from the live config. Only mix_* can \
             be re-scored from a cached matrix — every other knob is already baked into the \
             channel values — so this comparison approximates the file:",
            diffs.len()
        );
        for d in diffs.iter().take(12) {
            eprintln!("[fit]     {d}");
        }
        if diffs.len() > 12 {
            eprintln!("[fit]     ... and {} more", diffs.len() - 12);
        }
    }

    eprintln!(
        "[fit]   {:<18} {:>12} {:>12} {:>8}",
        "channel", "incumbent", "reference", "Δ"
    );
    for (c, name) in BLEND_CHANNEL_NAMES.iter().enumerate() {
        eprintln!(
            "[fit]   {name:<18} {:>12.6} {:>12.6} {:>+8.4}",
            incumbent[c],
            w[c],
            w[c] - incumbent[c]
        );
    }

    let inc_obj = objective(cache, incumbent, temp, penalty);
    let ref_obj = objective(cache, &w, temp, penalty);
    eprintln!(
        "[fit] reference objective L={ref_obj:.6} vs incumbent L={inc_obj:.6} (Δ={:+.6})",
        ref_obj - inc_obj
    );

    let m = rescore(cache, &live, &w, top_k_ndcg, top_k_recall).average();
    eprintln!(
        "[fit] reference NDCG@{top_k_ndcg}={:.4} Recall@{top_k_recall}={:.4} MRR={:.4}",
        m.ndcg_at_k, m.recall_at_k, m.mrr
    );

    let gate = gate_probe(&m, inc.ndcg_at_k, &inc.ndcg_per_account);
    eprintln!(
        "[fit] paired reference vs incumbent: delta={:+.6} SE={:.6} z=t(df={})={:.4} \
         threshold={:.6} paired={}",
        gate.delta,
        gate.se,
        m.n_accounts.saturating_sub(1),
        gate.z,
        gate.threshold(),
        gate.paired
    );
    eprintln!(
        "[fit] verdict (reference vs incumbent): {}",
        if gate.accept {
            "ACCEPT — the file's vector is significantly better than the shipped one"
        } else {
            "REJECT — the file's vector is not significantly better than the shipped one"
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use chrono::{Duration, Utc};

    use e621_account_parser_api::models::AccountPreferenceProfile;
    use e621_account_parser_api::utils::{ScoringContext, TagRelationGraph};

    use crate::cache::{ChannelScores, M_ALL, score_with_cache};
    use crate::test_support;

    // ---- synthetic caches --------------------------------------------------

    /// A cache from raw channel rows: each entry is `(channels, is_positive)`.
    #[derive(Clone, Copy)]
    struct Row {
        sim: f32,
        quality: f32,
        recency: f32,
        rating: f32,
        media: f32,
        popularity: f32,
        interaction: f32,
        tag_relation: f32,
        uploader: f32,
        exclusivity: f32,
        novelty: f32,
        artist_discovery: f32,
        veto: bool,
    }

    impl Row {
        fn zeros() -> Self {
            Self {
                sim: 0.0,
                quality: 0.0,
                recency: 0.0,
                rating: 0.0,
                media: 0.0,
                popularity: 0.0,
                interaction: 0.0,
                tag_relation: 0.0,
                uploader: 0.0,
                exclusivity: 0.0,
                novelty: 0.0,
                artist_discovery: 0.0,
                veto: false,
            }
        }

        fn to_scores(self) -> ChannelScores {
            ChannelScores {
                sim: self.sim,
                quality: self.quality,
                recency: self.recency,
                rating: self.rating,
                media: self.media,
                popularity: self.popularity,
                interaction: self.interaction,
                tag_relation: self.tag_relation,
                uploader: self.uploader,
                exclusivity: self.exclusivity,
                novelty: self.novelty,
                artist_discovery: self.artist_discovery,
                veto: self.veto,
            }
        }
    }

    fn synthetic_cache(accounts: &[(usize, Vec<Row>)]) -> ScoreCache {
        ScoreCache {
            accounts: accounts
                .iter()
                .map(|(n_test, rows)| crate::cache::AccountChannelCache {
                    channels: rows.iter().map(|r| r.to_scores()).collect(),
                    n_test: *n_test,
                    context_base: None,
                })
                .collect(),
        }
    }

    /// Two accounts x 4 candidates: `sim` separates positives perfectly, every
    /// other channel is constant (hence unidentifiable). `interaction` is the
    /// channel §2.7 names as the real offline instance.
    fn separating_cache() -> ScoreCache {
        let mut accounts = Vec::new();
        for acc in 0..2 {
            let mut rows = Vec::new();
            for i in 0..4usize {
                let mut r = Row::zeros();
                r.sim = if i < 2 { 1.0 } else { 0.0 };
                r.quality = 0.5;
                r.recency = 0.25;
                r.rating = 0.1;
                r.media = 0.2;
                r.popularity = 0.3;
                // Constant within the account but different *between*
                // accounts: still zero gradient, because the objective is
                // separable per account.
                r.interaction = 0.4 + acc as f32 * 0.1;
                r.tag_relation = 0.6;
                r.uploader = 0.15;
                r.exclusivity = 0.05;
                r.novelty = 0.07;
                r.artist_discovery = 0.09;
                rows.push(r);
            }
            accounts.push((2usize, rows));
        }
        synthetic_cache(&accounts)
    }

    fn temp_penalty() -> (f32, f32) {
        let p = Priors::default();
        (p.score_temperature, p.strong_negative_penalty)
    }

    // ---- the blend is the production blend ---------------------------------

    /// Anti-drift: the fit's scoring path must equal the real
    /// `ScoringContext::final_blend` for identical inputs.
    ///
    /// The formula itself is shared by construction (`final_blend` calls
    /// `blend_score`), so what this test actually pins is the part that *can*
    /// drift: the mapping `Priors` → normalised weight vector and the channel
    /// order. Every one of the twelve weights has a distinct value, so a
    /// permuted `MixWeights::as_array` or a permuted channel list changes the
    /// result rather than cancelling out.
    #[test]
    fn fit_scoring_path_matches_the_real_final_blend() {
        // Distinct, all non-zero: defaults leave exclusivity/novelty/
        // artist_discovery at 0.0, which would hide an ordering bug at those
        // positions.
        let mut p = Priors {
            mix_sim: 0.71,
            mix_quality: 0.12,
            mix_recency: 0.13,
            mix_rating: 0.14,
            mix_media: 0.15,
            mix_popularity: 0.16,
            mix_interaction: 0.17,
            mix_tag_relation: 0.18,
            mix_uploader: 0.19,
            mix_exclusivity: 0.20,
            mix_novelty: 0.21,
            mix_artist_discovery: 0.22,
            score_temperature: 2.5,
            strong_negative_penalty: 0.35,
            now: Utc::now(),
            ..Priors::default()
        };

        let idf = test_support::empty_idf();
        let global = TagRelationGraph::empty();
        let user = TagRelationGraph::empty();
        let profile = AccountPreferenceProfile::default();
        let ctx = ScoringContext::new(&[], &p, &idf, &profile, &global, &user);

        let w = normalised_mix_weights(&p);
        let ch = [
            1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0,
        ];
        for veto in [false, true] {
            let ours = blend_score(w, ch, p.score_temperature, p.strong_negative_penalty, veto);
            let theirs = ctx.final_blend(
                ch[0], ch[1], ch[2], ch[3], ch[4], ch[5], ch[6], ch[7], ch[8], ch[9], ch[10],
                ch[11], veto,
            );
            assert_eq!(
                ours.to_bits(),
                theirs.to_bits(),
                "veto={veto}: fit path {ours} != final_blend {theirs}"
            );
        }

        // The no-temperature branch is a separate code path in the blend.
        p.score_temperature = 0.0;
        let ctx = ScoringContext::new(&[], &p, &idf, &profile, &global, &user);
        let ours = blend_score(w, ch, 0.0, p.strong_negative_penalty, false);
        let theirs = ctx.final_blend(
            ch[0], ch[1], ch[2], ch[3], ch[4], ch[5], ch[6], ch[7], ch[8], ch[9], ch[10], ch[11],
            false,
        );
        assert_eq!(ours.to_bits(), theirs.to_bits());
    }

    /// `ChannelScores::values()` must be the same order `normalised_mix_weights`
    /// produces; otherwise the fit multiplies each weight by the wrong channel.
    #[test]
    fn channel_scores_flatten_in_the_weight_order() {
        let ch = ChannelScores {
            sim: 1.0,
            quality: 2.0,
            recency: 3.0,
            rating: 4.0,
            media: 5.0,
            popularity: 6.0,
            interaction: 7.0,
            tag_relation: 8.0,
            uploader: 9.0,
            exclusivity: 10.0,
            novelty: 11.0,
            artist_discovery: 12.0,
            veto: false,
        };
        assert_eq!(
            ch.values(),
            [
                1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0
            ]
        );
        assert_eq!(BLEND_CHANNEL_NAMES.len(), BLEND_CHANNEL_COUNT);

        // Tie the flatten order to the *named* weights, not merely to 1..12.
        // With only `mix_sim` non-zero the effective weight vector is the sim
        // unit vector, so a sim-only channel must score 1.0 — any permutation
        // shared between `as_array` and `ChannelScores::values` would multiply
        // the sim value by a zero weight and score 0.0 instead.
        let p = Priors {
            mix_sim: 1.0,
            mix_quality: 0.0,
            mix_recency: 0.0,
            mix_rating: 0.0,
            mix_media: 0.0,
            mix_popularity: 0.0,
            mix_interaction: 0.0,
            mix_tag_relation: 0.0,
            mix_uploader: 0.0,
            mix_exclusivity: 0.0,
            mix_novelty: 0.0,
            mix_artist_discovery: 0.0,
            ..Priors::default()
        };
        let sim_only = ChannelScores {
            sim: 1.0,
            ..Default::default()
        };
        assert_eq!(
            blend_score(
                normalised_mix_weights(&p),
                sim_only.values(),
                0.0,
                0.0,
                false
            ),
            1.0
        );
    }

    // ---- solver ------------------------------------------------------------

    /// The realistic shape: every channel has within-account spread, so nothing
    /// is projected out and the solver has twelve live directions.
    #[test]
    fn a_multi_channel_fit_stays_monotone_and_converges_within_the_budget() {
        let mut accounts = Vec::new();
        for a in 0..40usize {
            let mut rows = Vec::new();
            for i in 0..60usize {
                // Deterministic spread, no RNG (the solver must be reproducible).
                let x = ((a * 60 + i) as f32 * 0.618_034).fract();
                let mut r = Row::zeros();
                r.sim = if i < 12 { 0.6 + x } else { x };
                r.quality = if i % 3 == 0 { x } else { 1.0 - x };
                r.recency = x * 0.5;
                r.rating = (1.0 - x) * 0.25;
                r.media = x * 0.3 + 0.1;
                r.popularity = (1.0 - x) * 0.2;
                r.interaction = x;
                r.tag_relation = 0.5 - x * 0.25;
                r.uploader = x * 0.7;
                r.exclusivity = 0.2 + x * 0.1;
                r.novelty = 1.0 - x;
                r.artist_discovery = x * x;
                rows.push(r);
            }
            accounts.push((12usize, rows));
        }
        let cache = synthetic_cache(&accounts);
        let (temp, penalty) = temp_penalty();
        let outcome = solve(&cache, &initial_weights(&Priors::default()), temp, penalty);

        assert!(
            outcome.unidentifiable.is_empty(),
            "every channel has spread here: {:?}",
            outcome.unidentifiable
        );
        assert!(outcome.history.len() > 1, "no step accepted");
        for pair in outcome.history.windows(2) {
            assert!(pair[1] >= pair[0], "objective decreased: {pair:?}");
        }
        assert!(outcome.objective > outcome.initial_objective);
        assert!(
            outcome.iterations <= MAX_ITERS,
            "solve claimed {} iterations for a budget of {MAX_ITERS}",
            outcome.iterations
        );
    }

    #[test]
    fn the_solver_improves_the_objective_monotonically() {
        let cache = separating_cache();
        let (temp, penalty) = temp_penalty();
        let p = Priors::default();
        let init = initial_weights(&p);
        let outcome = solve(&cache, &init, temp, penalty);

        assert!(
            outcome.history.len() > 1,
            "solver accepted no step; history={:?}",
            outcome.history
        );
        for pair in outcome.history.windows(2) {
            assert!(
                pair[1] >= pair[0],
                "objective decreased: {} -> {}",
                pair[0],
                pair[1]
            );
        }
        assert!(
            outcome.objective > outcome.initial_objective,
            "fit did not improve on the start: {} -> {}",
            outcome.initial_objective,
            outcome.objective
        );
        // The start is the incumbent (up to the floor), so the objective at the
        // incumbent must be no better than at the fitted weights.
        let inc_obj = objective(&cache, &normalised_mix_weights(&p), temp, penalty);
        assert!(outcome.objective >= inc_obj - 1e-6);
    }

    /// Known ground truth: `sim` separates positives from negatives, everything
    /// else is noise. The fit must concentrate the simplex on `sim` and the
    /// exact re-score must then be perfect.
    #[test]
    fn the_fit_recovers_the_channel_that_ranks_the_favourites() {
        let cache = separating_cache();
        let (temp, penalty) = temp_penalty();
        let p = Priors::default();
        let outcome = solve(&cache, &initial_weights(&p), temp, penalty);

        assert!(
            outcome.weights[0] > 0.9,
            "sim weight {} did not dominate the simplex {:?}",
            outcome.weights[0],
            outcome.weights
        );
        // The unidentifiable channels may keep their start value, but none of
        // them may be promoted above the channel that actually discriminates.
        for (c, (&w, name)) in outcome
            .weights
            .iter()
            .zip(BLEND_CHANNEL_NAMES.iter())
            .enumerate()
            .skip(1)
        {
            assert!(
                w < outcome.weights[0],
                "channel {c} ({name}) outranked sim: {:?}",
                outcome.weights
            );
        }

        let k = 20;
        let m = rescore(&cache, &p, &outcome.weights, k, 50).average();
        assert!(
            (m.ndcg_at_k - 1.0).abs() < 1e-9,
            "perfect separation should give NDCG@20=1, got {}",
            m.ndcg_at_k
        );
    }

    /// An unidentifiable channel is reported as such and does not receive a
    /// spurious "important" weight.
    #[test]
    fn unidentifiable_channels_are_reported_not_fitted() {
        let cache = separating_cache();
        let unident = unidentifiable_channels(&cache);
        assert!(
            unident.contains(&6),
            "interaction must be unidentifiable: {unident:?}"
        );
        assert!(
            !unident.contains(&0),
            "sim varies within every account and is identifiable"
        );

        let (temp, penalty) = temp_penalty();
        let p = Priors::default();
        let init = initial_weights(&p);
        let outcome = solve(&cache, &init, temp, penalty);
        assert_eq!(outcome.unidentifiable, unident);

        // Its gradient contribution is projected out, so the solver cannot move
        // it away from the start.
        let (_, g) = objective_and_grad(&cache, &outcome.weights, temp, penalty, &unident);
        assert_eq!(g[6], 0.0, "unidentifiable channel got a non-zero gradient");
        // Renormalisation still squeezes it (the identifiable channel grows and
        // takes the simplex share), so the invariant is "never promoted", not
        // "unchanged".
        assert!(
            outcome.weights[6] <= init[6] + f32::EPSILON,
            "unidentifiable weight was promoted: {} -> {}",
            init[6],
            outcome.weights[6]
        );
        assert!(
            outcome.weights[6] < outcome.weights[0],
            "unidentifiable channel must not be reported as important"
        );
    }

    /// The projection in [`objective_and_grad`] is not cosmetic: with a
    /// *non-zero* constant channel the raw gradient does not cancel once the
    /// clamp is active. This is the case the naive "constant ⇒ zero gradient"
    /// argument misses, and it is why the solver projects rather than relies on
    /// cancellation.
    #[test]
    fn a_constant_nonzero_channel_is_projected_out_even_though_its_raw_gradient_is_not_zero() {
        let cache = synthetic_cache(&[(
            1usize,
            vec![
                Row {
                    sim: 1.0,
                    interaction: 0.5,
                    ..Row::zeros()
                },
                Row {
                    sim: 0.0,
                    interaction: 0.5,
                    ..Row::zeros()
                },
            ],
        )]);
        // sim = 1.0 puts the positive at raw = 1.05 — on the clamp plateau, so
        // its sensitivity is 0 while the negative's is 1.
        let mut w = [0.0f32; BLEND_CHANNEL_COUNT];
        w[0] = 1.0;
        w[6] = 0.1;

        let unident = unidentifiable_channels(&cache);
        assert!(unident.contains(&6), "{unident:?}");
        let (_, raw) = objective_and_grad(&cache, &w, 0.0, 0.0, &[]);
        assert_ne!(
            raw[6], 0.0,
            "with the clamp active the constant channel's raw gradient should survive"
        );
        let (_, projected) = objective_and_grad(&cache, &w, 0.0, 0.0, &unident);
        assert_eq!(projected[6], 0.0);
    }

    // ---- the weights may be negative --------------------------------------

    /// The point of the two-sided simplex. A channel running *against* the
    /// label must be able to end up with a negative weight, and it must be able
    /// to get there from a zero start: a one-sided multiplicative update leaves
    /// an exact zero absorbing, so `mix_novelty = 0.0` — its shipped value in
    /// `Priors::default()` — could never have gone negative however loudly the
    /// data asked.
    #[test]
    fn a_channel_that_anti_predicts_the_label_goes_negative() {
        let cache = synthetic_cache(&[(
            1usize,
            vec![
                // `sim` is equal on both rows, so it is structurally
                // unidentifiable and its gradient is projected out; it is here
                // only to lift the raw blend off the clamp plateaus. `novelty`
                // is higher on the negative than on the positive, so the only
                // way to separate the pair is to subtract it.
                Row {
                    sim: 0.5,
                    novelty: 0.2,
                    ..Row::zeros()
                },
                Row {
                    sim: 0.5,
                    novelty: 0.8,
                    ..Row::zeros()
                },
            ],
        )]);
        let init = initial_weights(&Priors::default());
        assert_eq!(
            init[10], 0.0,
            "novelty ships at zero, which is the hard case"
        );

        let outcome = solve(&cache, &init, 0.0, 0.0);
        assert!(
            outcome.weights[10] < 0.0,
            "novelty should be subtracted, got {}",
            outcome.weights[10]
        );
        assert!(
            outcome.objective > outcome.initial_objective,
            "a negative weight must be an improvement: {} -> {}",
            outcome.initial_objective,
            outcome.objective
        );
    }

    /// A negative incumbent must survive the warm start instead of being
    /// silently clamped, which is what the one-sided `renormalise` did.
    #[test]
    fn a_negative_incumbent_is_not_clamped_away() {
        let p = Priors {
            mix_sim: 1.0,
            mix_novelty: -0.5,
            ..Priors::default()
        };

        let init = initial_weights(&p);
        assert!(
            init[10] < 0.0,
            "warm start must keep the incumbent's sign, got {}",
            init[10]
        );
        let l1: f32 = init.iter().map(|x| x.abs()).sum();
        assert!(
            (l1 - 1.0).abs() < 1e-5,
            "warm start should be L1-normalised, got {l1}"
        );
    }

    // ---- exact re-scoring agrees with score_with_cache ---------------------

    /// The cache → ranking → metric path must reproduce the metrics
    /// `score_with_cache` computed from the same cache under the same weights.
    /// This is what makes the acceptance gate meaningful: the fit's re-score
    /// and the eval baseline must be the same measurement.
    #[test]
    fn rescoring_reproduces_score_with_cache_per_account_ndcg() {
        let now = Utc::now();
        let mk = |id: i64, days: i64| {
            test_support::post(id, now - Duration::days(days), id * 7, &[(0, "cat")])
        };
        let ds = test_support::dataset(vec![
            test_support::account(
                vec![mk(1, 1), mk(2, 2)],
                vec![mk(3, 9), mk(4, 10), mk(5, 11)],
            ),
            test_support::account(vec![mk(6, 3)], vec![mk(7, 12), mk(8, 13)]),
            test_support::account(vec![mk(9, 4), mk(10, 5)], vec![mk(11, 14)]),
        ]);
        let priors = Priors {
            now,
            ..Priors::default()
        };
        let (top_k_ndcg, top_k_recall) = (20usize, 50usize);

        let (metrics, cache) = score_with_cache(
            &ds,
            &priors,
            now,
            top_k_ndcg,
            top_k_recall,
            false,
            None,
            M_ALL,
        );
        let baseline = metrics.average();

        let w = normalised_mix_weights(&priors);
        let rescored = rescore(&cache, &priors, &w, top_k_ndcg, top_k_recall).average();

        assert_eq!(rescored.n_accounts, baseline.n_accounts);
        assert_eq!(
            rescored.ndcg_per_account.len(),
            baseline.ndcg_per_account.len()
        );
        for (i, (a, b)) in rescored
            .ndcg_per_account
            .iter()
            .zip(baseline.ndcg_per_account.iter())
            .enumerate()
        {
            assert!(
                (a - b).abs() < 1e-12,
                "account {i}: rescored NDCG {a} != score_with_cache NDCG {b}"
            );
        }
        assert!((rescored.ndcg_at_k - baseline.ndcg_at_k).abs() < 1e-12);
        assert!((rescored.recall_at_k - baseline.recall_at_k).abs() < 1e-12);
        assert!((rescored.mrr - baseline.mrr).abs() < 1e-12);

        // The cache must also carry the positive count the metrics divide by.
        assert_eq!(cache.accounts[0].n_test, 2);
        assert_eq!(cache.accounts[1].n_test, 1);
    }

    /// The report path must survive a real end-to-end pass — it reads the
    /// production `Priors` from config, re-scores twice, prints the weight table
    /// and calls the grid gate. The behaviour is asserted by the tests above;
    /// this one is a no-panic guard for the plumbing between them.
    #[test]
    fn the_report_path_runs_on_a_synthetic_cache() {
        let cache = separating_cache();
        let priors = cfg().priors.clone();
        let w = normalised_mix_weights(&priors);
        let baseline = rescore(&cache, &priors, &w, 20, 50).average();
        run_fit_and_report(&cache, &baseline, false, None);
    }

    /// The objective evaluated through the fit must agree with the exact blend
    /// used for re-scoring — i.e. the surrogate is built on the real scores,
    /// not on a re-derived formula.
    #[test]
    fn objective_is_built_from_the_exact_blend_scores() {
        let cache = separating_cache();
        let (temp, penalty) = temp_penalty();
        let p = Priors::default();
        let w = normalised_mix_weights(&p);

        let mut expected = 0.0f64;
        for acc in &cache.accounts {
            let us: Vec<f64> = acc
                .channels
                .iter()
                .map(|ch| f64::from(candidate_score(&w, ch, temp, penalty)))
                .collect();
            let max = us.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let sum_exp: f64 = us.iter().map(|u| (u - max).exp()).sum();
            let lse = max + sum_exp.ln();
            for u in us.iter().take(acc.n_test) {
                expected += u - lse;
            }
        }
        let got = objective(&cache, &w, temp, penalty);
        assert!(
            (got - expected).abs() < 1e-9,
            "objective {got} != hand-computed {expected}"
        );
    }

    // ---- reference vectors -------------------------------------------------

    /// A `grid_<ts>.toml` in miniature: leading comments, a `[priors]` body
    /// carrying all twelve weights, and optional extra knobs.
    fn reference_toml(mix_sim: f32, drop_key: Option<&str>, extra: &str) -> String {
        let mut s = String::from("# calibrate grid result — synthetic\n[priors]\n");
        for &name in BLEND_CHANNEL_NAMES.iter() {
            if drop_key == Some(name) {
                continue;
            }
            let v = if name == "sim" { mix_sim } else { 0.0 };
            s.push_str(&format!("mix_{name} = {v:.6}\n"));
        }
        s.push_str(extra);
        s
    }

    fn write_temp_toml(name: &str, body: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("e621fitref-{}-{name}.toml", std::process::id()));
        fs::write(&path, body).expect("write temp toml");
        path
    }

    /// The happy path: the file the grid actually writes loads, and the mix
    /// block reaches the blend as written.
    #[test]
    fn a_grid_result_file_loads_and_yields_its_mix_weights() {
        let body = reference_toml(2.261, None, "idf_max = 100.00\nscore_temperature = 0.000\n");
        let path = write_temp_toml("complete", &body);
        let reference = ReferenceVector::load(&path).expect("a complete [priors] must load");

        // Every other weight is zero and the block is L1-normalised, so `sim`
        // takes the whole share.
        let w = normalised_mix_weights(&reference.priors);
        assert!((w[0] - 1.0).abs() < 1e-6, "sim share {w:?}");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_reference_file_without_a_priors_section_is_refused() {
        let path = write_temp_toml("nosection", "[database]\npath = \"x.db\"\n");
        let err = ReferenceVector::load(&path).expect_err("must refuse");
        assert!(err.to_string().contains("no [priors] section"), "{err}");
        let _ = fs::remove_file(&path);
    }

    /// The refusal that matters. `Priors` is `#[serde(default)]`, so a partial
    /// mix block parses happily and silently substitutes the built-in default
    /// for the omitted weight — the report would then describe a vector that is
    /// not in the file, and nothing downstream could tell.
    #[test]
    fn a_partial_mix_block_is_refused_rather_than_defaulted() {
        let body = reference_toml(1.0, Some("tag_relation"), "");
        let path = write_temp_toml("partial", &body);
        let err = ReferenceVector::load(&path).expect_err("must refuse");
        assert!(err.to_string().contains("mix_tag_relation"), "{err}");
        let _ = fs::remove_file(&path);
    }

    /// The files round to two or three decimals, so an exact compare would
    /// report every knob they merely rounded. Real drift must still show.
    #[test]
    fn non_mix_differences_ignore_rounding_but_report_real_drift() {
        let live = Priors {
            idf_lambda: 1.0,
            group_w_general: 0.7378,
            recency_tau_days: 10.0,
            ..Priors::default()
        };
        let file = Priors {
            idf_lambda: 1.0,        // "1.000" in the file — the same knob
            group_w_general: 0.738, // rounding to three decimals, not drift
            recency_tau_days: 9.25, // the file says something else
            ..Priors::default()
        };

        let diffs = non_mix_differences(&live, &file);
        assert_eq!(diffs.len(), 1, "{diffs:?}");
        assert!(diffs[0].starts_with("recency_tau_days:"), "{diffs:?}");

        // `mix_*` is what the comparison varies, so it must never be listed as
        // a difference — only the knobs that are baked into the cache.
        for d in non_mix_differences(
            &live,
            &Priors {
                mix_sim: 99.0,
                ..file
            },
        ) {
            assert!(!d.starts_with("mix_"), "{d}");
        }
    }

    /// Three vectors on one matrix: `run_fit_and_report` must survive carrying
    /// a reference through, including the non-mix warning path (the synthetic
    /// reference is a bare `Priors::default()`, which differs from the
    /// calibrated config in far more than twelve fields).
    #[test]
    fn the_reference_path_runs_on_a_synthetic_cache() {
        let cache = separating_cache();
        let priors = cfg().priors.clone();
        let w = normalised_mix_weights(&priors);
        let baseline = rescore(&cache, &priors, &w, 20, 50).average();
        let reference = ReferenceVector {
            label: "synthetic-with-drift".to_string(),
            priors: Priors {
                mix_sim: 1.0,
                ..Priors::default()
            },
        };
        run_fit_and_report(&cache, &baseline, false, Some(&reference));
    }
}
