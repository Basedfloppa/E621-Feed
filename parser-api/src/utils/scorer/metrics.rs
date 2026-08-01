//! Per-channel and pipeline-level performance metrics.
//! Zero-cost when `perf_metrics` feature is disabled — all `Instant` calls
//! are `#[cfg(feature = "perf_metrics")]` gated.
//!
//! Enable with: `cargo run --features perf_metrics`
//!
//! Logs per-channel timing and pipeline phase breakdowns at `info!` level.

#[cfg(feature = "perf_metrics")]
use std::time::Instant;

/// Nanosecond-precision timing for one scoring pass.
#[derive(Default, Clone, Copy, Debug)]
pub struct ChannelTiming {
    pub tag_similarity: u64,
    pub quality_fit: u64,
    pub popularity_fit: u64,
    pub rating_fit: u64,
    pub media_fit: u64,
    pub interaction_fit: u64,
    pub recency_fit: u64,
    pub tag_relation_fit: u64,
    pub uploader_fit: u64,
    pub exclusivity_fit: u64,
    pub novelty_fit: u64,
    pub artist_discovery_fit: u64,
    pub final_blend: u64,
}

#[derive(Default, Clone, Debug)]
pub struct ScoringMetrics {
    /// Per-channel cumulative nanoseconds across all scored posts.
    pub channel: ChannelTiming,
    /// Number of posts scored.
    pub count: usize,
}

impl ScoringMetrics {
    /// Accumulate timings from a single post's `ChannelTiming`.
    #[inline]
    pub fn accumulate(&mut self, t: &ChannelTiming) {
        self.channel.tag_similarity += t.tag_similarity;
        self.channel.quality_fit += t.quality_fit;
        self.channel.popularity_fit += t.popularity_fit;
        self.channel.rating_fit += t.rating_fit;
        self.channel.media_fit += t.media_fit;
        self.channel.interaction_fit += t.interaction_fit;
        self.channel.recency_fit += t.recency_fit;
        self.channel.tag_relation_fit += t.tag_relation_fit;
        self.channel.uploader_fit += t.uploader_fit;
        self.channel.exclusivity_fit += t.exclusivity_fit;
        self.channel.novelty_fit += t.novelty_fit;
        self.channel.artist_discovery_fit += t.artist_discovery_fit;
        self.channel.final_blend += t.final_blend;
        self.count += 1;
    }

    /// Log a human-readable summary at `info!`.
    pub fn log_summary(&self) {
        if self.count == 0 {
            return;
        }
        let c = &self.channel;
        let n = self.count as f64;
        info!(
            "── Scoring metrics ({} posts) ─────────────────",
            self.count
        );
        let rows = [
            ("tag_similarity", c.tag_similarity),
            ("quality_fit", c.quality_fit),
            ("popularity_fit", c.popularity_fit),
            ("rating_fit", c.rating_fit),
            ("media_fit", c.media_fit),
            ("interaction_fit", c.interaction_fit),
            ("recency_fit", c.recency_fit),
            ("tag_relation_fit", c.tag_relation_fit),
            ("uploader_fit", c.uploader_fit),
            ("exclusivity_fit", c.exclusivity_fit),
            ("novelty_fit", c.novelty_fit),
            ("artist_discovery_fit", c.artist_discovery_fit),
            ("final_blend", c.final_blend),
        ];
        let total_all: u64 = rows.iter().map(|(_, ns)| ns).sum();
        for (name, ns) in &rows {
            let avg_ns = *ns as f64 / n;
            let pct = if total_all > 0 {
                *ns as f64 / total_all as f64 * 100.0
            } else {
                0.0
            };
            info!("  {name:>22}: {avg_ns:>8.1} ns/op ({pct:>5.1}%)");
        }
        info!("──────────────────────────────────────────────");
    }
}

// ==================================================================
//  Pipeline-level metrics
//  Tracks named phases across the entire request pipeline
//  (DB reads, e621 API calls, scoring, diversify, ...).
// ==================================================================

/// A single recorded phase in a request pipeline.
#[derive(Debug, Clone)]
pub struct PhaseRecord {
    pub name: &'static str,
    pub nanos: u64,
}

/// Accumulator for pipeline phase timings.
///
/// Each `mark(name)` records the elapsed ns since the previous mark
/// (or since construction for the very first mark).
///
/// Usage (async context):
///
/// ```text
/// let mut pipe = PipelineMetrics::new("recommendations");
/// // ... implicit start at construction ...
/// pipe.mark("db_hydrate");
/// let data = load_from_db().await;
/// pipe.mark("e621_fetch");
/// let posts = api::fetch().await;
/// pipe.mark("scoring");
/// // ...
/// pipe.finish_and_log();
/// ```
#[derive(Debug)]
pub struct PipelineMetrics {
    #[cfg(feature = "perf_metrics")]
    label: &'static str,
    #[cfg(feature = "perf_metrics")]
    last_mark: std::time::Instant,
    #[cfg(feature = "perf_metrics")]
    phases: Vec<PhaseRecord>,
    /// Dummy field so the struct is always instantiable (zero-cost when
    /// `perf_metrics` is off — the struct is a ZST and mark/finish are no-ops).
    _dummy: (),
}

impl PipelineMetrics {
    /// Start a new pipeline trace.
    #[inline]
    #[must_use]
    pub fn new(#[allow(unused_variables)] label: &'static str) -> Self {
        Self {
            #[cfg(feature = "perf_metrics")]
            label,
            #[cfg(feature = "perf_metrics")]
            last_mark: std::time::Instant::now(),
            #[cfg(feature = "perf_metrics")]
            phases: Vec::with_capacity(16),
            _dummy: (),
        }
    }

    /// Record elapsed time since the previous mark (or construction).
    /// `name` describes the phase that JUST completed.
    #[inline]
    pub fn mark(&mut self, name: &'static str) {
        #[cfg(feature = "perf_metrics")]
        {
            let now = std::time::Instant::now();
            let nanos = now.duration_since(self.last_mark).as_nanos() as u64;
            self.phases.push(PhaseRecord { name, nanos });
            self.last_mark = now;
        }
        #[cfg(not(feature = "perf_metrics"))]
        {
            let _ = name;
        }
    }

    /// Log the full pipeline breakdown at `info!`.
    #[inline]
    pub fn finish_and_log(&self) {
        #[cfg(feature = "perf_metrics")]
        {
            if self.phases.is_empty() {
                return;
            }
            let total: u64 = self.phases.iter().map(|p| p.nanos).sum();
            let n = self.phases.len();
            info!(
                "── Pipeline: {} ({} phases, {:.1} ms total) ─────────────────",
                self.label,
                n,
                total as f64 / 1_000_000.0
            );
            for p in &self.phases {
                let ms = p.nanos as f64 / 1_000_000.0;
                let pct = if total > 0 {
                    p.nanos as f64 / total as f64 * 100.0
                } else {
                    0.0
                };
                info!("  {name:>30}: {ms:>8.2} ms ({pct:>5.1}%)", name = p.name);
            }
            info!("────────────────────────────────────────────────────────");
        }
        #[cfg(not(feature = "perf_metrics"))]
        {}
    }
}

// ------------------------------------------------------------------
//  Timed wrappers used by `score_cached_with_metrics`.
//  Only compiled when perf_metrics feature is enabled.
// ------------------------------------------------------------------

#[cfg(feature = "perf_metrics")]
macro_rules! timed_channel {
    ($ctx:expr, $method:ident, $features:expr) => {{
        let _start = Instant::now();
        let _result = $ctx.$method($features);
        let _elapsed = _start.elapsed().as_nanos() as u64;
        (_result, _elapsed)
    }};
}

impl super::context::ScoringContext<'_> {
    /// Like `score_cached`, but also returns per-channel nanosecond timing.
    /// Only compiles when `feature = "perf_metrics"`.
    #[cfg(feature = "perf_metrics")]
    pub fn score_cached_with_metrics(
        &self,
        features: &super::cached::CachedPostFeatures,
    ) -> (f32, crate::models::ScoreBreakdown, ChannelTiming) {
        let (sim, sim_ns) = timed_channel!(self, tag_similarity_cached, features);
        let (quality, q_ns) = timed_channel!(self, quality_fit_cached, features);
        let (popularity, p_ns) = timed_channel!(self, popularity_fit_cached, features);
        let (rating, r_ns) = timed_channel!(self, rating_fit_cached, features);
        let (media, m_ns) = timed_channel!(self, media_fit_cached, features);
        let ((interaction, veto), i_ns) = timed_channel!(self, interaction_fit_cached, features);

        let age_days = (self.priors.now - features.created_at).num_seconds() as f32 / 86_400.0;
        let (recency, rec_ns) = {
            let _start = Instant::now();
            let r = self.recency_fit(age_days);
            (r, _start.elapsed().as_nanos() as u64)
        };
        let (tag_relation, tr_ns) = timed_channel!(self, tag_relation_fit_cached, features);
        let (uploader, u_ns) = timed_channel!(self, uploader_fit_cached, features);
        let (exclusivity, exc_ns) = timed_channel!(self, exclusivity_fit_cached, features);
        let (novelty, nov_ns) = timed_channel!(self, novelty_fit_cached, features);
        let (artist_discovery, ad_ns) = timed_channel!(self, artist_discovery_fit_cached, features);

        let blend_start = Instant::now();
        let score = self.final_blend(
            sim,
            quality,
            recency,
            rating,
            media,
            popularity,
            interaction,
            tag_relation,
            uploader,
            exclusivity,
            novelty,
            artist_discovery,
            veto,
        );
        let blend_ns = blend_start.elapsed().as_nanos() as u64;

        let breakdown = crate::models::ScoreBreakdown {
            tag_similarity: sim,
            quality_fit: quality,
            recency_fit: recency,
            rating_fit: rating,
            media_fit: media,
            popularity_fit: popularity,
            interaction_fit: interaction,
            tag_relation_fit: tag_relation,
            uploader_fit: uploader,
            exclusivity_fit: exclusivity,
            novelty_fit: novelty,
            artist_discovery_fit: artist_discovery,
        };

        let timing = ChannelTiming {
            tag_similarity: sim_ns,
            quality_fit: q_ns,
            popularity_fit: p_ns,
            rating_fit: r_ns,
            media_fit: m_ns,
            interaction_fit: i_ns,
            recency_fit: rec_ns,
            tag_relation_fit: tr_ns,
            uploader_fit: u_ns,
            exclusivity_fit: exc_ns,
            novelty_fit: nov_ns,
            artist_discovery_fit: ad_ns,
            final_blend: blend_ns,
        };

        (score, breakdown, timing)
    }

    /// Non-metrics stub — compiles when `perf_metrics` is disabled.
    #[cfg(not(feature = "perf_metrics"))]
    #[must_use]
    pub fn score_cached_with_metrics(
        &self,
        features: &super::cached::CachedPostFeatures,
    ) -> (f32, crate::models::ScoreBreakdown, ChannelTiming) {
        let (score, breakdown) = self.score_cached(features);
        (score, breakdown, ChannelTiming::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_timing_default_is_zero() {
        let t = ChannelTiming::default();
        assert_eq!(t.tag_similarity, 0);
        assert_eq!(t.quality_fit, 0);
        assert_eq!(t.popularity_fit, 0);
        assert_eq!(t.rating_fit, 0);
        assert_eq!(t.media_fit, 0);
        assert_eq!(t.interaction_fit, 0);
        assert_eq!(t.recency_fit, 0);
        assert_eq!(t.tag_relation_fit, 0);
        assert_eq!(t.uploader_fit, 0);
        assert_eq!(t.final_blend, 0);
    }

    #[test]
    fn scoring_metrics_accumulate_additively() {
        let mut m = ScoringMetrics::default();
        let t = ChannelTiming {
            tag_similarity: 100,
            quality_fit: 200,
            ..Default::default()
        };
        m.accumulate(&t);
        assert_eq!(m.count, 1);
        assert_eq!(m.channel.tag_similarity, 100);
        assert_eq!(m.channel.quality_fit, 200);

        // Second accumulation
        m.accumulate(&t);
        assert_eq!(m.count, 2);
        assert_eq!(m.channel.tag_similarity, 200);
    }

    #[test]
    fn log_summary_does_not_panic_on_empty() {
        let m = ScoringMetrics::default();
        m.log_summary(); // should not panic
    }

    #[cfg(feature = "perf_metrics")]
    #[test]
    fn pipeline_metrics_new_creates_empty() {
        let p = PipelineMetrics::new("test");
        assert_eq!(p.phases.len(), 0);
    }

    #[cfg(feature = "perf_metrics")]
    #[test]
    fn pipeline_metrics_mark_records_phase() {
        let mut p = PipelineMetrics::new("test");
        std::thread::sleep(std::time::Duration::from_nanos(1));
        p.mark("phase_one");
        assert_eq!(p.phases.len(), 1);
        assert_eq!(p.phases[0].name, "phase_one");
        assert!(
            p.phases[0].nanos > 0,
            "expected positive nanos, got {}",
            p.phases[0].nanos
        );

        p.mark("phase_two");
        assert_eq!(p.phases.len(), 2);
        assert_eq!(p.phases[1].name, "phase_two");
    }

    #[cfg(feature = "perf_metrics")]
    #[test]
    fn pipeline_metrics_finish_and_log_does_not_panic() {
        let p = PipelineMetrics::new("empty_test");
        p.finish_and_log();
    }

    #[cfg(feature = "perf_metrics")]
    #[test]
    fn pipeline_metrics_mark_after_sleep_records_timing() {
        let mut p = PipelineMetrics::new("sleep_test");
        std::thread::sleep(std::time::Duration::from_micros(10));
        p.mark("slept");
        assert!(
            p.phases[0].nanos >= 10_000,
            "expected at least 10_000 ns, got {}",
            p.phases[0].nanos
        );
    }
}
