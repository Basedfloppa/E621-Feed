# E621 Feed

A self-hosted personalized feed engine for e621: import favorites, build
per-account preference profiles, and get a scored + diversified feed.

[![Stars](https://img.shields.io/github/stars/Basedfloppa/E621-Feed?style=flat-square)](https://github.com/Basedfloppa/E621-Feed/stargazers)
[![Forks](https://img.shields.io/github/forks/Basedfloppa/E621-Feed?style=flat-square)](https://github.com/Basedfloppa/E621-Feed/network/members)
[![Issues](https://img.shields.io/github/issues/Basedfloppa/E621-Feed?style=flat-square)](https://github.com/Basedfloppa/E621-Feed/issues)
[![License](https://img.shields.io/github/license/Basedfloppa/E621-Feed?style=flat-square)](https://github.com/Basedfloppa/E621-Feed/blob/master/LICENCE)

## About

A self-hosted alternative to e621's built-in feed: import your favorites
once, and the server builds a per-account preference profile that powers a
personalized feed, daily digest, and search.

<https://e621feed.zorolin.rs/feed>

---

## Features

- **Personalized feed** — 12-channel scoring (tag similarity, quality,
  recency, rating, media, popularity, interaction, tag-relation, uploader,
  exclusivity, novelty, artist-discovery) with MMR diversification
- **Recommendation breakdowns** — see *why* a post was recommended
- **Daily digest** — stratified picks (top, trending, exploration, wildcard,
  recent)
- **Search** — server-proxied e621 tag queries with alias suggestions and
  optional per-account result scoring (Wide / Balanced / Strict cutoffs)
- **Similar posts** — find posts by tag overlap
- **Interactive tag-relation graph** — account data visualisations
- **PWA / offline** — installable as an app (Chromium); works offline with
  cached static assets and your recent data, re-syncs feedback when back
  online, and the installed app refreshes itself in the background

---

## Quick start

Requirements: [Rust](https://www.rust-lang.org/tools/install), Node.js
(for the frontend build).

### 0. Run the published image (Docker, no toolchain needed)

The prebuilt image is published to **GHCR** —
[`ghcr.io/basedfloppa/e621-feed`](https://github.com/Basedfloppa/E621-Feed/pkgs/container/e621-feed)
(`latest` tracks `master`, `v*` tags are published as versions):

```bash
mkdir -p ~/e621-feed && cd ~/e621-feed
# Grab a config skeleton, then fill in admin_user / admin_api (e621 credentials)
curl -fsSL -o config.toml \
  https://raw.githubusercontent.com/Basedfloppa/E621-Feed/master/parser-api/config.example.toml
# edit config.toml …
touch database.db   # docker only bind-mounts existing files; a missing path becomes a directory
docker run -d --name e621-feed --restart unless-stopped \
  -p 8181:8080 \
  -v "$PWD/config.toml:/app/config.toml:ro" \
  -v "$PWD/database.db:/app/database.db" \
  ghcr.io/basedfloppa/e621-feed:latest
```

Open <http://localhost:8181>. The container listens on `:8080` (per the
baked `Rocket.toml`); `-p 8181:8080` maps it to the host. To rebuild the
image locally instead of pulling, see [docs/deployment.md](docs/deployment.md).

### 1. Configure the backend (build from source)

```bash
cd parser-api
cp config.example.toml config.toml
# edit config.toml: fill in admin_user / admin_api, adjust the rest as needed
```

### 2. Run the backend (build from source)

```bash
cd parser-api
cargo run --release
```

The API listens on <http://localhost:8080>.

### 3. Run the frontend (build from source)

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
| Publishing Docker images to GHCR (GitHub Actions workflow) | [.github/workflows/docker-publish.yml](.github/workflows/docker-publish.yml) |
| Scoring knobs — "lower vs. higher" guide | [docs/scoring.md](docs/scoring.md) |
| Offline calibration (`calibrate eval` / `grid`) | [docs/calibration.md](docs/calibration.md) |
| Technical / development notes (HTTP caching, perf tooling) | [docs/development.md](docs/development.md) |
| Contributing & local quality gate | [CONTRIBUTING.md](CONTRIBUTING.md) |

---
