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
   `neg=uniform` switches to pure random). Each test fixture also carries a
   **per-account tag-relation graph** built from `train_posts`
   cooccurrences, so the personal `tag_relation` channel and its
   `*_user_*` knobs see real signal under the synthetic split (added in
   v5.7).

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

   The grid currently covers 85 numeric knobs + 2 categorical:
   - 12 `mix_*` weights (`mix_uploader`, `mix_exclusivity`, `mix_novelty`,
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
   - Novelty channel (1): `novelty_n0` (`novelty_use_feedback` is swept by
     the categorical registry below)
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
   - Categorical (2): `tag_relation_pair_aggregator` ∈ {mean, max, geomean};
     `novelty_use_feedback` ∈ {true, false}

   Subsets / flags:
   - `grid mix-only` — only the 12 mix weights (fastest)
   - `grid pairs-only` — skip the single-knob sweep, only the paired moves
   - `grid no-pairs` — skip the paired sweep
   - `grid with-diversify` — run `diversify_indices` before NDCG so
     `diversity_*` knobs become measurable
   - `verbose` — log every probe (including rejected ones) plus
     per-knob early-exit notices
   - `fit-mix` — after the eval pass, solve the twelve `mix_*` weights
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

The grid runtime above includes the per-channel score cache + pre-resolved
post-tag features added in v5.4 — without those, full-grid wall-clock at
N=1000 was ~8 hours. See "Performance" for what each layer costs.

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
tighter confidence intervals. Each `pass N: ... ->` line is a step that
improved NDCG by more than the noise floor (1e-4). The
`[best priors — non-default values]` block prints only the fields that
moved off baseline, so you can see at a glance what to copy into
`config.toml`.

## Caveats — don't blindly copy `[best priors]` into prod

Several signals are systematically biased by the offline harness and will
not move the same way in a real A/B test:

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
3. **`mix_interaction` doesn't move.** Synthetic split has no
   `feed_interactions` rows. Same applies to anything else that depends on
   feedback — `meta_interaction_weight`, `feedback_decay_*`,
   `strong_negative_*` — none are in the grid because they have zero
   gradient here. Tune online.
4. **Diversity knobs (`diversity_*`)** are in the grid only under
   `with-diversify` (gated via `diversify_only`). ⚠️ **The ΔNDCG@20 of
   0.001–0.003 this item used to quote is void.** It was measured while the
   redundancy penalty was computed against a reference score that got re-based
   after every pick, which forced the penalty to zero: the MMR pass was a plain
   sort by score, so *no* `diversity_*` value could move any metric and the
   ~0.002 spread was noise. The reference is now fixed
   (see [scoring.md](scoring.md#the-redundancy-penalty)), so the grid can probe
   these knobs for the first time. Treat the `diversity_*` values in
   `config.example.toml` (`diversity_max_penalty`, `diversity_interaction_damp`,
   `diversity_w_*`, `mmr_redundancy_exp`, `diversity_semantic_*`) as uncalibrated
   until they have been re-probed.
5. **Personal `tag_relation` channel** has signal as of v5.7: each
   fixture builds its own user-graph from train_posts. `tag_relation_
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

- **The file must carry all twelve `mix_*` keys.** `Priors` is
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

**First measurement (2026-10-10).** Pointed at the grid result that zeroes five
`mix_*` channels, the report returned:

| vector | NDCG@20 | Recall@50 | MRR |
|---|---:|---:|---:|
| shipped config (five channels restored) | 0.6110 | 0.1043 | 0.7732 |
| `grid_20261010_054734.toml` (five at 0) | **0.6799** | 0.1153 | 0.8341 |
| the solver's fitted vector | 0.4060 | 0.0672 | 0.5627 |

`delta=+0.068940 SE=0.002616` against a `0.004303` threshold at `df=675` —
**+26 SE, ACCEPT** — and the surrogate objective agrees (`ΔL=+231.78`). The
fitted vector is rejected (`−0.2049`), as it was on every run so far.

So the harness does not merely drift toward the zeroed block: it prefers it, by
a wide margin. **That is a statement about the harness, not about the
channels.** The reference vector is the output of a search driven by exactly the
biased signals listed under **Caveats** above, so this measurement quantifies
the bet instead of settling it. That is its value: keeping the five channels is
now known to cost ≈0.069 NDCG@20 *offline*, rather than being of unknown size.
See [Run 2026-10-10](#run-2026-10-10--what-the-zeroed-block-is-worth) for the
full record.

## Online A/B via experiment buckets

The v5-calibrated values are **already baked into the defaults** in
[`parser-api/config.example.toml`](../parser-api/config.example.toml).
To validate the change in production, define a `control` bucket that
rolls the mix weights back to the pre-v5 values.

Buckets can override **any Priors field** via the generic `priors` JSON
override — not just the `mix_*` weights. See `BucketOverride` in
[`src/models/config.rs`](../parser-api/src/models/config.rs).

```toml
[buckets.control]
mix_sim          = 0.48   # pre-v5 default (legacy syntax)
mix_quality      = 0.10
mix_recency      = 0.07
mix_rating       = 0.10
mix_media        = 0.08
mix_popularity   = 0.07
mix_interaction  = 0.10
mix_tag_relation = 0.08

# Or use the generic priors override (JSON inline table in TOML):
[buckets.control_v2]
priors = { mix_sim = 0.48, mix_quality = 0.10, diversity_max_penalty = 0.30 }

[buckets.exp_v5]
# empty = current config (the v5-calibrated mix weights from config.example.toml)
```

The `priors` field accepts any key from `[priors]` as a JSON inline table
(e.g. `{ group_w_artist = 2.0, diversity_max_penalty = 0.3 }`). When both
specify the same knob, **`priors` wins**: `BucketOverride::apply_to` applies the
legacy `mix_*` fields first and then merges the JSON `priors` over them. The
merge only overwrites fields that are non-default in the JSON (a numeric field
left at its default — `0.0`, or the sentinel `NAN` — is ignored), so **a JSON
`priors` entry cannot force a knob to `0.0`**; use the legacy `mix_*` field for
that. See `merge_priors()` in
[`src/models/config.rs`](../parser-api/src/models/config.rs) for the full
list of overridable fields.

Accounts are auto-bucketed by `account_id` hash (deterministic across
restarts). Per-interaction bucket assignment is logged into
`feed_interactions.experiment_bucket`, so a few weeks later you can
compare CTR (`opens / qualified_impressions`) and hide-rate per arm with
a plain SQL query.

Override an account into a specific bucket by setting
`accounts.experiment_bucket` directly — useful for pinning your own
account to `control` while testing the v5 arm (or vice versa).

### Calibration history

Per-run `[best]` weights and the production defaults distilled from
them live in [`calibration-results/`](calibration-results/) — drop new
write-ups there as grid runs accumulate. Machine-readable TOML
artifacts from `write_grid_log` continue to land in
`calibration_results/` under the working directory the run uses
(normally `parser-api/`).

#### Run 2026-10-09 — the paired gate fix, and what the prod snapshot returns

Two runs, before and after the gate was paired. Both used
`calibrate eval grid with-diversify`.

**(a) Local `database.db`, N = 16 — accepted nothing.** 170/335 probes
ran, 165 were skipped by per-knob early exit, and the paired sweep over
23 pairs added nothing (`calibration_results/grid_20261009_174759.toml`).
`[best priors — non-default values]` was empty. This is a property of the
gate, **not** evidence that the current knobs are optimal:

- The dataset was small: `probe` reported 65 255 posts, 51 accounts, only
  **27 accounts** above `min_favs = 100`.
- Eleven of those 27 then failed to hydrate with
  `Failed to acquire sqlite connection: timed out waiting for connection`
  (see the parallelism note below), so the scored set was **N = 16**.
  Baseline `NDCG@20 = 0.2882`, **SE = 0.0543**.
- **The gate was unpaired.** It accepted a probe only when
  `new > best + max(1.645 · SE, 1e-4)` with `SE = m.ndcg_se()` — the SE of
  the **mean NDCG**, not of the per-account **difference**. At N = 16 that
  demanded **+0.089 NDCG**, a third of the entire baseline. No single-knob
  move clears it, so every knob was abandoned after two probes.

**(b) Prod snapshot, N = 51 — 16 knobs accepted.** The snapshot was taken
from the live box while the service was in a WAL checkpoint (0-byte WAL),
via `sqlite3 database.db ".backup …"`, then streamed as `zstd -3` over
ssh. Copy verified: `page_count = 7 767 655`, 90 accounts, 164 388
`accounts_post`, 150 596 posts, 10 559 734 `tags_posts` — all identical to
prod. `probe` then found **51 accounts** with ≥100 favourites.

| | baseline | best |
|---|---:|---:|
| NDCG@20 | 0.2366 (CI 0.1776–0.3024, SE **0.0311**) | 0.5483 (CI 0.4823–0.6220) |
| Recall@50 | 0.1072 | 0.1928 |
| MRR | 0.4580 | 0.7227 |

The paired SE per probe was **0.0028–0.0145**, i.e. 2–11× smaller than the
level SE of 0.0311 — that ratio is the whole point of pairing, and it is
what turns the same dataset from "inert" into "16 accepted moves". The
quantile is `t(df = 50) = 1.679`, not 1.645. 528 probes ran, 477 were
skipped by early exit, 1853 s total.

**The result is a coherent but drastic direction: it zeroes 6 of the twelve
`mix_*` channels.** `mix_quality`, `mix_recency`, `mix_popularity`,
`mix_tag_relation`, `mix_exclusivity`, `mix_novelty` all went to 0
(`mix_sim` unchanged at 0.603), `mix_rating` 0.034 → 0.009, `mix_media`
0.042 → 0.017. (This run predates the result-file fix, so only eight of the
twelve weights were written: `mix_uploader` and `mix_artist_discovery` are
absent from the record, and whether they also moved cannot be recovered from
it. Six is the count the record actually names.) The L1-normalised blend collapses onto `mix_sim = 0.846`,
`mix_interaction = 0.118`, `mix_rating = 0.013`, `mix_media = 0.024`.
This is structurally expected: the weights reaching `final_blend` are already
**L1-normalised** by `MixWeights::from_priors`
([`util.rs:230`](../parser-api/src/utils/scorer/util.rs#L230)) — the twelve raw
`mix_*` are divided by the sum of their absolute values — so
zeroing a channel raises every other channel's *relative* share, and the
grid will zero any channel that is net-negative for NDCG@20 on this data.
(At the time of this run the last six weights were additionally floored at 0,
which made the collapse one-way. That floor was removed on 2026-10-10: with a
signed sum the shares blow up beside zero, so the normalisation became L1 and a
channel may now carry a negative weight. The shares quoted above are unaffected
— they come from a block whose twelve weights are all non-negative.)
`quality_w_relative_comments` 0.15 → 0, `group_w_general` 0.60 → 0.45 and
`mmr_redundancy_exp` 1.0 → 0.55 also moved.

**Out-of-sample check.** Re-running `eval` with the grid's priors under a
different split and a different negative sampler — neither of which the
grid selected against — keeps the direction:

| eval setting | baseline | grid best |
|---|---:|---:|
| `split=time_causal neg=hybrid` (the grid's own) | 0.2366 | 0.5483 |
| `split=random` | 0.6217 | 0.7767 |
| `neg=uniform` | 0.4295 | 0.7114 |

Note how much the *baseline* itself swings between settings
(0.24 / 0.62 / 0.43): absolute NDCG here is a property of the dataset
configuration, so only within-setting comparisons mean anything.

**What is still unproven.** The 51 accounts are the same ones the search
ran on, so account-level selection is untested; 528 probes at α = 0.05
also means roughly two dozen chance acceptances are expected, and the
greedy ratchet locks them in. Do **not** apply this result to production
on the strength of the offline numbers alone — put it behind the
`BucketOverride` A/B path and measure it online.

#### Harness note: `calibrate_threads` does not bound dataset prep

`prepare_eval_dataset` fans out over accounts with a bare
`into_par_iter()` ([`dataset.rs:219`](../parser-api/src/bin/calibrate/dataset.rs#L219)),
i.e. on the **global** rayon pool, not the `calibrate_threads` pool built
by `metrics::pool()`. On a 24-core box that is 24 concurrent consumers
against a 16-connection SQLite pool
([`db/mod.rs:100`](../parser-api/src/db/mod.rs#L100)), and accounts get
dropped with `timed out waiting for connection` after the 2-minute
checkout timeout — this is what cost 11 of 27 accounts in run (a).
Running with `RAYON_NUM_THREADS=12` took run (b) to 51/51 hydrated with
zero failures. The proper fix is to wrap the prep fan-out in
`pool().install(…)`.

#### Run 2026-10-10 — what the zeroed block is worth

After the grid closed (`calibration-results/grid_20261010_054734.toml`,
`NDCG@20 = 0.6803`), the shipped config deliberately kept five of the channels
the grid had zeroed — `mix_quality`, `mix_recency`, `mix_popularity`,
`mix_tag_relation`, `mix_novelty` — on the grounds that the offline evidence
for them is confounded by the biases in **Caveats**. That left an obvious
question open: how much does the offline harness actually disagree? The
cross-run numbers could not answer it, because `eval` re-samples its candidate
set from the live database every time (see
[Re-scoring a persisted vector](#re-scoring-a-persisted-vector)).

`eval --fit-mix=<the grid file>` answers it on **one** matrix, one clock, one
pair of blend parameters — `CALIBRATE_NOW = 2026-10-10T12:00:00Z`, dataset
fingerprint `0xc77974d5395cdece`, N = 676 accounts / 2 768 896 candidates /
173 056 positives:

| vector | NDCG@20 | Recall@50 | MRR |
|---|---:|---:|---:|
| shipped config (five channels restored) | 0.6110 | 0.1043 | 0.7732 |
| the grid file (five at 0) | **0.6799** | 0.1153 | 0.8341 |

```
[fit] reference objective L=-1436893.18 vs incumbent L=-1437124.96 (Δ=+231.78)
[fit] paired reference vs incumbent: delta=+0.068940 SE=0.002616
      z=t(df=675)=1.6449 threshold=0.004303 paired=true
[fit] verdict (reference vs incumbent): ACCEPT
```

**+0.069 NDCG@20, 26 standard errors.** The sanity check that makes the row
comparable also passed: `re-score check vs the eval baseline: identical
per-account NDCG (max |Δ|=0.0e0)`, so the incumbent column is the eval's own
baseline rather than a re-derivation of it. The solver's own fitted vector,
judged by the same gate on the same matrix, went the other way: 0.4060,
`delta=−0.204929`, REJECT.

Two things this run is *not*.

- **It is not evidence that the five channels are useless.** The reference
  vector is the output of a greedy search over precisely the signals the
  caveats describe as biased: `recency` and `popularity` are scored relative to
  statistics derived from the same favourites the eval treats as labels, the
  split is `time_causal` so every test item is newer than the profile it is
  compared against, and the hard negatives are mined with the incumbent
  weights. A harness with those properties *should* prefer to switch those
  channels off. The number measures the preference, and the preference is the
  thing in doubt.
- **It is not a cross-run comparison.** `0.6799` here reproduces the `0.6799`
  seen in the 08:51 standalone `eval` — but that agreement is a coincidence of
  two different datasets, not a replication. Only the two rows above, sharing a
  matrix, are a measurement.

What it does settle is the *size* of the decision: keeping the five channels is
worth ≈0.069 NDCG@20 offline, so the choice to override the harness is a
concrete trade rather than an appeal to caution. The report also confirmed the
`--fit-mix=<path>` warning path on a real file, listing two non-mix fields the
grid file disagrees on (`diversity_pmi_threshold` 0.5 → 0.0,
`diversity_semantic_blend` 0.05 → 0.0) — inert here, because `diversify` is off
and MMR never runs, but printed rather than silently absorbed.

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
  cached entry points (`tag_similarity_cached`, `tag_relation_fit_cached`,
  `interaction_fit_cached`) live in `channels_cached.rs` and are a math-
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

### Two grid-time speedups (v5.4)

1. **Pre-resolved post tags** ([`scorer/cached.rs`](../parser-api/src/utils/scorer/cached.rs)).
   At hydration time each post's tags are walked once and stored as a flat
   `Vec<CachedTag>` carrying `(group, lc, df_raw, global_tid, user_tid)` already
   resolved. The hot scoring loop then skips two HashMap-by-string probes
   per tag per post per probe (`IdfIndex::df_for` and
   `TagRelationGraph::tag_id`), which on a full grid is the difference
   between billions and zero.

2. **Per-channel score cache** ([`bin/calibrate/cache.rs`](../parser-api/src/bin/calibrate/cache.rs)).
   Every `KnobSpec` declares an `invalidates: u16` bitmask naming which of
   the 12 scoring channels its delta affects. The baseline run computes all
   channels and stores them per (account, post). Each subsequent probe
   recomputes only the invalidated channels and reuses the rest from the
   cache; the final mix-blend / temperature / strong-negative-penalty
   shape is always reapplied by the shared `final_blend` — the same
   function the production `score` / `score_cached` paths call, so cached
   and uncached scores cannot drift. Effects on common probe categories:

   | Knob class (typical examples) | Channels recomputed | Probe cost vs uncached |
   |---|---|---|
   | `mix_*` (12) / `score_temperature` / `mmr_redundancy_exp` (no diversify) / `strong_negative_penalty` | none — final blend only | ~3% |
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
- Per-account `user_relation` tag-relation graph (added v5.7): pairs
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

### Historical context

Before v5.4, a full grid at N=1000 was ~8 hours. Cached tags + per-channel
invalidation pull that to ~2-3 hours. The `eval grid` chain shares two
expensive passes between the modes: the hydration pass, and the baseline
scoring — `eval` scores it through the production scorer, and the grid reuses
that pass's channel cache as its baseline instead of rebuilding it. On the
2026-10-10 run the two baselines printed byte-identical metrics
(`NDCG@20=0.3547`, CI 0.3378–0.3724, `Recall@50=0.0595`, `MRR=0.5600`) for
62.1s + 56.9s, so the second 56.9s was pure duplication.

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
