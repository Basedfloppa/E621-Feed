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
//! By default MMR uses Jaccard similarity on 64-bit SipHashes of tag
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

use crate::models::{Post, ScoredPost};
use crate::utils::tag_relation::TagRelationGraph;

use super::priors::Priors;
use super::util::{normalize_tag, FEEDBACK_NEUTRAL};

type TagId = u32;

/// Pre-computed fingerprints for one post. Each entry is a `(hash, tag_id)`
/// tuple — the hash is the SipHash of the lowercased tag name (for Jaccard),
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
    /// still stored but never queried — the HashMap lookups per tag are
    /// negligible overhead compared to the rest of the scoring pipeline.
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
/// the hash component only (ignoring TagId). Exact-match similarity.
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
    let threshold_f = threshold as f64;
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
/// Jaccard uses the pre-resolved TagIds from `graph` (global, consistent
/// ID mapping). PMI uses `user_graph` when provided and `user_pmi_weight`
/// is positive — capturing personalized tag co-occurrence so MMR diversity
/// personalises around per-user tag associations (e.g. a `skeb`+`canine`
/// co-favorite gets less MMR penalty for that specific pair).
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

fn max_redundancy_indexed(
    cand: &DiversityFeatures,
    selected: &[usize],
    features: &[DiversityFeatures],
    graph: &TagRelationGraph,
    user_graph: Option<&TagRelationGraph>,
    priors: &Priors,
) -> f32 {
    let window = priors.diversity_window.max(1);
    let blend = priors.diversity_semantic_blend.clamp(0.0, 1.0);
    let pmi_threshold = priors.diversity_pmi_threshold;
    let user_pmi_weight = priors.diversity_user_pmi_weight;
    let max_tags = priors.diversity_semantic_max_tags.max(1);

    let mut max_sim = 0.0f32;
    for &i in selected.iter().rev().take(window) {
        let chosen = &features[i];
        let sim = if blend <= 0.0 {
            // Fast path: pure Jaccard — no graph queries needed beyond
            // what was already done at feature-construction time.
            jaccard_hashes(&cand.artist, &chosen.artist) * priors.diversity_w_artist
                + jaccard_hashes(&cand.character, &chosen.character) * priors.diversity_w_character
                + jaccard_hashes(&cand.copyright, &chosen.copyright) * priors.diversity_w_copyright
                + jaccard_hashes(&cand.species, &chosen.species) * priors.diversity_w_species
                + jaccard_hashes(&cand.general, &chosen.general) * priors.diversity_w_general
        } else {
            group_similarity(
                &cand.artist,
                &chosen.artist,
                graph,
                user_graph,
                blend,
                pmi_threshold,
                user_pmi_weight,
                max_tags,
            ) * priors.diversity_w_artist
                + group_similarity(
                    &cand.character,
                    &chosen.character,
                    graph,
                    user_graph,
                    blend,
                    pmi_threshold,
                    user_pmi_weight,
                    max_tags,
                ) * priors.diversity_w_character
                + group_similarity(
                    &cand.copyright,
                    &chosen.copyright,
                    graph,
                    user_graph,
                    blend,
                    pmi_threshold,
                    user_pmi_weight,
                    max_tags,
                ) * priors.diversity_w_copyright
                + group_similarity(
                    &cand.species,
                    &chosen.species,
                    graph,
                    user_graph,
                    blend,
                    pmi_threshold,
                    user_pmi_weight,
                    max_tags,
                ) * priors.diversity_w_species
                + group_similarity(
                    &cand.general,
                    &chosen.general,
                    graph,
                    user_graph,
                    blend,
                    pmi_threshold,
                    user_pmi_weight,
                    max_tags,
                ) * priors.diversity_w_general
        };
        if sim > max_sim {
            max_sim = sim;
        }
    }
    let exp = priors.mmr_redundancy_exp;
    if (exp - 1.0).abs() < 1e-3 {
        max_sim
    } else {
        max_sim.max(0.0).powf(exp.clamp(0.1, 5.0)).clamp(0.0, 1.0)
    }
}

/// Index-based MMR re-ranker. Returns indices in their final order.
///
/// `entries[i] = (score, interaction_fit, tiebreak_id)` is parallel to
/// `features[i]`. `head_limit` caps how many top-by-score items
/// participate in MMR; everything past that keeps its raw-score
/// ordering. Pass `head_limit >= entries.len()` for full-list MMR
/// (legacy behaviour).
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

    let mut available: Vec<usize> = idx_by_score[..head_n].to_vec();
    let mut selected: Vec<usize> = Vec::with_capacity(head_n);
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
            let redundancy = max_redundancy_indexed(
                &features[i],
                &selected,
                features,
                graph,
                user_graph,
                priors,
            );
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
    let features: Vec<DiversityFeatures> = posts
        .iter()
        .map(|sp| DiversityFeatures::from_post(&sp.post, graph))
        .collect();
    let entries: Vec<(f32, f32, i64)> = posts
        .iter()
        .map(|sp| {
            let interaction = sp
                .breakdown
                .as_ref()
                .map(|b| b.interaction_fit)
                .unwrap_or(FEEDBACK_NEUTRAL);
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
        sp.post.tags.character.first().map(|c| c.to_ascii_lowercase())
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
            id, created_at: Utc::now(), updated_at: Utc::now(), change_seq: 0.0,
            files: Files::default(),
            uploader_id: 0, uploader_name: None, approver_id: None,
            stats: Stats::default(), flags: Flags::default(),
            has: Has::default(), relationships: Relationships::default(),
            pools: vec![], rating: Rating::S, locked_tags: vec![], sources: vec![],
            description: None,
            tags: Tags {
                artist: artists.iter().map(|s| s.to_string()).collect(),
                character: characters.iter().map(|s| s.to_string()).collect(),
                ..Tags::default()
            },
        }
    }

    fn scored(id: i64, artists: &[&str], characters: &[&str]) -> ScoredPost {
        ScoredPost {
            post: post(id, artists, characters),
            score: 1.0,
            breakdown: None,
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
                if !set.contains(&a) { set.push(a); }
            }
        }
        set.len()
    }

    fn distinct_characters(posts: &[ScoredPost], window: usize) -> usize {
        let mut set: Vec<String> = Vec::new();
        for sp in posts.iter().take(window) {
            if let Some(c) = sp.post.tags.character.first() {
                let c = c.to_ascii_lowercase();
                if !set.contains(&c) { set.push(c); }
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

    /// When diversity_semantic_blend > 0 and user_graph is provided,
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
}
