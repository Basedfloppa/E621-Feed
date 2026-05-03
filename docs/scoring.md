# Scoring Knobs Reference

Quick what-happens-if-I-move-this guide for every scoring knob exposed
in [`parser-api/config.example.toml`](../parser-api/config.example.toml).

For default values see the example config; for offline-grid evidence on
which directions are trustworthy see [calibration.md](calibration.md);
for the math see
[`parser-api/src/utils/scorer.rs`](../parser-api/src/utils/scorer.rs).

## IDF / frequency shaping

|Variable|Lower →|Higher →|
|---|---|---|
|`df_floor`|rarer tags hit harder (risk: spiky)|rarer tags toned down (stable)|
|`idf_max`|compress extremes|allow rarities to dominate more|
|`idf_lambda`|blend IDF toward 1 (flatter)|keep raw IDF contrast (sharper)|
|`idf_alpha`|stronger compression (flatter)|less compression (sharper)|
|`freq_alpha`|downplay frequency (treat counts similarly; more diversity)|amplify frequent tags (favorites dominate; less diversity)|

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

## Quality channel internals

|Variable|Lower →|Higher →|
|---|---|---|
|`quality_a`|score_total matters less|score_total matters more|
|`quality_b`|fav_count matters less|fav_count matters more|
|`quality_log_bias`|more posts register as "quality"|raises the bar before the absolute-quality sigmoid lights up|
|`quality_w_absolute`|trust user-relative score/comment ratios more|rely more on raw site score + fav thresholds|
|`quality_w_relative_score`|absolute score dominates|user's typical score level matters more|
|`quality_w_relative_comments`|comments barely count|comment volume vs user's norm matters more|

## Popularity channel internals

|Variable|Lower →|Higher →|
|---|---|---|
|`popularity_w_fav`|duration dominates|fav count dominates|
|`popularity_w_duration`|duration ignored|video/animation length near user's norm matters more|

## Recency channel

|Variable|Lower →|Higher →|
|---|---|---|
|`recency_tau_days`|faster decay (newer wins)|slower decay (older survives)|
|`recency_w_global`|personal age profile dominates|newer posts always win regardless of user pattern|
|`recency_w_personal`|uniform newer-is-better|posts close to user's typical age are favored|
|`recency_personal_floor_frac`|personal recency window can collapse tight|personal recency always at least this fraction of `recency_tau_days`|
|`recency_log_personal`|`false`: personal recency uses linear age — 30→60d gap matters as much as 300→600d|`true` (default): personal recency uses log-age — scale-free, treats those gaps equivalently|

## Diversity / MMR

|Variable|Lower →|Higher →|
|---|---|---|
|`diversity_window`|shorter memory; repeats can resurface|longer memory; more variety per page|
|`diversity_w_artist`|same artist can stack up|harder cap on back-to-back artists|
|`diversity_w_character`|same character can stack up|harder cap on repeated characters|
|`diversity_w_general`|general-tag overlap ignored|penalises posts with too-similar tag sets|

## Discrete-preference smoothing + strong-negative veto

|Variable|Lower →|Higher →|
|---|---|---|
|`discrete_smoothing_alpha`|rating/media prefs react sharply to small samples|cold-start profiles stay near neutral longer|
|`strong_negative_count`|one or two dislikes can veto a post|requires a pattern of dislikes before vetoing|
|`strong_negative_wilson_threshold`|veto fires more easily (loose statistical bar)|veto only when the negative rate is confidently above this fraction (95% lower bound). 0.5 ≈ "more likely negative than not"; 0.6 ≈ "confidently negative"|
|`strong_negative_penalty`|vetoed posts barely affected|vetoed posts pushed far down the feed (1.0 = zeroed)|

## Feedback decay + meta interaction

|Variable|Lower →|Higher →|
|---|---|---|
|`feedback_decay_half_life_days`|tag feedback fades fast — recent shifts in taste dominate quickly|tag feedback persists longer — old preferences linger|
|`meta_interaction_weight`|meta tags ignored even for interaction signal|meta-tag feedback (monochrome, absurd_res, english_text…) feeds the interaction channel; meta is still excluded from tag_similarity / tag_relation|

## Tag-relation graph

|Variable|Lower →|Higher →|
|---|---|---|
|`tag_relation_w_global`|ignore whole-catalog tag pairing|global PMI-style lift dominates the tag-relation component|
|`tag_relation_w_personal`|ignore user-specific tag pairings|pair co-occurrence inside the user's own favourites dominates the tag-relation component (auto-shrunk on small profiles)|
|`tag_relation_pmi_scale`|low-lift pairs already saturate the global signal|only strongly-associated pairs contribute meaningfully|
|`tag_relation_min_cooc`|thin pairs contribute global signal (and load into memory)|require more joint occurrences before a pair is trusted; raising this also prunes more rows out of the in-memory graph at load time|
|`tag_relation_user_min_cooc`|let cooc=1 user pairs contribute|require multiple user-side co-occurrences before a pair contributes (default 1 — user pair samples are an order of magnitude sparser than catalog samples)|
|`tag_relation_cooc_ref`|even rare global pairs trusted at full weight|global pairs need more cooc before earning full PMI weight (rare pairs get linearly shrunk toward zero)|
|`tag_relation_user_cooc_ref`|rare user pairs trusted at full weight|user pairs need more co-occurrences before earning full personal-PMI weight|
