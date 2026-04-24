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

[priors]
now = "2000-1-01T12:00:00Z" # dummy value, will be replaced with current date
recency_tau_days = 10.0

quality_a = 0.003
quality_b = 0.00035

mix_sim = 0.48
mix_quality = 0.10
mix_recency = 0.07
mix_rating = 0.10
mix_media = 0.08
mix_popularity = 0.07
mix_interaction = 0.10

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

http://localhost:8080

```bash
cd ./parser-api/
cargo watch -x run
```

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

