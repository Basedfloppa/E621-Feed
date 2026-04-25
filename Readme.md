# E621 Account Parser

A tiny web app for storing personal favorites and generating a personalized post feed.

[![Stars](https://img.shields.io/github/stars/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/stargazers)
[![Forks](https://img.shields.io/github/forks/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/network/members)
[![Issues](https://img.shields.io/github/issues/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/issues)
[![Contributors](https://img.shields.io/github/contributors/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/graphs/contributors)
[![License](https://img.shields.io/github/license/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/E621-Account-Parser/blob/master/LICENCE)
[![Last Commit](https://img.shields.io/github/last-commit/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/commits)
[![Commit Activity](https://img.shields.io/github/commit-activity/m/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/pulse)

## Live at a temporary domain
https://e621scraper.duckdns.org

---

## Features
- Save and manage personal favorites
- Generate a customized feed based on your preferences
- Learn lightweight preference signals from feed usage
- Show recommendation score breakdowns in the feed UI
- Interactive tag-relation graph: force-directed visualisation of tag co-occurrence with community detection, panning, and zoom
- Simple local dev setup (Rust backend + Trunk-served frontend)

---

## Tooling Installation
Make sure you have [Rust](https://www.rust-lang.org/tools/install) and `cargo` installed. Then:

```bash
cargo install cargo-watch
cargo install --locked trunk
```
>cargo-watch enables hot-reload for the backend, and trunk serves/builds the frontend.

# Running Locally

---

## Backend

./config.toml
```toml
admin_user = "username"
admin_api = "api_key"
tag_blacklist = ["tag1", "tag2", "tagN"]
posts_domain = "https://uri.com"
posts_limit = 160 # 320 is max
rps_delay_ms = 250
max_retries = 3
df_floor = 0.7
idf_max = 100.0

[group_weights]
'artist' = 2.0
'character' = 1.6
'copyright' = 1.2
'species' = 1.1
'general' = 1.0
'lore' = 0.45
'meta' = 0.0

[priors]
now = "2000-1-01T12:00:00Z" # dummy value, will be replaced with current date
recency_tau_days = 10.0

quality_a = 0.50
quality_b = 0.20
quality_log_bias = -3.0

mix_sim = 0.48
mix_quality = 0.10
mix_recency = 0.07
mix_rating = 0.10
mix_media = 0.08
mix_popularity = 0.07
mix_interaction = 0.10
mix_tag_relation = 0.08

idf_lambda = 0.35
idf_alpha = 0.65
freq_alpha = 0.45

quality_w_absolute = 0.55
quality_w_relative_score = 0.30
quality_w_relative_comments = 0.15

popularity_w_fav = 0.80
popularity_w_duration = 0.20

recency_w_global = 0.40
recency_w_personal = 0.60

diversity_window = 32
diversity_w_artist = 0.22
diversity_w_character = 0.16
diversity_w_general = 0.08

# Optional (defaults shown)
discrete_smoothing_alpha = 1.0
strong_negative_count = 3
strong_negative_penalty = 0.40
strong_negative_wilson_threshold = 0.55
recency_personal_floor_frac = 1.0
recency_log_personal = true

tag_relation_w_global = 0.4
tag_relation_w_personal = 0.6
tag_relation_pmi_scale = 5.0
tag_relation_min_cooc = 2
tag_relation_user_min_cooc = 1
tag_relation_cooc_ref = 20.0
tag_relation_user_cooc_ref = 5.0

feedback_decay_half_life_days = 90.0
meta_interaction_weight = 0.3
```

Small guide on scoring vars

|Variable|Lower →|Higher →|
|---|---|---|
|`df_floor`|rarer tags hit harder (risk: spiky)|rarer tags toned down (stable)|
|`idf_max`|compress extremes|allow rarities to dominate more|
|`idf_lambda`|blend IDF toward 1 (flatter)|keep raw IDF contrast (sharper)|
|`idf_alpha`|stronger compression (flatter)|less compression (sharper)|
|`freq_alpha`|downplay frequency (treat counts similarly; more diversity)|amplify frequent tags (favorites dominate; less diversity)|
|`quality_a`|score_total matters less|score_total matters more|
|`quality_b`|fav_count matters less|fav_count matters more|
|`recency_tau_days`|faster decay (newer wins)|slower decay (older survives)|
|`mix_sim`|personalization weaker|personalization stronger|
|`mix_quality`|quality matters less|quality matters more|
|`mix_recency`|freshness matters less|freshness matters more|
|`mix_rating`|rating compatibility matters less|rating compatibility matters more|
|`mix_media`|media-type preference matters less|media-type preference matters more|
|`mix_popularity`|popularity fit matters less|feed tracks account's usual popularity range more|
|`mix_interaction`|feed history matters less|recent browsing behavior matters more|
|`mix_tag_relation`|tag co-occurrence graph ignored|tag pairs that cluster together (globally or in favourites) lift scores more|
|`quality_w_absolute`|trust user-relative score/comment ratios more|rely more on raw site score + fav thresholds|
|`quality_w_relative_score`|absolute score dominates|user's typical score level matters more|
|`quality_w_relative_comments`|comments barely count|comment volume vs user's norm matters more|
|`popularity_w_fav`|duration dominates|fav count dominates|
|`popularity_w_duration`|duration ignored|video/animation length near user's norm matters more|
|`recency_w_global`|personal age profile dominates|newer posts always win regardless of user pattern|
|`recency_w_personal`|uniform newer-is-better|posts close to user's typical age are favored|
|`diversity_window`|shorter memory; repeats can resurface|longer memory; more variety per page|
|`diversity_w_artist`|same artist can stack up|harder cap on back-to-back artists|
|`diversity_w_character`|same character can stack up|harder cap on repeated characters|
|`diversity_w_general`|general-tag overlap ignored|penalises posts with too-similar tag sets|
|`quality_log_bias`|more posts register as "quality"|raises the bar before the absolute-quality sigmoid lights up|
|`discrete_smoothing_alpha`|rating/media prefs react sharply to small samples|cold-start profiles stay near neutral longer|
|`strong_negative_count`|one or two dislikes can veto a post|requires a pattern of dislikes before vetoing|
|`strong_negative_wilson_threshold`|veto fires more easily (loose statistical bar)|veto only when the negative rate is confidently above this fraction (95% lower bound). 0.5 ≈ "more likely negative than not"; 0.6 ≈ "confidently negative"|
|`strong_negative_penalty`|vetoed posts barely affected|vetoed posts pushed far down the feed (1.0 = zeroed)|
|`recency_personal_floor_frac`|personal recency window can collapse tight|personal recency always at least this fraction of `recency_tau_days`|
|`recency_log_personal`|`false`: personal recency uses linear age — 30→60d gap matters as much as 300→600d|`true` (default): personal recency uses log-age — scale-free, treats those gaps equivalently|
|`feedback_decay_half_life_days`|tag feedback fades fast — recent shifts in taste dominate quickly|tag feedback persists longer — old preferences linger|
|`meta_interaction_weight`|meta tags ignored even for interaction signal|meta-tag feedback (monochrome, absurd_res, english_text…) feeds the interaction channel; meta is still excluded from tag_similarity / tag_relation|
|`tag_relation_w_global`|ignore whole-catalog tag pairing|global PMI-style lift dominates the tag-relation component|
|`tag_relation_w_personal`|ignore user-specific tag pairings|pair co-occurrence inside the user's own favourites dominates the tag-relation component (auto-shrunk on small profiles)|
|`tag_relation_pmi_scale`|low-lift pairs already saturate the global signal|only strongly-associated pairs contribute meaningfully|
|`tag_relation_min_cooc`|thin pairs contribute global signal (and load into memory)|require more joint occurrences before a pair is trusted; raising this also prunes more rows out of the in-memory graph at load time|
|`tag_relation_user_min_cooc`|let cooc=1 user pairs contribute|require multiple user-side co-occurrences before a pair contributes (default 1 — user pair samples are an order of magnitude sparser than catalog samples)|
|`tag_relation_user_smooth`|*(unused — kept for parse compat with older configs)*|*(unused)*|
|`tag_relation_cooc_ref`|even rare global pairs trusted at full weight|global pairs need more cooc before earning full PMI weight (rare pairs get linearly shrunk toward zero)|
|`tag_relation_user_cooc_ref`|rare user pairs trusted at full weight|user pairs need more co-occurrences before earning full personal-PMI weight|

http://localhost:8080

```bash
cd ./parser-api/
cargo watch -x run
```

### HTTP caching

`GET /account/{id}/tag_relations` returns an `ETag` derived from the response
body and `Cache-Control: private, max-age=60`. Clients sending
`If-None-Match` with the matching ETag get a `304 Not Modified` (no body).
Combined with the per-user nature of the data, this means:

- Browsers cache the graph for up to a minute without re-asking the server.
- After that, a conditional request validates with the server; if nothing
  has changed, only headers travel back.
- Shared caches (CDN/proxy) are explicitly excluded by `private`.

The endpoint isn't listed in the OpenAPI/Swagger output (it returns a custom
ETag-aware responder rather than the standard `Json<T>`), but it is
reachable at `/api/account/<id>/tag_relations` exactly like before.

---

## Frontend

./static/config.js
```js
window.APP_CONFIG = Object.freeze({
    posts_domain: "https://uri.com",
    backend_domain: "https://uri.com",
});
```

http://localhost:8000

```bash
cd ./parser-web/
trunk serve
```

