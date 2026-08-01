# Production deployment

Build-time and server-side setup for hosting the app behind nginx with
release-mode pre-compression. For local dev, see the main
[README](../Readme.md).

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

## nginx

The shipped [`nginx-template`](../nginx-template) is a working server
block — replace `domain.com` with the real hostname and the Let's
Encrypt cert paths and drop it into `/etc/nginx/sites-available/<your-site>`.

Two files outside the template need to land on disk before `nginx -t`
will pass:

- **`parser-web/dist/`** — output of `trunk build --release`, served as
  `root` (default path in the template:
  `/var/www/E621-Account-Parser/parser-web/dist`).
- **`parser-web/.well-known/security.txt`** — copy from the shipped
  template and fill in two values:

  ```bash
  cp parser-web/.well-known/security.example.txt parser-web/.well-known/security.txt
  $EDITOR parser-web/.well-known/security.txt          # replace TODO_CONTACT and TODO_EXPIRES
  ```

  The real `security.txt` is **gitignored** so the deploy contact stays
  out of source control. Trunk's `copy-dir` hook then carries it into
  `dist/.well-known/security.txt` on the next build, where nginx serves
  it at `/.well-known/security.txt`. Without the rename, that path
  returns 404 (honest — placeholder content would mislead scanners) and
  `security.example.txt` itself is hidden by an explicit nginx
  `return 404;`.

After symlinking into `sites-enabled`:

```bash
sudo nginx -t              # syntax / paths sanity check
sudo systemctl reload nginx
```

## Compression

Release builds pre-compress every JS / WASM / CSS / HTML / JSON / SVG /
TXT / XML asset over 1 KiB to `.br` and `.gz` siblings. The work happens
in [`parser-web/scripts/compress-dist.sh`](../parser-web/scripts/compress-dist.sh),
wired in as a Trunk `post_build` hook in
[`parser-web/Trunk.toml`](../parser-web/Trunk.toml). The hook is gated on
`TRUNK_PROFILE=release`, so `trunk serve` and the default `trunk build`
stay instant.

**gzip works out of the box.** `gzip_static on;` is already enabled in
`nginx-template` and the standard nginx package on Debian/Ubuntu builds
with `--with-http_gzip_static_module`. WASM compresses ~1.6 MB → ~466 KB
(≈29% of original) at `gzip -9`.

**Brotli is opt-in** — it requires the `ngx_brotli` module that isn't
shipped with stock nginx. Activation is three steps:

1. Install the module on the **server**:

   ```bash
   # Debian/Ubuntu — try the packaged build first
   sudo apt install libnginx-mod-http-brotli-filter libnginx-mod-http-brotli-static
   ```

   If the package is unavailable on your distro, build the dynamic
   module from [google/ngx_brotli](https://github.com/google/ngx_brotli)
   against your installed nginx version and drop
   `ngx_http_brotli_{filter,static}_module.so` into
   `/usr/lib/nginx/modules/`. nginx ≥ 1.25 plus a `load_module` line in
   `/etc/nginx/modules-enabled/` is enough.

2. Uncomment the `brotli_*` block at the bottom of
   [`nginx-template`](../nginx-template) (it's left commented because
   the directives raise `unknown directive "brotli"` until the module is
   loaded).

3. Install the **build-side** CLI on the machine that runs
   `trunk build --release`:

   ```bash
   sudo apt install brotli
   ```

   The pre-compression script soft-fails when `brotli` is missing, so
   omitting this step just means no `.br` files get generated and
   `brotli_static` falls back to on-the-fly `brotli on;` (or to gzip if
   even that's off).

After all three steps, `trunk build --release` writes `*.br` next to
every compressible asset and `nginx -t && systemctl reload nginx`
activates the new directives. Verify with:

```bash
curl -sI -H 'Accept-Encoding: br' https://your.domain/ | grep -i content-encoding
# → content-encoding: br
```

If you ever re-deploy without re-running the pre-compression step, nginx
happily falls back to gzip / on-the-fly — `brotli_static` only serves
what's on disk, it doesn't 404 when the sibling is missing.

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
