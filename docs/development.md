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

## Per-account e621 API key + direct sync

### Encrypted key storage (account-scoped)

A linked account may hold its **e621 API key**, encrypted at rest in
`accounts.e621_api_key_encrypted` (one canonical key per account —
`db/set_account_e621_key`). It is encrypted with **AES-256-GCM**
(`src/crypto.rs`); the encryption key is derived from
`config.e621_key_encryption_secret` (SHA-256). The raw key is never stored,
returned over the API, or included in the export payload (the export is built
from explicit fields and omits these columns) — only booleans and timestamps
are exposed.

Keys are **account-scoped**, not device-scoped: any device linked to the
account (via `account_device_links`, enforced on every key accessor) can
read/manage it and run sync with it. This is deliberate so direct sync works
from whichever device is active. The **admin_user account is special-cased in
sync**: it syncs with the shared `admin_api` directly and needs no stored
per-account key (see *Read-only direct sync*).

### Ownership proof (M2) — optional at claim time

`POST /api/account` may include `api_key` to **prove ownership** / enable
direct sync; the key is **optional**. When provided, the route verifies it
against e621 (`api::verify_e621_key`, using the user's own credentials and a
per-user rate-limit bucket) before storing it encrypted at linking; when
omitted, the account is linked **without** a key (key test/sync then report
“no key configured”).

Per provided key: a malformed key is `400`, a key that doesn't
authenticate the claimed account is `403`, and an e621 outage during
verification fails with `503` (our `ApiError::Upstream` maps to HTTP 503).
Every link (with or without a key) still gates reads (`tag_counts` / profile /
`export` / interactions / …) by `account_device_links` ownership, so an
anonymous token cannot claim an arbitrary public account and read its
accumulated parser data (closes the piolium M2 finding — ownership proof is
offered as an option, not imposed as a hard requirement).

### Key management endpoints

- **`PUT /api/account/<id>/key`** — set or rotate the key (`{ "key": … }`);
  stores it encrypted. Returns `AccountKeyState`.
- **`GET /api/account/<id>/key/state`** — `{ accountId, hasKey, addedAt,
  verifiedAt, name, operations }` (e.g. `operations=["direct_sync"]`). No key
  material.
- **`POST /api/account/<id>/key/test`** — verifies the *configured* key against
  e621; returns `{ valid, name, verifiedAt }` and refreshes `verified_at`.
- **`DELETE /api/account/<id>/key`** — revoke (remove) the key.

Frontend: `parser-web/src/components/account_key_card.rs` (Settings → “e621
Account Key & Sync”).

### Read-only direct sync

`POST /api/account/<id>/sync` (owner-gated; `400` when no key can be resolved)
imports/refreshes the account's private e621 data **using its key** — the
stored per-account key, or the shared admin_api for the `admin_user` account
(see below):

- **favorites** — pages fetched as the owner (`api::get_favorites_with_key`) and
  persisted via the pipeline's writers (`save_posts` + `save_posts_tags_batch`),
  which maintain co-occurrence / tag feedback (profile + preferred tags derive
  from favourites);
- **votes** — on e621 an upvote *is* a favorite, so votes are covered by the
  favourites import;
- **blacklist** — the owner's real (private) blacklist from
  `users/<id>.json` (`api::get_user_with_key`) is written to their device
  blacklist.

All reads use the user's credentials and a **per-user rate-limit bucket**
(`e621:user:{account_id}`), distinct from the shared `e621:admin-key` bucket —
sync traffic can never throttle the admin key or other owners. **No write-back
is performed**: sync never POSTs/PUTs/DELETEs to e621.

`GET /api/account/<id>/sync/status` returns `{ hasKey, lastSyncedAt, datasets }`
(read-only, offline-deterministic). The last-sync timestamp is account-wide and
lives in `accounts.last_direct_synced_at`. For the `admin_user` account the
admin_api is used directly — sync needs no stored per-account key for it.

## Frontend — masonry grid layout

The feed/browse grids (`parser-web/src/components/post_grid.rs`) are a
faux-masonry: a CSS `grid-cols-*` container holds one `flex flex-col` per
column, and each post is placed into the **shortest column** (by accumulated
`height/width` preview ratio). Cards render via `PostCard`
(`post_card.rs`), which reserves the media box with
`aspect-ratio: {w} / {h}`.

### Extreme-tall media handling

A single **extremely tall** image (e.g. a long comic strip, `height/width`
far above normal) would otherwise stretch one card (and thus one column) to
many screen-heights, unbalancing the columns and leaving a large gap. Two
matching guards keep the columns stable:

- `post_grid.rs` defines `MAX_CARD_MEDIA_RATIO = 2.5` and
  `card_media_ratio()` — the capped `height/width` ratio used for
  **column balancing** in `render_post_grid`. Ultra-tall posts are counted at
  most `2.5` instead of their raw (huge) ratio, so one post can't make its
  column the runaway "shortest"/"tallest" target.
- `post_card.rs` applies the **same** cap to the media box, and keeps it
  **after load**: media with `ratio > 2.5` gets
  `aspect-ratio: 1 / 2.5; max-height: 70vh; overflow: hidden` (object-cover
  crops the tall image), so the card itself can't tower. The card cap and the
  balancing weight always agree, so the greedy column assignment matches the
  rendered height.

Normal and wide posts are unaffected (`ratio ≤ 2.5` keeps the existing
reserve-then-natural behavior). Tune with `MAX_CARD_MEDIA_RATIO` (both sides
bleed together — change it in `post_grid.rs` and `post_card.rs`).
