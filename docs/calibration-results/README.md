# Calibration results

Manual write-ups of `calibrate grid` runs. The unfiltered `[best]`
weights from each grid run plus the production defaults distilled from
them. Drop new files in this folder as you accumulate runs (e.g.
`v5.4-N1000.md`, `2026-05-10-paired-only.md`); machine-readable TOML
artifacts continue to land in `calibration_results/` at the repo root.

See [`../calibration.md`](../calibration.md) for the harness, sweep
modes, knob descriptions, and caveats on holdout artifacts before
copying numbers into `config.example.toml`.

## History

The right-most bold column is the live default in
[`../../parser-api/config.example.toml`](../../parser-api/config.example.toml).
Earlier columns are kept for reference: each `[best]` lists the raw
grid winner, and the matching `default` column shows what was actually
copied into prod (typically a partial step toward `[best]` to hedge
against holdout artifacts described in [`../calibration.md`](../calibration.md)).

| Knob | Pre-v5 | N=150 v5 best | v5 default | N=915 v5.1 best | v5.1 default | N=500 v5.4 best (post_id) | **v5.4 default** |
|---|---|---|---|---|---|---|---|
| `mix_sim` | 0.48 | 0.63 | 0.58 | 0.63 | 0.60 | 0.80 | **0.65** |
| `mix_quality` | 0.10 | 0.00 | 0.05 | 0.00 | 0.05 | 0.00 | **0.04** |
| `mix_recency` | 0.07 | 0.00 | 0.04 | 0.00 | 0.04 | 0.00 | **0.03** |
| `mix_rating` | 0.10 | 0.00 | 0.07 | 0.00 | 0.07 | 0.00 | **0.05** |
| `mix_media` | 0.08 | 0.03 | 0.05 | 0.05 | 0.05 | 0.05 | **0.05** |
| `mix_popularity` | 0.07 | 0.00 | 0.04 | 0.00 | 0.04 | 0.00 | **0.03** |
| `mix_interaction` | 0.10 | 0.10 | 0.10 | 0.10 | 0.10 | 0.10 | **0.10** |
| `mix_tag_relation` | 0.08 | 0.08 | 0.08 | 0.08 | 0.08 | 0.08 | **0.08** |
| `idf_lambda` | 0.35 | 0.80 | 0.55 | 1.00 (pinned) | 0.70 | 1.00 (pinned) | **0.85** |
| `idf_alpha` | 0.65 | 1.00 (pinned) | 0.85 | 1.00 (pinned) | 0.92 | 1.00 (pinned) | **0.96** |
| `freq_alpha` | 0.45 | 0.90 | 0.65 | 1.00 (pinned) | 0.80 | 1.00 (pinned) | **0.90** |
| `df_floor` | — | — | — | — | 0.70 | 0.15 | **0.40** |
| `bm25_k` | — | — | — | — | 1.6 | 2.45 | **2.0** |
| `idf_rsj_smoothing` | — | — | — | — | 0.5 | 0.30 | **0.4** |
| `group_w_artist` | — | — | — | — | 2.0 | 2.525 | **2.25** |
| `group_w_character` | — | — | — | — | 1.6 | 2.075 | **1.85** |
| `group_w_copyright` | — | — | — | — | 1.2 | 1.40 | **1.35** |
| `group_w_species` | — | — | — | — | 1.1 | 1.25 | **1.20** |
| `group_w_general` | — | — | — | — | 1.0 | 0.738 | **0.85** |
| `group_w_lore` | — | — | — | — | 0.45 | 0.45 | **0.40** |
| `tag_relation_pmi_scale` | 5.00 | 2.00 | 3.5 | 3.5 | 3.5 | 3.5 | **3.5** |
| `tag_relation_cooc_ref` | 20.0 | 13.0 | 16.0 | 16.0 | 16.0 | 16.0 | **16.0** |

### v5.4 grid context (May 2026)

Three N=500 grid runs informed the v5.4 defaults:
- [`grid_20260509_195315.toml`](grid_20260509_195315.toml) — post_id split, no diversify (NDCG@20 0.8242)
- [`grid_20260509_202704.toml`](grid_20260509_202704.toml) — random split, no diversify (NDCG@20 0.9226 — 10 pp easier task by construction)
- [`grid_20260509_234627.toml`](grid_20260509_234627.toml) — post_id split, with diversify (NDCG@20 0.8254, ~6× slower)

`idf_lambda` / `idf_alpha` / `freq_alpha` pinned at the upper clamp
(1.0) in 2-of-3 runs; their clamps were widened to **1.5** in v5.4 so
the next grid can escape the wall. The `[best]` column above is from
the post_id no-diversify run; the diversify run agrees within 1–3 % on
every knob.

### v5.3 added knobs (defaults match prior production behaviour)

| Class | Knobs | Default |
|---|---|---|
| A — promoted constants | `idf_rsj_smoothing` / `coldstart_smoothing_boost` / `interaction_ctr_prior_alpha` | 0.5 / 2.0 / 4.0 |
| B — per-group multipliers | `group_w_{artist,character,copyright,species,general,lore}` | 2.0 / 1.6 / 1.2 / 1.1 / 1.0 / 0.45 |
| C — algorithmic shape | `score_temperature` / `confidence_steepness` / `mmr_redundancy_exp` / `tag_sim_jaccard_blend` | 0.0 / 1.0 / 1.0 / 0.0 (all no-ops) |
| D — point splits (NaN = off) | `idf_lambda_meta` / `recency_tau_recent` (with `recency_split_age_days=30`) / `tag_relation_pmi_scale_user` | NaN |
| E — categorical | `tag_relation_pair_aggregator` ∈ {mean, max, geomean} | "mean" |

The v5.x defaults take **partial steps** toward `[best]` rather than
copying it verbatim — the `mix_quality` / `mix_recency` /
`mix_popularity` / `mix_rating` columns drift to 0 because of the
holdout artifacts described in [`../calibration.md`](../calibration.md);
aggressive zero-ing those would produce a worse production feed. The
IDF / `freq_alpha` moves are trustworthy in direction but the N=915
grid pinned all three to the upper clamp boundary (1.0), which signals
that random-negative retrieval keeps rewarding ever-sharper rare-tag
contrast — also a known artifact. v5.1 advances each by ~½ of the gap
between the v5 default and the saturated [best], leaving headroom for
online tuning.

NDCG@20 / Recall@50 / MRR on the N=915 v5.1 full-grid `[best]` run was
**0.7274 / 0.1475 / 0.8871**. Real production lift will be a fraction
of this — copy via the bucket A/B mechanism rather than as an
unconditional rollout.
