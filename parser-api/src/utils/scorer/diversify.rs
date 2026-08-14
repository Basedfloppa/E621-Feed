//! MMR-style post-list diversification.
//!
//! Two entry points:
//! * [`diversify_scored_posts`] — owning, prod-side helper. Builds
//!   features on the fly from `Vec<ScoredPost>` and re-orders the full
//!   list. Used by `/recommendations`.
//! * [`diversify_indices`] — calibrate-side helper. Operates on
//!   pre-built [`DiversityFeatures`] + parallel arrays of `(score,
//!   interaction_fit)`, never clones a `Post`, and only runs MMR over
//!   the top-`head_limit` items by raw score (the tail keeps its score
//!   order). The grid loop calls this once per probe with features
//!   computed once at dataset prep.
//!
//! ## Semantic similarity (v5.11)
//!
//! By default MMR uses Jaccard similarity on 64-bit `SipHashes` of tag
//! names — exact-match only. When `diversity_semantic_blend > 0`, a
//! fraction of the similarity comes from PMI-based tag-pair association.
//! Tags that co-occur more often than chance (PMI > threshold) count as
//! a "soft match", catching related but different tags (e.g. `canine`
//! and `wolf`) that Jaccard would miss.
//!
//! Each group-level similarity becomes:
//!   `sim = (1 - blend) × Jaccard(hashes) + blend × PMI_match_ratio`
//!
//! The PMI match ratio caps at `diversity_semantic_max_tags` tags per
//! post per group to keep the O(T²) loop bounded.
//!
//! To minimise overhead, when `diversity_semantic_blend == 0` (the
//! default) the PMI computation is skipped entirely and the fast Jaccard
//! path runs — identical to the v5.10 behaviour.
//!
//! Memory: each [`DiversityFeatures`] holds three sorted `Vec<(u64,
//! Option<TagId>)>` of per-tag (hash, optional graph-id) pairs.
//! Collisions at 64-bit are negligible at the tag cardinalities
//! involved (≤ 10⁵ unique tags, ~10⁻¹⁵ collision probability per pair).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use rayon::prelude::*;

use crate::models::{Post, ScoredPost};
use crate::utils::tag_relation::TagRelationGraph;

use super::priors::Priors;
use super::util::{FEEDBACK_NEUTRAL, normalize_tag};

type TagId = u32;

/// Pre-computed fingerprints for one post. Each entry is a `(hash, tag_id)`
/// tuple — the hash is the `SipHash` of the lowercased tag name (for Jaccard),
/// and `tag_id` is the optional graph-interned id (for PMI-based semantic
/// similarity). Sorted by hash for O(n) merge-intersection.
#[derive(Clone)]
pub struct DiversityFeatures {
    artist: Vec<(u64, Option<TagId>)>,
    character: Vec<(u64, Option<TagId>)>,
    copyright: Vec<(u64, Option<TagId>)>,
    species: Vec<(u64, Option<TagId>)>,
    general: Vec<(u64, Option<TagId>)>,
}

impl DiversityFeatures {
    /// Build features for one post.
    ///
    /// `graph` is only used to resolve `TagId`s for PMI similarity; when
    /// `diversity_semantic_blend` is 0 in the calling code, the ids are
    /// still stored but never queried — the `HashMap` lookups per tag are
    /// negligible overhead compared to the rest of the scoring pipeline.
    #[must_use]
    pub fn from_post(p: &Post, graph: &TagRelationGraph) -> Self {
        Self {
            artist: hashed_tag_set(&p.tags.artist, 0, graph),
            character: hashed_tag_set(&p.tags.character, 1, graph),
            copyright: hashed_tag_set(&p.tags.copyright, 2, graph),
            species: hashed_tag_set(&p.tags.species, 3, graph),
            general: hashed_tag_set(&p.tags.general, 4, graph),
        }
    }
}

fn hash_tag(t: &str) -> u64 {
    let lc = normalize_tag(t);
    let mut h = DefaultHasher::new();
    lc.hash(&mut h);
    h.finish()
}

fn hashed_tag_set(
    tags: &[String],
    group: u8,
    graph: &TagRelationGraph,
) -> Vec<(u64, Option<TagId>)> {
    let mut out: Vec<(u64, Option<TagId>)> = tags
        .iter()
        .filter_map(|t| {
            let trimmed = t.trim();
            if trimmed.is_empty() {
                return None;
            }
            let h = hash_tag(trimmed);
            let tid = graph.tag_id(group, trimmed);
            Some((h, tid))
        })
        .collect();
    out.sort_by_key(|a| a.0);
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// Jaccard between two sorted-deduped slices via merge-intersection on
/// the hash component only (ignoring `TagId`). Exact-match similarity.
fn jaccard_hashes(a: &[(u64, Option<TagId>)], b: &[(u64, Option<TagId>)]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (mut i, mut j) = (0usize, 0usize);
    let mut inter = 0u32;
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Equal => {
                inter += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    let union = (a.len() + b.len()) as u32 - inter;
    if union == 0 {
        0.0
    } else {
        inter as f32 / union as f32
    }
}

/// PMI-based soft similarity between two tag sets. For every pair
/// `(tag_a, tag_b)` across the two posts (within the same group), compute
/// the pointwise mutual information and count pairs where PMI exceeds
/// `threshold` as "semantic matches".
///
/// `PMI(a,b) = ln(cooc(a,b) × N / (marginal(a) × marginal(b)))`
///
/// Returns the fraction of pairs that match — a score in [0, 1].
fn pmi_group_similarity(
    a: &[(u64, Option<TagId>)],
    b: &[(u64, Option<TagId>)],
    graph: &TagRelationGraph,
    threshold: f32,
    max_tags: usize,
) -> f32 {
    let a_tids: Vec<TagId> = a
        .iter()
        .filter_map(|(_, tid)| *tid)
        .take(max_tags)
        .collect();
    let b_tids: Vec<TagId> = b
        .iter()
        .filter_map(|(_, tid)| *tid)
        .take(max_tags)
        .collect();

    if a_tids.is_empty() || b_tids.is_empty() {
        return 0.0;
    }

    let n_posts = graph.n_posts();
    if n_posts <= 0 {
        return 0.0;
    }

    let n_posts_f = n_posts as f64;
    let threshold_f = f64::from(threshold);
    let mut matches = 0u32;
    let mut total = 0u32;

    for &ta in &a_tids {
        let ma = graph.marginal_by_id(ta);
        if ma <= 0 {
            continue;
        }
        for &tb in &b_tids {
            if ta == tb {
                // Exact id match — always counts as a semantic match.
                matches += 1;
                total += 1;
                continue;
            }
            total += 1;
            let cooc = graph.cooc_by_id(ta, tb);
            if cooc <= 0 {
                continue;
            }
            let mb = graph.marginal_by_id(tb);
            if mb <= 0 {
                continue;
            }
            let pmi = ((cooc as f64) * n_posts_f / ((ma as f64) * (mb as f64))).ln();
            if pmi > threshold_f {
                matches += 1;
            }
        }
    }

    if total == 0 {
        0.0
    } else {
        matches as f32 / total as f32
    }
}

/// Blended group-level similarity: Jaccard (exact tag-match) and PMI
/// (semantic soft-match). When `blend <= 0`, falls back to pure Jaccard.
///
/// Jaccard uses the pre-resolved `TagIds` from `graph` (global, consistent
/// ID mapping). PMI uses `user_graph` when provided and `user_pmi_weight`
/// is positive — capturing personalized tag co-occurrence so MMR diversity
/// personalises around per-user tag associations (e.g. a `skeb`+`canine`
/// co-favorite gets less MMR penalty for that specific pair).
#[allow(
    clippy::too_many_arguments,
    reason = "The scoring inputs are distinct model parameters; a context struct would obscure the call-site blend configuration."
)]
fn group_similarity(
    a: &[(u64, Option<TagId>)],
    b: &[(u64, Option<TagId>)],
    graph: &TagRelationGraph,
    user_graph: Option<&TagRelationGraph>,
    blend: f32,
    pmi_threshold: f32,
    user_pmi_weight: f32,
    max_tags: usize,
) -> f32 {
    if blend <= 0.0 {
        return jaccard_hashes(a, b);
    }
    let jac = jaccard_hashes(a, b);
    let graph_for_pmi = if user_graph.is_some() && user_pmi_weight > 1e-4 {
        user_graph.unwrap_or(graph)
    } else {
        graph
    };
    let pmi = pmi_group_similarity(a, b, graph_for_pmi, pmi_threshold, max_tags);
    // Apply user-PMI amplification: when diversity_user_pmi_weight > 1, the
    // user-graph PMI gets an extra boost proportional to how strongly those
    // tags co-occur in the user's favorites.
    let pmi_boost = if user_graph.is_some() && user_pmi_weight > 1e-4 {
        user_pmi_weight
    } else {
        1.0
    };
    (1.0 - blend) * jac + blend * pmi * pmi_boost
}

/// Per-pair diversification redundancy — the weighted sum of the five group
/// similarities between two posts (no MMR-power applied; that happens on the
/// window max inside `diversify_indices`). Deterministic given the pair, so it
/// is safe to memoize in a matrix.
fn pair_redundancy(
    a: &DiversityFeatures,
    b: &DiversityFeatures,
    graph: &TagRelationGraph,
    user_graph: Option<&TagRelationGraph>,
    priors: &Priors,
) -> f32 {
    let blend = priors.diversity_semantic_blend.clamp(0.0, 1.0);
    if blend <= 0.0 {
        // Fast path: pure Jaccard — no graph queries needed beyond
        // what was already done at feature-construction time.
        return jaccard_hashes(&a.artist, &b.artist) * priors.diversity_w_artist
            + jaccard_hashes(&a.character, &b.character) * priors.diversity_w_character
            + jaccard_hashes(&a.copyright, &b.copyright) * priors.diversity_w_copyright
            + jaccard_hashes(&a.species, &b.species) * priors.diversity_w_species
            + jaccard_hashes(&a.general, &b.general) * priors.diversity_w_general;
    }
    let pmi_threshold = priors.diversity_pmi_threshold;
    let user_pmi_weight = priors.diversity_user_pmi_weight;
    let max_tags = priors.diversity_semantic_max_tags.max(1);
    group_similarity(
        &a.artist,
        &b.artist,
        graph,
        user_graph,
        blend,
        pmi_threshold,
        user_pmi_weight,
        max_tags,
    ) * priors.diversity_w_artist
        + group_similarity(
            &a.character,
            &b.character,
            graph,
            user_graph,
            blend,
            pmi_threshold,
            user_pmi_weight,
            max_tags,
        ) * priors.diversity_w_character
        + group_similarity(
            &a.copyright,
            &b.copyright,
            graph,
            user_graph,
            blend,
            pmi_threshold,
            user_pmi_weight,
            max_tags,
        ) * priors.diversity_w_copyright
        + group_similarity(
            &a.species,
            &b.species,
            graph,
            user_graph,
            blend,
            pmi_threshold,
            user_pmi_weight,
            max_tags,
        ) * priors.diversity_w_species
        + group_similarity(
            &a.general,
            &b.general,
            graph,
            user_graph,
            blend,
            pmi_threshold,
            user_pmi_weight,
            max_tags,
        ) * priors.diversity_w_general
}

/// Index-based MMR re-ranker. Returns indices in their final order.
///
/// `entries[i] = (score, interaction_fit, tiebreak_id)` is parallel to
/// `features[i]`. `head_limit` caps how many top-by-score items
/// participate in MMR; everything past that keeps its raw-score
/// ordering. Pass `head_limit >= entries.len()` for full-list MMR
/// (legacy behaviour).
///
/// Performance: MMR recomputes the redundancy of every candidate against
/// every previously-selected item in the active window. Because the weighted
/// group similarity of a pair is deterministic, it is precomputed once into a
/// symmetric `head_n × head_n` matrix (in parallel), turning repeated
/// recomputation into O(1) reads. This is behaviour-identical to the previous
/// per-iteration recompute, but removes the `diversity_window` factor.
pub fn diversify_indices(
    entries: &[(f32, f32, i64)],
    features: &[DiversityFeatures],
    graph: &TagRelationGraph,
    user_graph: Option<&TagRelationGraph>,
    priors: &Priors,
    head_limit: usize,
) -> Vec<usize> {
    let n = entries.len();
    if n == 0 {
        return Vec::new();
    }
    debug_assert_eq!(n, features.len());

    let mut idx_by_score: Vec<usize> = (0..n).collect();
    idx_by_score.sort_by(|&a, &b| {
        entries[b]
            .0
            .partial_cmp(&entries[a].0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let head_n = head_limit.min(n);
    if head_n <= 1 {
        return idx_by_score;
    }
    let head_ids: Vec<usize> = idx_by_score[..head_n].to_vec();

    // Precompute the pairwise redundancy matrix once over the head set.
    let mut pair_sim: Vec<Vec<f32>> = vec![vec![0.0f32; head_n]; head_n];
    pair_sim.par_iter_mut().enumerate().for_each(|(pa, row)| {
        let a = &features[head_ids[pa]];
        for pb in 0..head_n {
            if pa == pb {
                continue;
            }
            row[pb] = pair_redundancy(a, &features[head_ids[pb]], graph, user_graph, priors);
        }
    });
    // actual-index → position-in-head lookup (sentinel `head_n` for tail items).
    let mut pos_of: Vec<usize> = vec![head_n; n];
    for (pos, &actual) in head_ids.iter().enumerate() {
        pos_of[actual] = pos;
    }

    let mut available: Vec<usize> = head_ids;
    let mut selected: Vec<usize> = Vec::with_capacity(head_n);
    let mut top_score = available
        .iter()
        .map(|&i| entries[i].0)
        .fold(f32::MIN, f32::max);

    let window = priors.diversity_window.max(1);
    let exp = priors.mmr_redundancy_exp;
    let damp = priors.diversity_interaction_damp.clamp(0.0, 1.0);
    let max_penalty = priors.diversity_max_penalty.clamp(0.0, 1.0);

    while !available.is_empty() {
        let mut best_pos = 0usize;
        let mut best_value = f32::MIN;
        let mut best_tiebreak = i64::MAX;

        for (pos, &i) in available.iter().enumerate() {
            let pi = pos_of[i];
            // Max redundancy over the active window of already-selected items.
            let mut max_sim = 0.0f32;
            for &sel in selected.iter().rev().take(window) {
                let v = pair_sim[pi][pos_of[sel]];
                if v > max_sim {
                    max_sim = v;
                }
            }
            let redundancy = if (exp - 1.0).abs() < 1e-3 {
                max_sim
            } else {
                max_sim.max(0.0).powf(exp.clamp(0.1, 5.0)).clamp(0.0, 1.0)
            };
            let (score, interaction, tid) = entries[i];
            let gap = (top_score - score).max(0.0);
            let penalty = (redundancy * gap * (1.0 - damp * interaction)).clamp(0.0, max_penalty);
            let adj = score - penalty;
            if adj > best_value || (adj == best_value && tid < best_tiebreak) {
                best_value = adj;
                best_pos = pos;
                best_tiebreak = tid;
            }
        }

        let chosen_idx = available.swap_remove(best_pos);
        let removed_was_top = (entries[chosen_idx].0 - top_score).abs() < 1e-6;
        selected.push(chosen_idx);
        if removed_was_top && !available.is_empty() {
            top_score = available
                .iter()
                .map(|&i| entries[i].0)
                .fold(f32::MIN, f32::max);
        }
    }

    selected.extend(idx_by_score[head_n..].iter().copied());
    selected
}

/// Owning re-rank used by the production `/recommendations` route.
/// Builds [`DiversityFeatures`] on the fly and runs MMR over the whole
/// list (no top-K cutoff) to preserve historical behaviour.
/// Owning re-rank used by the production `/recommendations` route.
/// Builds [`DiversityFeatures`] on the fly and runs MMR over the whole
/// list (no top-K cutoff) to preserve historical behaviour.
///
/// When `user_graph` is provided, PMI-based soft similarity (controlled
/// by `diversity_semantic_blend`) uses the user's tag co-occurrence
/// statistics instead of the global graph, allowing the diversity pass
/// to personalise around per-account tag associations.
pub fn diversify_scored_posts(
    posts: Vec<ScoredPost>,
    graph: &TagRelationGraph,
    user_graph: Option<&TagRelationGraph>,
    priors: &Priors,
) -> Vec<ScoredPost> {
    if posts.is_empty() {
        return posts;
    }
    // Build `DiversityFeatures` against the SAME graph that the PMI pass will
    // query. `group_similarity` uses the user graph (when supplied and weighted)
    // as `graph_for_pmi`; the stored `TagId`s are interned per graph instance in
    // insertion order, so resolving them from the global graph and then querying
    // the user graph produced garbage similarity (each graph assigns different
    // `u32` ids to the same tag). Resolving from the graph that will actually be
    // queried keeps the id namespace consistent.
    let use_user_for_pmi = user_graph.is_some() && priors.diversity_user_pmi_weight > 1e-4;
    let feature_graph = if use_user_for_pmi {
        user_graph.unwrap_or(graph)
    } else {
        graph
    };
    let features: Vec<DiversityFeatures> = posts
        .iter()
        .map(|sp| DiversityFeatures::from_post(&sp.post, feature_graph))
        .collect();
    let entries: Vec<(f32, f32, i64)> = posts
        .iter()
        .map(|sp| {
            let interaction = sp
                .breakdown
                .as_ref()
                .map_or(FEEDBACK_NEUTRAL, |b| b.interaction_fit);
            (sp.score, interaction, sp.post.id)
        })
        .collect();

    let order = diversify_indices(&entries, &features, graph, user_graph, priors, posts.len());

    let mut slots: Vec<Option<ScoredPost>> = posts.into_iter().map(Some).collect();
    let mut out: Vec<ScoredPost> = Vec::with_capacity(slots.len());
    for i in order {
        if let Some(sp) = slots[i].take() {
            out.push(sp);
        }
    }
    // Apply diversity quota as a final pass.
    enforce_diversity_quota(&mut out);
    out
}

/// Post-MMR diversity quota: ensure the top-K window holds at least a
/// minimum number of distinct primary artists / characters. This is a
/// *minimum* guarantee, not a dedup — when MMR already produced a diverse
/// top-K the function is a no-op and the MMR order is left untouched. The
/// quota only fires for degenerate windows (e.g. all top results from a
/// single artist), in which case diverse posts are promoted from below the
/// window.
fn enforce_diversity_quota(scored: &mut [ScoredPost]) {
    const MIN_ARTISTS: usize = 2;
    const MIN_CHARACTERS: usize = 3;

    let top_k = 20usize.min(scored.len());
    if top_k < 4 {
        return;
    }

    enforce_group_quota(scored, top_k, MIN_ARTISTS, |sp| {
        sp.post.tags.artist.first().map(|a| a.to_ascii_lowercase())
    });
    enforce_group_quota(scored, top_k, MIN_CHARACTERS, |sp| {
        sp.post
            .tags
            .character
            .first()
            .map(|c| c.to_ascii_lowercase())
    });
}

/// Ensure at least `min_distinct` distinct `key` values appear among the
/// first `top_k` posts. When the window falls short, posts with a fresh
/// key are promoted from below the window, each swapped with the
/// lowest-ranked redundant in-window post so the fewest possible MMR
/// positions are disturbed.
///
/// Terminates in at most `min_distinct` promotions — every iteration
/// either adds a distinct key or breaks, so it can never loop (unlike the
/// previous swap-and-re-evaluate implementation, which could oscillate two
/// posts forever).
fn enforce_group_quota(
    scored: &mut [ScoredPost],
    top_k: usize,
    min_distinct: usize,
    key: impl Fn(&ScoredPost) -> Option<String>,
) {
    // Distinct named keys already inside the window, plus the in-window
    // slots that are demotable: posts repeating an earlier key, or posts
    // with no key at all. Collected front-to-back so `pop()` yields the
    // lowest-ranked redundant slot first.
    let mut seen: Vec<String> = Vec::new();
    let mut redundant: Vec<usize> = Vec::new();
    for (i, sp) in scored.iter().enumerate().take(top_k) {
        match key(sp) {
            Some(k) if !seen.contains(&k) => seen.push(k),
            _ => redundant.push(i),
        }
    }
    if seen.len() >= min_distinct {
        return; // quota already satisfied — leave the MMR order alone
    }

    // Pull posts with a not-yet-seen key up from below the window.
    let mut next_below = top_k;
    while seen.len() < min_distinct {
        let Some(j) = (next_below..scored.len())
            .find(|&j| key(&scored[j]).is_some_and(|k| !seen.contains(&k)))
        else {
            break; // no more diverse posts available — best effort
        };
        next_below = j + 1;
        let Some(slot) = redundant.pop() else {
            break; // nothing redundant left to evict — quota physically unmet
        };
        scored.swap(slot, j);
        if let Some(k) = key(&scored[slot]) {
            seen.push(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Files, Flags, Has, Post, Rating, Relationships, ScoredPost, Stats, Tags};
    use crate::utils::tag_relation::TagRelationGraph;
    use chrono::Utc;

    /// Minimal `Post` for diversity tests — only `id`, `tags.artist` and
    /// `tags.character` feed the quota logic; everything else is a neutral
    /// placeholder.
    fn post(id: i64, artists: &[&str], characters: &[&str]) -> Post {
        Post {
            id,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            change_seq: 0.0,
            files: Files::default(),
            uploader_id: 0,
            uploader_name: None,
            approver_id: None,
            stats: Stats::default(),
            flags: Flags::default(),
            has: Has::default(),
            relationships: Relationships::default(),
            pools: vec![],
            rating: Rating::S,
            locked_tags: vec![],
            sources: vec![],
            description: None,
            tags: Tags {
                artist: artists
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
                character: characters
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
                ..Tags::default()
            },
        }
    }

    fn scored(id: i64, artists: &[&str], characters: &[&str]) -> ScoredPost {
        ScoredPost {
            post: post(id, artists, characters),
            score: 1.0,
            breakdown: None,
            reasons: Vec::new(),
        }
    }

    fn ids(posts: &[ScoredPost]) -> Vec<i64> {
        posts.iter().map(|sp| sp.post.id).collect()
    }

    /// The quota must only ever re-order — never lose or duplicate a post.
    fn assert_permutation_of(posts: &[ScoredPost], expected: &[i64]) {
        let mut got = ids(posts);
        let mut want = expected.to_vec();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want, "quota must be a permutation of its input");
    }

    fn distinct_artists(posts: &[ScoredPost], window: usize) -> usize {
        let mut set: Vec<String> = Vec::new();
        for sp in posts.iter().take(window) {
            if let Some(a) = sp.post.tags.artist.first() {
                let a = a.to_ascii_lowercase();
                if !set.contains(&a) {
                    set.push(a);
                }
            }
        }
        set.len()
    }

    fn distinct_characters(posts: &[ScoredPost], window: usize) -> usize {
        let mut set: Vec<String> = Vec::new();
        for sp in posts.iter().take(window) {
            if let Some(c) = sp.post.tags.character.first() {
                let c = c.to_ascii_lowercase();
                if !set.contains(&c) {
                    set.push(c);
                }
            }
        }
        set.len()
    }

    // ── enforce_group_quota — core logic ────────────────────────────────

    /// A window that already meets the quota is left byte-for-byte intact.
    #[test]
    fn group_quota_noop_when_satisfied() {
        let mut posts = vec![
            scored(0, &["a"], &[]),
            scored(1, &["b"], &[]),
            scored(2, &["c"], &[]),
            scored(3, &["d"], &[]),
        ];
        enforce_group_quota(&mut posts, 4, 2, |sp| sp.post.tags.artist.first().cloned());
        assert_eq!(
            ids(&posts),
            vec![0, 1, 2, 3],
            "satisfied quota must not re-order"
        );
    }

    /// An all-one-artist window pulls a single diverse post up from below,
    /// landing it in the lowest-ranked redundant slot.
    #[test]
    fn group_quota_promotes_one_diverse_post() {
        let mut posts = vec![
            scored(0, &["a"], &[]),
            scored(1, &["a"], &[]),
            scored(2, &["a"], &[]),
            scored(3, &["a"], &[]),
            scored(4, &["b"], &[]), // below window
            scored(5, &["c"], &[]), // below window
            scored(6, &["a"], &[]), // below window
        ];
        enforce_group_quota(&mut posts, 4, 2, |sp| sp.post.tags.artist.first().cloned());

        // `b` (id 4) is swapped into slot 3 — the lowest-ranked redundant
        // slot — and the displaced `a` (id 3) drops to slot 4.
        assert_eq!(posts[3].post.id, 4);
        assert_eq!(posts[3].post.tags.artist, vec!["b".to_string()]);
        assert_eq!(posts[4].post.id, 3);
        assert_eq!(distinct_artists(&posts, 4), 2);
        assert_permutation_of(&posts, &[0, 1, 2, 3, 4, 5, 6]);
    }

    /// Multiple promotions run until the minimum distinct count is met,
    /// each demoting the next lowest-ranked redundant slot.
    #[test]
    fn group_quota_promotes_until_minimum_met() {
        let mut posts = vec![
            scored(0, &["a"], &[]),
            scored(1, &["a"], &[]),
            scored(2, &["a"], &[]),
            scored(3, &["a"], &[]),
            scored(4, &["b"], &[]),
            scored(5, &["c"], &[]),
            scored(6, &["d"], &[]),
            scored(7, &["a"], &[]),
        ];
        enforce_group_quota(&mut posts, 4, 3, |sp| sp.post.tags.artist.first().cloned());

        assert_eq!(posts[3].post.id, 4, "first promotion fills slot 3");
        assert_eq!(posts[2].post.id, 5, "second promotion fills slot 2");
        assert_eq!(distinct_artists(&posts, 4), 3);
        assert_permutation_of(&posts, &[0, 1, 2, 3, 4, 5, 6, 7]);
    }

    /// When no diverse post exists the quota does its best and returns —
    /// no panic, no loop, order untouched.
    #[test]
    fn group_quota_best_effort_when_no_diversity_available() {
        let mut posts = vec![
            scored(0, &["a"], &[]),
            scored(1, &["a"], &[]),
            scored(2, &["a"], &[]),
            scored(3, &["a"], &[]),
            scored(4, &["a"], &[]),
            scored(5, &["a"], &[]),
        ];
        enforce_group_quota(&mut posts, 4, 3, |sp| sp.post.tags.artist.first().cloned());
        assert_eq!(ids(&posts), vec![0, 1, 2, 3, 4, 5]);
    }

    /// Posts with no tag in the group key produce `None` and are treated as
    /// demotable filler — they never panic and never count toward the quota.
    #[test]
    fn group_quota_handles_missing_keys() {
        let mut posts = vec![
            scored(0, &[], &[]), // None
            scored(1, &["a"], &[]),
            scored(2, &[], &[]), // None
            scored(3, &["a"], &[]),
            scored(4, &["b"], &[]), // below window
            scored(5, &["c"], &[]), // below window
        ];
        enforce_group_quota(&mut posts, 4, 2, |sp| sp.post.tags.artist.first().cloned());

        assert_eq!(
            posts[3].post.id, 4,
            "diverse `b` fills the last redundant slot"
        );
        assert_eq!(distinct_artists(&posts, 4), 2);
        assert_permutation_of(&posts, &[0, 1, 2, 3, 4, 5]);
    }

    /// `redundant` running dry before the quota is met must break cleanly.
    #[test]
    fn group_quota_stops_when_no_redundant_slot_left() {
        let mut posts = vec![
            scored(0, &["a"], &[]),
            scored(1, &["b"], &[]),
            scored(2, &["c"], &[]),
            scored(3, &["d"], &[]),
            scored(4, &["e"], &[]), // below window
        ];
        // min 5 distinct, but the window has no redundant slot to evict.
        enforce_group_quota(&mut posts, 4, 5, |sp| sp.post.tags.artist.first().cloned());
        assert_eq!(
            ids(&posts),
            vec![0, 1, 2, 3, 4],
            "no redundant slot — left intact"
        );
    }

    // ── enforce_diversity_quota — integration (top_k = 20) ──────────────

    /// Lists shorter than 4 are below the quota's minimum window and pass
    /// straight through.
    #[test]
    fn diversity_quota_noop_on_short_list() {
        let mut posts = vec![
            scored(0, &["a"], &["x"]),
            scored(1, &["a"], &["x"]),
            scored(2, &["a"], &["x"]),
        ];
        enforce_diversity_quota(&mut posts);
        assert_eq!(ids(&posts), vec![0, 1, 2]);
    }

    /// A top-20 that already holds plenty of distinct artists and
    /// characters is left exactly as MMR ordered it.
    #[test]
    fn diversity_quota_noop_when_top_k_already_diverse() {
        let mut posts: Vec<ScoredPost> = (0..22)
            .map(|i| {
                let a = format!("artist{i}");
                let c = format!("char{i}");
                scored(i, &[a.as_str()], &[c.as_str()])
            })
            .collect();
        let before = ids(&posts);
        enforce_diversity_quota(&mut posts);
        assert_eq!(ids(&posts), before, "diverse top-K must keep its MMR order");
    }

    /// Artist quota: a top-20 monopolised by one artist pulls a second
    /// artist up from below the window.
    #[test]
    fn diversity_quota_enforces_artist_minimum() {
        let mut posts: Vec<ScoredPost> = (0..24)
            .map(|i| {
                // Distinct characters everywhere → character quota is a no-op,
                // isolating artist-quota behaviour.
                let c = format!("char{i}");
                let a = if i == 20 { "bob" } else { "alice" };
                scored(i, &[a], &[c.as_str()])
            })
            .collect();
        enforce_diversity_quota(&mut posts);

        assert!(
            distinct_artists(&posts, 20) >= 2,
            "top-20 must hold ≥2 artists"
        );
        assert_eq!(posts[19].post.id, 20, "`bob` promoted into the window");
        assert_eq!(posts[19].post.tags.artist, vec!["bob".to_string()]);
        assert_permutation_of(&posts, &(0..24).collect::<Vec<_>>());
    }

    /// Character quota: a top-20 monopolised by one character pulls two
    /// more characters up to reach the minimum of three.
    #[test]
    fn diversity_quota_enforces_character_minimum() {
        let mut posts: Vec<ScoredPost> = (0..24)
            .map(|i| {
                // Distinct artists everywhere → artist quota is a no-op.
                let a = format!("artist{i}");
                let c = match i {
                    20 => "villain",
                    21 => "rogue",
                    _ => "hero",
                };
                scored(i, &[a.as_str()], &[c])
            })
            .collect();
        enforce_diversity_quota(&mut posts);

        assert!(
            distinct_characters(&posts, 20) >= 3,
            "top-20 must hold ≥3 characters"
        );
        assert_eq!(posts[19].post.id, 20, "`villain` promoted first");
        assert_eq!(posts[18].post.id, 21, "`rogue` promoted second");
        assert_permutation_of(&posts, &(0..24).collect::<Vec<_>>());
    }

    /// Artist matching is case-insensitive: "Alice"/"alice"/"ALICE" count
    /// as one artist, so the window still triggers a promotion.
    #[test]
    fn diversity_quota_artist_match_is_case_insensitive() {
        let mut posts: Vec<ScoredPost> = (0..24)
            .map(|i| {
                let c = format!("char{i}");
                let a = match i % 3 {
                    _ if i == 20 => "bob",
                    0 => "Alice",
                    1 => "alice",
                    _ => "ALICE",
                };
                scored(i, &[a], &[c.as_str()])
            })
            .collect();
        enforce_diversity_quota(&mut posts);

        // If case were significant the top-20 would look fully diverse and
        // nothing would move; the promotion proves case-folding.
        assert_eq!(posts[19].post.id, 20);
        assert!(posts[19].post.tags.artist[0].eq_ignore_ascii_case("bob"));
    }

    /// Regression for the infinite loop in the pre-fix swap-and-re-evaluate
    /// implementation: a list whose top-K (and tail) are dominated by a
    /// couple of repeated artists/characters used to oscillate two posts
    /// forever. Run on a worker thread and fail loudly if it does not
    /// return promptly, rather than hanging the test binary.
    #[test]
    fn regression_terminates_on_artist_heavy_list() {
        let input: Vec<ScoredPost> = (0..200)
            .map(|i| {
                let artist = if i % 2 == 0 { "alice" } else { "bob" };
                let character = if i % 3 == 0 { "hero" } else { "rival" };
                scored(i, &[artist], &[character])
            })
            .collect();
        let expected = ids(&input);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut posts = input;
            enforce_diversity_quota(&mut posts);
            let _ = tx.send(posts);
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("enforce_diversity_quota did not return within 5s — infinite-loop regression");

        assert_permutation_of(&result, &expected);
    }

    // ── jaccard_hashes ──────────────────────────────────────────────────

    #[test]
    fn jaccard_hashes_empty_sides() {
        assert_eq!(jaccard_hashes(&[], &[]), 0.0);
        assert_eq!(jaccard_hashes(&[(1, None)], &[]), 0.0);
        assert_eq!(jaccard_hashes(&[], &[(1, None)]), 0.0);
    }

    #[test]
    fn jaccard_hashes_identical() {
        let a = vec![(1, None), (2, None), (3, None)];
        assert!((jaccard_hashes(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn jaccard_hashes_partial_overlap() {
        let a = vec![(1, None), (2, None), (3, None)];
        let b = vec![(2, None), (3, None), (4, None)];
        // intersection = {2, 3} = 2, union = {1,2,3,4} = 4
        assert!((jaccard_hashes(&a, &b) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn jaccard_hashes_no_overlap() {
        let a = vec![(1, None), (2, None)];
        let b = vec![(3, None), (4, None)];
        assert_eq!(jaccard_hashes(&a, &b), 0.0);
    }

    // ── pmi_group_similarity (empty graph) ──────────────────────────────

    #[test]
    fn pmi_group_similarity_zero_on_empty_graph() {
        let graph = TagRelationGraph::empty();
        let a = vec![(1, Some(0)), (2, Some(1))];
        let b = vec![(3, Some(2)), (4, Some(3))];
        let score = pmi_group_similarity(&a, &b, &graph, 0.0, 10);
        assert_eq!(score, 0.0, "empty graph has n_posts=0 -> 0.0");
    }

    // ── pmi_group_similarity (populated graph) ──────────────────────────

    #[test]
    fn pmi_group_similarity_matches_high_pmi_pairs() {
        let mut graph = TagRelationGraph::with_posts(100);
        // "dog" appears in 50 posts, "canine" in 40, they co-occur in 35.
        graph.set_marginal(0, "dog", 50);
        graph.set_marginal(0, "canine", 40);
        graph.set_marginal(0, "cat", 30);
        graph.set_marginal(0, "feline", 20);
        // dog-canine cooc=35 -> PMI = ln(35*100/(50*40)) = ln(1.75) ≈ 0.56
        graph.insert_pair(0, "dog", 0, "canine", 35);
        // dog-cat cooc=5 -> PMI = ln(5*100/(50*30)) = ln(0.33) ≈ -1.10
        graph.insert_pair(0, "dog", 0, "cat", 5);

        let tid_dog = graph.tag_id(0, "dog").unwrap();
        let tid_canine = graph.tag_id(0, "canine").unwrap();
        let tid_cat = graph.tag_id(0, "cat").unwrap();

        let a = vec![(0, Some(tid_dog))];
        let b = vec![(1, Some(tid_canine))];
        let c = vec![(2, Some(tid_cat))];

        // dog vs canine: PMI ≈ 0.56 > 0.0 threshold -> match
        let score = pmi_group_similarity(&a, &b, &graph, 0.0, 10);
        assert!(
            (score - 1.0).abs() < 1e-6,
            "dog-canine should match at threshold 0.0, got {score}"
        );

        // dog vs cat: PMI ≈ -1.10 < 0.0 threshold -> no match
        let score = pmi_group_similarity(&a, &c, &graph, 0.0, 10);
        assert_eq!(
            score, 0.0,
            "dog-cat should not match at threshold 0.0, got {score}"
        );
    }

    // ── group_similarity blend ──────────────────────────────────────────

    #[test]
    fn group_similarity_blend_zero_returns_jaccard() {
        let graph = TagRelationGraph::empty();
        let a = vec![(1, None), (2, None)];
        let b = vec![(2, None), (3, None)];
        // Jaccard = 1/3 ≈ 0.333
        let jac = jaccard_hashes(&a, &b);
        let blended = group_similarity(&a, &b, &graph, None, 0.0, 0.0, 1.0, 10);
        assert!(
            (blended - jac).abs() < 1e-6,
            "blend=0 should equal jaccard: {blended} vs {jac}"
        );
    }

    /// When `diversity_semantic_blend` > 0 and `user_graph` is provided,
    /// PMI queries should use the user graph's co-occurrence stats
    /// instead of the global graph's.
    #[test]
    fn group_similarity_uses_user_graph_for_pmi() {
        // Build a global graph with a low-cooc pair (low PMI).
        let mut global = TagRelationGraph::with_posts(100);
        global.set_marginal(0, "tagA", 50);
        global.set_marginal(0, "tagB", 50);
        global.insert_pair(0, "tagA", 0, "tagB", 2); // cooc=2 → PMI ≈ ln(0.04) ≈ -3.2

        // Build a user graph with high co-occurrence (high PMI).
        let mut user = TagRelationGraph::with_posts(100);
        user.set_marginal(0, "tagA", 50);
        user.set_marginal(0, "tagB", 50);
        user.insert_pair(0, "tagA", 0, "tagB", 40); // cooc=40 → PMI ≈ ln(3.2) ≈ 1.16

        let tid_a = global.tag_id(0, "tagA").unwrap();
        let tid_b = global.tag_id(0, "tagB").unwrap();

        let a = vec![(hash_tag("tagA"), Some(tid_a))];
        let b = vec![(hash_tag("tagB"), Some(tid_b))];

        // With blend=1.0 and PMI threshold=0, user_graph should yield
        // high PMI (40 cooc) while global yields low PMI (2 cooc).
        let with_user = group_similarity(&a, &b, &global, Some(&user), 1.0, 0.0, 2.0, 10);
        let without_user = group_similarity(&a, &b, &global, None, 1.0, 0.0, 1.0, 10);
        assert!(
            with_user > without_user,
            "user_graph PMI ({with_user}) should exceed global PMI ({without_user})"
        );
    }

    // ── DiversityFeatures::from_post (empty graph) ──────────────────────

    #[test]
    fn from_post_empty_graph_produces_some_features() {
        let graph = TagRelationGraph::empty();
        let p = post(1, &["artist_a"], &["char_x"]);
        let feats = DiversityFeatures::from_post(&p, &graph);
        assert_eq!(feats.artist.len(), 1, "should have one artist tag");
        assert_eq!(feats.character.len(), 1, "should have one character tag");
        assert!(feats.copyright.is_empty());
        assert!(feats.species.is_empty());
        assert!(feats.general.is_empty());
    }

    // ── memoized MMR ≡ naive per-iteration recompute ─────────────────────

    /// Reference implementation mirroring the pre-optimisation algorithm:
    /// recompute each candidate's redundancy against the window from scratch
    /// on every iteration (no matrix). Used to prove the memoized path
    /// produces bit-identical ordering (i.e. no quality regression).
    fn mmr_redundancy_recompute(
        cand: &DiversityFeatures,
        selected: &[usize],
        features: &[DiversityFeatures],
        graph: &TagRelationGraph,
        user_graph: Option<&TagRelationGraph>,
        priors: &Priors,
    ) -> f32 {
        let window = priors.diversity_window.max(1);
        let mut max_sim = 0.0f32;
        for &i in selected.iter().rev().take(window) {
            let s = pair_redundancy(cand, &features[i], graph, user_graph, priors);
            if s > max_sim {
                max_sim = s;
            }
        }
        let exp = priors.mmr_redundancy_exp;
        if (exp - 1.0).abs() < 1e-3 {
            max_sim
        } else {
            max_sim.max(0.0).powf(exp.clamp(0.1, 5.0)).clamp(0.0, 1.0)
        }
    }

    fn naive_mmr_indices(
        entries: &[(f32, f32, i64)],
        features: &[DiversityFeatures],
        graph: &TagRelationGraph,
        user_graph: Option<&TagRelationGraph>,
        priors: &Priors,
    ) -> Vec<usize> {
        let n = entries.len();
        let mut idx_by_score: Vec<usize> = (0..n).collect();
        idx_by_score.sort_by(|&a, &b| {
            entries[b]
                .0
                .partial_cmp(&entries[a].0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut available: Vec<usize> = idx_by_score;
        let mut selected: Vec<usize> = Vec::with_capacity(n);
        let mut top_score = available
            .iter()
            .map(|&i| entries[i].0)
            .fold(f32::MIN, f32::max);
        let damp = priors.diversity_interaction_damp.clamp(0.0, 1.0);
        let max_penalty = priors.diversity_max_penalty.clamp(0.0, 1.0);
        while !available.is_empty() {
            let mut best_pos = 0usize;
            let mut best_value = f32::MIN;
            let mut best_tiebreak = i64::MAX;
            for (pos, &i) in available.iter().enumerate() {
                let (score, interaction, tid) = entries[i];
                let redundancy = mmr_redundancy_recompute(
                    &features[i],
                    &selected,
                    features,
                    graph,
                    user_graph,
                    priors,
                );
                let gap = (top_score - score).max(0.0);
                let penalty =
                    (redundancy * gap * (1.0 - damp * interaction)).clamp(0.0, max_penalty);
                let adj = score - penalty;
                if adj > best_value || (adj == best_value && tid < best_tiebreak) {
                    best_value = adj;
                    best_pos = pos;
                    best_tiebreak = tid;
                }
            }
            let chosen = available.swap_remove(best_pos);
            selected.push(chosen);
            let removed_was_top = (entries[chosen].0 - top_score).abs() < 1e-6;
            if removed_was_top && !available.is_empty() {
                top_score = available
                    .iter()
                    .map(|&i| entries[i].0)
                    .fold(f32::MIN, f32::max);
            }
        }
        selected
    }

    /// Full priors with every field populated (Priors has no `Default`).
    fn test_priors() -> Priors {
        let mut p = Priors {
            now: Utc::now(),
            recency_tau_days: 10.0,
            quality_a: 0.50,
            quality_b: 0.20,
            quality_log_bias: -3.0,
            mix_sim: 0.603,
            mix_quality: 0.017,
            mix_recency: 0.017,
            mix_rating: 0.034,
            mix_media: 0.042,
            mix_popularity: 0.017,
            mix_interaction: 0.084,
            mix_tag_relation: 0.067,
            mix_uploader: 0.05,
            mix_exclusivity: 0.02,
            mix_novelty: 0.02,
            mix_artist_discovery: 0.03,
            idf_lambda: 1.0,
            idf_alpha: 1.05,
            freq_alpha: 0.95,
            quality_w_absolute: 0.55,
            quality_w_relative_score: 0.30,
            quality_w_relative_comments: 0.15,
            quality_c: 0.3,
            popularity_w_fav: 0.80,
            popularity_w_duration: 0.20,
            recency_w_global: 0.40,
            recency_w_personal: 0.60,
            tag_relation_w_global: 0.4,
            tag_relation_w_personal: 0.6,
            tag_relation_pmi_scale: 3.5,
            tag_relation_min_cooc: 2,
            tag_relation_user_min_cooc: 1,
            tag_relation_cooc_ref: 16.0,
            tag_relation_user_cooc_ref: 5.0,
            tag_relation_max_tags: 20,
            tag_relation_pair_aggregator: "mean".to_string(),
            diversity_window: 8,
            diversity_w_artist: 0.2,
            diversity_w_character: 0.2,
            diversity_w_copyright: 0.2,
            diversity_w_species: 0.2,
            diversity_w_general: 0.2,
            discrete_smoothing_alpha: 1.0,
            strong_negative_count: 3,
            strong_negative_penalty: 0.40,
            strong_negative_wilson_threshold: 0.55,
            recency_personal_floor_frac: 1.0,
            recency_log_personal: true,
            feedback_decay_half_life_days: 90.0,
            meta_interaction_weight: 0.3,
            coldstart_n0: 25.0,
            discrete_pref_floor: 0.05,
            diversity_max_penalty: 0.45,
            diversity_interaction_damp: 0.35,
            df_floor: 0.40,
            idf_max: 100.0,
            bm25_k: 2.25,
            one_sided_ratio_exp: 0.5,
            coldstart_smoothing_boost: 2.0,
            interaction_ctr_prior_alpha: 4.0,
            idf_rsj_smoothing: 0.35,
            group_w_artist: 2.40,
            group_w_character: 2.00,
            group_w_copyright: 1.45,
            group_w_species: 1.30,
            group_w_general: 0.60,
            group_w_lore: 0.40,
            score_temperature: 0.0,
            confidence_steepness: 1.0,
            mmr_redundancy_exp: 1.0,
            tag_sim_jaccard_blend: 0.0,
            idf_lambda_meta: f32::NAN,
            tag_relation_pmi_scale_user: f32::NAN,
            recency_tau_recent: f32::NAN,
            recency_split_age_days: 30.0,
            recency_tau_hot: f32::NAN,
            recency_split_age_hours: 24.0,
            exploration_epsilon: 0.0,
            uploader_n0: 5.0,
            uploader_w_avg_score: 0.6,
            uploader_w_avg_fav: 0.4,
            min_exclusivity_cooc: 2,
            exclusivity_scale: 0.5,
            exclusivity_max_tags: 15,
            exclusivity_cross_group_weight: 0.5,
            novelty_n0: 3.0,
            novelty_use_feedback: true,
            diversity_semantic_blend: 0.05,
            diversity_pmi_threshold: 0.5,
            diversity_semantic_max_tags: 10,
            diversity_user_pmi_weight: 1.0,
            artist_discovery_n0: 3.0,
            artist_discovery_novelty_bonus: 0.2,
        };
        p.diversity_window = 8;
        p
    }

    /// Build features/posts whose tags are registered (with co-occurring pairs)
    /// in both a global and a user graph so the PMI soft-match path is exercised.
    type Harness = (
        Vec<(f32, f32, i64)>,
        Vec<DiversityFeatures>,
        TagRelationGraph,
        TagRelationGraph,
        Priors,
    );
    #[allow(clippy::type_complexity)]
    fn equivalence_harness(n: i32) -> Harness {
        let mut global = TagRelationGraph::with_posts(1000);
        let mut user = TagRelationGraph::with_posts(500);
        let all_tags: Vec<String> = (0..16).map(|k| format!("tag{k}")).collect();
        for t in &all_tags {
            global.set_marginal(4, t, 60);
            user.set_marginal(4, t, 40);
        }
        for k in 0..16i64 {
            for off in 1..4 {
                let j = k + off;
                if j < 16 {
                    global.insert_pair(4, &format!("tag{k}"), 4, &format!("tag{j}"), 30);
                    user.insert_pair(4, &format!("tag{k}"), 4, &format!("tag{j}"), 25);
                }
            }
        }
        let graph = TagRelationGraph::with_posts(0); // unused, keep API identical
        let mut features = Vec::with_capacity(n as usize);
        let mut entries = Vec::with_capacity(n as usize);
        for i in 0..n {
            let general: Vec<String> = (0..5)
                .map(|k| format!("tag{}", (i as usize * 7 + k * 3) % 16))
                .collect();
            let p = Post {
                id: i as i64,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                change_seq: 0.0,
                files: Files::default(),
                uploader_id: 0,
                uploader_name: None,
                approver_id: None,
                stats: Stats::default(),
                flags: crate::models::Flags::default(),
                has: crate::models::Has::default(),
                relationships: crate::models::Relationships::default(),
                pools: vec![],
                rating: crate::models::Rating::Q,
                locked_tags: vec![],
                sources: vec![],
                description: None,
                tags: crate::models::Tags {
                    artist: vec![format!("artist{}", i % 5)],
                    character: vec![],
                    copyright: vec![],
                    species: vec![],
                    general,
                    invalid: vec![],
                    meta: vec![],
                    lore: vec![],
                    contributor: vec![],
                },
            };
            features.push(DiversityFeatures::from_post(&p, &user));
            entries.push(((i as f32) / n as f32, 0.1, i as i64));
        }
        (entries, features, graph, user, test_priors())
    }

    #[test]
    fn memoized_mmr_ordering_identical_to_naive() {
        let (entries, features, graph, user, priors) = equivalence_harness(30);
        let memoized = diversify_indices(
            &entries,
            &features,
            &graph,
            Some(&user),
            &priors,
            entries.len(),
        );
        let naive = naive_mmr_indices(&entries, &features, &graph, Some(&user), &priors);
        assert_eq!(memoized, naive, "memoized MMR must preserve exact ordering");
        assert!(memoized.len() == entries.len());
        let mut sorted = memoized.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            (0..entries.len()).collect::<Vec<_>>(),
            "must be a permutation"
        );
    }
}
