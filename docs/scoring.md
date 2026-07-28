# Scoring Knobs Reference

Quick what-happens-if-I-move-this guide for every scoring knob exposed
in [`parser-api/config.example.toml`](../parser-api/config.example.toml).

For default values see the example config; for offline-grid evidence on
which directions are trustworthy see [calibration.md](calibration.md);
for the math see
[`parser-api/src/utils/scorer/`](../parser-api/src/utils/scorer/).

The final score is a weighted blend of **11 scoring channels**:

| Channel | Mix weight | What it measures |
|---|---|---|
| `sim` | `mix_sim` | Tag cosine-similarity to the user's profile |
| `quality` | `mix_quality` | Absolute + relative quality signals (score, favs, comments, upvote ratio) |
| `recency` | `mix_recency` | How fresh the post is (multi-timescale exponential decay) |
| `rating` | `mix_rating` | Rating compatibility (S/Q/E) vs user's preference profile |
| `media` | `mix_media` | Media-type preference (image/video/flash) |
| `popularity` | `mix_popularity` | Fav count and duration vs user's norm |
| `interaction` | `mix_interaction` | Per-tag feedback (opens, hides) with Bayesian CTR and staleness decay |
| `tag_relation` | `mix_tag_relation` | PMI-based pairwise tag co-occurrence (global + personal) |
| `uploader` | `mix_uploader` | Uploader quality (avg score, avg fav count vs user's profile) |
| `exclusivity` | `mix_exclusivity` | Tag exclusivity — rare tag combinations get a boost |
| `novelty` | `mix_novelty` | Tag novelty — tags the user hasn't seen before get a boost |

## IDF / frequency shaping

|Variable|Lower →|Higher →|
|---|---|---|
|`df_floor`|rarer tags hit harder (risk: spiky)|rarer tags toned down (stable)|
|`idf_max`|compress extremes|allow rarities to dominate more|
|`idf_lambda`|blend IDF toward 1 (flatter)|keep raw IDF contrast (sharper)|
|`idf_lambda_meta`|meta-tag IDF contrast reduced separately (NaN = track `idf_lambda`)|meta tags keep raw IDF contrast even when `idf_lambda` is flat|
|`idf_alpha`|stronger compression (flatter)|less compression (sharper)|
|`freq_alpha`|downplay frequency (treat counts similarly; more diversity)|amplify frequent tags (favorites dominate; less diversity)|
|`bm25_k`|TF saturates faster (rare and frequent tags treated similarly)|TF saturates slower (frequent tags dominate more)|
|`one_sided_ratio_exp`|sqrt-shaped ratio curve (muted difference)|linear ratio curve (amplified difference between profile and post)|
|`idf_rsj_smoothing`|less Laplace smoothing in RSJ IDF|more smoothing — extreme IDF values compressed|

## Mix weights (channel blend)

|Variable|Lower →|Higher →|
|---|---|---|
|`mix_sim`|personalization weaker|personalization stronger|
|`mix_quality`|quality matters less|quality matters more|
|`mix_recency`|freshness matters less|freshness matters more|
|`mix_rating`|rating compatibility matters less|rating compatibility matters more|
|`mix_media`|media-type preference matters less|media-type preference matters more|
|`mix_popularity`|popularity fit matters less|feed tracks account's usual popularity range more|
|`mix_interaction`|feed history matters less|recent browsing behavior matters more|
|`mix_tag_relation`|tag co-occurrence graph ignored|tag pairs that cluster together (globally or in favourites) lift scores more|
|`mix_exclusivity`|exclusivity channel disabled|rare tag combinations boost the final score|
|`mix_novelty`|novelty channel disabled|unseen-before tags boost the final score|

## Quality channel internals

|Variable|Lower →|Higher →|
|---|---|---|
|`quality_a`|score_total matters less|score_total matters more|
|`quality_b`|fav_count matters less|fav_count matters more|
|`quality_c`|upvote ratio ignored|`up / (up + down)` blended into quality with this weight (0 = disabled, default 0.3)|
|`quality_log_bias`|more posts register as "quality"|raises the bar before the absolute-quality sigmoid lights up|
|`quality_w_absolute`|trust user-relative score/comment ratios more|rely more on raw site score + fav thresholds|
|`quality_w_relative_score`|absolute score dominates|user's typical score level matters more|
|`quality_w_relative_comments`|comments barely count|comment volume vs user's norm matters more|

The upvote ratio component (weighted by `quality_c`) is blended into the existing
three-component score as a fourth weighted term. At `quality_c = 0` (legacy
default) the behaviour is unchanged. The ratio defaults to 0.5 for posts with
zero votes. Uses the `score.up` / `score.down` fields from the e621 API.

## Popularity channel internals

|Variable|Lower →|Higher →|
|---|---|---|
|`popularity_w_fav`|duration dominates|fav count dominates|
|`popularity_w_duration`|duration ignored|video/animation length near user's norm matters more|

## Recency channel

Three-piece exponential kernel: hot → recent → days.

1. **Hot piece** — posts younger than `recency_split_age_hours` (default 24 h).
   Uses `recency_tau_hot` τ. Disabled when `recency_tau_hot` is NaN (the default).
2. **Recent piece** — posts younger than `recency_split_age_days` (default 30 d).
   Uses `recency_tau_recent` τ. Disabled when `recency_tau_recent` is NaN
   (then falls through directly to days).
3. **Days piece** — all older posts. Uses `recency_tau_days` τ.

|Variable|Lower →|Higher →|
|---|---|---|
|`recency_tau_days`|faster decay (newer wins)|slower decay (older survives)|
|`recency_tau_recent`|faster decay within the recent window|slower decay within the recent window (NaN = disabled, 2-piece kernel)|
|`recency_tau_hot`|faster decay for hot posts (<24 h)|slower decay for hot posts (NaN = disabled, falls back to recent/days)|
|`recency_split_age_days`|smaller recent window|larger recent window (default 30 d)|
|`recency_split_age_hours`|smaller hot window (hours)|larger hot window (default 24 h; only used when `recency_tau_hot` is not NaN)|
|`recency_w_global`|personal age profile dominates|newer posts always win regardless of user pattern|
|`recency_w_personal`|uniform newer-is-better|posts close to user's typical age are favored|
|`recency_personal_floor_frac`|personal recency window can collapse tight|personal recency always at least this fraction of `recency_tau_days`|
|`recency_log_personal`|`false`: personal recency uses linear age — 30→60d gap matters as much as 300→600d|`true` (default): personal recency uses log-age — scale-free, treats those gaps equivalently|

## Diversity / MMR

MMR is applied first (Jaccard-based redundancy penalty across 5 scored tag
groups — artist, character, copyright, species, general), then a
diversity-quota pass guarantees minimum variety in the top 20:

- **Artist quota** — at least 2 different artists
- **Character quota** — at least 3 different characters

### Semantic similarity (v5.11)

By default MMR uses **Jaccard similarity** on exact tag-name hashes — two posts
share a tag or they don't. When `diversity_semantic_blend > 0`, a fraction of
the similarity comes from **PMI-based soft matching**: tags that co-occur more
often than chance (e.g. `canine` and `wolf`) count as a "soft match" even when
the tag name differs. The per-group similarity becomes:

```
sim = (1 - blend) × Jaccard(hashes) + blend × PMI_match_ratio
```

Where `PMI_match_ratio` is the fraction of (tag_a, tag_b) pairs whose pointwise
mutual information exceeds `diversity_pmi_threshold`, capped at
`diversity_semantic_max_tags` tags per group per post to bound O(T²) cost.

When `diversity_semantic_blend > 0`, the personal tag-relation graph contributes
PMI alongside the global graph. `diversity_user_pmi_weight` controls the balance:
user-graph PMI is multiplied by this factor before blending with global PMI.
Values > 1.0 amplify per-user diversity personalisation (a user who co-favourites
`skeb`+`canine` gets less MMR penalty for those tags together); 0 disables the
user graph entirely for diversity even when `diversity_semantic_blend > 0`.

|Variable|Lower →|Higher →|
|---|---|---|
|`diversity_window`|shorter memory; repeats can resurface|longer memory; more variety per page|
|`diversity_w_artist`|same artist can stack up|harder cap on back-to-back artists|
|`diversity_w_character`|same character can stack up|harder cap on repeated characters|
|`diversity_w_copyright`|copyright overlap ignored|penalises posts sharing copyright tags|
|`diversity_w_species`|species overlap ignored|penalises posts sharing species tags|
|`diversity_w_general`|general-tag overlap ignored|penalises posts with too-similar tag sets|
|`diversity_max_penalty`|MMR penalty capped lower|redundancy penalty can push duplicate posts further down|
|`diversity_interaction_damp`|interaction signal doesn't reduce redundancy penalty|liked similar posts → less penalty for similarity|
|`diversity_semantic_blend`|pure Jaccard (legacy)|PMI-soft-match blended into MMR similarity (0 = disabled, default)|
|`diversity_pmi_threshold`|more pairs count as semantic matches|only strongly-associated pairs (PMI above this) count|
|`diversity_semantic_max_tags`|fewer tags in O(T²) loop (faster)|more tags per group considered (slower, more signal)|
|`diversity_user_pmi_weight`|global PMI dominates diversity signal|per-user tag co-occurrence patterns drive diversity personalisation; 0 = disable user graph|

## Search result scoring

The Search page normally returns raw e621 matches. Enabling **Score results**
uses the selected account's full scoring context and returns a `ScoredPost` for
each match, including the same breakdown used by the feed. Results are sorted
by affinity before the client applies one of these per-page cutoffs:

|Preset|Cutoff|Meaning|
|---|---:|---|
|Wide|0%|Keep every scored match.|
|Balanced|30%|Drop the lowest-scoring 30% of the page.|
|Strict|60%|Keep only the highest-scoring 40% of the page.|

Cutoffs only affect Search presentation; they do not add a blacklist or alter
the account's preference profile.

## Discrete-preference smoothing + strong-negative veto

|Variable|Lower →|Higher →|
|---|---|---|
|`discrete_smoothing_alpha`|rating/media prefs react sharply to small samples|cold-start profiles stay near neutral longer|
|`strong_negative_count`|one or two dislikes can veto a post|requires a pattern of dislikes before vetoing|
|`strong_negative_wilson_threshold`|veto fires more easily (loose statistical bar)|veto only when the negative rate is confidently above this fraction (95% lower bound). 0.5 ≈ "more likely negative than not"; 0.6 ≈ "confidently negative"|
|`strong_negative_penalty`|vetoed posts barely affected|vetoed posts pushed far down the feed (1.0 = zeroed)|

## Feedback decay + meta interaction

The post card's … recommendation menu supports three explicit signals:
**Like**, **Strong like**, and **Not interested**. Like records one positive
feedback count for every tag on the post; Strong like records three. Not
interested hides the post and records one negative count per tag, reducing
future `interaction_fit` scores for similar candidates rather than creating a
permanent blacklist rule. The same menu can add a confirmed permanent account
blacklist rule for a tag, artist, uploader, rating, or media category. The
media rules use e621 meta tags: `video`, `animated`, or `-animated` (static
media). Undo removes the current session's hide interaction
and reverses that negative tag feedback. The menu closes when the pointer
leaves it or the user clicks outside it.

Feedback counts are decayed eagerly at `/process` time. Between refreshes, a
supplementary decay factor is applied per-tag in `interaction_fit` based on
`profile_refreshed_at` and `feedback_decay_half_life_days` — the longer since
the last profile rebuild, the less the interaction signal is trusted.

|Variable|Lower →|Higher →|
|---|---|---|
|`feedback_decay_half_life_days`|tag feedback fades fast — recent shifts in taste dominate quickly|tag feedback persists longer — old preferences linger|
|`meta_interaction_weight`|meta tags ignored even for interaction signal|meta-tag feedback (monochrome, absurd_res, english_text…) feeds the interaction channel; meta is still excluded from tag_similarity / tag_relation|

## Rating fit

Confidence-weighted blend between Bayesian-smoothed and raw observed rate.
When the user has a strong preference for a rating (e.g. 500 S vs 50 Q),
the raw rate dominates. When preference is weak or noisy, the smoothed
estimate keeps the score conservative.

|Variable|Lower →|Higher →|
|---|---|---|
|`discrete_smoothing_alpha`|rating/media prefs react sharply to small samples|cold-start profiles stay near neutral longer|
|`coldstart_smoothing_boost`|less extra smoothing below `coldstart_n0`|more aggressive smoothing for cold profiles|
|`discrete_pref_floor`|ratings/media-types with zero samples get near-zero score|minimum score floor for unseen categories|

## Uploader channel

When the account's profile has been refreshed, each uploader the user has
favourited carries aggregated statistics (`avg_score`, `avg_fav`). Posts
from uploaders whose stats are above the user's personal average receive a
boost; below-average uploaders are penalised. Confidence-weighted by the
number of posts seen from that uploader.

|Variable|Lower →|Higher →|
|---|---|---|
|`mix_uploader`|uploader quality ignored|uploader quality blends into the final score (default 0.05)|
|`uploader_n0`|fewer posts needed to trust uploader signal|more evidence required before uploader stats affect the score|
|`uploader_w_avg_score`|fav count dominates uploader signal|avg score dominates uploader signal|
|`uploader_w_avg_fav`|avg score dominates uploader signal|avg fav count dominates uploader signal|

## Tag exclusivity channel (v5.11)

Tags that rarely co-occur on the same post form a rare combination; posts
dominated by such rare pairs earn a higher exclusivity score. The channel
runs an O(T²) loop over the post's tags (truncated to `exclusivity_max_tags`
by group weight, similar to Cluster-PMI) and reads pairwise co-occurrence
counts from the global tag-relation graph. The average co-occurrence per
pair is mapped to a score via:

`exclusivity = 1.0 − sigmoid(avg_cooc / exclusivity_scale − min_exclusivity_cooc)`

Pairs that span different tag groups (e.g. artist × species) are weighted
by `exclusivity_cross_group_weight` relative to within-group pairs. The
effective cross-group contribution is `cross_group_weight / (cross_group_weight + 1.0)`.
Default 0.5 → cross-group pairs contribute ~⅓ of total exclusivity weight.
Higher values give more credit to rare multi-group tag combos.

Default mix weight is 0 (disabled). Exclusivity signal is most useful for
surfacing niche or cross-over content that doesn't naturally cluster into
the user's established tag groups.

|Variable|Lower →|Higher →|
|---|---|---|
|`mix_exclusivity`|exclusivity channel disabled|rare tag combinations boost the final score (default 0)|
|`exclusivity_scale`|exclusivity curve sharper — small cooc differences matter|curve flatter — only very rare pairs get full credit (default 0.5)|
|`min_exclusivity_cooc`|more pairs labelled "rare"|fewer pairs labelled rare — requires truly unusual combos (default 2)|
|`exclusivity_max_tags`|more tags enter the O(K²) loop (slower, more signal)|fewer tags — faster, but may miss rare pairs at the tail (default 15)|
|`exclusivity_cross_group_weight`|within-group pairs dominate (cross-group nearly ignored)|cross-group rare combos contribute equally with within-group (default 0.5)|

## Tag novelty channel (v5.11)

A post scores higher when it carries tags the user hasn't seen before (or has
seen only a few times). The channel checks each tag against the user's
favourites (`self.user` maps) and optionally against `feed_interactions`
impression counts.

For each tag, novelty is computed as `1.0 − confidence(impressions, n0, 1.0)`,
where confidence is the `n^p / (n^p + n0^p)` curve. Tags with zero
impressions get full novelty (1.0); tags the user has seen many times
approach 0. The per-tag novelty scores are averaged across all tags on
the post.

Default mix weight is 0 (disabled). Most useful for cold-start users or
for users who want broader discovery beyond their established tag profile.

|Variable|Lower →|Higher →|
|---|---|---|
|`mix_novelty`|novelty channel disabled|unseen-before tags boost the final score (default 0)|
|`novelty_n0`|fewer impressions needed to consider a tag "known"|more impressions required before a tag is familiar (default 3)|
|`novelty_use_feedback`|`false`: only check favourites for novelty|`true` (default): also check feed_interaction impression counts|

## Exploration bonus

|Variable|Lower →|Higher →|
|---|---|---|
|`exploration_epsilon`|pure exploit (default 0)|more exploration toward novel content (capped at 0.5)|

## Tag-relation graph + Cluster-PMI

The tag-relation channel computes PMI-weighted pairwise associations
between every pair of tags on a post. To keep this O(T²) loop tractable
on posts with many tags, **Cluster-PMI** keeps only the top-K tags by
group weight (default 20, controlled by `tag_relation_max_tags`).

Per-pair PMI values are aggregated into a single channel score via
`tag_relation_pair_aggregator`:

- `mean` (default) — arithmetic mean of all pair scores
- `max` — strongest pair dominates
- `geomean` — geometric mean, dampens outlier pairs

|Variable|Lower →|Higher →|
|---|---|---|
|`tag_relation_w_global`|ignore whole-catalog tag pairing|global PMI-style lift dominates the tag-relation component|
|`tag_relation_w_personal`|ignore user-specific tag pairings|pair co-occurrence inside the user's own favourites dominates the tag-relation component (auto-shrunk on small profiles)|
|`tag_relation_pmi_scale`|low-lift pairs already saturate the global signal|only strongly-associated pairs contribute meaningfully|
|`tag_relation_pmi_scale_user`|separate PMI scale for user graph (NaN = track `tag_relation_pmi_scale`)|personal PMI amplified independently of global|
|`tag_relation_min_cooc`|thin pairs contribute global signal (and load into memory)|require more joint occurrences before a pair is trusted; raising this also prunes more rows out of the in-memory graph at load time|
|`tag_relation_user_min_cooc`|let cooc=1 user pairs contribute|require multiple user-side co-occurrences before a pair contributes (default 1 — user pair samples are an order of magnitude sparser than catalog samples)|
|`tag_relation_cooc_ref`|even rare global pairs trusted at full weight|global pairs need more cooc before earning full PMI weight (rare pairs get linearly shrunk toward zero)|
|`tag_relation_user_cooc_ref`|rare user pairs trusted at full weight|user pairs need more co-occurrences before earning full personal-PMI weight|
|`tag_relation_max_tags`|more tags enter the O(K²) pairwise loop (slower, more signal)|fewer tags used — O(T²) → O(K²) speedup via Cluster-PMI (default 20, set to 0 for no limit)|
