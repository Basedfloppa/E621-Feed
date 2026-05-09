# Calibration & Offline Backtesting

## What it does

1. **`seed`** populates the local database with the favourites of the top-N
   public e621 users (ranked by `favorite_count`). Pure read-only against
   e621; no public side effects.
2. **`calibrate`** builds a holdout-based evaluation harness from those
   favourites: per user, splits favs into train/test (default: oldest 80%
   → synthetic profile, newest 20% → test set; switchable to uniform-random
   via `split=random`). Negatives are sampled from the catalog (default:
   mixed hard-negatives — popularity- and time-matched; switchable to
   pure-random via `neg=uniform`). Reports **NDCG@20**, **Recall@50**, **MRR**.
3. **`calibrate grid`** runs a multi-pass greedy line search with adaptive
   probe steps (×1.0 / ×0.5 / ×0.25 across passes), followed by a paired
   sweep over known-correlated knob pairs and a categorical sweep for
   enum-valued knobs. Reports `[best priors — non-default values]` and a
   clamp-saturation warning if any knob landed at its search boundary.

   v5.3 grid covers ~52 numeric knobs + 1 categorical:
   * 8 `mix_*` weights
   * IDF / frequency shaping (7): `df_floor`, `idf_max`, `idf_lambda`,
     `idf_alpha`, `freq_alpha`, `bm25_k`, `one_sided_ratio_exp`,
     `idf_rsj_smoothing`
   * Quality channel (5): `quality_a/b/log_bias`,
     `quality_w_absolute/relative_score/relative_comments`
   * Popularity channel (2): `popularity_w_fav/duration`
   * Recency channel (4): `recency_tau_days`, `recency_w_global/personal`,
     `recency_personal_floor_frac`
   * Discrete-pref + cold-start (3): `discrete_smoothing_alpha`,
     `discrete_pref_floor`, `coldstart_n0`
   * Tag-relation (5): `tag_relation_pmi_scale`, `tag_relation_w_global/personal`,
     `tag_relation_cooc_ref/user_cooc_ref`
   * Cold-start internals (2): `coldstart_smoothing_boost`,
     `interaction_ctr_prior_alpha`
   * Per-group multipliers (6): `group_w_artist/character/copyright/species/general/lore`
   * Algorithmic shape (4): `score_temperature`, `confidence_steepness`,
     `mmr_redundancy_exp`, `tag_sim_jaccard_blend`
   * Point splits (3, NaN-sentinel disabled): `idf_lambda_meta`,
     `recency_tau_recent`, `tag_relation_pmi_scale_user`
   * Categorical (1): `tag_relation_pair_aggregator` ∈ {mean, max, geomean}

   Subsets:
   * `grid mix-only` — only the 8 mix weights (fastest)
   * `grid pairs-only` — skip the single-knob sweep, only the paired moves
   * `grid no-pairs` — skip the paired sweep
   * `grid with-diversify` — run `diversify_scored_posts` before NDCG so
     `diversity_*` knobs become measurable

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
cargo build --release --bin seed --bin calibrate

# 3. Inspect what's already in the DB before seeding more.
./target/release/calibrate probe

# 4. Pull in N users worth of public favourites.
#    Each successful import adds ~1280 favourites (cap is 8 pages × 160).
./target/release/seed 100         # ~15-20 min, adds ~50 actual users

# 5. Re-probe to see what you got.
./target/release/calibrate probe

# 6. Run baseline + full grid in one go (single hydration).
./target/release/calibrate eval grid split=random with-diversify
```

(If you only want one mode, swap step 6 for `calibrate eval ...` or
`calibrate grid ...`. Hydration cost is identical.)

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
[grid] 52 knobs × ~4 probes/pass × 3 passes = up to 624 evals + paired sweep
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
   `recency_w_*`) drift toward "off".** Train/test is split by post id
   (newer posts are the test set), so the user's "average favourite age"
   computed from train is older than the test items. The recency channel
   penalises exactly what we want to retrieve. Recency is generally useful
   in production — don't zero these out blindly.
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
4. **Diversity knobs (`diversity_*`) aren't in the grid.** Calibrate
   doesn't run `diversify_scored_posts`; it just scores + ranks. Tune by
   hand or measure online.

What **is** trustworthy from the offline grid: the **direction** of knobs
that don't depend on those biases — `mix_sim`, `mix_rating`,
`mix_tag_relation`, `idf_*`, `freq_alpha`, `discrete_*`, `tag_relation_*`,
`coldstart_n0`. Apply moderate adjustments rather than copying the extreme
`[best]` values verbatim.

## Online A/B via experiment buckets

The v5-calibrated values are **already baked into the defaults** in
[`parser-api/config.example.toml`](../parser-api/config.example.toml).
To validate the change in production, define a `control` bucket that
rolls the mix weights back to the pre-v5 values. Only the 8 `mix_*`
knobs are bucket-overridable (see `BucketOverride` in
[`src/models/config.rs`](../parser-api/src/models/config.rs)) — IDF,
`freq_alpha`, and `tag_relation_*` shifts apply to all arms.

```toml
[buckets.control]
mix_sim          = 0.48   # pre-v5 default
mix_quality      = 0.10
mix_recency      = 0.07
mix_rating       = 0.10
mix_media        = 0.08
mix_popularity   = 0.07
mix_interaction  = 0.10
mix_tag_relation = 0.08

[buckets.exp_v5]
# empty = current config (the v5-calibrated mix weights from config.example.toml)
```

Accounts are auto-bucketed by `account_id` hash (deterministic across
restarts). Per-interaction bucket assignment is logged into
`feed_interactions.experiment_bucket`, so a few weeks later you can
compare CTR (`opens / qualified_impressions`) and hide-rate per arm with
a plain SQL query.

Override an account into a specific bucket by setting
`accounts.experiment_bucket` directly — useful for pinning your own
account to `control` while testing the v5 arm (or vice versa).

### Calibration history

For reference, the unfiltered `[best]` weights from each grid run and
the production defaults distilled from them. The "Pre-v5" column is
what the defaults *used to be*; "v5.1 default" is what they are now
in [`config.example.toml`](../parser-api/config.example.toml).

| Knob | Pre-v5 | N=150 v5 best | v5 default | N=915 v5.1 best | **v5.1 default** |
|---|---|---|---|---|---|
| `mix_sim` | 0.48 | 0.63 | 0.58 | 0.63 | **0.60** |
| `mix_quality` | 0.10 | 0.00 | 0.05 | 0.00 | **0.05** |
| `mix_recency` | 0.07 | 0.00 | 0.04 | 0.00 | **0.04** |
| `mix_rating` | 0.10 | 0.00 | 0.07 | 0.00 | **0.07** |
| `mix_media` | 0.08 | 0.03 | 0.05 | 0.05 | **0.05** |
| `mix_popularity` | 0.07 | 0.00 | 0.04 | 0.00 | **0.04** |
| `mix_interaction` | 0.10 | 0.10 | 0.10 | 0.10 | **0.10** |
| `mix_tag_relation` | 0.08 | 0.08 | 0.08 | 0.08 | **0.08** |
| `idf_lambda` | 0.35 | 0.80 | 0.55 | 1.00 (pinned) | **0.70** |
| `idf_alpha` | 0.65 | 1.00 (pinned) | 0.85 | 1.00 (pinned) | **0.92** |
| `freq_alpha` | 0.45 | 0.90 | 0.65 | 1.00 (pinned) | **0.80** |
| `tag_relation_pmi_scale` | 5.00 | 2.00 | 3.5 | 3.5 | **3.5** |
| `tag_relation_cooc_ref` | 20.0 | 13.0 | 16.0 | 16.0 | **16.0** |

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
holdout artifacts described above; aggressive zero-ing those would
produce a worse production feed. The IDF / `freq_alpha` moves are
trustworthy in direction but the N=915 grid pinned all three to the
upper clamp boundary (1.0), which signals that random-negative
retrieval keeps rewarding ever-sharper rare-tag contrast — also a
known artifact. v5.1 advances each by ~½ of the gap between the v5
default and the saturated [best], leaving headroom for online tuning.

NDCG@20 / Recall@50 / MRR on the N=915 v5.1 full-grid `[best]` run was
**0.7274 / 0.1475 / 0.8871**. Real production lift will be a fraction
of this — copy via the bucket A/B mechanism described above rather
than as an unconditional rollout.

## Files

- [`parser-api/src/bin/seed.rs`](../parser-api/src/bin/seed.rs) —
  discovery, probing, import. Single-threaded by design.
- [`parser-api/src/bin/calibrate/`](../parser-api/src/bin/calibrate/) —
  the calibrate harness, split into:
  * `main.rs` — CLI parsing, mode chaining, dataset hand-off.
  * `dataset.rs` — eligible-account selection, train/test split,
    post hydration, per-post `CachedPostFeatures` build.
  * `sampling.rs` — train/test split + uniform/mixed-hard negatives.
  * `metrics.rs` — NDCG / Recall / MRR + uncached `score_with_progress`.
  * `cache.rs` — `ChannelMask`, `ScoreCache`, `score_with_cache` (the
    grid hot path).
  * `knobs.rs` — `KnobSpec` registry + per-knob invalidation masks.
  * `grid.rs` — line search + paired sweep + categorical sweep.
  * `log.rs`, `options.rs`, `probe.rs` — printing, CLI options, DB probe.
- [`parser-api/src/utils/scorer/`](../parser-api/src/utils/scorer/) —
  the scoring math being calibrated; same code paths in production. The
  cached entry points (`tag_similarity_cached`, `tag_relation_fit_cached`,
  `interaction_fit_cached`) live in `channels_cached.rs` and are a math-
  identical mirror of their `&Post` counterparts in `channels.rs`.
- [`parser-api/src/utils/scorer/cached.rs`](../parser-api/src/utils/scorer/cached.rs) —
  `CachedPostFeatures` / `CachedTag`: pre-resolved (group, lc, df_raw,
  global_tag_id) for every post in the eval set.
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
   `Vec<CachedTag>` carrying `(group, lc, df_raw, global_tag_id)` already
   resolved. The hot scoring loop then skips two HashMap-by-string probes
   per tag per post per probe (`IdfIndex::df_for` and
   `TagRelationGraph::tag_id`), which on a full grid is the difference
   between billions and zero.

2. **Per-channel score cache** ([`bin/calibrate/cache.rs`](../parser-api/src/bin/calibrate/cache.rs)).
   Every `KnobSpec` declares an `invalidates: u16` bitmask naming which of
   the 8 scoring channels its delta affects. The baseline run computes all
   channels and stores them per (account, post). Each subsequent probe
   recomputes only the invalidated channels and reuses the rest from the
   cache; the final mix-blend / temperature / strong-negative-penalty
   shape is always reapplied. Effects on common probe categories:

   | Knob class (typical examples) | Channels recomputed | Probe cost vs uncached |
   |---|---|---|
   | `mix_*` (8) / `score_temperature` / `mmr_redundancy_exp` (no diversify) / `strong_negative_penalty` | none — final blend only | ~3% |
   | `quality_*`, `popularity_*`, `recency_*`, `interaction_ctr_*`, single-channel TR knobs | one channel | ~5–40% |
   | `idf_*`, `df_floor`, `idf_max`, `bm25_k`, `freq_alpha`, `idf_rsj_smoothing`, `tag_sim_jaccard_blend` | sim only | ~30% |
   | `tag_relation_*` (PMI / cooc_ref) | tag_relation only | ~40% |
   | `coldstart_n0` / `confidence_steepness` (drives personal_confidence) | rating + media + recency + tag_relation | ~50% |
   | `group_w_*` | sim + interaction + tag_relation | ~80% |
   | `with-diversify` (any knob) | full rebuild forced | 100% |

   Knob → mask wiring lives next to each `KnobSpec` in
   [`knobs.rs`](../parser-api/src/bin/calibrate/knobs.rs); if a knob's
   semantics shift (e.g. quality_a starts depending on group_wts), update
   the mask there.

### Dataset memory budget (calibrate-only)

- `Post` objects: ~3.5 KB / post → ~770 MB at N=1000 × ~220 posts/account.
- `CachedPostFeatures`: ~2 KB / post (avg 25 tags × ~80 B/tag) → ~450 MB.
- `ScoreCache` (channels per post + transient trial cache): ~16 MB peak.
- Global graph + IDF index: same as production server (~3-5 GB).

Total calibrate peak at N=1000 sits around 6.5 GB on a 15 GB box.
Production server memory is **unaffected** — none of the cached types
are constructed by the prod scoring path.

### Historical context

Before v5.4, a full grid at N=1000 was ~8 hours. Cached tags + per-channel
invalidation pull that to ~2-3 hours; the `eval grid` chain saves the
duplicated hydration pass when running both back to back.

## Tunable knobs

All calibration / seed knobs live in `config.toml` under `[backtest]`. Every
field has a default — you only need the section if you want to override.

### Calibrate harness

| Knob | Default | What it controls |
|---|---|---|
| `min_favs` | 100 | Min favourites for an account to enter the eval set. Lower → more accounts (better statistical power), but each account's synthetic profile + holdout become noisier. Accounts with fewer than `min_favs / 2` train posts after the split are also rejected, so going below ~50 stops being useful. |
| `test_fraction` | 0.20 | Share of each user's favourites held out as test (the newest 20% by post id). Higher → more test items per account (less noisy per-account NDCG), but the synthetic profile is sparser. 0.20 is the standard ML train/test split. |
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
