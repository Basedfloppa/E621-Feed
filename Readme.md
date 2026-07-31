# E621 Feed

A self-hosted personalised feed engine for e621: import favourites, build
per-account preference profiles, and serve scored + diversified
recommendations via a 12-channel ensemble model. Includes offline
calibration tooling (grid search over ~80 scoring knobs) and a
WASM/Yew frontend with an interactive tag-relation graph.

[![Stars](https://img.shields.io/github/stars/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/stargazers)
[![Forks](https://img.shields.io/github/forks/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/network/members)
[![Issues](https://img.shields.io/github/issues/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/issues)
[![Contributors](https://img.shields.io/github/contributors/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/graphs/contributors)
[![License](https://img.shields.io/github/license/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/E621-Account-Parser/blob/master/LICENCE)
[![Last Commit](https://img.shields.io/github/last-commit/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/commits)
[![Commit Activity](https://img.shields.io/github/commit-activity/m/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/pulse)

## Live at a FINAL DOMAIN (YIPPIE)

<https://e621feed.zorolin.rs/feed>

---

## About

A self-hosted alternative to e621's built-in feed: imports your favourites, builds a per-account preference profile, and serves a personalized feed.

---

## Features

- Save and manage personal favourites with full reconciliation and fast
  incremental updates
- Power-user feed controls: relative per-page cutoff presets plus persistent
  Auto/3/2/1-column layouts
- Personalised feed with 12-channel scoring (tag similarity, quality, recency,
  rating, media, popularity, interaction, tag-relation, uploader, exclusivity,
  novelty, artist-discovery) and MMR diversification
- Recommendation score breakdowns in the feed UI
- Server-proxied `/search` page for e621 tag queries, alias suggestions, and
  optional per-account result scoring with Wide/Balanced/Strict cutoffs
- Persistent Search display/scoring preferences; score-dependent controls stay
  visible but disabled until result scoring is enabled
- One mobile navigation entry point: the header drawer (no duplicate nav row)
- Animated post-card previews autoplay in a loop but are hard-muted at the
  HTML attribute and media-property layers, so feed cards never emit audio
- A post-card recommendation menu with **Like**, **Strong like**, and **Not
  interested**. Like adds one positive tag-feedback signal, Strong like adds
  three, and Not interested adds one negative signal and can be undone for the
  current browsing session. It can also add confirmed permanent e621 blacklist
  rules for a tag, artist, uploader, rating, or media category; the menu closes
  when the pointer leaves it or the user clicks elsewhere
- Public `GET /api/health` readiness probe for SQLite, scoring-cache readiness,
  and e621 reachability (returns `503` when a dependency is unavailable)
- Background [catalog hydration](docs/catalog-hydration.md) repairs stale media,
  tags, and uploader metadata for every catalog post, including orphaned
  recommendation candidates, while sharing the global e621 rate limit
- Account owners can clear all interaction-derived recommendation state through
  `DELETE /api/account/<id>/interaction` without deleting favourites, blacklist,
  preferred tags, or the account itself
- Authenticated state-changing requests enforce same-origin CSRF validation;
  outbound e621 requests share a server-wide admin-key rate limit
- Daily digest with stratified sampling (top picks, trending, exploration,
  wildcard, recent)
- Session-based cross-page dedup for infinite scroll
- Similar-posts lookup by tag overlap
- Interactive tag-relation graph: force-directed visualisation of tag
  co-occurrence with community detection, panning, zoom, and ETag caching
- Per-account preferred tags with group-level weights
- Offline calibration harness: `seed` (import public favs) + `calibrate eval` /
  `calibrate grid` (NDCG/Recall/MRR with bootstrap CIs, greedy line search)
- Simple local dev setup (Rust backend + Trunk-served Yew/WASM frontend)

### Full analysis vs incremental update

Use **Full re-analysis** for the first import or whenever removed e621
favourites must be reconciled. It requests every expected favourites page,
replaces the stored account links with the pages fetched successfully, and
rebuilds the profile. Individual page failures are emitted to audit logs; two
consecutive failures abort and surface a failed status. Operators should retry
any failed or known-incomplete run.

Use **Update favourites** for routine refreshes. Incremental mode reads newest
pages until it reaches a post already stored locally, persists only new
favourites, and skips the destructive teardown. On very large accounts this
can avoid roughly 20 minutes of full-rebuild work. Because it stops at the
first known post, it cannot discover favourites removed on e621; run a full
re-analysis periodically when deletions matter.

### Power-user feed controls

The **Per-page cutoff** is relative to each scored page, not an absolute score
threshold: Wide keeps every result, Balanced drops the bottom 30%, and Strict
drops the bottom 60%. The **Grid type** control chooses responsive Auto or a
fixed three-, two-, or one-column layout. Both settings are stored in browser
`localStorage`; they change filtering/layout only and do not retrain or alter
the scoring model.

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

There is no CI on this repo. For each affected Rust crate, the pre-commit hook in [`.githooks/`](.githooks/) runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` (serially, because API integration tests share SQLite). When a crate's `Cargo.toml`, `Cargo.lock`, or audit configuration changes, it also runs `cargo audit --deny warnings` for that lockfile. See [CONTRIBUTING.md](CONTRIBUTING.md) for the full local quality-gate workflow. Activate once per clone:

```bash
git config core.hooksPath .githooks
```

If a new RUSTSEC advisory blocks a commit, fix the dependency (`cargo update -p <crate>`) or — if it is a transitive warning you cannot fix today — add an explicitly justified ignore to the affected crate's audit configuration. Use `git commit --no-verify` only for emergencies.

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

### Memory & allocator

By default the server uses glibc's malloc, which keeps freed memory pages
in internal free-lists instead of returning them to the kernel. As a result,
RSS in `top` / Grafana stays high (1.3–1.4 GB) even after idle-eviction
clears the in-memory caches.

Build with jemalloc for prompt page return:

```bash
cargo build --release --features jemalloc
```

For even more aggressive releases at runtime, add:

```bash
MALLOC_CONF=dirty_decay_ms:0,muzzy_decay_ms:0 ./target/release/e621-account-parser-api
```

Without jemalloc, the `MALLOC_ARENA_MAX=2` / `MALLOC_TRIM_THRESHOLD_=65536`
env vars cut glibc waste by ~30–50% without a rebuild.

<http://localhost:8080>

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

`parser-web/static/config.js`

```js
window.APP_CONFIG = Object.freeze({
    posts_domain: "https://uri.com",
    backend_domain: "https://uri.com",
});
```

Requires Node.js (for Tailwind CSS + DaisyUI via npm):

```bash
cd ./parser-web/
npm install
```

<http://localhost:8000>

```bash
cd ./parser-web/
trunk serve
```

`npm install` is a one-time setup step. After that, `trunk serve` / `trunk build`
automatically runs the Tailwind CLI (`npx @tailwindcss/cli`) via its
pre-build hook before every compilation. The generated CSS in
`src/tailwind-output.css` is gitignored.

---

## Production deployment

Hosting the app behind nginx (or Caddy — see [`Caddyfile`](Caddyfile))
with release-mode pre-compression is covered separately in
[docs/deployment.md](docs/deployment.md).

---

## License

[MIT-0 (MIT No Attribution)](LICENCE) — use, modify, and redistribute
freely, no attribution required.
