# Metrics — reference & how to read

Prometheus metrics are exposed at `/api/metrics` (unauthenticated pool).
All names have the `e621_` prefix. Metrics are lazily registered on first use.
There is also a Loki structured log `SCORING_TRACE` (not a Prometheus metric) — see the end.

> Observability invariant: counters keyed by `account_id`/`route`/`class` carry
> potential cardinality. Per-account counters (`feed_interactions_by_type_total`)
> grow only by `type`, not by id. Histogram buckets are fixed.

## Account metrics

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_accounts_total` | gauge | — | number of known accounts |
| `e621_accounts_created_total` | counter | — | accounts created (link) |
| `e621_accounts_deleted_total` | counter | — | accounts deleted/unlinked |

## Feed views

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_feed_views_total` | counter | — | feed views |
| `e621_digest_views_total` | counter | — | digest views |
| `e621_browse_views_total` | counter | `source`=trending\|favorites | browse page views |

## /process (background profile import)

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_process_runs_total` | counter | `status`=started\|success | /process runs (success = success/started) |
| `e621_process_running_seconds` | gauge | — | age of the current run; >1800s (30 min) — "stuck" (alert) |
| `e621_process_last_duration_seconds` | gauge | — | duration of the last /process |

## Prefetch / breaker

| Metric | Type | Read as |
|---|---|---|
| `e621_prefetch_breaker_trips_total` | counter | times the prefetch circuit breaker opened |

## Interactions / A/B

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_feed_interactions_total` | counter | `bucket`, `type` | interactions by type and bucket |
| `e621_feed_interactions_by_type_total` | counter | `type` | interactions by type (for dedup/types) |
| `e621_experiment_bucket_accounts` | gauge | `bucket` | accounts in each bucket |
| `e621_experiment_bucket_interactions_total` | counter | `bucket` | interactions in each bucket |

## Catalog / cache

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_catalog_posts_total` | gauge | — | posts in the local catalog |
| `e621_cache_pruned_total` | counter | `category` (api_ttl, api_idle, jobs, ratelimit, candidates, revoked, idf, relation) | cache entries pruned by category (rising = cache is working) |

## HTTP layer (via fairing)

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_http_requests_total` | counter | `method`, `route` | requests per route |
| `e621_http_request_duration_seconds` | histogram | `method`, `route` | full response time per route (receive → response) |

## Outbound to e621

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_upstream_errors_total` | counter | `class`=429\|5xx\|4xx\|timeout\|network\|decode | final outbound e621 errors |
| `e621_upstream_request_seconds` | histogram | `class`=success\|429\|… | latency of each outbound e621 attempt |

### `class` values

`success` / `429` / `5xx` / `4xx` / `timeout` / `network` / `decode`.

### How to read a "high % of e621 errors"

Compute the 429/5xx share using the shared attempt counter:

```
sum(rate(e621_upstream_errors_total{class=~"429|5xx"}[5m]))
/ clamp_min(sum(rate(e621_upstream_request_seconds_count[5m])), 1) * 100
```

>20% — intense rate-limiting / e621 failure → alert.

## SCORING_TRACE (Loki, structured log)

Not a Prometheus metric but a per-request recommendation trace — emitted to
stdout as a clean JSON string (`println!`); Loki filter: `|= "\"trace\":\"scoring\""`.
Useful flat fields for `| unwrap`:

- `total_ms` — total assembly/scoring time of the response
- `top_channel` / `top_channel_ms` — which channel component took the most time
- `phase_<name>` — each pipeline phase (flat, for Loki): `phase_db_hydrate`,
  `phase_e621_fetch`, `phase_cache_build`, `phase_scoring`, `phase_diversify_post`.
- `phase_ms` — the same phases as a nested object (for humans/back-compat; Loki cannot read it).
- `endpoint` / `account` / `posts` — request context.

These flat fields feed the `Scoring — phase … (ms)` panels in `docs/grafana-dashboard.json`.

## Alerts

3 rules (legacy in dashboard panels + unified provisioning `docs/grafana-alerting.yml`):

1. `/process` running > 30 min — `e621_process_running_seconds > 1800`
2. prefetch breaker — `rate(e621_prefetch_breaker_trips_total[5m]) > 0`
3. high 429/5xx share — the formula above > 20%
