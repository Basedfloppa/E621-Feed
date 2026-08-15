# Development notes

Technical details for developers working on the codebase. User-facing
setup lives in the [README](../Readme.md); production deployment in
[deployment.md](deployment.md).

## HTTP caching — `GET /api/account/<id>/tag_relations`

The endpoint returns an `ETag` derived from the response body and
`Cache-Control: private, max-age=60`. Clients sending `If-None-Match`
with the matching ETag get a `304 Not Modified` (no body). Combined with
the per-user nature of the data, this means:

- Browsers cache the graph for up to a minute without re-asking the server.
- After that, a conditional request validates with the server; if nothing
  has changed, only headers travel back.
- Shared caches (CDN/proxy) are explicitly excluded by `private`.

The endpoint isn't listed in the OpenAPI/Swagger output (it returns a
custom ETag-aware responder rather than the standard `Json<T>`), but it
is reachable at `/api/account/<id>/tag_relations` exactly like before.

## Memory & allocator

Production memory behaviour (jemalloc, RSS release, where the 1.3–1.4 GB
goes) is covered in [deployment.md](deployment.md) and
[memory-profile.md](memory-profile.md).

## Performance tooling

Three companion guides cover where the server spends time and memory,
with repeatable harnesses:

- **[`memory-profile.md`](memory-profile.md)** — measured per-structure
  RSS breakdown (IDF ~21 MB, tag-relation graph ~22 MB frozen @ 12 B/pair
  at current catalog size). Run it with:

  ```bash
  cd parser-api && cargo run --release --features jemalloc --example memory_profile
  ```

- **[`load-testing.md`](load-testing.md)** — SQLite behaviour under
  concurrent read/write load (locust against the real DB, workers
  disabled). Shows the dedicated-writer design is error-free under
  25+ parallel writers, and that recommendations are the real bottleneck:

  ```bash
  cd parser-api/loadtest && ROCKET_PORT=8088 ../target/release/e621-account-parser-api
  locust -f locustfile.py --host http://127.0.0.1:8088 --headless -u 50 -t 2m --only-summary
  ```

- **`cargo bench --bench scoring`** — criterion benchmark of the scoring
  hot path (`ScoringContext::score` ≈ 3.5 µs/post on realistic data);
  results under `parser-api/target/criterion/`.

## Offline taxonomy seed — `catalog-seed`

Bootstrapping the global taxonomy (tags / aliases / implications) from e621's
weekly `db_exports` dumps is an **operational** task — the reasoning,
setup and runbook live in [deployment.md](deployment.md) under “Catalog
bootstrap (db_exports)”.

Development notes on the seed internals: `catalog-seed` streams each gz CSV
into a bounded batch and issues idempotent upserts
(`db::tags::upsert_catalog_tags`, `db::save_tag_aliases` /
`db::save_tag_implications`). It deliberately does **not** import posts — the
dumps carry no media URLs — so posts/media are left to the incremental
prefetch / `/process` / media-hydrator path.

## Scoring & calibration internals

- Per-knob "lower vs. higher" guide for every scoring variable:
  [scoring.md](scoring.md)
- Offline calibration (`calibrate eval` / `calibrate grid`, holdout
  artifacts, A/B experiment buckets): [calibration.md](calibration.md)
- Background catalog hydration (media/tags/uploader repair):
  [catalog-hydration.md](catalog-hydration.md)

## Session / device management

A device is identified by its `owner_token` HttpOnly cookie; a device links to
one or more public e621 accounts via `account_device_links`. Two endpoints cope
with multi-device installs managed by one operator:

- **`GET /api/session/devices`** — lists every device that shares any of the
  caller's linked accounts. Each entry exposes a stable, non-reversible
  `id` (lowercase-hex `sha256` of the raw token — the token itself is never
  returned), `isCurrent`, `firstSeenAt`, `lastSeenAt`, `active` (seen within a
  30-day window) and the `accounts[]` it owns. Implementation:
  `db::accounts::list_device_sessions`.
- **`POST /api/session/revoke`** — accepts `{ "deviceId": <sha256 hex> }` and
  revokes that device: the raw token is matched among devices sharing the
  caller's accounts, added to the persistent revocation denylist, and its
  account links are severed (with the per-account teardown cascade). The
  caller's own/current device is excluded (`DELETE /api/session` is the logout
  path for self), and an unknown id returns `404`. Implementation:
  `db::accounts::find_device_token_by_id` + `delete_all_device_links_for_token`.

The settings page surfaces both in a “Devices & sessions” card
(`parser-web/src/components/session_devices_card.rs`).

## Profile backup / restore

`GET /api/account/<id>/export` produces a JSON snapshot including blacklist,
preferred tags, the preference profile, and the raw **interaction model**
(`interactions` — the account's open/like/hide/… events, newest-first, capped
at 100k). No secrets (owner-token or API keys) and no per-session ids are
exported.

`POST /api/account/<id>/import` restores the user-settable fields (blacklist,
preferred tags) **and** the interaction model: `db::restore_feed_interactions`
replays each event (idempotent `INSERT OR IGNORE` under a fixed import
session) and rebuilds the derived `account_tag_feedback` aggregate, so the
interaction-derived part of the taste profile transfers. Ownership is verified
before any interaction write.

The non-interaction parts of the profile (rating/media/quality/recency/
uploaders) derive from public favourites and are recomputed by `/process` on the
target install — they are exported for archival value but deliberately not
imported directly.
