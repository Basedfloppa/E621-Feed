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
//! Memory: each [`DiversityFeatures`] holds three sorted `Vec<u64>` of
//! per-tag SipHashes. Collisions at 64-bit are negligible at the tag
//! cardinalities involved (≤ 10⁵ unique tags, ~10⁻¹⁵ collision
//! probability per pair). This trades a few cents of false-positive
//! similarity risk for a ~10× memory reduction over the previous
//! `HashSet<String>` representation, keeping 500-account calibration
//! datasets inside 15 GB.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::models::{Post, ScoredPost};

use super::priors::Priors;
use super::util::{normalize_tag, FEEDBACK_NEUTRAL};

/// Pre-computed Jaccard-friendly tag fingerprints for one post. Tags are
/// hashed once and stored as a sorted `Vec<u64>` per group so MMR's
/// per-pair set intersection is a linear merge instead of a HashSet
/// probe.
#[derive(Clone)]
pub struct DiversityFeatures {
    artist: Vec<u64>,
    character: Vec<u64>,
    copyright: Vec<u64>,
    species: Vec<u64>,
    general: Vec<u64>,
}

impl DiversityFeatures {
    pub fn from_post(p: &Post) -> Self {
        Self {
            artist: hashed_tag_set(&p.tags.artist),
            character: hashed_tag_set(&p.tags.character),
            copyright: hashed_tag_set(&p.tags.copyright),
            species: hashed_tag_set(&p.tags.species),
            general: hashed_tag_set(&p.tags.general),
        }
    }
}

fn hash_tag(t: &str) -> u64 {
    let lc = normalize_tag(t);
    let mut h = DefaultHasher::new();
    lc.hash(&mut h);
    h.finish()
}

fn hashed_tag_set(tags: &[String]) -> Vec<u64> {
    let mut out: Vec<u64> = tags
        .iter()
        .filter_map(|t| {
            let trimmed = t.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(hash_tag(trimmed))
            }
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Jaccard between two sorted-deduped `Vec<u64>` via merge-intersection.
fn jaccard(a: &[u64], b: &[u64]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (mut i, mut j) = (0usize, 0usize);
    let mut inter = 0u32;
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
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

fn max_redundancy_indexed(
    cand: &DiversityFeatures,
    selected: &[usize],
    features: &[DiversityFeatures],
    priors: &Priors,
) -> f32 {
    let window = priors.diversity_window.max(1);
    let mut max_sim = 0.0f32;
    for &i in selected.iter().rev().take(window) {
        let chosen = &features[i];
        let sim = jaccard(&cand.artist, &chosen.artist) * priors.diversity_w_artist
            + jaccard(&cand.character, &chosen.character) * priors.diversity_w_character
            + jaccard(&cand.copyright, &chosen.copyright) * priors.diversity_w_copyright
            + jaccard(&cand.species, &chosen.species) * priors.diversity_w_species
            + jaccard(&cand.general, &chosen.general) * priors.diversity_w_general;
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
    let mut top_score = available.iter().map(|&i| entries[i].0).fold(f32::MIN, f32::max);

    let damp = priors.diversity_interaction_damp.clamp(0.0, 1.0);
    let max_penalty = priors.diversity_max_penalty.clamp(0.0, 1.0);

    while !available.is_empty() {
        let mut best_pos = 0usize;
        let mut best_value = f32::MIN;
        let mut best_tiebreak = i64::MAX;

        for (pos, &i) in available.iter().enumerate() {
            let (score, interaction, tid) = entries[i];
            let redundancy = max_redundancy_indexed(&features[i], &selected, features, priors);
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
            top_score = available.iter().map(|&i| entries[i].0).fold(f32::MIN, f32::max);
        }
    }

    selected.extend(idx_by_score[head_n..].iter().copied());
    selected
}

/// Owning re-rank used by the production `/recommendations` route.
/// Builds [`DiversityFeatures`] on the fly and runs MMR over the whole
/// list (no top-K cutoff) to preserve historical behaviour.
pub fn diversify_scored_posts(posts: Vec<ScoredPost>, priors: &Priors) -> Vec<ScoredPost> {
    if posts.is_empty() {
        return posts;
    }
    let features: Vec<DiversityFeatures> = posts
        .iter()
        .map(|sp| DiversityFeatures::from_post(&sp.post))
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

    let order = diversify_indices(&entries, &features, priors, posts.len());

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
    use crate::models::{Flags, Post, Rating, Relationships, Score, ScoredPost, Tags};
    use chrono::Utc;

    /// Minimal `Post` for diversity tests — only `id`, `tags.artist` and
    /// `tags.character` feed the quota logic; everything else is a neutral
    /// placeholder.
    fn post(id: i64, artists: &[&str], characters: &[&str]) -> Post {
        Post {
            id,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            file: None,
            preview: None,
            sample: None,
            score: Score { up: 0, down: 0, total: 0 },
            tags: Tags {
                general: vec![],
                artist: artists.iter().map(|s| s.to_string()).collect(),
                copyright: vec![],
                character: characters.iter().map(|s| s.to_string()).collect(),
                species: vec![],
                invalid: vec![],
                meta: vec![],
                lore: vec![],
                contributor: vec![],
            },
            locked_tags: None,
            change_seq: 0.0,
            flags: Flags {
                pending: false,
                flagged: false,
                note_locked: false,
                status_locked: false,
                rating_locked: false,
                deleted: false,
            },
            rating: Rating::S,
            fav_count: 0,
            sources: vec![],
            pools: vec![],
            relationships: Relationships {
                parent_id: None,
                has_children: false,
                has_active_children: false,
                children: vec![],
            },
            approver_id: None,
            uploader_id: 0,
            description: None,
            comment_count: 0,
            is_favorited: false,
            has_notes: false,
            duration: None,
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
        assert_eq!(ids(&posts), vec![0, 1, 2, 3], "satisfied quota must not re-order");
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
        assert_eq!(posts[3].post.tags.artist, vec!["b"]);
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
            scored(0, &[], &[]),    // None
            scored(1, &["a"], &[]),
            scored(2, &[], &[]),    // None
            scored(3, &["a"], &[]),
            scored(4, &["b"], &[]), // below window
            scored(5, &["c"], &[]), // below window
        ];
        enforce_group_quota(&mut posts, 4, 2, |sp| sp.post.tags.artist.first().cloned());

        assert_eq!(posts[3].post.id, 4, "diverse `b` fills the last redundant slot");
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
        assert_eq!(ids(&posts), vec![0, 1, 2, 3, 4], "no redundant slot — left intact");
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

        assert!(distinct_artists(&posts, 20) >= 2, "top-20 must hold ≥2 artists");
        assert_eq!(posts[19].post.id, 20, "`bob` promoted into the window");
        assert_eq!(posts[19].post.tags.artist, vec!["bob"]);
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

        assert!(distinct_characters(&posts, 20) >= 3, "top-20 must hold ≥3 characters");
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
}
