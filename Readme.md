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
# Grab a config skeleton, then fill in the required fields (see below)
curl -fsSL -o config.toml \
  https://raw.githubusercontent.com/Basedfloppa/E621-Feed/master/parser-api/config.example.toml
# edit config.toml — required before first start:
#   admin_user / admin_api          your e621 username + API key (create the key in
#                                   your e621 account, login required: https://e621.net/api_keys)
#   e621_key_encryption_secret      random string; set it ONCE and never change it
#                                   (it encrypts stored e621 API keys at rest)
#   posts_domain                    must be https://e621.net — an older published skeleton
#                                   still shows a "uri.com" placeholder; fix it if present
#   user_agent contact              replace you@example.com with a reachable address
# Everything else is safe to leave as-is.
touch database.db   # docker only bind-mounts existing files; a missing path becomes a directory
# optional: -v "$PWD/media:/app/media" adds a persistent folder for the offline
# media cache (only used if you enable the [catalog] media settings later)
docker run -d --name e621-feed --restart unless-stopped \
  -p 8181:8181 \
  -v "$PWD/config.toml:/app/config.toml:ro" \
  -v "$PWD/database.db:/app/database.db" \
  -v "$PWD/media:/app/media" \
  ghcr.io/basedfloppa/e621-feed:latest
```

Open <http://localhost:8181>. The container listens on `:8181` (per the
baked `Rocket.toml`); `-p 8181:8181` maps it to the host. To rebuild the
image locally instead of pulling, see [docs/deployment.md](docs/deployment.md).

> **Platform note:** the published image is `linux/amd64`. Docker Desktop on
> Apple Silicon runs it via built-in emulation automatically; on ARM Linux
> (e.g. a Raspberry Pi) it won't start — build from source there.
>
> **Note:** the container runs as root, so on Linux the folders it creates
> inside the bind mounts (e.g. `media/`) end up root-owned — use `sudo` to
> delete or edit them later.

#### Windows (PowerShell)

Same steps with [Docker Desktop](https://www.docker.com/products/docker-desktop/),
in **PowerShell**. The Linux snippet won't work as-is in cmd.exe or PowerShell:
`mkdir -p`, `touch`, `curl -fsSL` and the `\` line continuations are bash-only.

```powershell
New-Item -ItemType Directory -Force -Path "$HOME\e621-feed"
Set-Location "$HOME\e621-feed"
# use curl.exe — plain `curl` in PowerShell is an alias for Invoke-WebRequest
# and does not understand -fsSL / -o
curl.exe -fsSL -o config.toml `
  https://raw.githubusercontent.com/Basedfloppa/E621-Feed/master/parser-api/config.example.toml
# edit config.toml (same required fields as above)
New-Item -ItemType File -Force database.db   # docker only bind-mounts existing files
# optional: -v "$PWD\media:/app/media" adds the persistent offline media cache
docker run -d --name e621-feed --restart unless-stopped `
  -p 8181:8181 `
  -v "$PWD\config.toml:/app/config.toml:ro" `
  -v "$PWD\database.db:/app/database.db" `
  -v "$PWD\media:/app/media" `
  ghcr.io/basedfloppa/e621-feed:latest
```

Open <http://localhost:8181>. Bind mounts on Windows must be absolute paths;
`$PWD\…` resolves to `C:\Users\…\e621-feed\…`, which Docker Desktop accepts.
Stop/start/logs: `docker stop e621-feed`, `docker start e621-feed`,
`docker logs -f e621-feed`.

For **cmd.exe** use the PowerShell commands above with these swaps: `mkdir
%USERPROFILE%\e621-feed` + `cd /d %USERPROFILE%\e621-feed`, `type nul >
database.db`, and run the whole `docker run` on **one line** using
`%CD%\config.toml` / `%CD%\database.db` instead of `$PWD\…`.

### 1. Configure the backend (build from source)

```bash
cd parser-api
cp config.example.toml config.toml
# edit config.toml: fill in admin_user / admin_api, set e621_key_encryption_secret,
# adjust the rest as needed
```

(`cp` works in PowerShell as an alias for `Copy-Item`; in cmd.exe use
`copy config.example.toml config.toml`.)

### 2. Run the backend (build from source)

```bash
cd parser-api
cargo run --release
```

The API listens on <http://localhost:8181>.

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
