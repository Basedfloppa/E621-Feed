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

| Knob | v5.5 | v5.7 (rolled back) | post_id best (N=915) | time_causal best (N=915, n=3) | **v5.9 default** |
|---|---|---|---|---|---|
| `mix_sim` | 0.72 | 0.80 | 0.945 | 0.720 (3/3) | **0.72** |
| `mix_quality` | 0.03 | 0.02 | 0 | 0 (3/3) | **0.02** |
| `mix_recency` | 0.02 | 0.02 | 0 | 0 (3/3) | **0.02** |
| `mix_rating` | 0.04 | 0.03 | 0.013 | 0.040 (3/3) | **0.04** |
| `mix_media` | 0.05 | 0.04 | 0.013–0.025 | 0.050 (3/3) | **0.05** |
| `mix_popularity` | 0.02 | 0.02 | 0 | 0 (3/3) | **0.02** |
| `mix_interaction` | 0.10 | 0.10 | 0.10 | 0.10 | **0.10** |
| `mix_tag_relation` | 0.08 | 0.08 | 0.08 | 0.08 | **0.08** |
| `idf_lambda` | 1.00 | 1.00 | 1.00 | 1.000 | **1.00** |
| `idf_alpha` | 1.05 | 1.05 | 1.05 | 1.050 | **1.05** |
| `freq_alpha` | 0.95 | 1.00 | 1.025–1.075 | 0.950 (3/3) | **0.95** |
| `df_floor` | 0.40 | 0.20 | 0.10 (5/6) | 0.400 (3/3) | **0.40** |
| `bm25_k` | 2.25 | 2.40 | 2.40–2.95 | 2.250 (3/3) | **2.25** |
| `idf_rsj_smoothing` | 0.35 | 0.25 | 0.15 / 0.35 | 0.350 (3/3) | **0.35** |
| `group_w_artist` | 2.40 | 2.65 | 2.85 (4/4) | 2.400 (3/3) | **2.40** |
| `group_w_character` | 2.00 | 2.15 | 2.20–2.35 | 2.000 (3/3) | **2.00** |
| `group_w_copyright` | 1.45 | 1.30 | 1.05–1.25 | 1.450 (3/3) | **1.45** |
| `group_w_species` | 1.30 | 1.20 | 0.975–1.30 | 1.300 (3/3) | **1.30** |
| `group_w_general` | 0.80 | 0.65 | 0.55 (4/4) | 0.700 (3/3) | **0.70** ⬇ |
| `group_w_lore` | 0.40 | 0.40 | 0.40 | 0.400 | **0.40** |
| `diversity_max_penalty` *(UX)* | 0.45 | 0.45 | 0.45 | 0.450 | **0.20** ⬇ |
| `tag_relation_pmi_scale` | 3.5 | 3.5 | 3.5 | 3.500 | **3.5** |
| `tag_relation_cooc_ref` | 16.0 | 16.0 | 16.0 | 16.00 | **16.0** |
| `tag_relation_min_cooc` *(v5.6)* | — | — | 2 | 2 | **2** |

(Pre-v5 / v5.1 / v5.4 / N=150 / N=500 / N=915 v5.4-best columns trimmed
for readability; preserved in git history.)

### v5.4 grid context (May 2026)

Three N=500 grid runs informed the v5.4 defaults:
- [`grid_20260509_195315.toml`](grid_20260509_195315.toml) — post_id split, no diversify (NDCG@20 0.8242)
- [`grid_20260509_202704.toml`](grid_20260509_202704.toml) — random split, no diversify (NDCG@20 0.9226 — 10 pp easier task by construction)
- [`grid_20260509_234627.toml`](grid_20260509_234627.toml) — post_id split, with diversify (NDCG@20 0.8254, ~6× slower)

`idf_lambda` / `idf_alpha` / `freq_alpha` pinned at the upper clamp
(1.0) in 2-of-3 runs; their clamps were widened to **1.5** in v5.4 so
the next grid could escape the wall.

### v5.5 grid context (May 2026, N=915 follow-up)

Four N=915 grid runs validated the widened clamps and refined v5.4 defaults:
- [`grid_20260510_000837.toml`](grid_20260510_000837.toml) — post_id, no diversify (NDCG@20 0.8228, 426 s)
- [`grid_20260510_010035.toml`](grid_20260510_010035.toml) — post_id, with diversify (NDCG@20 0.8256, 3092 s)
- [`grid_20260510_010852.toml`](grid_20260510_010852.toml) — random, no diversify (NDCG@20 0.9149, 460 s)
- [`grid_20260510_015410.toml`](grid_20260510_015410.toml) — random, with diversify (NDCG@20 0.9158, 2698 s)

Headlines:

- **The widened IDF clamps barely moved.** `idf_lambda=1.012` in **all 4
  runs** (one probe step above the old wall); `idf_alpha=1.07–1.095`;
  `freq_alpha=0.84–1.09`. The true optima sit ~just above 1.0, not in
  the 1.4+ region. The old clamp was biting, but only modestly.
- **Diversify barely affects offline metrics.** ΔNDCG@20 between on/off
  is +0.003 (post_id) and +0.001 (random). MMR is a UX feature, not an
  offline-metric tuning knob — running grids with `--with-diversify`
  every time is wasted compute (it's ~6–7× slower for 0.3 % gain).
- **Group weights are highly stable.** `group_w_{artist,character,
  general}` were *identical* across all 4 runs (2.525 / 2.125 / 0.738).
  v5.5 takes a confident step toward those.
- **`df_floor` is noise-dominated** under mixed-hard negatives (range
  0.10–0.70 across runs). Held at 0.40.
- **Random vs post_id.** Random NDCG@20 ≈ 0.92 vs post_id ≈ 0.82 — the
  10pp gap is the split, not progress. post_id is the honest signal;
  recency-related knobs in particular diverge between the two splits
  (random pulls `recency_tau_days` to 15–17, post_id holds at 10).

NDCG@20 / Recall@50 / MRR on the N=915 v5.4 post_id no-diversify
`[best]` run was **0.8228 / 0.1742 / 0.9333** — markedly higher than
the v5.1 number (0.7274 / 0.1475 / 0.8871) but most of that lift is
from the v5.4 group-weight bumps and (now extended) IDF parameters
already shipping in prod. Real production gain v5.4 → v5.5 will be
small; copy via the bucket A/B mechanism, not unconditional rollout.

### v5.9 — stability run + UX fixes (May 2026, N=915)

Two follow-up runs on the v5.8 baseline plus a diversify counterpart:

- [`grid_20260510_174257.toml`](grid_20260510_174257.toml) — time_causal,
  no diversify (NDCG@20 0.8024, basically identical to 133817's 0.8026)
- [`grid_20260510_184616.toml`](grid_20260510_184616.toml) — time_causal,
  with diversify, N=800 (NDCG@20 0.8116, ΔRecall@50 -0.022)

**Stability confirmed.** Three independent `time_causal` runs all
converged to the same `[best]` (down to the 3rd decimal). Drift between
runs 174257 and 133817 is 0.0002 NDCG@20 — well below the SE-based
acceptance threshold (~0.0066). v5.9 takes the only consistent grid
move: `group_w_general` 0.75 → 0.70 (full step; all 3 runs agree).

**Diversify still doesn't pay offline** but does cause real UX problems
flagged by users — "feed jumps" between adjacent slots, hidden posts
reappearing on next page. Out-of-band fixes shipped with v5.9:

- `diversity_max_penalty` 0.45 → **0.20**. The previous value let MMR
  drop a high-score post by up to 0.45, letting a far-weaker post slot
  in between two strong recommendations. Offline ΔNDCG@20 ≤ 0.003
  across calibrate runs, so this is a free UX win.
- `get_recently_seen_post_ids` now matches `event_type IN
  ('qualified_impression', 'hide', 'open')`, not just qualified
  impressions. Hidden posts no longer reappear on the next page; the
  previous behaviour was a real bug not caught by any offline metric.

### v5.8 — time_causal split + memory diet (May 2026, N=915)

First successful full-N=915 run on the `time_causal` split after the
calibrate-side memory diet (`TagRelationGraph::freeze`/`freeze_with_query_set`,
direct-to-Vec global-graph load, jemalloc as the global allocator on
Linux). Earlier attempts OOM-killed around 720 accounts due to glibc
arena fragmentation across hundreds of thousands of small `String`
allocations during `Post` hydration.

- [`grid_20260510_133817.toml`](grid_20260510_133817.toml) — time_causal,
  no diversify (NDCG@20 0.8026, Recall@50 0.1668, MRR 0.9185, 35 min)

Headlines:

- **time_causal is ~4 pp NDCG@20 harder than post_id (0.80 vs 0.84)**
  by construction. It mirrors the production "predict the user's next
  favourite" task more honestly. Real production gain expectations
  should be calibrated against this number, not the easier post_id /
  random splits.
- **`[best]` matched v5.5 defaults almost exactly.** v5.7 over-shot
  several knobs (`mix_sim`, `df_floor`, `bm25_k`, `idf_rsj_smoothing`,
  group weights) based on post_id-only data; the more honest
  time_causal grid pulls them all back. v5.8 reverts those moves.
- **Only meaningful drift from v5.5:** `group_w_general` 0.80 → 0.70
  (grid net move). v5.8 takes a half-step from 0.80 to 0.75 toward
  the time_causal best.
- **Memory after diet:** N=915 hydrates and runs to completion well
  inside 15 GB. Per-account RSS growth dropped from ~20 MB (glibc) to
  the expected ~1-2 MB after enabling jemalloc via the `--features
  jemalloc` flag.

### v5.7 grid context (May 2026, N=915 follow-up after v5.5/v5.6)

Six N=915 grid runs against the v5.5-shipping defaults (and v5.6 grid
with the +8 added knobs):

| File | Split | Diversify | NDCG@20 |
|---|---|---|---|
| [`grid_20260510_024019.toml`](grid_20260510_024019.toml) | random | — | 0.9374 |
| [`grid_20260510_024748.toml`](grid_20260510_024748.toml) | post_id | — | 0.8381 |
| [`grid_20260510_033150.toml`](grid_20260510_033150.toml) | random | +div | 0.9338 |
| [`grid_20260510_042040.toml`](grid_20260510_042040.toml) | post_id | +div | 0.8414 (`df_floor` hit lower clamp) |
| [`grid_20260510_044504.toml`](grid_20260510_044504.toml) | post_id | — | 0.8407 (re-run) |
| [`grid_20260510_053940.toml`](grid_20260510_053940.toml) | post_id | +div | 0.8427 (re-run) |

Headlines:

- **v5.5 prod defaults moved the [best] floor up by ~0.018 NDCG@20 in
  every condition.** Real progress, not noise.
- **Repeatability is at the 4th decimal.** Two post_id no-div runs
  drifted 0.0026; two post_id +div runs drifted 0.0013. Fix prod to
  3-decimal precision; below that is grid noise.
- **The 8 v5.6 knobs added to the grid did not move.** `tag_relation_
  min_cooc` stayed at 2 in 6/6 runs, all 6 `diversity_*` knobs stayed
  at their defaults across all 3 +div runs, `recency_split_age_days`
  never engaged. They lack offline gradient under this harness — see
  [`../calibration.md`](../calibration.md) caveats. Tune online via
  bucket A/B once real `feed_interactions` flows in.
- **Group weights flipped direction for some**: `copyright`/`species`/
  `general` were over-shot in v5.5, new data says step them DOWN. v5.7
  trims `copyright` 1.45→1.30, `species` 1.30→1.20, `general` 0.80→0.65.
  Meanwhile `artist`/`character` keep climbing (consensus 2.85/2.20).
- **`df_floor` keeps drifting low.** 5/6 runs picked 0.10; one diversify
  run hit the lower clamp 0.05. v5.7 steps from 0.40 to 0.20 (half-step)
  and watches whether the next grid hits the lower clamp again — if so,
  widen it.
- **`mix_sim` is now at 0.945 in all 3 post_id no-div runs.** v5.7 only
  steps from 0.72 to 0.80 because this is the most holdout-sensitive
  knob; full step would crowd out interaction/tag_relation/media in
  prod where positives aren't artificially close to train tags.

### v5.3 added knobs (defaults match prior production behaviour)

| Class | Knobs | Default |
|---|---|---|
| A — promoted constants | `idf_rsj_smoothing` / `coldstart_smoothing_boost` / `interaction_ctr_prior_alpha` | 0.5 / 2.0 / 4.0 |
| B — per-group multipliers | `group_w_{artist,character,copyright,species,general,lore}` | 2.0 / 1.6 / 1.2 / 1.1 / 1.0 / 0.45 |
| C — algorithmic shape | `score_temperature` / `confidence_steepness` / `mmr_redundancy_exp` / `tag_sim_jaccard_blend` | 0.0 / 1.0 / 1.0 / 0.0 (all no-ops) |
| D — point splits (NaN = off) | `idf_lambda_meta` / `recency_tau_recent` (with `recency_split_age_days=30`) / `tag_relation_pmi_scale_user` | NaN |
| E — categorical | `tag_relation_pair_aggregator` ∈ {mean, max, geomean} | "mean" |

### Methodology note

The v5.x defaults take **partial steps** toward `[best]` rather than
copying it verbatim — the `mix_quality` / `mix_recency` /
`mix_popularity` / `mix_rating` columns drift to 0 in the grid because
of the holdout artifacts described in
[`../calibration.md`](../calibration.md); aggressive zero-ing those
would produce a worse production feed. Standing rule: when 4/4 runs
agree on a knob (e.g. group weights in v5.5), step ~75 % of the gap;
when runs disagree (e.g. `df_floor`, recency knobs across splits),
hold or move ≤ 25 % of the gap.
