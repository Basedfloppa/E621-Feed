# Calibration & Offline Backtesting

## What it does

1. **`seed`** populates the local database with the favourites of the top-N
   public e621 users (ranked by `favorite_count`). Pure read-only against
   e621; no public side effects.
2. **`calibrate`** builds a holdout-based evaluation harness from those
   favourites: per user, splits favs into train/test. Three split
   strategies:
   - `split=post_id` — sort by `post_id`, oldest 80% → synthetic
     profile, newest 20% → test set. Cheap; biases recency.
   - `split=random` — deterministic uniform-random hold-out. Easier task
     because positives can pre-date training items.
   - `split=time_causal` (default) — sort favourites by `created_at`, hold
     the newest 20% as test. Closer to the production "predict next
     favourite" task and resilient to id-aliasing on imported data.

   Negatives are sampled from the catalog (default: hybrid hard-negatives
   — 70% mixed, popularity- and time-matched, plus 30% tag-similarity-based
   hard negatives; `neg=mixed` drops the hard-mined third and
   `neg=uniform` switches to pure random). ⚠️ **`neg=uniform` is uniform inside
   the production candidate pool, not over the catalog** — the pool is the
   newest ids of a few tag streams plus recent-popular, so negatives drawn from
   it inherit that age and popularity profile. `neg=catalog` is the
   research mode that skips the pool restriction entirely and samples uniformly
   over every non-deleted post. It answers "would the user have favourited this
   at all", which is the question the channel weights are for, at the cost of
   being an easier retrieval task than the live feed. Each test fixture also
   carries a **per-account tag-relation graph** built from `train_posts`
   cooccurrences, so the personal `tag_relation` channel and its
   `*_user_*` knobs see real signal under the synthetic split.

   There is **no favourite timestamp in the e621 API** — `/favorites.json`
   returns the *post's* `created_at`, and neither the `v1` (`{"posts": […]}`) nor
   the `v2` (`[…]`) shape carries a favourite date. `accounts_post.added_at`
   (V40) therefore records the first time *this* process stored the row: a lower
   bound for accounts first synced after the migration, `NULL` for older rows.
   Until enough favourites have been added after a cutoff, the temporal split
   can only approximate "what the user knew at T" through the post's creation
   date; a re-sync keeps the first `added_at`, so the column is a usable
   holdout the moment a future sync adds a favourite.

   Reports **NDCG@20**, **Recall@50**, **MRR** with **bootstrap 95% CI**
   on NDCG. Probe acceptance during grid uses a **paired** 1-sided 95 %
   t-test over the per-account NDCG@K vectors: a probe is accepted only
   when the mean of the per-account differences `candidate − incumbent`
   clears `t(df=N−1) · SE(differences)`, floored at `1e-4`. Pairing is
   what makes the gate usable on a small dataset — the *level* SE is
   dominated by how intrinsically hard each account is to rank, and that
   spread cancels in the difference. See the 2026-10-09 run notes below
   for the measurements.
3. **`calibrate grid`** runs a multi-pass greedy line search with adaptive
   probe steps (×1.0 / ×0.5 / ×0.25 across passes), followed by a paired
   sweep over known-correlated knob pairs and a categorical sweep for
   enum-valued knobs. Reports `[best priors — non-default values]`, and
   classifies how the search ended into two separate lists:

   - **clamp-saturated** — a knob *moved* and stopped exactly on a limit, so
     the optimum may lie outside the swept range.
   - **switched off** — a `mix_*` weight the search drove to zero with the
     explicit `-1.0` probe. This is a *result*, not a range problem: relative
     probes cannot reach zero by accident, so every interior point was
     measured and rejected first. Filing these under the clamp warning is
     what made the 2026-10-09 run read as "the semantic channels are dead"
     instead of "the search switched them off".

   The grid currently covers 84 numeric knobs + 2 categorical:
   - 11 `mix_*` weights (`mix_uploader`, `mix_exclusivity`, `mix_novelty`,
     `mix_artist_discovery`)
   - IDF / frequency shaping (8): `df_floor`, `idf_max`, `idf_lambda`,
     `idf_alpha`, `freq_alpha`, `bm25_k`, `one_sided_ratio_exp`,
     `idf_rsj_smoothing`
   - Quality channel (7): `quality_a/b/log_bias`, `quality_c`,
     `quality_w_absolute/relative_score/relative_comments`
   - Popularity channel (2): `popularity_w_fav/duration`
   - Recency channel (8): `recency_tau_days`, `recency_tau_recent`,
     `recency_tau_hot`, `recency_split_age_days`, `recency_split_age_hours`,
     `recency_w_global/personal`, `recency_personal_floor_frac`
   - Discrete-pref + cold-start (3): `discrete_smoothing_alpha`,
     `discrete_pref_floor`, `coldstart_n0`
   - Tag-relation (8): `tag_relation_pmi_scale`, `tag_relation_w_global/personal`,
     `tag_relation_cooc_ref/user_cooc_ref`, `tag_relation_max_tags`,
     `tag_relation_min_cooc`, `tag_relation_user_min_cooc`
   - Cold-start internals (2): `coldstart_smoothing_boost`,
     `interaction_ctr_prior_alpha`
   - Per-group multipliers (6): `group_w_artist/character/copyright/species/general/lore`
   - Algorithmic shape (5): `score_temperature`, `confidence_steepness`,
     `mmr_redundancy_exp`, `tag_sim_jaccard_blend`, `exploration_epsilon`
   - Point splits (5, NaN-sentinel disabled): `idf_lambda_meta`,
     `recency_tau_recent`, `recency_tau_hot`, `tag_relation_pmi_scale_user`,
     `recency_split_age_hours`
   - Uploader channel (3): `uploader_n0`, `uploader_w_avg_score`, `uploader_w_avg_fav`
   - Exclusivity channel (4): `min_exclusivity_cooc`, `exclusivity_scale`,
     `exclusivity_max_tags`, `exclusivity_cross_group_weight`
   - Novelty channel (1): `novelty_n0` (the `novelty_use_feedback` switch is
     registered categorically but not swept — see the categorical bullet below)
   - Artist discovery channel (2): `artist_discovery_n0`,
     `artist_discovery_novelty_bonus` (its `mix_artist_discovery` weight is
     listed with the other `mix_*` knobs)
   - Diversity weights (5): `diversity_w_artist/character/copyright/species/general`
   - **Class J — diversity semantic (4):** `diversity_semantic_blend`,
     `diversity_pmi_threshold`, `diversity_semantic_max_tags`,
     `diversity_user_pmi_weight`
   - **Class L — production MMR head (1):** `diversity_head_limit` bounds how
     many top-by-score posts the production diversifier re-ranks. It is **not**
     in the grid: `diversify_indices` receives its head from top-K here, so the
     knob cannot change an offline metric.
   - Categorical (2 registered, 1 swept): `tag_relation_pair_aggregator` ∈
     {mean, max, geomean}; `novelty_use_feedback` ∈ {true, false} is in the
     registry but `grid.rs` skips it (`_ => continue`) — synthetic runs carry no
     `account_tag_feedback` rows, so the switch has no gradient offline

   Subsets / flags:
   - `grid mix-only` — only the 11 mix weights (fastest)
   - `grid pairs-only` — skip the single-knob sweep, only the paired moves
   - `grid no-pairs` — skip the paired sweep
   - `grid with-diversify` — run `diversify_indices` before NDCG so
     `diversity_*` knobs become measurable
   - `verbose` — log every probe (including rejected ones) plus
     per-knob early-exit notices
   - `fit-mix` — after the eval pass, solve the eleven `mix_*` weights
     analytically from the channel matrix that pass already built, and
     report the same paired gate a grid probe must pass (TODO §2.7).
     Costs one scoring pass; the solve itself is seconds. Diagnostic:
     the surrogate it maximises is not NDCG@20, so a fitted vector is a
     candidate for a grid probe, not a result to paste into config.
   - `fit-mix=<priors.toml>` — the same, plus re-score the vector in that
     file against the **same** matrix, so the shipped config, the file and
     the fitted vector become three rows of one measurement. See
     [Re-scoring a persisted vector](#re-scoring-a-persisted-vector).

   Per-knob early exit: after 2 consecutive non-improving probes inside
   a knob's probe list, the rest of that knob's probes are skipped for
   the current pass. Saves 30–50% of probe budget on converged knobs.
   NaN/Inf probe results are flagged + dropped (don't promote to
   `[best]` via `partial_cmp` Equal fallback).

### Chaining modes (shared hydration)

Hydration (loading IDF, the global tag-relation graph, eligible accounts,
posts, and per-post cached features) is the dominant fixed cost of a run.
Modes can be **chained on the command line** so a single invocation pays
that cost once and runs every requested mode against the same dataset:

```bash
./target/release/calibrate eval grid              # prep once → eval, then grid
./target/release/calibrate eval grid mix-only     # same, but grid restricted to mix_*
./target/release/calibrate grid                   # grid only (still preps internally)
```

The first line of a chained run prints
`[run] preparing dataset (shared across N mode(s))...`. Each mode's
output (`[eval]…`, `[grid]…`) follows in order against that single
hydrated dataset.

## What offline NDCG cannot decide

The negative sampler, not the model, decides which weights win. Measured on one
dataset (N=150, `CALIBRATE_NOW=1792972800`), fitting the eleven `mix_*` weights
with `--fit-mix` under each distribution and then evaluating both fitted sets
under both distributions:

| weights | hybrid-hard negatives | catalog negatives |
|---|---:|---:|
| shipped (`sim` = 0.878) | 0.6070 | 0.8107 |
| fit on hybrid (`quality` 0.61, `novelty` −0.34, `media` 0.05) | **0.6413** | 0.2463 |
| fit on catalog (`rating` 0.34, `recency` −0.56, `media` 0.09) | 0.6838 | 0.1823 |

Two readings matter. First, the order of the models **inverts** between the two
samplers: the set fitted on catalog negatives is the best of the three on hybrid
negatives (0.6838) and the worst on catalog negatives (0.1823), and the set
fitted on hybrid negatives is second on hybrid (0.6413) and second-worst on
catalog (0.2463). A comparison made inside one sampler therefore says nothing
about the other. Second, only the shipped weights stay competitive under both
(0.6070 / 0.8107); that robustness, not a peak on one distribution, is what an
offline harness can certify. Two corollaries:

- A "better" NDCG inside one sampler is a statement about that sampler. The
  earlier +0.008 between the twelve-channel blend and `sim`-only is the same
  effect at a smaller scale: the ranks are nearly flat once the negatives come
  from the same similarity space the model scores.
- `--fit-mix` is not an oracle. The catalog fit puts a *negative* weight on
  recency (`−0.56`) — preferring older posts — which is what a Plackett–Luce
  fit does when most of its negatives are old and most positives are recent:
  it minimises the loss by inverting a channel rather than by ranking taste.

What the harness *can* decide: whether a knob is dead (constant channel, zero
gradient), whether a change is inside the noise, and how the head's composition
moves (age, favourites, artists, tag jaccard). Ranking two models for the live
feed needs either an `added_at` holdout — favourites the user added after the
cutoff, which no sampler can fake — or online interleaving.

## Quickstart

```bash
cd ./parser-api/

# 1. Back up your DB.
cp database.db database.db.bak

# 2. Build.
cargo build --release --bin seed --bin calibrate --features jemalloc

# 3. Inspect what's already in the DB before seeding more.
./target/release/calibrate probe

# 4. Pull in N users worth of public favourites.
#    Each successful import adds ~1280 favourites (cap is 8 pages × 160).
./target/release/seed 100         # ~15-20 min, adds ~50 actual users

# 5. Re-probe to see what you got.
./target/release/calibrate probe

# 6. Run baseline + full grid in one go (single hydration).
./target/release/calibrate eval grid split=time_causal
```

(If you only want one mode, swap step 6 for `calibrate eval ...` or
`calibrate grid ...`. Hydration cost is identical.)

### Starting from an empty database

The quickstart assumes a DB that already carries the global tag catalogue and
the `tag_cooccurrence` graph (it is a copy of the production DB). If you wipe
it and rebuild with `seed` alone, **one step is missing and the run is
invalid**: `seed` writes posts with `track_cooccurrence = false`
(`src/bin/seed.rs`), so `tag_cooccurrence` stays empty and every
`tag_relation_*` knob is measured against an empty graph — the grid then
"discovers" that tag relations don't help and zeroes them.

Rebuild the graph explicitly, without booting the scraper:

```bash
./target/release/calibrate graph     # rebuilds tag_cooccurrence from tags_posts
```

It is a no-op unless `tag_cooccurrence` is empty, so it is safe to re-run. It
is also the only thing that rebuilds the graph on demand: otherwise the
backfill is spawned once at server start (`src/db/mod.rs`) and nowhere else.
Budget for it — it upserts every tag pair of every post, so a few hundred
thousand posts take a while.

`tag_aliases` / `tag_implications` stay empty without `catalog-seed`. That does
not affect calibration: their only readers are the tag-relations and
taste-profile API routes, not the scorer.

## Time and disk budget

| Step | N=100 users | N=300 users | N=1000 users |
|---|---|---|---|
| `seed` (8 pages/user) | ~15-20 min | ~45-60 min | ~3-4 h |
| Catalog growth | ~+150 MB | ~+400 MB | ~+1.2 GB |
| Hydration (`[run] preparing dataset…`) | ~3 min | ~8-12 min | ~25-40 min |
| `eval` over hydrated dataset | ~30-60 s | ~2-3 min | ~5-8 min |
| `grid` over hydrated dataset | ~5-10 min | ~20-40 min | **~2-3 h** |

The on-disk catalog is shared with the production DB but is additive —
existing accounts and feed history aren't touched. CPU build parallelism
is capped via [`parser-api/.cargo/config.toml`](../parser-api/.cargo/config.toml)
so the box stays usable while a multi-hour run is in flight.

The grid runtime above includes the per-channel score cache and the
pre-resolved post-tag features — without them a full grid at N=1000 was
~8 hours. See "Performance" for what each layer costs.

## Reading the output

```text
[run] preparing dataset (shared across 2 mode(s))...
[run] dataset ready in 412.3s
[eval] scoring 915 accounts under config.toml priors...
[baseline] N=915  NDCG@20=0.0368  Recall@50=0.0092  MRR=0.0698
[grid] 64 knobs × ~4 probes/pass × 3 passes = up to 768 evals + paired sweep
[grid] adaptive step: [1.0, 0.5, 0.25]
[grid] running baseline eval...
pass 1(×1.00): mix_sim +0.100        NDCG@20 0.037 -> 0.066
pass 1(×1.00): mix_recency -0.100    NDCG@20 0.066 -> 0.142
pass 1(×1.00): idf_alpha +0.050      NDCG@20 0.142 -> 0.155
...
[best] N=915  NDCG@20=0.2034
[best priors — non-default values]
mix_sim                         = 0.730    (was 0.480)
mix_recency                     = 0.000    (was 0.070)
idf_alpha                       = 0.700    (was 0.650)
...
```

`N` is the count of accounts that cleared `min_favs`. Higher N means
tighter confidence intervals.

The numbers are conditional on the candidate pool. Negatives are sampled from
the ids `production_pool_ids` returns — that is, from
`runtime.local_candidate_limit` local candidates — so changing that limit
changes the negative universe and the baseline moves even when no weight did.
Measured on 2026-10-26 over the same 30 accounts and the same pinned clock:
NDCG@20 = 0.5835 at `local_candidate_limit = 400` versus 0.8396 at `80`, while
two runs of the same config landed 0.0009 apart. Compare runs only at the same
pool size; a step change in that key invalidates every earlier NDCG.

The numbers also depend on how the pool is **ordered**, and that changed on
2026-10-27: the measured runs above still used the random window, while the pool
is now newest-first only (`local_candidate_order` and the random branch are
gone). So those two figures are historical — they compare pool *sizes* under an
ordering that no longer exists, and must not be compared against a fresh run.

The current tree was measured on 2026-10-27 with the pool at
`local_candidate_limit = 400`, newest-first and without buckets, at N=100 with
the clock pinned to `CALIBRATE_NOW=1792972800`:

| Metric | Value |
|---|---|
| dataset fingerprint | `0xed9410a793fa063d` |
| NDCG@20 | **0.6066** (95% CI 0.5614-0.6510, SE 0.0228) |
| Recall@50 / MRR | 0.1107 / 0.7928 |
| repeat run | same fingerprint, same metrics to the last digit |

The identical repeat is the useful part: with the random ordering gone there is
no run-to-run noise left on a fixed dataset, so **any non-zero delta in a later
run is a real change**, not sampling jitter. The `SE` above is the spread
across accounts, not across runs. N=100 is below the `max_accounts` default of
150, so the CI is wider than a full run's - enough to catch a regression, not
enough to settle a single weight.

Each `pass N: ... ->` line is a step that
improved NDCG by more than the noise floor (1e-4). The
`[best priors — non-default values]` block prints only the fields that
moved off baseline, so you can see at a glance what to copy into
`config.toml`.

## Caveats — don't blindly copy `[best priors]` into prod

Several signals are systematically biased by the offline harness and would not
survive contact with production traffic:

1. **Recency-related knobs (`mix_recency`, `recency_tau_days`,
   `recency_w_*`) drift toward "off".** `split=time_causal` (the default)
   holds out the newest 20% of each account's favourites, so every test item
   is newer than the train favourites the profile's "average favourite age"
   is computed from. The channel then says "too new" about positives and
   negatives alike — the same sign for both — and what it contributes to the
   ranking is the split boundary, not recency preference. `split=post_id`
   biases the same way and more strongly. Recency is generally useful in
   production — don't zero these out blindly.
2. **`mix_quality` / `mix_popularity` / `quality_*` / `popularity_*` drift
   toward 0.** Random negatives over-represent popular content (it shows
   up in many users' favourites). The model correctly says "popular = good"
   but the holdout positive is the user's specific niche favourite, so the
   weights cancel.
3. **`mix_novelty` doesn't move.** Synthetic split has no
   `account_tag_feedback` rows, so the impressions input is empty. Anything
   else that depends on feedback (`feedback_decay_*`) is out of the grid for
   the same reason: zero gradient here. Tune online.
4. **Diversity knobs (`diversity_*`)** are in the grid only under
   `with-diversify` (gated via `diversify_only`). ⚠️ **The ΔNDCG@20 of
   0.001–0.003 this item used to quote is void.** It was measured while the
   redundancy penalty was computed against a reference score that got re-based
   after every pick, which forced the penalty to zero: the MMR pass was a plain
   sort by score, so *no* `diversity_*` value could move any metric and the
   ~0.002 spread was noise. The reference is now fixed
   (see [scoring.md](scoring.md#the-redundancy-penalty)).
5. **`diversity_max_penalty` is now calibrated** — and the honest result is that
   NDCG cannot pick it. Five runs on one dataset (`CALIBRATE_NOW=1792972800`,
   N=150, fingerprint `0x2a6d692efbd1f0f5`, `with-diversify`), only the penalty
   changed:

   | `diversity_max_penalty` | NDCG@20 | SE | distinct artists in head | tag-jaccard |
   |---:|---:|---:|---:|---:|
   | 0.00 | 0.6070 | 0.0185 | 10.5 | 0.15 |
   | 0.20 | 0.6154 | 0.0162 | 13.7 | 0.12 |
   | 0.45 | 0.6090 | 0.0155 | 15.6 | 0.11 |
   | **0.65** | **0.6106** | 0.0155 | **16.1** | **0.10** |
   | 0.90 | 0.6073 | 0.0154 | 16.4 | 0.10 |

   Every NDCG difference is inside one SE, while the diversity columns move
   monotonically: MMR removes near-duplicates from the head without costing
   ranked relevance, because a duplicate never earns a second positive. The head
   stops improving past ~0.65 (0.9 buys +0.3 artists and no jaccard), so
   `diversity_max_penalty = 0.65` is the knee. The live feed agrees: on account
   298553 the most-repeated artist in the top 20 fell from 6 posts to 3, adjacent
   same-artist pairs from 1 to 0, and the median head age from 37 to 27 days.
   Re-probe the remaining `diversity_*` knobs (weights, `mmr_redundancy_exp`,
   `diversity_semantic_*`) the same way: they are still uncalibrated.
5. **Personal `tag_relation` channel** has signal: each fixture builds its
   own user-graph from train_posts. `tag_relation_
   w_personal`, `tag_relation_pmi_scale_user`, `tag_relation_user_min_
   cooc`, `tag_relation_user_cooc_ref` will all move when the grid
   finds gain there.

What **is** trustworthy from the offline grid: the **direction** of knobs
that don't depend on those biases — `mix_sim`, `mix_rating`,
`mix_tag_relation`, `idf_*`, `freq_alpha`, `discrete_*`, `tag_relation_*`,
`coldstart_n0`. Apply moderate adjustments rather than copying the extreme
`[best]` values verbatim.

### Re-scoring a persisted vector

The caveats above are about *which* knobs the offline harness can judge. This
one is about *how* to compare two vectors at all — and the short answer is:
not across runs.

`eval` re-samples its candidate set from the live database on every
invocation, so two runs do not share a dataset. This is visible rather than
assumed, because `eval` prints its `dataset fingerprint`: two runs with
identical flags and a pinned `CALIBRATE_NOW`, minutes apart, print different
fingerprints. **Two metric lines from two runs are therefore not a
comparison**, however close the flags were — the difference between them is
at least partly a different candidate set.

`--fit-mix=<path>` exists to make the comparison legitimate. It reads a
`[priors]` section from any file (a `calibration_results/grid_*.toml`, or a
`config.toml`), re-scores that vector against the matrix the eval pass just
built, and judges it with the same paired gate a grid probe must pass:

```bash
CALIBRATE_NOW=<unix-secs> cargo run --release --bin calibrate -- \
  eval --fit-mix=docs/calibration-results/grid_20261010_054734.toml
```

One matrix, one clock, one pair of blend parameters; three vectors — the
shipped config, the file, and the solver's. Each gets its surrogate objective
and its exact re-score, so "the file wins on the surrogate but loses on
NDCG@20" is a sentence the report can support.

Two limits, both enforced rather than merely warned about:

- **The file must carry all eleven `mix_*` keys.** `Priors` is
  `#[serde(default)]`, so a partial block parses happily and silently
  substitutes the built-in default for every omitted weight. The report would
  then describe a vector that is not in the file, and nothing downstream could
  tell — so an incomplete reference is refused.
- **Only `mix_*` can actually be varied.** Every other knob is already baked
  into the cached channel values, which were computed once under the live
  config. If the file disagrees with the live config on any non-mix field the
  report lists them: the comparison then *approximates* the file rather than
  reproducing it. (Comparison is tolerance-based, because result files round
  to two or three decimals.)

A run that *prefers* the file is therefore a statement about the harness's
biases as much as about the channels: the reference vector is itself the output
of a search driven by them. Read the delta as the size of the bet, not as a
recommendation to take it.

## Run artifacts

Each `calibrate grid` / `eval` run leaves a machine-readable TOML in
`calibration_results/` (or wherever `write_grid_log` points): the accepted
`[priors]`, its headline metrics and the warnings the search raised. Those files
are the record of what was measured — this document describes the harness, the
flags and the caveats, and deliberately keeps no per-run narrative, because a
prose retelling of numbers drifts from the numbers.

A curated set of artifacts lives in [`calibration-results/`](calibration-results/)
with a short index of what each one was run for.

## Files

- [`parser-api/src/bin/seed.rs`](../parser-api/src/bin/seed.rs) —
  discovery, probing, import. Single-threaded by design.
- [`parser-api/src/bin/calibrate/`](../parser-api/src/bin/calibrate/) —
  the calibrate harness, split into:
  - `main.rs` — CLI parsing, mode chaining, dataset hand-off.
  - `dataset.rs` — eligible-account selection, train/test split,
    post hydration, per-post `CachedPostFeatures` build.
  - `sampling.rs` — train/test split + uniform / mixed-hard / hybrid-hard
    negatives.
  - `metrics.rs` — NDCG / Recall / MRR + uncached `score_with_progress`.
  - `cache.rs` — `ChannelMask`, `ScoreCache`, `score_with_cache` (the
    grid hot path).
  - `knobs.rs` — `KnobSpec` registry + per-knob invalidation masks.
  - `grid.rs` — line search + paired sweep + categorical sweep.
  - `log.rs`, `options.rs`, `probe.rs` — printing, CLI options, DB probe.
- [`parser-api/src/utils/scorer/`](../parser-api/src/utils/scorer/) —
  the scoring math being calibrated; same code paths in production. The
  cached entry points (`tag_similarity_cached`, `tag_relation_fit_cached`)
  live in `channels_cached.rs` and are a math-
  identical mirror of their `&Post` counterparts in `channels.rs`.
- [`parser-api/src/utils/scorer/cached.rs`](../parser-api/src/utils/scorer/cached.rs) —
  `CachedPostFeatures` / `CachedTag`: pre-resolved (group, lc, df_raw,
  global_tid, user_tid) for every post in the eval set.
- [`parser-api/.cargo/config.toml`](../parser-api/.cargo/config.toml) —
  build parallelism cap.

## Performance

`calibrate` parallelises per-account scoring via rayon. Default thread count
is `nproc / 2` (matches `.cargo/config.toml`'s build-jobs cap), which leaves
half the box free for the editor / browser during multi-hour grid runs.

Override via `backtest.calibrate_threads` in `config.toml`:

```toml
[backtest]
calibrate_threads = 0   # 0 = auto (nproc/2). Set explicitly to e.g. 4 to be more conservative.
```

### Grid-time speedups

1. **Pre-resolved post tags** ([`scorer/cached.rs`](../parser-api/src/utils/scorer/cached.rs)).
   At hydration time each post's tags are walked once and stored as a flat
   `Vec<CachedTag>` carrying `(group, lc, df_raw, global_tid, user_tid)` already
   resolved. The hot scoring loop then skips two HashMap-by-string probes
   per tag per post per probe (`IdfIndex::df_for` and
   `TagRelationGraph::tag_id`), which on a full grid is the difference
   between billions and zero.

2. **Per-channel score cache** ([`bin/calibrate/cache.rs`](../parser-api/src/bin/calibrate/cache.rs)).
   Every `KnobSpec` declares an `invalidates: u16` bitmask naming which of
   the 11 scoring channels its delta affects. The baseline run computes all
   channels and stores them per (account, post). Each subsequent probe
   recomputes only the invalidated channels and reuses the rest from the
   cache; the final mix-blend / temperature shape is always reapplied by
   the shared `final_blend` — the same
   function the production `score` / `score_cached` paths call, so cached
   and uncached scores cannot drift. Effects on common probe categories:

   | Knob class (typical examples) | Channels recomputed | Probe cost vs uncached |
   |---|---|---|
   | `mix_*` (11) / `score_temperature` / `mmr_redundancy_exp` (no diversify) | none — final blend only | ~3% |
   | `quality_*`, `popularity_*`, `recency_*`, `interaction_ctr_*`, single-channel TR knobs | one channel | ~5–40% |
   | `idf_*`, `df_floor`, `idf_max`, `bm25_k`, `freq_alpha`, `idf_rsj_smoothing`, `tag_sim_jaccard_blend` | sim only | ~30% |
   | `tag_relation_*` (PMI / cooc_ref) | tag_relation only | ~40% |
   | `coldstart_n0` / `confidence_steepness` (drives personal_confidence) | rating + media + recency + tag_relation | ~50% |
   | `coldstart_smoothing_boost` (also seeds `preferred_tags` into the base) | sim + rating + media | ~30–45% |
   | `group_w_*` | sim + interaction + tag_relation | ~80% |
   | `with-diversify` (any knob) | full rebuild forced | 100% |

   Knob → mask wiring lives next to each `KnobSpec` in
   [`knobs.rs`](../parser-api/src/bin/calibrate/knobs.rs); if a knob's
   semantics shift (e.g. quality_a starts depending on group_wts), update
   the mask there.

   Masks also have to respect the base cache: any knob folded into
   `context_fingerprint` forces `ContextBase` to be rebuilt, so every
   channel that reads the rebuilt base must be listed in its mask even
   when the knob nominally belongs to another channel.
   `coldstart_smoothing_boost` is the case in point — it looks like a
   rating/media knob, but it also seeds `preferred_tags` into the
   `user` / `u_norm` vector, so its mask is
   `M_BOOST = M_SIM | M_DISCRETE`. Adding the field to the fingerprint
   without widening the mask would rebuild the base while reusing a `sim`
   computed from the previous vector.

3. **Persisted channel matrix**
   ([`bin/calibrate/cache_file.rs`](../parser-api/src/bin/calibrate/cache_file.rs)).
   The cache above is rebuilt from scratch on every invocation, and that
   rebuild is a full scoring pass (~60s at N=676, on top of ~190s of
   hydration). `eval grid` writes the baseline matrix to
   `calibration_results/channel-cache.bin`, and a later invocation reloads
   it when it still matches.

   | | |
   |---|---|
   | Contents | per account: `n_test`, `n_cand`, then `n_cand` × (12 × `f32` + veto byte) |
   | Key | dataset fingerprint, priors fingerprint, `diversify`, top-k |
   | Dataset fingerprint | SHA-256 over every account's candidate ids **in order** — catches a changed split, changed negative sampling, changed catalog |
   | Priors fingerprint | SHA-256 over the whole `Priors` struct as JSON, so a knob added later is covered automatically |

   The clock is deliberately excluded from the priors fingerprint:
   `Priors::now` is the evaluation clock, injected per scoring pass, so
   including it would miss on every run. A hit therefore recomputes
   `M_RECENCY` and nothing else — every other `priors.now` reader in the
   scorer is a recency age computation.

   The file is **self-contained**, which is the other half of its value: an
   analytic blend fit (TODO §2.7) needs the candidate matrix and the
   positive/negative split, and both are in it — positives are `i < n_test`
   within each account, and post ids are unnecessary because every test
   candidate has relevance 1 and every negative 0.

   Failure policy: the cache is an optimisation, so a missing, truncated,
   corrupt, stale or version-mismatched file is a **miss with a printed
   reason, never an error**. Writes go to a temporary file and are
   `rename`d into place, so a crash mid-write leaves the previous file
   intact. `no-cache` disables both directions.

   The saving is bounded by the baseline pass, which is small next to
   hydration. The matrix is the more durable payoff.

### Dataset memory budget (calibrate-only)

- `Post` objects (transient, per-iteration): ~3.5 KB / post; dropped at
  the end of each account's hydration so they don't accumulate.
- `CachedPostFeatures`: ~2 KB / post (avg 25 tags × ~80 B/tag) → ~450 MB
  at N=1000 × ~220 posts/account.
- `DiversityFeatures` (only when `with-diversify`): ~120 B / post →
  ~25 MB. Skipped on default no-diversify runs.
- Per-account `user_relation` tag-relation graph: pairs
  stored as a sorted `Vec<(u32,u32,u32)>` after
  `freeze_with_query_set(min_cooc=2)` — 12 B / pair. The freeze drops:
    1. pairs with `count < min_cooc` (singleton noise),
    2. pairs whose endpoints don't appear in any (test ∪ neg) post —
       they would never be queried by `tag_relation_fit_cached` since
       it walks the *current* post's tags.
  Typical: 5-20 K pairs / account → ~60-250 KB / account → ~60-250 MB
  at N=1000. (Pre-freeze HashMap form is ~5-10× larger and lives only
  during hydration of one account.)
- `ScoreCache` (channels per post + transient trial cache): ~16 MB peak.
- Global graph + IDF index: same as production server (~3-5 GB).

Total calibrate peak at N=1000 sits around 5-7 GB on a 15 GB box.
Production server memory is **unaffected** — none of the cached types
are constructed by the prod scoring path.

## Tunable knobs

All calibration / seed knobs live in `config.toml` under `[backtest]`. Every
field has a default — you only need the section if you want to override.

### Calibrate harness

| Knob | Default | What it controls |
|---|---|---|
| `min_favs` | 100 | Min favourites for an account to enter the eval set. Lower → more accounts (better statistical power), but each account's synthetic profile + holdout become noisier. Accounts with fewer than `min_favs / 2` train posts after the split are also rejected, so going below ~50 stops being useful. |
| `test_fraction` | 0.20 | Share of each user's favourites held out as test (the newest 20% by `created_at` under the default `time_causal` split; by `post_id` under `split=post_id`). Higher → more test items per account (less noisy per-account NDCG), but the synthetic profile is sparser. 0.20 is the standard ML train/test split. |
| `negative_ratio` | 10 | Random negatives per held-out positive. Higher → stricter retrieval task (closer to production where most catalog posts aren't favourites). **Dominates cached dataset RAM** at roughly `max_accounts × 256·NEG × 5 KB`. At defaults that's ~2 GB. Push higher only if you have free RAM — exceeding physical memory triggers swap thrashing and per-eval cost balloons ~10×. |
| `top_k_ndcg` | 20 | Cutoff k for NDCG@k. Smaller k weights only the top of the feed (matches user attention); larger k evaluates a wider slice. 20 ≈ first screen of an infinite-scroll feed. |
| `top_k_recall` | 50 | Cutoff k for Recall@k. Wider than NDCG cutoff on purpose — recall measures *coverage*, not ranking, so we care whether a held-out positive lands somewhere in the first ~50 results, not at the very top. |
| `max_accounts` | 150 | Cap on accounts evaluated per run. CIs tighten as `sqrt(N)`; cost grows linearly in both prep time and per-eval scoring. Calibrate picks accounts in descending order of fav count, so lowering this prefers the deepest profiles. |
| `calibrate_threads` | 0 | Rayon pool size for parallel per-account scoring. `0` = auto = `nproc / 2`, matching the build-jobs cap in `.cargo/config.toml`. Set higher (up to `nproc`) to finish a grid faster at the cost of saturating the box. |

### Seed binary

| Knob | Default | What it controls |
|---|---|---|
| `max_pages_per_user` | 8 | Cap on favourites pages the seed binary fetches per user (160 posts/page on e621). Lower → faster seed runs, smaller catalog growth. Higher → richer profiles but seed wall-clock grows linearly. 8 pages = 1280 favs sits well above `min_favs` and bounds each user's import to a few seconds of fetch + DB work. |
| `seed_owner_token` | `"calibration-seed"` | `owner_token` written into the device-link table for seeded accounts. They have no real device linked, so they never appear in any user's `/recommendations` request — they coexist with production accounts safely. Change only when running multiple isolated seed campaigns against the same DB (e.g. `"calibration-seed-v2"`). |

## Cleaning up

The seeded accounts use `owner_token = "calibration-seed"` and won't
appear in any normal `/recommendations/<id>` request (which require a
real device token). They live alongside production data without
interfering with it. To remove them entirely:

```sql
-- Run against database.db
DELETE FROM account_device_links WHERE owner_token = 'calibration-seed';
DELETE FROM accounts WHERE id IN (
    SELECT a.id FROM accounts a
    LEFT JOIN account_device_links adl ON adl.account_id = a.id
    WHERE adl.account_id IS NULL
);
-- Cascade deletes accounts_post / account_*_profile / etc.
```

The catalog (`posts`, `tags`, `tag_cooccurrence`) is left intact since
production benefits from a richer catalog regardless of where the
posts came from.
