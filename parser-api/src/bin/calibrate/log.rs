//! `print_diff` (end-of-search classification: genuine clamp saturation vs
//! blend weights switched off on purpose) and `write_grid_log` (TOML artifact
//! writer).

use std::fs;
use std::path::PathBuf;

use chrono::Utc;

use e621_account_parser_api::utils::Priors;

use crate::metrics::Metrics;
use crate::options::GridOptions;

/// What the winning configuration cost against the baseline.
///
/// The two lists are kept apart because they call for opposite readings. A real
/// clamp boundary means the swept range may be too narrow. A `mix_*` weight at
/// zero does **not**: relative probes cannot reach zero by accident (see
/// `knobs::tests::only_the_explicit_off_probe_can_reach_zero`), so a zero there
/// means every interior point was measured, rejected, and only then was the
/// explicit `-1.0` off-probe accepted. Filing both under one heading —
/// "search range may be too narrow" — is what made the 2026-10-09 run read as
/// "the semantic channels are dead" rather than "the search switched them off".
pub(crate) struct DiffReport {
    /// Knobs that landed on a genuine clamp boundary.
    pub(crate) clamp_saturated: Vec<String>,
    /// `mix_*` weights switched off with the explicit `-1.0` probe.
    pub(crate) switched_off: Vec<String>,
}

/// Prints non-default fields and classifies how the search ended.
pub(crate) fn print_diff(baseline: &Priors, best: &Priors) -> DiffReport {
    let mut clamp_saturated: Vec<String> = Vec::new();
    let mut switched_off: Vec<String> = Vec::new();

    macro_rules! diff {
        ($label:literal, $field:ident, $fmt:literal) => {
            if (baseline.$field - best.$field).abs() > 1e-6 {
                println!(
                    concat!("{:<32} = ", $fmt, "    (was ", $fmt, ")"),
                    $label, best.$field, baseline.$field
                );
            }
        };
    }
    macro_rules! check_clamp {
        ($label:literal, $field:ident, $lo:expr, $hi:expr) => {
            // "Moved AND landed on a boundary", not "is on a boundary".
            // `tag_sim_jaccard_blend` ships at 0.0 (`config.toml`), so checking
            // the value alone named it as clamp-saturated on every run —
            // including the 2026-10-10 artifacts, where it never moved at all.
            // A knob that did not move gives no evidence the range is too
            // narrow; the search simply did not want to go there.
            let v = best.$field;
            let moved = (baseline.$field - v).abs() > 1e-6;
            if !moved {
                // no-op
            } else if (v - $lo).abs() < 1e-4 {
                clamp_saturated.push(format!("{}=lower clamp ({:.3})", $label, $lo as f32));
            } else if (v - $hi).abs() < 1e-4 {
                clamp_saturated.push(format!("{}=upper clamp ({:.3})", $label, $hi as f32));
            }
        };
    }
    /// A blend weight has no upper bound — `apply` only floors it at zero — so
    /// zero is the only boundary it can saturate against. Report it only when
    /// the search actually pushed it there: flagging every zero-valued weight
    /// would name the channels that simply ship at zero (`mix_exclusivity`,
    /// `mix_novelty`, `mix_artist_discovery`) on every run.
    macro_rules! check_mix_floor {
        ($label:literal, $field:ident) => {
            if (baseline.$field - best.$field).abs() > 1e-6 && best.$field.abs() < 1e-4 {
                switched_off.push(format!("{}=switched off (-1.0 probe, 0.000)", $label));
            }
        };
    }

    diff!("mix_sim", mix_sim, "{:.3}");
    diff!("mix_quality", mix_quality, "{:.3}");
    diff!("mix_recency", mix_recency, "{:.3}");
    diff!("mix_rating", mix_rating, "{:.3}");
    diff!("mix_media", mix_media, "{:.3}");
    diff!("mix_popularity", mix_popularity, "{:.3}");
    diff!("mix_interaction", mix_interaction, "{:.3}");
    diff!("mix_tag_relation", mix_tag_relation, "{:.3}");
    diff!("mix_uploader", mix_uploader, "{:.3}");
    diff!("mix_exclusivity", mix_exclusivity, "{:.3}");
    diff!("mix_novelty", mix_novelty, "{:.3}");
    diff!("mix_artist_discovery", mix_artist_discovery, "{:.3}");

    // L1-normalised mix block — the weights the scorer actually applies.
    let mix_sum = mix_raw_sum(best);
    if mix_sum > 1e-6 && (mix_sum - 1.0).abs() > 0.01 {
        println!("[mix L1-normalised, raw sum {mix_sum:.3}]");
        for (name, share) in mix_shares(best) {
            println!("  {name:<20} = {share:.3}");
        }
    }
    diff!("idf_lambda", idf_lambda, "{:.3}");
    diff!("idf_alpha", idf_alpha, "{:.3}");
    diff!("freq_alpha", freq_alpha, "{:.3}");
    diff!("df_floor", df_floor, "{:.3}");
    diff!("idf_max", idf_max, "{:.2}");
    diff!("bm25_k", bm25_k, "{:.3}");
    diff!("one_sided_ratio_exp", one_sided_ratio_exp, "{:.3}");
    diff!("quality_a", quality_a, "{:.3}");
    diff!("quality_b", quality_b, "{:.3}");
    diff!("quality_log_bias", quality_log_bias, "{:.3}");
    diff!("quality_w_absolute", quality_w_absolute, "{:.3}");
    diff!(
        "quality_w_relative_score",
        quality_w_relative_score,
        "{:.3}"
    );
    diff!(
        "quality_w_relative_comments",
        quality_w_relative_comments,
        "{:.3}"
    );
    diff!("recency_tau_days", recency_tau_days, "{:.2}");
    diff!("recency_w_global", recency_w_global, "{:.3}");
    diff!("recency_w_personal", recency_w_personal, "{:.3}");
    diff!(
        "recency_personal_floor_frac",
        recency_personal_floor_frac,
        "{:.3}"
    );
    diff!("popularity_w_fav", popularity_w_fav, "{:.3}");
    diff!("popularity_w_duration", popularity_w_duration, "{:.3}");
    diff!(
        "discrete_smoothing_alpha",
        discrete_smoothing_alpha,
        "{:.3}"
    );
    diff!("discrete_pref_floor", discrete_pref_floor, "{:.3}");
    diff!("tag_relation_pmi_scale", tag_relation_pmi_scale, "{:.3}");
    diff!("tag_relation_w_global", tag_relation_w_global, "{:.3}");
    diff!("tag_relation_w_personal", tag_relation_w_personal, "{:.3}");
    diff!("tag_relation_cooc_ref", tag_relation_cooc_ref, "{:.2}");
    diff!(
        "tag_relation_user_cooc_ref",
        tag_relation_user_cooc_ref,
        "{:.2}"
    );
    diff!("coldstart_n0", coldstart_n0, "{:.1}");
    // v5.3 Class A
    diff!("idf_rsj_smoothing", idf_rsj_smoothing, "{:.3}");
    diff!(
        "coldstart_smoothing_boost",
        coldstart_smoothing_boost,
        "{:.3}"
    );
    diff!(
        "interaction_ctr_prior_alpha",
        interaction_ctr_prior_alpha,
        "{:.3}"
    );
    // v5.3 Class B
    diff!("group_w_artist", group_w_artist, "{:.3}");
    diff!("group_w_character", group_w_character, "{:.3}");
    diff!("group_w_copyright", group_w_copyright, "{:.3}");
    diff!("group_w_species", group_w_species, "{:.3}");
    diff!("group_w_general", group_w_general, "{:.3}");
    diff!("group_w_lore", group_w_lore, "{:.3}");
    // v5.3 Class C
    diff!("score_temperature", score_temperature, "{:.3}");
    diff!("confidence_steepness", confidence_steepness, "{:.3}");
    diff!("mmr_redundancy_exp", mmr_redundancy_exp, "{:.3}");
    diff!("tag_sim_jaccard_blend", tag_sim_jaccard_blend, "{:.3}");
    // v5.3 Class D — splits print only when engaged.
    if !best.idf_lambda_meta.is_nan() {
        println!("{:<32} = {:.3}", "idf_lambda_meta", best.idf_lambda_meta);
    }
    if !best.recency_tau_recent.is_nan() {
        println!(
            "{:<32} = {:.2}  (split @ {:.0} d)",
            "recency_tau_recent", best.recency_tau_recent, best.recency_split_age_days
        );
    }
    if !best.tag_relation_pmi_scale_user.is_nan() {
        println!(
            "{:<32} = {:.3}",
            "tag_relation_pmi_scale_user", best.tag_relation_pmi_scale_user
        );
    }
    // v5.3 Class E
    if best.tag_relation_pair_aggregator != baseline.tag_relation_pair_aggregator {
        println!(
            "{:<32} = \"{}\"   (was \"{}\")",
            "tag_relation_pair_aggregator",
            best.tag_relation_pair_aggregator,
            baseline.tag_relation_pair_aggregator
        );
    }

    // v5.11: exclusivity channel
    diff!("exclusivity_scale", exclusivity_scale, "{:.3}");
    diff!(
        "exclusivity_cross_group_weight",
        exclusivity_cross_group_weight,
        "{:.3}"
    );
    // Integer knobs printed as-is (diff! uses {:.3} for f32)
    if (baseline.min_exclusivity_cooc - best.min_exclusivity_cooc).abs() > 0 {
        println!(
            "{:<32} = {}    (was {})",
            "min_exclusivity_cooc", best.min_exclusivity_cooc, baseline.min_exclusivity_cooc
        );
    }
    if baseline.exclusivity_max_tags != best.exclusivity_max_tags {
        println!(
            "{:<32} = {}    (was {})",
            "exclusivity_max_tags", best.exclusivity_max_tags, baseline.exclusivity_max_tags
        );
    }

    // v5.11: novelty channel
    diff!("novelty_n0", novelty_n0, "{:.1}");
    // novelty_use_feedback is a boolean toggle — print when different
    if best.novelty_use_feedback != baseline.novelty_use_feedback {
        println!(
            "{:<32} = {}    (was {})",
            "novelty_use_feedback", best.novelty_use_feedback, baseline.novelty_use_feedback
        );
    }

    // v5.12: diversity user-PMI amplification
    diff!(
        "diversity_user_pmi_weight",
        diversity_user_pmi_weight,
        "{:.3}"
    );

    // Clamp-saturation: mirrors the clamps in knobs::GRID_KNOBS.
    check_clamp!("idf_lambda", idf_lambda, 0.0_f32, 1.5_f32);
    check_clamp!("idf_alpha", idf_alpha, 0.0_f32, 1.5_f32);
    check_clamp!("freq_alpha", freq_alpha, 0.0_f32, 1.5_f32);
    check_clamp!("df_floor", df_floor, 0.05_f32, 5.0_f32);
    check_clamp!("idf_max", idf_max, 1.0_f32, 200.0_f32);
    check_clamp!("bm25_k", bm25_k, 0.1_f32, 10.0_f32);
    check_clamp!(
        "one_sided_ratio_exp",
        one_sided_ratio_exp,
        0.05_f32,
        3.0_f32
    );
    check_clamp!(
        "recency_personal_floor_frac",
        recency_personal_floor_frac,
        0.0_f32,
        2.0_f32
    );
    check_clamp!("discrete_pref_floor", discrete_pref_floor, 0.0_f32, 0.5_f32);
    check_clamp!("idf_rsj_smoothing", idf_rsj_smoothing, 0.05_f32, 5.0_f32);
    check_clamp!(
        "confidence_steepness",
        confidence_steepness,
        0.1_f32,
        5.0_f32
    );
    check_clamp!("mmr_redundancy_exp", mmr_redundancy_exp, 0.1_f32, 5.0_f32);
    check_clamp!(
        "tag_sim_jaccard_blend",
        tag_sim_jaccard_blend,
        0.0_f32,
        1.0_f32
    );
    // All twelve blend weights, so a channel driven to zero is always named.
    check_mix_floor!("mix_sim", mix_sim);
    check_mix_floor!("mix_quality", mix_quality);
    check_mix_floor!("mix_recency", mix_recency);
    check_mix_floor!("mix_rating", mix_rating);
    check_mix_floor!("mix_media", mix_media);
    check_mix_floor!("mix_popularity", mix_popularity);
    check_mix_floor!("mix_interaction", mix_interaction);
    check_mix_floor!("mix_tag_relation", mix_tag_relation);
    check_mix_floor!("mix_uploader", mix_uploader);
    check_mix_floor!("mix_exclusivity", mix_exclusivity);
    check_mix_floor!("mix_novelty", mix_novelty);
    check_mix_floor!("mix_artist_discovery", mix_artist_discovery);
    check_clamp!("novelty_n0", novelty_n0, 1.0_f32, 50.0_f32);
    check_clamp!(
        "exclusivity_cross_group_weight",
        exclusivity_cross_group_weight,
        0.1_f32,
        10.0_f32
    );
    check_clamp!(
        "diversity_user_pmi_weight",
        diversity_user_pmi_weight,
        0.1_f32,
        10.0_f32
    );
    if !best.idf_lambda_meta.is_nan() {
        check_clamp!("idf_lambda_meta", idf_lambda_meta, 0.0_f32, 1.5_f32);
    }

    DiffReport {
        clamp_saturated,
        switched_off,
    }
}

/// Total of the twelve `mix_*` weights before normalisation. Mirrors
/// `MixWeights::from_priors`, which is what the scorer divides by; summing
/// only the first eight (as this did) understated the total whenever
/// `mix_uploader` / `mix_exclusivity` / `mix_novelty` /
/// `mix_artist_discovery` were non-zero, and so misreported every share.
fn mix_raw_sum(p: &Priors) -> f32 {
    p.mix_sim
        + p.mix_quality
        + p.mix_recency
        + p.mix_rating
        + p.mix_media
        + p.mix_popularity
        + p.mix_interaction
        + p.mix_tag_relation
        + p.mix_uploader
        + p.mix_exclusivity
        + p.mix_novelty
        + p.mix_artist_discovery
}

/// The normalised blend the scorer applies, as `(name, share)`. Empty when the
/// block sums to zero, which is what `MixWeights::from_priors` treats as
/// "fall back to the defaults".
fn mix_shares(p: &Priors) -> Vec<(&'static str, f32)> {
    let sum = mix_raw_sum(p);
    if sum <= 1e-6 {
        return Vec::new();
    }
    vec![
        ("mix_sim", p.mix_sim / sum),
        ("mix_quality", p.mix_quality / sum),
        ("mix_recency", p.mix_recency / sum),
        ("mix_rating", p.mix_rating / sum),
        ("mix_media", p.mix_media / sum),
        ("mix_popularity", p.mix_popularity / sum),
        ("mix_interaction", p.mix_interaction / sum),
        ("mix_tag_relation", p.mix_tag_relation / sum),
        ("mix_uploader", p.mix_uploader / sum),
        ("mix_exclusivity", p.mix_exclusivity / sum),
        ("mix_novelty", p.mix_novelty / sum),
        ("mix_artist_discovery", p.mix_artist_discovery / sum),
    ]
}

/// Persist the winning priors as a ready-to-paste TOML snippet under
/// `calibration_results/grid_<UTC-timestamp>.toml`.
pub(crate) fn write_grid_log(
    best: &Priors,
    metrics: &Metrics,
    opts: &GridOptions,
    elapsed: std::time::Duration,
    diff: &DiffReport,
) -> anyhow::Result<()> {
    let dir = PathBuf::from("calibration_results");
    fs::create_dir_all(&dir)?;
    let ts = Utc::now().format("%Y%m%d_%H%M%S");
    let path = dir.join(format!("grid_{ts}.toml"));

    let mut s = String::new();
    s.push_str(&format!(
        "# calibrate grid result — {}\n",
        Utc::now().to_rfc3339()
    ));
    s.push_str(&format!(
        "# split={} neg={} diversify={} pairs_only={} run_paired={}\n",
        opts.split.label(),
        opts.neg_mode.label(),
        opts.diversify,
        opts.pairs_only,
        opts.run_paired
    ));
    s.push_str(&format!(
        "# N={} NDCG@20={:.4} Recall@50={:.4} MRR={:.4} elapsed={:.1}s\n",
        metrics.n_accounts,
        metrics.ndcg_at_k,
        metrics.recall_at_k,
        metrics.mrr,
        elapsed.as_secs_f32()
    ));
    if !diff.clamp_saturated.is_empty() {
        s.push_str("# WARN clamp-saturated (search range may be too narrow):\n");
        for sat in &diff.clamp_saturated {
            s.push_str(&format!("#   {sat}\n"));
        }
    }
    if !diff.switched_off.is_empty() {
        s.push_str(
            "# NOTE mix weights switched off by the search — interior points were\n\
             #   measured and rejected first, then the explicit -1.0 off-probe won:\n",
        );
        for off in &diff.switched_off {
            s.push_str(&format!("#   {off}\n"));
        }
    }
    s.push_str(&priors_toml(best));

    fs::write(&path, s)?;
    eprintln!("[grid] wrote {}", path.display());
    Ok(())
}

/// The `[priors]` body of the result file. Split out of `write_grid_log` so a
/// test can assert on the text: the defect this guards against was a field
/// silently missing from the file, which nothing but the content can catch.
fn priors_toml(best: &Priors) -> String {
    let mut s = String::new();
    s.push_str("\n[priors]\n");
    s.push_str(&format!(
        "recency_tau_days = {:.2}\nquality_a = {:.3}\nquality_b = {:.3}\nquality_log_bias = {:.3}\n",
        best.recency_tau_days, best.quality_a, best.quality_b, best.quality_log_bias
    ));
    // All twelve blend weights — the scorer normalises the whole block, so a
    // file that omits any of them does not reproduce the run it describes.
    s.push_str(&format!(
        "mix_sim = {:.3}\nmix_quality = {:.3}\nmix_recency = {:.3}\nmix_rating = {:.3}\nmix_media = {:.3}\nmix_popularity = {:.3}\nmix_interaction = {:.3}\nmix_tag_relation = {:.3}\nmix_uploader = {:.3}\nmix_exclusivity = {:.3}\nmix_novelty = {:.3}\nmix_artist_discovery = {:.3}\n",
        best.mix_sim, best.mix_quality, best.mix_recency, best.mix_rating, best.mix_media,
        best.mix_popularity, best.mix_interaction, best.mix_tag_relation, best.mix_uploader,
        best.mix_exclusivity, best.mix_novelty, best.mix_artist_discovery
    ));
    s.push_str(&format!(
        "df_floor = {:.3}\nidf_max = {:.2}\nidf_lambda = {:.3}\nidf_alpha = {:.3}\nfreq_alpha = {:.3}\nbm25_k = {:.3}\none_sided_ratio_exp = {:.3}\nidf_rsj_smoothing = {:.3}\n",
        best.df_floor, best.idf_max, best.idf_lambda, best.idf_alpha, best.freq_alpha,
        best.bm25_k, best.one_sided_ratio_exp, best.idf_rsj_smoothing
    ));
    s.push_str(&format!(
        "group_w_artist = {:.3}\ngroup_w_character = {:.3}\ngroup_w_copyright = {:.3}\ngroup_w_species = {:.3}\ngroup_w_general = {:.3}\ngroup_w_lore = {:.3}\n",
        best.group_w_artist, best.group_w_character, best.group_w_copyright,
        best.group_w_species, best.group_w_general, best.group_w_lore
    ));
    s.push_str(&format!(
        "coldstart_smoothing_boost = {:.3}\ninteraction_ctr_prior_alpha = {:.3}\nscore_temperature = {:.3}\nconfidence_steepness = {:.3}\nmmr_redundancy_exp = {:.3}\ntag_sim_jaccard_blend = {:.3}\n",
        best.coldstart_smoothing_boost, best.interaction_ctr_prior_alpha,
        best.score_temperature, best.confidence_steepness, best.mmr_redundancy_exp,
        best.tag_sim_jaccard_blend
    ));
    if !best.idf_lambda_meta.is_nan() {
        s.push_str(&format!("idf_lambda_meta = {:.3}\n", best.idf_lambda_meta));
    }
    if !best.recency_tau_recent.is_nan() {
        s.push_str(&format!(
            "recency_tau_recent = {:.2}\nrecency_split_age_days = {:.1}\n",
            best.recency_tau_recent, best.recency_split_age_days
        ));
    }
    if !best.tag_relation_pmi_scale_user.is_nan() {
        s.push_str(&format!(
            "tag_relation_pmi_scale_user = {:.3}\n",
            best.tag_relation_pmi_scale_user
        ));
    }
    s.push_str(&format!(
        "tag_relation_pair_aggregator = \"{}\"\n",
        best.tag_relation_pair_aggregator
    ));
    s.push_str(&format!(
        "quality_w_absolute = {:.3}\nquality_w_relative_score = {:.3}\nquality_w_relative_comments = {:.3}\n",
        best.quality_w_absolute, best.quality_w_relative_score, best.quality_w_relative_comments
    ));
    s.push_str(&format!(
        "popularity_w_fav = {:.3}\npopularity_w_duration = {:.3}\n",
        best.popularity_w_fav, best.popularity_w_duration
    ));
    s.push_str(&format!(
        "recency_w_global = {:.3}\nrecency_w_personal = {:.3}\nrecency_personal_floor_frac = {:.3}\nrecency_log_personal = {}\n",
        best.recency_w_global, best.recency_w_personal, best.recency_personal_floor_frac, best.recency_log_personal
    ));
    s.push_str(&format!(
        "discrete_smoothing_alpha = {:.3}\ndiscrete_pref_floor = {:.3}\nstrong_negative_count = {}\nstrong_negative_penalty = {:.3}\nstrong_negative_wilson_threshold = {:.3}\n",
        best.discrete_smoothing_alpha, best.discrete_pref_floor, best.strong_negative_count,
        best.strong_negative_penalty, best.strong_negative_wilson_threshold
    ));
    s.push_str(&format!(
        "tag_relation_w_global = {:.3}\ntag_relation_w_personal = {:.3}\ntag_relation_pmi_scale = {:.3}\ntag_relation_min_cooc = {}\ntag_relation_user_min_cooc = {}\ntag_relation_cooc_ref = {:.2}\ntag_relation_user_cooc_ref = {:.2}\n",
        best.tag_relation_w_global, best.tag_relation_w_personal, best.tag_relation_pmi_scale,
        best.tag_relation_min_cooc, best.tag_relation_user_min_cooc, best.tag_relation_cooc_ref,
        best.tag_relation_user_cooc_ref
    ));
    s.push_str(&format!(
        "feedback_decay_half_life_days = {:.2}\nmeta_interaction_weight = {:.3}\ncoldstart_n0 = {:.1}\n",
        best.feedback_decay_half_life_days, best.meta_interaction_weight, best.coldstart_n0
    ));
    s.push_str(&format!(
        "diversity_window = {}\ndiversity_w_artist = {:.3}\ndiversity_w_character = {:.3}\ndiversity_w_general = {:.3}\ndiversity_max_penalty = {:.3}\ndiversity_interaction_damp = {:.3}\n",
        best.diversity_window, best.diversity_w_artist, best.diversity_w_character,
        best.diversity_w_general, best.diversity_max_penalty, best.diversity_interaction_damp
    ));
    // v5.11: exclusivity + novelty channel knobs
    s.push_str(&format!(
        "exclusivity_scale = {:.3}\nexclusivity_cross_group_weight = {:.3}\nexclusivity_max_tags = {}\n",
        best.exclusivity_scale, best.exclusivity_cross_group_weight, best.exclusivity_max_tags
    ));
    s.push_str(&format!(
        "novelty_n0 = {:.1}\nnovelty_use_feedback = {}\n",
        best.novelty_n0, best.novelty_use_feedback
    ));
    // v5.12: diversity user-PMI amplification
    s.push_str(&format!(
        "diversity_user_pmi_weight = {:.3}\n",
        best.diversity_user_pmi_weight
    ));

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values the grid starts from (`config.toml`). `Priors::default()`
    /// ships three of the twelve mix weights at zero, which would make the
    /// "was it pushed to the floor?" check vacuous for those.
    fn shipped_priors() -> Priors {
        Priors {
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
            ..Priors::default()
        }
    }

    const MIX_KEYS: [&str; 12] = [
        "mix_sim",
        "mix_quality",
        "mix_recency",
        "mix_rating",
        "mix_media",
        "mix_popularity",
        "mix_interaction",
        "mix_tag_relation",
        "mix_uploader",
        "mix_exclusivity",
        "mix_novelty",
        "mix_artist_discovery",
    ];

    /// Every weight the search drives to zero has to be named. The run that
    /// produced this guard zeroed nine channels and the warning listed two.
    #[test]
    fn every_mix_weight_driven_to_zero_is_reported() {
        let baseline = shipped_priors();
        let mut best = baseline.clone();
        best.mix_sim = 0.0;
        best.mix_quality = 0.0;
        best.mix_recency = 0.0;
        best.mix_rating = 0.0;
        best.mix_media = 0.0;
        best.mix_popularity = 0.0;
        best.mix_interaction = 0.0;
        best.mix_tag_relation = 0.0;
        best.mix_uploader = 0.0;
        best.mix_exclusivity = 0.0;
        best.mix_novelty = 0.0;
        best.mix_artist_discovery = 0.0;

        let diff = print_diff(&baseline, &best);
        for key in MIX_KEYS {
            assert!(
                diff.switched_off.iter().any(|s| s.starts_with(key)),
                "{key} was zeroed but not reported; got {:?}",
                diff.switched_off
            );
        }
        assert!(
            diff.clamp_saturated.is_empty(),
            "a switched-off blend weight is not a clamp boundary; got {:?}",
            diff.clamp_saturated
        );
    }

    /// A weight that ships at zero and was never touched is not a saturation —
    /// reporting it would name three knobs on every single run.
    #[test]
    fn a_mix_weight_that_ships_at_zero_and_stays_is_not_reported() {
        let mut baseline = shipped_priors();
        // Ships at zero in `Priors::default()`. It has to stay unreported while
        // it is untouched, otherwise the warning names it on every single run —
        // which is what makes "changed and at the floor" the right condition
        // rather than "at the floor".
        baseline.mix_novelty = 0.0;
        let best = baseline.clone();
        let diff = print_diff(&baseline, &best);
        for key in MIX_KEYS {
            assert!(
                !diff.switched_off.iter().any(|s| s.starts_with(key)),
                "{key} did not change but was reported; got {:?}",
                diff.switched_off
            );
        }
    }

    /// A knob that ships on its boundary and never moves is not evidence that
    /// the swept range is too narrow. `tag_sim_jaccard_blend` defaults to 0.0,
    /// and the old value-only check named it clamp-saturated in **both**
    /// 2026-10-10 artifacts even though it never moved.
    #[test]
    fn a_knob_that_ships_on_its_boundary_and_never_moves_is_not_saturated() {
        let baseline = shipped_priors();
        assert!(
            baseline.tag_sim_jaccard_blend.abs() < 1e-6,
            "this test is about a knob whose shipped default is the lower boundary"
        );
        let best = baseline.clone();
        let diff = print_diff(&baseline, &best);
        assert!(
            diff.clamp_saturated.is_empty(),
            "an unmoved knob is not a clamp boundary; got {:?}",
            diff.clamp_saturated
        );
    }

    /// The genuine case: the search moved a knob and it stopped exactly on a
    /// limit, so the optimum may lie outside the swept range.
    #[test]
    fn a_knob_that_moved_onto_a_boundary_is_reported() {
        let baseline = shipped_priors();
        let mut best = baseline.clone();
        // `idf_lambda` is clamped to [0.0, 1.5] by the knob's `apply`.
        best.idf_lambda = 1.5;
        let diff = print_diff(&baseline, &best);
        assert!(
            diff.clamp_saturated
                .iter()
                .any(|s| s.starts_with("idf_lambda=upper clamp")),
            "a knob that moved onto its ceiling must be reported; got {:?}",
            diff.clamp_saturated
        );
        assert!(
            !diff
                .switched_off
                .iter()
                .any(|s| s.starts_with("idf_lambda")),
            "a non-mix knob must never be filed as a switched-off blend weight"
        );
    }

    /// The result file has to reproduce the run it describes. It used to omit
    /// `mix_uploader`, `mix_exclusivity`, `mix_novelty` and
    /// `mix_artist_discovery`, so reloading it scored a different configuration
    /// than the one reported as best.
    #[test]
    fn the_result_file_records_every_mix_weight() {
        let mut p = shipped_priors();
        p.mix_uploader = 0.04;
        p.mix_exclusivity = 0.011;
        p.mix_novelty = 0.012;
        p.mix_artist_discovery = 0.013;

        let toml = priors_toml(&p);
        for key in MIX_KEYS {
            assert!(
                toml.contains(&format!("{key} = ")),
                "{key} missing from the result file"
            );
        }
        // And with the right values, not just the right keys.
        assert!(toml.contains("mix_uploader = 0.040"), "{toml}");
        assert!(toml.contains("mix_exclusivity = 0.011"), "{toml}");
        assert!(toml.contains("mix_novelty = 0.012"), "{toml}");
        assert!(toml.contains("mix_artist_discovery = 0.013"), "{toml}");
    }

    /// The printed shares have to cover the same twelve fields the scorer
    /// divides by — and actually be shares of the whole block.
    #[test]
    fn the_printed_shares_cover_every_mix_weight_and_sum_to_one() {
        let p = shipped_priors();
        let shares = mix_shares(&p);
        let names: Vec<&str> = shares.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            MIX_KEYS.to_vec(),
            "share block and mix block disagree"
        );
        let total: f32 = shares.iter().map(|(_, s)| *s).sum();
        assert!((total - 1.0).abs() < 1e-5, "shares sum to {total}, not 1");

        // The divisor is the whole block, not a subset of it. Enumerated here
        // independently so that dropping a field from `mix_raw_sum` — the
        // original defect — fails rather than silently shifting every share.
        let expected: f32 = MIX_KEYS.iter().map(|k| mix_value(&p, k)).sum();
        let got = mix_raw_sum(&p);
        assert!(
            (got - expected).abs() < 1e-4,
            "raw sum {got} != sum of the twelve weights {expected}"
        );
    }

    /// Reads a mix weight by name. Deliberately independent of the production
    /// field list: it is what makes the check above a cross-check rather than a
    /// restatement.
    fn mix_value(p: &Priors, name: &str) -> f32 {
        match name {
            "mix_sim" => p.mix_sim,
            "mix_quality" => p.mix_quality,
            "mix_recency" => p.mix_recency,
            "mix_rating" => p.mix_rating,
            "mix_media" => p.mix_media,
            "mix_popularity" => p.mix_popularity,
            "mix_interaction" => p.mix_interaction,
            "mix_tag_relation" => p.mix_tag_relation,
            "mix_uploader" => p.mix_uploader,
            "mix_exclusivity" => p.mix_exclusivity,
            "mix_novelty" => p.mix_novelty,
            "mix_artist_discovery" => p.mix_artist_discovery,
            other => panic!("{other} is not a mix_* knob"),
        }
    }

    /// `print_diff` announces changes through `diff!`; a knob with no `diff!`
    /// line can change without ever appearing in the summary. That is how
    /// `mix_artist_discovery` went missing while the run zeroed it.
    #[test]
    fn the_summary_has_a_diff_line_for_every_mix_weight() {
        let src = include_str!("log.rs");
        for key in MIX_KEYS {
            assert!(
                src.contains(&format!("diff!(\"{key}\"")),
                "{key} has no diff! line in print_diff"
            );
        }
    }
}
