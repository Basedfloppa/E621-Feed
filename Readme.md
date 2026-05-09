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

## About

A self-hosted alternative to e621's built-in feed: imports your favourites, builds a per-account preference profile, and serves a personalized feed.

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
cargo install --locked cargo-audit
```

`cargo-watch` enables hot-reload for the backend, `trunk` serves/builds the frontend, and `cargo-audit` is required by the pre-commit hook.

### Pre-commit hook
There is no CI on this repo. The pre-commit hook in [`.githooks/`](.githooks/) runs `cargo audit --deny warnings` against `parser-api/` before any commit that touches it. Activate once per clone:

```bash
git config core.hooksPath .githooks
```

If a new RUSTSEC advisory blocks a commit, fix the dependency (`cargo update -p <crate>`) or — if it's a transitive warning you can't fix today — add it to `parser-api/.cargo/audit.toml` with a one-line justification. Use `git commit --no-verify` only for emergencies.

---

# Running Locally

## Backend

Defaults for every config key live in
[`parser-api/config.example.toml`](parser-api/config.example.toml) — copy
it to `parser-api/config.toml`, fill in `admin_user` / `admin_api`, and
override only the knobs you want to change. The example file is the
single source of truth referenced from this README and from
[`docs/calibration.md`](docs/calibration.md).

Per-knob "lower vs. higher" guide for every scoring variable lives in
[`docs/scoring.md`](docs/scoring.md), grouped by channel (IDF, mix
weights, quality, popularity, recency, diversity, feedback, tag
relations).

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

## Calibration & Offline Backtesting

```bash
cd ./parser-api/
cargo build --release --bin seed --bin calibrate

./target/release/seed 100          # ~15-20 min, imports ~50 users' favs
./target/release/calibrate eval    # baseline metrics with current priors
./target/release/calibrate grid    # full 27-knob greedy search (~4h on 12-core)
```

See [docs/calibration.md](docs/calibration.md) for the full guide
including caveats on holdout artifacts and online A/B via experiment
buckets.

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

---

## Production deployment

Hosting the app behind nginx with release-mode pre-compression is
covered separately in [docs/deployment.md](docs/deployment.md).

---

## License

[MIT-0 (MIT No Attribution)](LICENCE) — use, modify, and redistribute
freely, no attribution required.
