# E621 Feed

A self-hosted personalised feed engine for e621: import favourites, build
per-account preference profiles, and get a scored + diversified feed.

[![Stars](https://img.shields.io/github/stars/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/stargazers)
[![Forks](https://img.shields.io/github/forks/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/network/members)
[![Issues](https://img.shields.io/github/issues/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/e621-Account-Parser/issues)
[![License](https://img.shields.io/github/license/Basedfloppa/e621-Account-Parser?style=flat-square)](https://github.com/Basedfloppa/E621-Account-Parser/blob/master/LICENCE)

## Live

<https://e621feed.zorolin.rs/feed>

---

## About

A self-hosted alternative to e621's built-in feed: import your favourites
once, and the server builds a per-account preference profile that powers a
personalised feed, daily digest, and search.

---

## Features

- **Personalised feed** — 12-channel scoring (tag similarity, quality,
  recency, rating, media, popularity, interaction, tag-relation, uploader,
  exclusivity, novelty, artist-discovery) with MMR diversification
- **Full vs. incremental updates** — first import reconciles everything;
  routine refreshes only pull new favourites and skip the rebuild
- **Recommendation breakdowns** — see *why* a post was recommended
- **Daily digest** — stratified picks (top, trending, exploration, wildcard,
  recent)
- **Search** — server-proxied e621 tag queries with alias suggestions and
  optional per-account result scoring (Wide / Balanced / Strict cutoffs)
- **Post-card feedback menu** — Like, Strong like, Not interested (undoable),
  plus confirmed e621 blacklist rules for a tag, artist, uploader, rating, or
  media category
- **Power-user feed controls** — per-page cutoff presets and persistent
  Auto/3/2/1-column layouts
- **Session-based dedup** — no repeats across infinite-scroll pages
- **Similar posts** — find posts by tag overlap
- **Interactive tag-relation graph** — force-directed visualisation with
  panning and zoom
- **Privacy** — owners can clear all interaction-derived state without losing
  favourites or settings
- **Health probe** — public `GET /api/health`

---

## Quick start

Requirements: [Rust](https://www.rust-lang.org/tools/install), Node.js
(for the frontend build).

### 1. Configure the backend

```bash
cd parser-api
cp config.example.toml config.toml
# edit config.toml: fill in admin_user / admin_api, adjust the rest as needed
```

### 2. Run the backend

```bash
cd parser-api
cargo run --release
```

The API listens on <http://localhost:8080>.

### 3. Run the frontend

```bash
cd parser-web
npm install          # one-time (Tailwind CSS + DaisyUI)
trunk serve          # http://localhost:8000
```

That's it — open <http://localhost:8000>, link your account, and the
first `/process` import builds your profile.

---

## Documentation

| Topic | Where |
|-------|-------|
| All config keys (defaults + explanations) | [`parser-api/config.example.toml`](parser-api/config.example.toml) |
| Production deployment (nginx/Caddy, compression, monitoring) | [docs/deployment.md](docs/deployment.md) |
| Scoring knobs — "lower vs. higher" guide | [docs/scoring.md](docs/scoring.md) |
| Offline calibration (`calibrate eval` / `grid`) | [docs/calibration.md](docs/calibration.md) |
| Technical / development notes (HTTP caching, perf tooling) | [docs/development.md](docs/development.md) |
| Contributing & local quality gate | [CONTRIBUTING.md](CONTRIBUTING.md) |

---

## License

[MIT-0 (MIT No Attribution)](LICENCE) — use, modify, and redistribute
freely, no attribution required.
