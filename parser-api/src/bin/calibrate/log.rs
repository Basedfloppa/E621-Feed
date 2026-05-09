//! `print_diff` (clamp-saturation detector) and `write_grid_log` (TOML
//! artifact writer).

use std::fs;
use std::path::PathBuf;

use chrono::Utc;

use e621_account_parser_api::utils::Priors;

use crate::metrics::Metrics;
use crate::options::GridOptions;

/// Prints non-default fields. Returns the names of any knob that landed at
/// its clamp boundary (caller emits a warning).
pub(crate) fn print_diff(baseline: &Priors, best: &Priors) -> Vec<String> {
    let mut saturated: Vec<String> = Vec::new();

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
            let v = best.$field;
            if (v - $lo).abs() < 1e-4 {
                saturated.push(format!("{}=lower clamp ({:.3})", $label, $lo as f32));
            } else if (v - $hi).abs() < 1e-4 {
                saturated.push(format!("{}=upper clamp ({:.3})", $label, $hi as f32));
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
    diff!("quality_w_relative_score", quality_w_relative_score, "{:.3}");
    diff!("quality_w_relative_comments", quality_w_relative_comments, "{:.3}");
    diff!("recency_tau_days", recency_tau_days, "{:.2}");
    diff!("recency_w_global", recency_w_global, "{:.3}");
    diff!("recency_w_personal", recency_w_personal, "{:.3}");
    diff!("recency_personal_floor_frac", recency_personal_floor_frac, "{:.3}");
    diff!("popularity_w_fav", popularity_w_fav, "{:.3}");
    diff!("popularity_w_duration", popularity_w_duration, "{:.3}");
    diff!("discrete_smoothing_alpha", discrete_smoothing_alpha, "{:.3}");
    diff!("discrete_pref_floor", discrete_pref_floor, "{:.3}");
    diff!("tag_relation_pmi_scale", tag_relation_pmi_scale, "{:.3}");
    diff!("tag_relation_w_global", tag_relation_w_global, "{:.3}");
    diff!("tag_relation_w_personal", tag_relation_w_personal, "{:.3}");
    diff!("tag_relation_cooc_ref", tag_relation_cooc_ref, "{:.2}");
    diff!("tag_relation_user_cooc_ref", tag_relation_user_cooc_ref, "{:.2}");
    diff!("coldstart_n0", coldstart_n0, "{:.1}");
    // v5.3 Class A
    diff!("idf_rsj_smoothing", idf_rsj_smoothing, "{:.3}");
    diff!("coldstart_smoothing_boost", coldstart_smoothing_boost, "{:.3}");
    diff!("interaction_ctr_prior_alpha", interaction_ctr_prior_alpha, "{:.3}");
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

    // Clamp-saturation: mirrors the clamps in knobs::GRID_KNOBS.
    check_clamp!("idf_lambda", idf_lambda, 0.0_f32, 1.5_f32);
    check_clamp!("idf_alpha", idf_alpha, 0.0_f32, 1.5_f32);
    check_clamp!("freq_alpha", freq_alpha, 0.0_f32, 1.5_f32);
    check_clamp!("df_floor", df_floor, 0.05_f32, 5.0_f32);
    check_clamp!("idf_max", idf_max, 1.0_f32, 200.0_f32);
    check_clamp!("bm25_k", bm25_k, 0.1_f32, 10.0_f32);
    check_clamp!("one_sided_ratio_exp", one_sided_ratio_exp, 0.05_f32, 3.0_f32);
    check_clamp!("recency_personal_floor_frac", recency_personal_floor_frac, 0.0_f32, 2.0_f32);
    check_clamp!("discrete_pref_floor", discrete_pref_floor, 0.0_f32, 0.5_f32);
    check_clamp!("idf_rsj_smoothing", idf_rsj_smoothing, 0.05_f32, 5.0_f32);
    check_clamp!("confidence_steepness", confidence_steepness, 0.1_f32, 5.0_f32);
    check_clamp!("mmr_redundancy_exp", mmr_redundancy_exp, 0.1_f32, 5.0_f32);
    check_clamp!("tag_sim_jaccard_blend", tag_sim_jaccard_blend, 0.0_f32, 1.0_f32);
    if !best.idf_lambda_meta.is_nan() {
        check_clamp!("idf_lambda_meta", idf_lambda_meta, 0.0_f32, 1.5_f32);
    }

    saturated
}

/// Persist the winning priors as a ready-to-paste TOML snippet under
/// `calibration_results/grid_<UTC-timestamp>.toml`.
pub(crate) fn write_grid_log(
    best: &Priors,
    metrics: &Metrics,
    opts: &GridOptions,
    elapsed: std::time::Duration,
    saturated: &[String],
) -> anyhow::Result<()> {
    let dir = PathBuf::from("calibration_results");
    fs::create_dir_all(&dir)?;
    let ts = Utc::now().format("%Y%m%d_%H%M%S");
    let path = dir.join(format!("grid_{ts}.toml"));

    let mut s = String::new();
    s.push_str(&format!("# calibrate grid result — {}\n", Utc::now().to_rfc3339()));
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
    if !saturated.is_empty() {
        s.push_str("# WARN clamp-saturated:\n");
        for sat in saturated {
            s.push_str(&format!("#   {sat}\n"));
        }
    }
    s.push_str("\n[priors]\n");
    s.push_str(&format!(
        "recency_tau_days = {:.2}\nquality_a = {:.3}\nquality_b = {:.3}\nquality_log_bias = {:.3}\n",
        best.recency_tau_days, best.quality_a, best.quality_b, best.quality_log_bias
    ));
    s.push_str(&format!(
        "mix_sim = {:.3}\nmix_quality = {:.3}\nmix_recency = {:.3}\nmix_rating = {:.3}\nmix_media = {:.3}\nmix_popularity = {:.3}\nmix_interaction = {:.3}\nmix_tag_relation = {:.3}\n",
        best.mix_sim, best.mix_quality, best.mix_recency, best.mix_rating, best.mix_media,
        best.mix_popularity, best.mix_interaction, best.mix_tag_relation
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

    fs::write(&path, s)?;
    eprintln!("[grid] wrote {}", path.display());
    Ok(())
}
