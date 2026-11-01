//! Composition guarantees for the head of the feed.
//!
//! Ranking optimises one number; the reader sees a page. Those two disagree in
//! a way no weight fixes: a fresh post with no favourites yet loses a few
//! thousandths of score to a month-old post with a hundred, so a blend tuned on
//! relevance quietly fills the head with the same age band. Raising
//! `mix_recency` moves every post a little; a quota moves only the slots that
//! are missing, and leaves the ranking of everything else untouched.
//!
//! The pass runs after diversification so it cannot be undone by the MMR
//! re-order, and it only ever swaps a post **inside** the window with one
//! **outside** it — the ranking below the window is preserved exactly.
//!
//! Two floors exist and they are independent: a floor of recent posts and a
//! floor of old ones. Both are minimums inside the same window, so a page can
//! satisfy them at once (fresh posts at the top, a couple of older picks below).
//!
//! Old posts also carry a ceiling. A floor alone is not enough once the backlog
//! stream feeds the pool: the most-favourited work of an artist someone follows
//! outscores almost every fresh post, so `old_quota = 2` measured a head that was
//! half decade-old posts. The ceiling is the same idea as the floor, pointed the
//! other way — at most `old_cap` old posts inside the window — and it never
//! undercuts the old floor.

use chrono::{DateTime, Utc};

use crate::models::ScoredPost;

/// How many recent/old posts the head must contain.
#[derive(Clone, Copy, Debug)]
pub struct CompositionTargets {
    /// Number of leading posts the quotas apply to.
    pub window: usize,
    /// Minimum posts younger than `fresh_days` inside the window; `0` disables.
    pub fresh_quota: usize,
    pub fresh_days: f64,
    /// Minimum posts older than `old_days` inside the window; `0` disables.
    pub old_quota: usize,
    /// Maximum posts older than `old_days` inside the window; `0` disables.
    /// Never lowered below `old_quota`: a ceiling must not eat the floor.
    pub old_cap: usize,
    pub old_days: f64,
}

impl Default for CompositionTargets {
    fn default() -> Self {
        Self {
            window: 20,
            fresh_quota: 0,
            fresh_days: 7.0,
            old_quota: 0,
            old_cap: 0,
            old_days: 365.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompositionReport {
    pub fresh_in_window: usize,
    pub fresh_promoted: usize,
    pub old_in_window: usize,
    pub old_promoted: usize,
    pub old_demoted: usize,
}

#[derive(Clone, Copy)]
enum Kind {
    Fresh,
    Old,
}

impl CompositionTargets {
    fn age_days(post: &ScoredPost, now: DateTime<Utc>) -> f64 {
        (now - post.post.created_at).num_seconds().max(0) as f64 / 86_400.0
    }

    fn matches(&self, kind: Kind, post: &ScoredPost, now: DateTime<Utc>) -> bool {
        let age = Self::age_days(post, now);
        match kind {
            Kind::Fresh => age < self.fresh_days,
            Kind::Old => age > self.old_days,
        }
    }

    fn quota(&self, kind: Kind) -> usize {
        match kind {
            Kind::Fresh => self.fresh_quota,
            Kind::Old => self.old_quota,
        }
    }

    fn other(&self, kind: Kind) -> Kind {
        match kind {
            Kind::Fresh => Kind::Old,
            Kind::Old => Kind::Fresh,
        }
    }
}

/// Apply the quotas in place and report what the window looks like afterwards
/// (plus how many posts had to be promoted to get there).
///
/// A rule whose target cannot be met — the pool simply has no post that old, or
/// the window is shorter than the quota — stops early and promotes nothing
/// further; the head then keeps the ranking it had. That is deliberate: a
/// guarantee the data cannot honour must not be faked by dropping posts.
pub fn apply_composition_quota(
    scored: &mut [ScoredPost],
    now: DateTime<Utc>,
    targets: &CompositionTargets,
) -> CompositionReport {
    let mut report = CompositionReport::default();
    let window = targets.window.min(scored.len());
    if window == 0 {
        return report;
    }

    for kind in [Kind::Fresh, Kind::Old] {
        let quota = targets.quota(kind).min(window);
        if quota == 0 {
            continue;
        }
        let mut count = scored[..window]
            .iter()
            .filter(|p| targets.matches(kind, p, now))
            .count();
        let promoted = match kind {
            Kind::Fresh => &mut report.fresh_promoted,
            Kind::Old => &mut report.old_promoted,
        };
        while count < quota {
            let Some(src) =
                (window..scored.len()).find(|&i| targets.matches(kind, &scored[i], now))
            else {
                break;
            };
            // The victim leaves the window, so the *other* quota must still be
            // satisfied without it. Without this check a fresh promotion can
            // evict the last old post and the two quotas fight.
            let other = targets.other(kind);
            let other_quota = targets.quota(other).min(window);
            let victim = (0..window).rev().find(|&i| {
                if targets.matches(kind, &scored[i], now) {
                    return false;
                }
                let other_now = scored[..window]
                    .iter()
                    .filter(|p| targets.matches(other, p, now))
                    .count();
                // Do not demand more of the other quota than the pool can give:
                // an unreachable floor (no post that old exists at all) used to
                // block the other rule entirely, because `other_now` could never
                // reach a target the data cannot satisfy.
                let other_target = other_quota.min(other_now);
                let removed = if targets.matches(other, &scored[i], now) {
                    1
                } else {
                    0
                };
                other_now - removed >= other_target
            });
            let Some(dst) = victim else { break };
            scored.swap(dst, src);
            // Report what the swap did to both counters. The promoted post is
            // inside the window now; the evicted one is not, so it can only
            // have removed a match if it matched too (it did not, by `victim`).
            count += 1;
            *promoted += 1;
        }
        match kind {
            Kind::Fresh => report.fresh_in_window = count,
            Kind::Old => report.old_in_window = count,
        }
    }

    // Ceiling on the old band, applied after both floors: it may not push the
    // old count below `old_quota`, so the two rules cannot fight.
    if targets.old_cap > 0 {
        let cap = targets.old_cap.max(targets.old_quota).min(window);
        let mut count = scored[..window]
            .iter()
            .filter(|p| targets.matches(Kind::Old, p, now))
            .count();
        while count > cap {
            // Demote the *lowest-ranked* old post in the window and take the
            // best non-old post from below it — the mirror image of a floor.
            let Some(dst) = (0..window)
                .rev()
                .find(|&i| targets.matches(Kind::Old, &scored[i], now))
            else {
                break;
            };
            let Some(src) =
                (window..scored.len()).find(|&i| !targets.matches(Kind::Old, &scored[i], now))
            else {
                break;
            };
            scored.swap(dst, src);
            count -= 1;
            report.old_demoted += 1;
        }
        report.old_in_window = count;
    }

    // Fill the untouched side of the report so callers can log both counts.
    if targets.fresh_quota == 0 {
        report.fresh_in_window = scored[..window]
            .iter()
            .filter(|p| targets.matches(Kind::Fresh, p, now))
            .count();
    }
    if targets.old_quota == 0 {
        report.old_in_window = scored[..window]
            .iter()
            .filter(|p| targets.matches(Kind::Old, p, now))
            .count();
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Files, Flags, Has, Post, Rating, Relationships, Stats, Tags};
    use chrono::Duration;

    fn post_at(id: i64, days_ago: i64, now: DateTime<Utc>) -> ScoredPost {
        ScoredPost {
            post: Post {
                id,
                created_at: now - Duration::days(days_ago),
                updated_at: now,
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
                tags: Tags::default(),
            },
            score: 1.0,
            breakdown: None,
            reasons: Vec::new(),
        }
    }

    fn head(ages: &[i64], now: DateTime<Utc>) -> Vec<ScoredPost> {
        ages.iter()
            .enumerate()
            .map(|(i, days)| {
                let mut p = post_at(i as i64 + 1, *days, now);
                p.score = 1.0 - i as f32 * 0.01;
                p
            })
            .collect()
    }

    fn targets_in(window: usize, fresh: usize, old: usize) -> CompositionTargets {
        CompositionTargets {
            window,
            fresh_quota: fresh,
            fresh_days: 7.0,
            old_quota: old,
            old_cap: 0,
            old_days: 365.0,
        }
    }

    fn targets(fresh: usize, old: usize) -> CompositionTargets {
        targets_in(20, fresh, old)
    }

    #[test]
    fn fresh_quota_promotes_the_best_fresh_post_from_below_the_window() {
        let now = Utc::now();
        // Window of 3 all older than a week; posts 4 and 5 are fresh, 4 comes
        // first (better score) and must be the one promoted.
        let mut posts = head(&[30, 31, 32, 1, 2], now);
        let report = apply_composition_quota(&mut posts, now, &targets_in(3, 1, 0));
        assert_eq!(report.fresh_promoted, 1);
        assert_eq!(
            posts[2].post.id, 4,
            "the best fresh post takes the last slot"
        );
        assert_eq!(posts[3].post.id, 3, "the demoted post leaves the window");
    }

    #[test]
    fn a_satisfied_quota_changes_nothing() {
        let now = Utc::now();
        let mut posts = head(&[1, 30, 31, 40], now);
        let before: Vec<i64> = posts.iter().map(|p| p.post.id).collect();
        let report = apply_composition_quota(&mut posts, now, &targets(1, 0));
        assert_eq!(report.fresh_promoted, 0);
        assert_eq!(posts.iter().map(|p| p.post.id).collect::<Vec<_>>(), before);
    }

    #[test]
    fn old_quota_and_fresh_quota_do_not_evict_each_other() {
        let now = Utc::now();
        // Window: two fresh, one month-old middle, one old. Fresh quota needs 2
        // (met), old quota needs 1 (met) — nothing should move.
        let mut posts = head(&[1, 2, 30, 800, 40, 41, 42, 43], now);
        let before: Vec<i64> = posts.iter().map(|p| p.post.id).collect();
        let report = apply_composition_quota(&mut posts, now, &targets(2, 1));
        assert_eq!(report.fresh_promoted, 0);
        assert_eq!(report.old_promoted, 0);
        assert_eq!(posts.iter().map(|p| p.post.id).collect::<Vec<_>>(), before);
    }

    /// An unreachable floor must not veto the *other* floor. Before this guard
    /// was relative to what the pool can actually give, `fresh_quota = 6` with
    /// `old_quota = 2` promoted nothing whenever the pool held no old post —
    /// the victim check demanded an old count that could not exist.
    #[test]
    fn an_unreachable_old_quota_does_not_block_the_fresh_quota() {
        let now = Utc::now();
        let mut posts = head(&[30, 31, 32, 33, 1, 2], now);
        let report = apply_composition_quota(&mut posts, now, &targets_in(4, 2, 2));
        assert_eq!(report.fresh_promoted, 2);
        assert_eq!(report.fresh_in_window, 2);
        assert_eq!(report.old_promoted, 0);
    }

    #[test]
    fn an_unreachable_quota_stops_instead_of_dropping_posts() {
        let now = Utc::now();
        // No post older than a year anywhere: the old quota cannot be met, and
        // the pass must leave the list a permutation of itself.
        let mut posts = head(&[1, 2, 3, 4], now);
        let report = apply_composition_quota(&mut posts, now, &targets(0, 2));
        assert_eq!(report.old_promoted, 0);
        let mut ids: Vec<i64> = posts.iter().map(|p| p.post.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_window_below_two_posts_is_left_alone() {
        let now = Utc::now();
        let mut posts = head(&[30], now);
        let report = apply_composition_quota(&mut posts, now, &targets(1, 1));
        assert_eq!(report, CompositionReport::default());
        assert_eq!(posts[0].post.id, 1);
    }

    /// The floor and the ceiling are the same rule pointed two ways: the backlog
    /// stream made "at least two old posts" turn into "ten old posts", because
    /// favourites beat freshness on score. The ceiling has to demote the worst
    /// old posts and leave the rest of the window alone.
    #[test]
    fn the_old_ceiling_demotes_the_lowest_ranked_old_posts() {
        let now = Utc::now();
        // Window of 5: four old, one middle-aged; below it, two fresh and one
        // month-old. Ceiling 2 must demote two old posts for the best non-old
        // ones, which are the fresh posts.
        let mut posts = head(&[800, 700, 600, 500, 30, 1, 2, 40], now);
        let mut targets = targets_in(5, 0, 1);
        targets.old_cap = 2;
        let report = apply_composition_quota(&mut posts, now, &targets);
        assert_eq!(report.old_demoted, 2);
        assert_eq!(report.old_in_window, 2);
        let in_window: Vec<i64> = posts[..5].iter().map(|p| p.post.id).collect();
        assert!(
            in_window.contains(&1) && in_window.contains(&2),
            "fresh posts promoted: {in_window:?}"
        );
        assert_eq!(
            posts[5].post.id, 4,
            "the demoted old post takes the slot below"
        );
        assert_eq!(
            posts[4].post.id, 5,
            "the middle-aged post stays where it was"
        );
    }

    #[test]
    fn the_old_ceiling_never_undercuts_the_old_floor() {
        let now = Utc::now();
        // Floor asks for 3 old, ceiling says 1: the floor wins, because a
        // ceiling below the floor would make the pair unsatisfiable.
        let mut posts = head(&[800, 700, 600, 30, 1, 2], now);
        let mut targets = targets_in(4, 0, 3);
        targets.old_cap = 1;
        let report = apply_composition_quota(&mut posts, now, &targets);
        assert_eq!(report.old_in_window, 3);
        assert_eq!(report.old_demoted, 0);
    }

    #[test]
    fn the_old_ceiling_is_disabled_at_zero() {
        let now = Utc::now();
        let mut posts = head(&[800, 700, 600, 30, 1], now);
        let before: Vec<i64> = posts.iter().map(|p| p.post.id).collect();
        let report = apply_composition_quota(&mut posts, now, &targets_in(4, 0, 1));
        assert_eq!(report.old_demoted, 0);
        assert_eq!(posts.iter().map(|p| p.post.id).collect::<Vec<_>>(), before);
    }

    #[test]
    fn quotas_never_change_the_multiset_of_posts() {
        let now = Utc::now();
        let mut posts = head(&[30, 31, 32, 33, 1, 800, 2, 34], now);
        let before: Vec<i64> = {
            let mut v: Vec<i64> = posts.iter().map(|p| p.post.id).collect();
            v.sort_unstable();
            v
        };
        apply_composition_quota(&mut posts, now, &targets(2, 1));
        let after: Vec<i64> = {
            let mut v: Vec<i64> = posts.iter().map(|p| p.post.id).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(before, after);
    }
}
