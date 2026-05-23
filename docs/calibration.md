# Calibration & Offline Backtesting

## What it does

1. **`seed`** populates the local database with the favourites of the top-N
   public e621 users (ranked by `favorite_count`). Pure read-only against
   e621; no public side effects.
2. **`calibrate`** builds a holdout-based evaluation harness from those
   favourites: per user, splits favs into train/test. Three split
   strategies:
   - `split=post_id` (default) — sort by `post_id`, oldest 80% → synthetic
     profile, newest 20% → test set. Cheap; biases recency.
   - `split=random` — deterministic uniform-random hold-out. Easier task
     because positives can pre-date training items.
   - `split=time_causal` — sort favourites by `created_at`, hold the
     newest 20% as test. Closer to the production "predict next
     favourite" task and resilient to id-aliasing on imported data.

   Negatives are sampled from the catalog (default: mixed hard-negatives
   — popularity- and time-matched; switchable to pure-random via
   `neg=uniform` or hybrid via `neg=hybrid` for 70% mixed + 30%
   tag-similarity-based hard negatives). Each test fixture also carries a
   **per-account tag-relation graph** built from `train_posts`
   cooccurrences, so the personal `tag_relation` channel and its
   `*_user_*` knobs see real signal under the synthetic split (added in
   v5.7).

   Reports **NDCG@20**, **Recall@50**, **MRR** with **bootstrap 95% CI**
   on NDCG. Probe acceptance during grid uses an SE-aware threshold
   (`new > best + 1.645·SE_NDCG`) so 4th-decimal noise no longer
   pollutes the search.
3. **`calibrate grid`** runs a multi-pass greedy line search with adaptive
   probe steps (×1.0 / ×0.5 / ×0.25 across passes), followed by a paired
   sweep over known-correlated knob pairs and a categorical sweep for
   enum-valued knobs. Reports `[best priors — non-default values]` and a
   clamp-saturation warning if any knob landed at its search boundary.

   As of v5.11 the grid covers ~80 numeric knobs + 1 categorical:
   * 11 `mix_*` weights (added `mix_uploader`, `mix_exclusivity`, `mix_novelty`)
   * IDF / frequency shaping (7): `df_floor`, `idf_max`, `idf_lambda`,
     `idf_alpha`, `freq_alpha`, `bm25_k`, `one_sided_ratio_exp`,
     `idf_rsj_smoothing`
   * Quality channel (4): `quality_a/b/log_bias`, `quality_c`,
     `quality_w_absolute/relative_score/relative_comments`
   * Popularity channel (2): `popularity_w_fav/duration`
   * Recency channel (6): `recency_tau_days`, `recency_tau_recent`,
     `recency_tau_hot`, `recency_split_age_days`, `recency_split_age_hours`,
     `recency_w_global/personal`, `recency_personal_floor_frac`
   * Discrete-pref + cold-start (3): `discrete_smoothing_alpha`,
     `discrete_pref_floor`, `coldstart_n0`
   * Tag-relation (6): `tag_relation_pmi_scale`, `tag_relation_w_global/personal`,
     `tag_relation_cooc_ref/user_cooc_ref`, `tag_relation_max_tags`
   * Cold-start internals (2): `coldstart_smoothing_boost`,
     `interaction_ctr_prior_alpha`
   * Per-group multipliers (6): `group_w_artist/character/copyright/species/general/lore`
   * Algorithmic shape (5): `score_temperature`, `confidence_steepness`,
     `mmr_redundancy_exp`, `tag_sim_jaccard_blend`, `exploration_epsilon`
   * Point splits (5, NaN-sentinel disabled): `idf_lambda_meta`,
     `recency_tau_recent`, `recency_tau_hot`, `tag_relation_pmi_scale_user`,
     `recency_split_age_hours`
   * Uploader channel (3): `uploader_n0`, `uploader_w_avg_score`, `uploader_w_avg_fav`
   * Exclusivity channel (3): `min_exclusivity_cooc`, `exclusivity_scale`, `exclusivity_max_tags`
   * Novelty channel (1): `novelty_n0` (`novelty_use_feedback` is bool — not grid-swept)
   * Diversity weights (5): `diversity_w_artist/character/copyright/species/general`
   * **Class J — diversity semantic (3):** `diversity_semantic_blend`,
     `diversity_pmi_threshold`, `diversity_semantic_max_tags`
   * Categorical (1): `tag_relation_pair_aggregator` ∈ {mean, max, geomean}

   Subsets / flags:
   * `grid mix-only` — only the 11 mix weights (fastest)
   * `grid pairs-only` — skip the single-knob sweep, only the paired moves
   * `grid no-pairs` — skip the paired sweep
   * `grid with-diversify` — run `diversify_scored_posts` before NDCG so
     `diversity_*` knobs become measurable
   * `verbose` — log every probe (including rejected ones) plus
     per-knob early-exit notices

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
4. **Diversity knobs (`diversity_*`)** are in the grid only under
   `--with-diversify` (gated via `diversify_only`). Even there ΔNDCG@20
   is 0.001–0.003 — MMR is a UX feature, not an offline-metric tuning
   knob. Tune online or measure UX directly.
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
(e.g. `{ group_w_artist = 2.0, diversity_max_penalty = 0.3 }`). Legacy
`mix_*` fields take precedence over `priors` when both specify the same
knob. See `merge_priors()` in
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
`calibration_results/` at the repo root.

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
   the 9 scoring channels its delta affects. The baseline run computes all
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

- `Post` objects (transient, per-iteration): ~3.5 KB / post; dropped at
  the end of each account's hydration so they don't accumulate.
- `CachedPostFeatures`: ~2 KB / post (avg 25 tags × ~80 B/tag) → ~450 MB
  at N=1000 × ~220 posts/account.
- `DiversityFeatures` (only when `--with-diversify`): ~120 B / post →
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
