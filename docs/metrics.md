# Metrics — reference & how to read

Prometheus metrics are exposed at `/api/metrics` (unauthenticated pool).
All names have the `e621_` prefix. Metrics are lazily registered on first use.
There is also a Loki structured log `SCORING_TRACE` (not a Prometheus metric) — see the end.

> Observability invariant: counters keyed by `route`/`class` carry potential
> cardinality. The interaction counter (`e621_feed_interactions_total`) grows
> only by `type`, not by id; the A/B `bucket` label was removed with the
> mechanism on 2026-10-27. Histogram buckets are fixed.

## Account metrics

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_accounts_total` | gauge | — | number of known accounts |
| `e621_accounts_created_total` | counter | — | accounts created (link) |
| `e621_accounts_deleted_total` | counter | — | accounts deleted/unlinked |
| `e621_active_accounts` | gauge | `window`=1d\|7d\|30d | accounts with a recorded visit inside the window (from `user_visit_tracker`, written by the digest route) |

The account and catalog gauges (`e621_accounts_total`, `e621_active_accounts`,
`e621_catalog_posts_total`) are **re-read from their tables every 60 s** rather
than maintained incrementally by the write paths — see `src/table_gauges.rs`.
None of them *can* be incremental: posts are UPSERTed (no path can tell a new
row from a rewrite), and orphan-account cleanup deletes rows straight from SQL
without passing through the delete handler. Expect up to a minute of lag.

`window="1d"` reads as *today or yesterday*: `user_visit_tracker.last_visit_date`
stores a bare date, so a sub-day window cannot be expressed from it. Accounts
that have never visited have no row in that table and are not counted.

## Feed views

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_feed_views_total` | counter | — | feed views |
| `e621_digest_views_total` | counter | — | digest views |
| `e621_browse_views_total` | counter | `source`=search\|search_scored\|trending\|trending_scored\|favorites | browse page views, one per served request (`source` = endpoint) |

## /process (background profile import)

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_process_runs_total` | counter | `status`=started\|success\|failed | /process runs (success = success/started) |
| `e621_process_running_seconds` | gauge | — | age of the current run; >1800s (30 min) — "stuck" (alert) |
| `e621_process_last_duration_seconds` | gauge | — | duration of the last /process |

## Prefetch / breaker

| Metric | Type | Read as |
|---|---|---|
| `e621_prefetch_breaker_trips_total` | counter | times the prefetch circuit breaker opened |

## Interactions

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_feed_interactions_total` | counter | `type` | interactions by type (`qualified_impression` / `open` / `like` / `strong_like` / `hide`) |

> **Both write paths move this counter.** The WASM client batches every
> interaction (`interaction_queue` → `POST /api/interaction/batch`), which is the
> route real traffic uses; only Undo calls the single one. It used to be counted
> on the single route alone, so `e621_feed_interactions_total` stayed unpopulated
> while `feed_interactions` filled up — the interaction dashboard panels read empty
> on a busy instance. Counts now follow the rows the DB reports as inserted, so a
> retried batch is not double-counted.

## Catalog / cache

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_catalog_posts_total` | gauge | — | posts in the local catalog |
| `e621_cache_pruned_total` | counter | `category` (api_ttl, api_idle, jobs, ratelimit, candidates, revoked, shown_posts, idf, relation) | cache entries pruned by category (rising = cache is working); `shown_posts` counts expired sightings from `feed_shown_posts` |

## HTTP layer (via fairing)

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_http_requests_total` | counter | `method`, `route` | requests per route |
| `e621_http_request_duration_seconds` | histogram | `method`, `route` | full response time per route (receive → response) |

## Outbound to e621

| Metric | Type | Labels | Read as |
|---|---|---|---|
| `e621_upstream_errors_total` | counter | `class`=429\|5xx\|4xx\|timeout\|network\|decode | final outbound e621 errors |
| `e621_upstream_request_seconds` | histogram | `class`=success\|304\|429\|… | latency of each outbound e621 attempt |

### `class` values

`success` / `304` / `429` / `5xx` / `4xx` / `timeout` / `network` / `decode`.

`304` is a *successful* conditional revalidation (`If-None-Match` against a
cached body): it belongs on the latency histogram but is deliberately **not**
passed to `e621_upstream_errors_total`. Before that split every 304 fell into
`4xx` — one seed run reported 61 "upstream failures" that were all cache
revalidations, which skewed both the error counter and the `[AUDIT-ERR]
e621.failed` stream.

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
  `phase_e621_fetch`, `phase_cache_build`, `phase_scoring`, `phase_diversify_post`,
  `phase_build` (digest assembly).
- `phase_ms` — the same phases as a nested object (for humans/back-compat; Loki cannot read it).
- `endpoint` / `account` / `posts` — request context.

These flat fields feed the `Scoring — phase … (ms)` panels in `docs/grafana-dashboard.json`.

## Alerts

3 rules (legacy in dashboard panels + unified provisioning `docs/grafana-alerting.yml`):

1. `/process` running > 30 min — `e621_process_running_seconds > 1800`
2. prefetch breaker — `rate(e621_prefetch_breaker_trips_total[5m]) > 0`
3. high 429/5xx share — the formula above > 20%
