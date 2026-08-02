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

---

## Monitoring & Metrics

The backend exposes a Prometheus metrics endpoint at `/api/metrics`.
No external dependencies — uses the pure-Rust `prometheus` crate.

### Available metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `e621_accounts_total` | Gauge | — | Current number of saved accounts |
| `e621_accounts_created_total` | Counter | — | Total accounts created since server start |
| `e621_accounts_deleted_total` | Counter | — | Total accounts deleted since server start |
| `e621_catalog_posts_total` | Gauge | — | Total posts in the local catalog |
| `e621_feed_views_total` | Counter | `account_id` | Feed recommendation views |
| `e621_digest_views_total` | Counter | `account_id` | Daily digest views (cache hit + fresh) |
| `e621_browse_views_total` | Counter | `source` (`trending` / `favorites`) | Browse page views |
| `e621_process_runs_total` | Counter | `status` (`started` / `success` / `failed`) | /process pipeline runs |
| `e621_feed_interactions_total` | Counter | `bucket`, `type` (`qualified_impression` / `open` / `like` / `strong_like` / `hide`) | Per-post feed interactions, split by A/B arm |
| `e621_feed_interactions_by_type_total` | Counter | `type` | Per-post feed interactions by type (legacy, no bucket split) |
| `e621_experiment_bucket_accounts` | Gauge | `bucket` (`control` / `exploration` / …) | Current number of accounts per A/B arm |
| `e621_experiment_bucket_interactions_total` | Counter | `bucket` | Total feed interactions per A/B arm since server start |
| `e621_upstream_errors_total` | Counter | `class` (`429` / `5xx` / `4xx` / `timeout` / `network` / `decode`) | Outbound e621 request failures — feeds the “high % 429/5xx” alert |

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

- If a live request passed within `backfill_live_window_ms` (default 2s),
  backfill adds extra delay proportional to recency
- When `x-ratelimit-remaining` from e621 drops below thresholds, the gate
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
