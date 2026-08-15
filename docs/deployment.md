# Production deployment

Build-time and server-side setup for hosting the app. The frontend
(`parser-web`) is **embedded into the API binary** at build time
(`parser-api/build.rs` → `parser-api/src/serve_embedded.rs`), so a single
binary serves both the SPA (`/`) and the API (`/api`). A reverse proxy
(Caddy or nginx) is only needed for TLS/Let's Encrypt. For local dev, see
the main [README](../Readme.md).

## Build-time tools

In addition to the regular toolchain (Rust, `trunk`), the release build
shells out to `brotli` and `gzip` for the pre-compression hook:

```bash
sudo apt install brotli gzip
```

Both are gated behind `TRUNK_PROFILE=release` and soft-fail (the hook
logs and skips) if a binary is missing — `trunk serve` and the default
`trunk build` work without them.

Trunk also needs `wasm-bindgen` on PATH, matching the version pinned in
`parser-web/Cargo.lock` (trunk can auto-install it, but the Dockerfile
pins it explicitly). Keep the pinned versions in sync:

| Tool | Pinned version | Where |
|---|---|---|
| rustc | 1.96.0 | [`rust-toolchain.toml`](../rust-toolchain.toml) + `FROM rust:1.96.0-slim-trixie` in the Dockerfile |
| trunk | 0.21.14 | Dockerfile |
| wasm-bindgen-cli | 0.2.126 | Dockerfile (must match `parser-web/Cargo.lock`) |

## Frontend asset integrity (SRI) & reproducible builds

The SPA index embeds SRI `integrity="sha384-…"` attributes for every hashed
bundle (js/wasm/css/snippet). Trunk names those files with a **weak 64-bit
hash of the wasm only** (`seahash`) and computes each file's SRI separately,
so two builds can reuse the **same URL with different bytes** — e.g. after a
wasm-bindgen / rustc upgrade the JS glue changes while the wasm (and thus
the filename) stays identical. Combined with the SW's cache-first static
cache and the `immutable` 1-year Cache-Control on hashed assets, a stale
copy gets served against a fresh index.html and the browser fails loudly:

```
None of the "sha384" hashes in the integrity attribute match the content
of the subresource at …e621-account-parser-web-<hash>.js
```

What keeps this from happening (and self-heals it when it does):

1. **Reproducible builds** — commit `parser-web/Cargo.lock`,
   `parser-web/package-lock.json` and `parser-api/Cargo.lock` (they are
   intentionally **not** gitignored; `*.lock` was removed from
   `.gitignore`), pin rustc via [`rust-toolchain.toml`](../rust-toolchain.toml)
   and the exact `rust:…` image tag, and keep trunk/wasm-bindgen pinned.
   Identical inputs then produce identical wasm **and** JS, so a reused URL
   always carries the same bytes. A floating `rust:slim-trixie` tag or an
   unpinned Cargo.lock silently breaks this.
2. **Self-healing service worker** — `static/sw.js` verifies every cached
   response against the request's `integrity` attribute before serving it;
   on mismatch it drops the entry and re-fetches bypassing the HTTP cache
   (and only re-caches bytes that pass SRI). So even if a URL is ever
   reused with different bytes, clients recover on the next load instead of
   staying stuck.
3. **Cache version bump on deploy** — bump `CACHE_VERSION` in
   `parser-web/static/sw.js` whenever the frontend changes. The SW purge
   runs on activation, so stale `e621-static-vN` entries (including any
   poisoned ones) are dropped. `sw.js` itself is served `no-cache` and
   browsers bypass the HTTP cache for SW updates, so the bump reaches
   clients automatically.

After shipping these fixes, clients that were stuck on stale cached bytes
self-heal on their next load once the new SW (v3) activates; a one-off hard
refresh is only needed if a browser never re-fetched `sw.js`.

## Memory allocator

The server keeps in-memory caches (IDF, tag-relation graph, e621 API
responses) that can total 1.3–1.4 GB RSS under load. Idle-eviction clears
the Rust data structures, but glibc's default malloc holds freed pages
in internal free-lists rather than returning them to the kernel.

**Build the server binary with jemalloc** for prompt RSS release:

```bash
cargo build --release --bin e621-account-parser-api --features jemalloc
```

For even more aggressive page return at runtime, add:

```bash
MALLOC_CONF=dirty_decay_ms:0,muzzy_decay_ms:0 ./target/release/e621-account-parser-api
```

Without jemalloc, `MALLOC_ARENA_MAX=2 MALLOC_TRIM_THRESHOLD_=65536` env
vars cut glibc waste by ~30–50% without a rebuild.

> **Where the memory actually goes**: a measured per-structure breakdown
> (IDF, tag-relation graph hot vs. frozen, scoring) lives in
> [`memory-profile.md`](memory-profile.md). At current catalog size the
> structures themselves are ~100 MB — the rest of the 1.3–1.4 GB is the
> e621 API response cache, SQLite page cache, and runtime.
> SQLite concurrency behaviour under load is covered separately in
> [`load-testing.md`](load-testing.md).

## Building the single binary (Docker, recommended)

> **Requires BuildKit.** The [`parser-api/Dockerfile`](../parser-api/Dockerfile)
> uses `--mount=type=cache` for persistent layer caches (cargo registry/target,
> npm). BuildKit ships with Docker ≥ 23. On older setups (or where the `docker
> buildx` plugin is missing), install it with the bundled cross-distro script
> (covers Arch-based `pacman` and `apt`/dnf-based distros, with a static-binary
> fallback):
>
> ```bash
> ./install-buildx.sh          # idempotent; --force to reinstall
> docker buildx version
> ```
>
> The script detects the distro: Arch/manjaro/cachyos/… use `pacman -S
> docker-buildx` (official repos), Debian/Ubuntu add the Docker apt repo and
> install `docker-buildx-plugin`, unknown distros download a static binary into
> `~/.docker/cli-plugins`. Manual fallback if you prefer:
>
> ```bash
> mkdir -p ~/.docker/cli-plugins
> cp buildx-v<VER>.linux-amd64 ~/.docker/cli-plugins/docker-buildx
> chmod +x ~/.docker/cli-plugins/docker-buildx
> docker buildx version
> ```
>
> Verify the caches persist between builds with
> `docker buildx build --load -t smoke .` run twice — the second run should
> show `CACHED` layers instead of rebuilding trunk / deps.

The repo ships a multi-stage [`parser-api/Dockerfile`](../parser-api/Dockerfile)
plus [`docker-compose.yml`](../parser-api/docker-compose.yml) that:

1. builds the **frontend** (`parser-web`) to WASM with `trunk build --release`
   (Node for the Tailwind hook, Rust WASM toolchain + `trunk`). Pinned
   toolchain (`cargo install trunk`) and deps are preserved in persistent
   BuildKit cache mounts, so unchanged sources don't rebuild;
2. builds the **backend** in release and embeds the compiled frontend
   (`dist`) into the binary via `build.rs`;
3. produces a single runtime image containing only the API binary (the
   embedded frontend is inside it).

```bash
cd parser-api
docker compose build app                  # or: app-jemalloc / app-perf / app-full
docker compose up -d app
```

The binary listens on the port from its `Rocket.toml` (default `:8080`) and
serves the SPA at `/`, static assets under their hashed paths, and the API
under `/api`. `docker-compose.yml` maps a host port (e.g. `8181`) to the
container.

> **The runtime image boots out of the box.** The Dockerfile bakes
> `config.example.toml` → `/app/config.toml` (placeholder e621 creds) and
> `Rocket.toml` → `/app/Rocket.toml` (binds `0.0.0.0:8080`) into the runtime
> image, so a bare `docker run ghcr.io/<owner>/e621-feed` starts and
> serves the embedded SPA even without mounts. These defaults exist because
> the binary FATALs at startup without a parseable `config.toml` (the config
> `LazyLock` exits on first access), and Rocket's own default bind is
> `127.0.0.1:8000`. docker-compose mounts the real `config.toml` /
> `Rocket.toml` / `database.db` **over** the baked files at runtime (bind
> mounts fully shadow image content), so production behaviour is unchanged.
>
> Startup is also safe against a **fresh/empty database**: the DB (migrations
>
> + WAL switch) is initialized before any background worker is spawned, and
> the writer connection sets `busy_timeout` *before* the `journal_mode = WAL`
> switch — so a concurrently-starting worker holding a shared lock makes the
> WAL switch wait instead of failing with a "database is locked" panic.

### Local build (no Docker)

```bash
cd parser-web && trunk build --release    # produces parser-web/dist
cd ../parser-api && cargo build --release --bin e621-account-parser-api
```

`build.rs` embeds `../parser-web/dist`; if it's missing the binary still
builds but serves `/` with a "Frontend not embedded" message.

## Reverse proxy (nginx or Caddy)

The shipped [`nginx-template`](../nginx-template) is now a minimal TLS
terminator + reverse proxy: it forwards **everything** (SPA, static assets,
`/api`) to the API binary on `:8080`. Replace `domain.com` with the real
hostname, swap the Let's Encrypt cert paths, drop it into
`/etc/nginx/sites-available/<your-site>`, then:

```bash
sudo nginx -t              # syntax / paths sanity check
sudo systemctl reload nginx
```

No web root is served by nginx any more — the binary owns all static files
and sets its own Cache-Control / Content-Type headers (mirroring the old
nginx rules: hashed assets + `.wasm` immutable, `config.js` no-store,
`index.html` no-cache).

The repo also ships a [`Caddyfile`](../Caddyfile) with the same idea — TLS
(Let's Encrypt auto) + a clean reverse proxy to the binary. Drop in your
domain and it works; Caddy handles certificate issuance itself:

```caddy
// point reverse_proxy at wherever the binary listens (e.g. :8080)
reverse_proxy 127.0.0.1:8080
```

In both cases do **not** serve `parser-web/dist` as a static root any more —
the binary owns it.

## Runtime frontend config (`static/config.js`)

The SPA loads a tiny mutable config script **before** the WASM bundle
(`parser-web/index.html`):

```html
<script src="./static/config.js"></script>
```

It sets `window.APP_CONFIG` (read by the frontend via
`read_config_from_head()`):

```js
window.APP_CONFIG = Object.freeze({
    posts_domain: "https://e621.net",  // base for client-side e621 links (post/artist/user/tag pages)
    backend_domain: "/api",            // backend API base — keep same-origin
});
```

**This file is required — the frontend cannot boot without it.** If it is
missing or unreachable, the home page shows a hard error ("App configuration
failed to load… check that /static/config.js is reachable"). Make sure your
reverse proxy forwards `/static/config.js` to the binary and does not block
or rewrite it.

Key deployment properties:

+ **Baked into every image** — `parser-web/static/config.js` is committed to
  the repo, so the Docker build (including CI) always serves the default
  (`e621.net` + `/api`) out of the box; the frontend boots with no extra
  setup. Override it only when you need a different `posts_domain`.
+ **Mutable, never cached** — the binary serves `static/config.js` with
  `no-store, no-cache` (hashed wasm/css/js assets are `immutable` instead),
  so a changed value is picked up on the next reload. For the same reason it
  deliberately carries **no SRI `integrity` attribute** in `index.html`.
+ **Embedded at build time** — the file is baked into the binary via
  rust-embed, so a Docker bind-mount over `/app/static/config.js` does **not**
  override what the binary serves. To change it, either
  1. serve an override from your proxy before the backend pass, e.g. nginx
     `location = /static/config.js { alias /path/to/config.js; }` (an exact
     match wins over the generic `location /` proxy);
  2. or rebuild the frontend with a custom `parser-web/static/config.js` — it
     flows into `dist/` and the image.
+ **`backend_domain` must stay same-origin (`/api`)** — session auth relies
  on `SameSite=Lax` cookies; the proxy forwards `/api/*` to the binary (see
  Reverse proxy above). A cross-origin `backend_domain` breaks auth unless
  you also configure CORS on the API.
+ **`posts_domain`** is the base for **client-side** links to e621 pages
  (open post/artist/user/tag links in the browser). It is independent of the
  server-side `posts_domain` in `config.toml`, which workers use to fetch
  data — set both when pointing at a mirror instance.

### Overriding it in Docker / docker-compose

The binary serves `config.js` from memory (embedded at build time), so a
bind mount has **no effect** — do not try
`./config.js:/app/static/config.js:ro` in compose, the container will still
serve the baked file. Override it at build time instead:

+ **Patch the Dockerfile (recommended)** — add a build arg that rewrites the
  file before `trunk build`, right after `COPY parser-web/static ./static`:

  ```dockerfile
  ARG POSTS_DOMAIN=https://e621.net
  RUN printf 'window.APP_CONFIG = Object.freeze({ posts_domain: "%s", backend_domain: "/api" });\n' \
      "$POSTS_DOMAIN" > static/config.js
  ```

  then build with `docker build --build-arg POSTS_DOMAIN=https://e926.net …`.
+ **Custom `parser-web/static/config.js` in your fork** — the existing
  `COPY parser-web/static ./static` picks it up automatically, no Dockerfile
  changes needed.
+ **Proxy override** — if your stack has nginx/Caddy in front of the binary
  (not the stock compose, which maps `8181:8080` directly), serve the file
  before the backend pass: nginx `location = /static/config.js { alias
  /path/to/config.js; }`.

The stock docker-compose has no proxy, so for it only the first two options
apply.

## Compression

**gzip for `/api` JSON and the SPA** is enabled on the reverse proxy
(on-the-fly), since the embedded binary serves raw bytes. With nginx:

```nginx
# inside the proxy server block
gzip on;
gzip_vary on;
gzip_min_length 1024;
gzip_types text/html text/css text/plain application/wasm
        application/javascript application/json image/svg+xml
        application/octet-stream;
```

The old `compress-dist.sh` still runs on `trunk build --release` and writes
`.br`/`.gz` siblings into `dist`, but **`parser-api/build.rs` deliberately
excludes those siblings** (they'd double binary size), so they're not
embedded. If you need Brotli at the edge, enable `ngx_brotli` and turn on
`brotli on;` on the proxy. With Caddy, add `encode zstd gzip` to the site
block for the same effect (Caddy handles Vary/Accept-Encoding for you).

> **Note on ports:** the binary binds to the port in its `Rocket.toml`
> (default `:8080`). Point the reverse proxy there. The `docker-compose.yml`
> maps a host port to the container — align both if you run Caddy/nginx on
> the host alongside the container.

## Publishing the image (GitHub Actions)

[`.github/workflows/docker-publish.yml`](../.github/workflows/docker-publish.yml)
builds the image with BuildKit on GitHub runners and publishes it to **GHCR**
(`ghcr.io/<owner>/e621-feed`):

+ **push to `master`** → `latest` + `sha-<commit>`;
+ **`v*` git tag** → `v1.2.3`, `1.2.3`, `1.2`, `sha-<commit>`;
+ **manual dispatch** → `sha-<commit>` plus optional `tag` and `features`
  inputs (e.g. `jemalloc`).

Build context is the repo root (same as docker-compose's `context: ..`), so
the root `.dockerignore` applies (it excludes the 71 GB `database.db`). The
`type=gha` BuildKit cache keeps the cargo/trunk layers warm between runs.

After pushing, the workflow **smoke-tests the image**: it runs the container
and requires `GET /` to answer `200` (which also catches the "Frontend not
embedded" 503 regression). With the baked boot defaults above, the
container comes up on a fresh database without external mounts.

All third-party actions are pinned to commit SHAs; the workflow passes
`actionlint` and `zizmor` with no findings. To also publish to Docker Hub,
add a `docker/login-action` step for it and append
`docker.io/<user>/e621-feed` to the metadata `images` list.

---

## Monitoring & Metrics

The backend exposes a Prometheus metrics endpoint at `/api/metrics`.
No external dependencies — uses the pure-Rust `prometheus` crate.

### Available metrics

The full metric reference (names, types, labels, how to read them, alerts,
`SCORING_TRACE`) lives in **`docs/metrics.md`** — the canonical source.
Only operational deployment context is kept here.

### Upstream e621 error stream

Every terminal outbound e621 failure (retries exhausted or a non-retryable
status) emits one audit line to **stderr** with the `[AUDIT-ERR] e621.failed`
tag and bumps `e621_upstream_errors_total`. Grep it to watch upstream health
without wading through `warn!` noise:

```bash
# live tail of just the e621 failure stream:
journalctl -u e621-parser-api -f | grep 'e621.failed'
# …or under Docker:
docker compose logs -f app | grep 'e621.failed'
# high 429 share in the last 15 min (PromQL):
sum(rate(e621_upstream_errors_total{class="429"}[15m])) / clamp_min(sum(rate(e621_upstream_errors_total[15m])), 0.001)

### Quick check

```bash
curl http://localhost:8080/api/metrics | head -20
```

Example output:

```prometheus
# HELP e621_accounts_total Current number of saved accounts
# TYPE e621_accounts_total gauge
e621_accounts_total 12
# HELP e621_feed_views_total Feed recommendation views
# TYPE e621_feed_views_total counter
e621_feed_views_total{account_id="658288"} 128
# HELP e621_catalog_posts_total Total posts in local catalog
# TYPE e621_catalog_posts_total gauge
e621_catalog_posts_total 1607
```

### Prometheus scrape config

Add to your `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: e621-feed
    static_configs:
      - targets:
          - host.docker.internal:8080   # if Prometheus is in Docker
          # - 192.168.1.50:8080         # or the host IP directly
    metrics_path: '/api/metrics'
```

Reload Prometheus after adding the job:

```bash
curl -X POST http://localhost:9090/-/reload
```

### Grafana dashboard

A pre-built dashboard is available at
[`docs/grafana-dashboard.json`](grafana-dashboard.json).
Import it in Grafana:

1. Open Grafana → **Dashboards** → **New** → **Import**
2. Upload `docs/grafana-dashboard.json`
3. Select the `e621-feed` Prometheus data source
4. Click **Import**

The dashboard includes panels for:

| Panel | Type | Source |
|-------|------|-------|
| DAU / WAU / MAU | Stats | `e621_feed_views_total`, `e621_digest_views_total`, `e621_browse_views_total` |
| Total Accounts / Posts Created | Stats | `e621_accounts_total`, `e621_accounts_created_total` |
| Feature Usage (Feed / Digest / Browse) | Time series | `e621_feed_views_total`, `e621_digest_views_total`, `e621_browse_views_total` |
| Process Success Rate + Runs | Gauge + Time series | `e621_process_runs_total` |
| Catalog Growth | Time series | `e621_catalog_posts_total` |
| Feed Interactions | Time series (stacked) | `e621_feed_interactions_total` by type |
| A/B — Accounts per Bucket | Stat | `e621_experiment_bucket_accounts` |
| A/B — Interactions per Bucket (24h) | Stat | `e621_experiment_bucket_interactions_total` |
| A/B — Engagement Rate | Time series | `e621_feed_interactions_total` (positive / impression, by bucket) |
| A/B — Hide Rate | Time series | `e621_feed_interactions_total` (hide / impression, by bucket) |
| A/B — Interactions by Type & Bucket | Time series (stacked) | `e621_feed_interactions_total` by bucket + type |
| Top-10 Accounts per Feature | Bar gauges | Per-account aggregation |
| HTTP Response Time (p50/p95/p99) | Time series | `e621_http_request_duration_seconds` (histogram_quantile) |
| e621 Upstream Latency (p50/p95/p99) | Time series | `e621_upstream_request_seconds` (histogram_quantile) |
| HTTP p95 by Route | Time series | `e621_http_request_duration_seconds` topk by route |
| e621 Upstream p95 by Class | Time series | `e621_upstream_request_seconds` by class |
| HTTP Request Rate by Route | Time series (stacked) | `e621_http_requests_total` by route |

### Latency & performance

The two latency histograms are the primary tool for diagnosing “the response
takes too long.” **Upstream latency** (`e621_upstream_request_seconds`) is how
long e621 itself takes to answer; **total handling time**
(`e621_http_request_duration_seconds`) is the whole response time per route —
it includes the e621 call, local scoring, and serialization. Their difference
is roughly the local processing cost.

Find the slowest routes (p95 across all of them):

```promql
# p95 total handling time, top routes
sort_desc(topk(8, histogram_quantile(0.95, sum by (le, route) (rate(e621_http_request_duration_seconds_bucket[5m])))))

# p95 e621 latency, by outcome class
histogram_quantile(0.95, sum by (le, class) (rate(e621_upstream_request_seconds_bucket[5m])))
```

Quick live check:

```bash
curl http://localhost:8080/api/metrics | grep -E "upstream_request_seconds|http_request_duration_seconds|http_requests_total"
```

Both histograms are **always-on** (not gated behind a compile-time feature):
they use a small, fixed bucket set (~5 ms … 300 s) so the scrape overhead is
negligible. The pre-built dashboard (`docs/grafana-dashboard.json`) includes a
latency section with p50/p95/p99 timeseries, the slowest-route and
slowest-class breakdowns, and a per-route request-rate panel.

### Scoring trace (per-request structured log)

To see *which scoring element* dominates a slow recommendations/digest request
there is a per-request structured trace. It is **always-on by default** — the
`perf_metrics` compile-time feature is now part of `default` in
`parser-api/Cargo.toml` (overhead ≈ 100 µs per request for a low-traffic
self-host; disable by building with `--no-default-features` if you ever need
it).

For every non-empty scoring pass the backend logs **one** `info!` line as a
single pure-JSON object (no prefix, so Grafana/Loki can `| json` it directly;
identifiable by the `"trace":"scoring"` field):

```json
{"trace":"scoring","endpoint":"recommendations","account":42,"posts":200,
 "total_ms":318000,"top_channel":"tag_relation_fit","top_channel_ms":312000,
 "channel_ms":{"tag_similarity":…,"tag_relation_fit":…,"novelty":…,"artist_discovery_fit":…,…},
 "phase_ms":{"db_hydrate":…,"e621_fetch":…,"cache_build":…,"scoring":…,"diversify_post":…}}
```

+ `endpoint` is `recommendations`, `digest_personalized`, or `digest_generic`.
+ `posts` is the number of scored posts; `total_ms` the summed pipeline phases.
+ `channel_ms.*` is the cumulative time each scoring channel spent (in ms).
  Whichever field is largest is the element to investigate.
+ `top_channel` / `top_channel_ms` repeat the biggest channel as flat fields so
  a Loki panel can `| unwrap` them directly.
+ `phase_ms.*` splits the pipeline into DB hydration / e621 fetch / cache build
  / scoring / diversify.

The recommendations trace is emitted from `build_recommendations_shared`
(`routes/feed.rs`); digest emits from both `build_personalized_digest` and
`build_generic_digest` (`routes/digest.rs`). Source of truth for the format and
emitter is `ScoringMetrics::emit_json` in `utils/scorer/metrics.rs`.

The shipped dashboard (`docs/grafana-dashboard.json`, uid `adrwh67` in this
deployment) already includes three Loki panels for this log: a timeseries of
`total_ms` by endpoint, a timeseries of the bottleneck channel
(`top_channel_ms` by `top_channel`), and a raw trace logs panel. They query the
`loki` datasource (uid `dfkfplmnysw74e`); the container selector
`container=~"e621.*|.*feed.*|.*parser.*"` may need adjusting to the backend
container's actual name. They are empty until the backend actually emits
traces and Alloy forwards stdout to Loki:

```logql
{container=~"e621.*|.*feed.*|.*parser.*"} |= "\"trace\":\"scoring\"" | json | trace="scoring"
```

### A/B experiment tracking

Accounts are deterministically assigned to an experiment arm (bucket) by
hashing their account id against the configured `[buckets.*]` tables in
`config.toml` (e.g. `control` with the default priors, `exploration` with
altered mix weights). The assignment is stable per account, visible on the
`/settings` page, and every recorded feed interaction is tagged with the
arm the user actually saw.

To compare which arm produces a *better* feed, build PromQL ratios per
bucket. **Engagement rate** (positive actions per impression — higher is
better):

```promql
sum(rate(e621_feed_interactions_total{type=~"open|like|strong_like"}[5m])) by (bucket)
/
clamp_min(sum(rate(e621_feed_interactions_total{type="qualified_impression"}[5m])) by (bucket), 0.001)
```

**Hide rate** (misses per impression — lower is better):

```promql
sum(rate(e621_feed_interactions_total{type="hide"}[5m])) by (bucket)
/
clamp_min(sum(rate(e621_feed_interactions_total{type="qualified_impression"}[5m])) by (bucket), 0.001)
```

Wait until both arms have accumulated enough interactions (at least a few
hundred impressions each) before concluding one arm wins — small samples
are noisy. `e621_experiment_bucket_interactions_total` and
`e621_experiment_bucket_accounts` show how much data each arm has.

Create a new dashboard in Grafana using the `e621-feed` Prometheus data
source. Useful panels:

**DAU (Daily Active Users)** — PromQL:

```promql
count by (account_id) (
  sum(rate(e621_feed_views_total[24h])) > 0
    or sum(rate(e621_digest_views_total[24h])) > 0
    or sum(rate(e621_browse_views_total[24h])) > 0
)
```

**Feature usage breakdown** — PromQL:

```promql
sum(rate(e621_feed_views_total[24h]))  # Feed
sum(rate(e621_digest_views_total[24h]))  # Digest
sum(rate(e621_browse_views_total[24h]))  # Browse
```

**Process success rate** — PromQL:

```promql
sum(rate(e621_process_runs_total{status="success"}[7d]))
/
sum(rate(e621_process_runs_total{status="started"}[7d]))
```

**Catalog growth** — PromQL:

```promql
e621_catalog_posts_total
```

## Catalog bootstrap (db_exports)

A fresh install starts with an empty catalog that grows only via the live
e621 API. To speed up the *global taxonomy*, run the offline `catalog-seed`
binary once against e621's weekly full-dump CSVs
(`https://e621.net/db_exports`):

+ `tags.csv.gz`             → `tags` table (name + category→`group_type`);
+ `tag_aliases.csv.gz`      → `tag_aliases`;
+ `tag_implications.csv.gz` → `tag_implications`.

The dumps are **not** used to fill posts: they carry no media URLs (only
`md5`), so every thumbnail/sample would still need a per-post API fetch by
the media-hydrator — no faster than the normal incremental prefetch. Posts
and media therefore accumulate through the usual `/process`, prefetch, and
media-hydrator requests (`Priority::Prefetch` / `Priority::Backfill` below),
which is the intended steady-state flow of fresh data.

```bash
# download the three dumps into ./db_exports (if missing), then ingest
cd parser-api && cargo run --release --bin catalog-seed

# reuse dumps you already downloaded
cargo run --release --bin catalog-seed -- --dir /path/to/dumps --skip-download
```

Considerations:

+ **Stop the main server first.** The seed grabs the single SQLite writer for
  the whole run; running it against a live DB will block writes. It is safe
  to run against a throwaway DB and then start the server on the result.
+ **Idempotent.** All writes are upserts, so re-runs are safe (useful for
  resuming after an interruption or refreshing taxonomy weekly).
+ The account layer (users, favourites, votes, interactions) is never
  touched — it stays API-only.

## Background workers

The server spawns three background workers that share the e621 API adaptive rate gate:

### Hot / Cold prefetcher

Warms the local catalog by fetching top-artist / top-character posts for
recently active accounts. The **hot** worker runs every 3 minutes (accounts
active within 48 hours), the **cold** worker runs every 15 minutes
(accounts active within 14 days but outside the hot window). Both use
`Priority::Prefetch` through the adaptive rate gate (base delay 500 ms).

### Backfill worker

Runs every 6 hours (configurable via `backfill_interval_secs`). For each
account that hasn't been backfilled recently, it iterates over **all**
preferred tags (not just the top-N) and fetches:

1. **Retro posts** — the last available page (page=9999, oldest posts)
2. **Fresh posts** — page 1 (newest posts)

Uses `Priority::Backfill` through the adaptive rate gate (base delay
750 ms). Automatically yields to live user traffic:

+ If a live request passed within `backfill_live_window_ms` (default 2s),
  backfill adds extra delay proportional to recency
+ When `x-ratelimit-remaining` from e621 drops below thresholds, the gate
  increases delays: 2× at < 200 remaining, 3× at < 100, 5× at < 50

The backfill has its own circuit breaker (`backfill_breaker_threshold`,
default 10 consecutive failures) separate from the hot/cold prefetcher's
breaker.

### Security note

The `/api/metrics` endpoint exposes `account_id` as label values. The
endpoint is intentionally unauthenticated so Prometheus can scrape it
without credentials. In production, restrict access via nginx:

```nginx
location = /api/metrics {
    allow 127.0.0.1;
    allow 10.0.0.0/8;      # docker network
    deny all;
    proxy_pass http://backend:8080;
}
```

Or with Caddy:

```caddy
@metrics {
    path /api/metrics
    remote_ip 127.0.0.1 10.0.0.0/8
}
handle @metrics {
    reverse_proxy localhost:8080
}
```
