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

### Instance ownership

Ownership *is* the `owner_token` cookie — there is no account login. What the
cookie holds is a **claim on the instance**, not a set of account links: the
claim is one row in `instance_ownership` (a singleton, `V34`), and the account
scope is derived from it. An owner addresses every account on the instance;
everyone else addresses only the accounts linked to their token. Claiming writes
no links, which is why the two questions — "who runs this instance?" and "which
accounts may this browser address?" — can no longer be confused for one another.

The claim covers what this browser may **read** — every account on the instance —
and the instance-wide actions that outlive a single browser session (starting a
`/process` run). It does **not** open the Catalog tab or `/admin`: those act on
the machine rather than on the library, and they take the administration tier
described below. `db::account_in_scope` / the `MAY_ADDRESS_SQL` predicate decide
per-account reads; `auth::require_instance_owner` (and
`auth::require_instance_operator`, which accepts either credential) decides the
instance-wide ones. `[instance].owner_secret` governs how the claim is taken:

- **Empty (default)** — trusted single-owner self-hosting. The first
  `POST /api/session/bootstrap` claims the instance for that cookie
  (`ClaimOutcome::Claimed`); a later anonymous bootstrap is `Refused`, never a
  takeover. The response carries `claimed`, `is_owner` and `instance_locked`.
- **Set** — `bootstrap` mints a guest token and reports `instance_locked: true`;
  `POST /api/session/enroll` with `{ "secret": "…" }` claims the instance, or
  **moves** an existing claim to the caller's cookie (`Transferred`). That move
  is the recovery path: nothing about the library was ever tied to the old
  cookie, so re-proving the secret restores everything.

Deliberately a dedicated value rather than `e621_key_encryption_secret` or
`admin_api`: those two cannot be rotated without either re-encrypting every
stored e621 key or rotating a third-party credential, so a leak of either would
leave no way to lock the instance again. The owner secret rotates on its own.
It is compared in constant time (`auth::secret_matches`) and the route is
rate-limited to 5 attempts/minute per IP. Implementation:
`db::instance::{claim_instance, is_instance_owner}`.

In the SPA this is **Settings → Instance ownership**
(`parser-web/src/components/instance_card.rs`). The card reads
`GET /api/session/state` — `instance_locked`, `instance_claimed`, `is_owner`,
`is_admin`, `admin_available`, `owned_accounts` (what this
browser can address) and `instance_accounts` (what the claim covers) — and offers
the secret field only when a secret is configured and this browser does not
already own the instance; on an open instance it says so instead of showing a
form that could only answer 400.

### Administration — the operator tier

Owning the instance is not operating it, and the two are deliberately separate
credentials. The operator is named in configuration: a browser becomes one by
linking `[runtime].admin_user` **with** a key matching `[runtime].admin_api`
(checked in constant time in `routes::account::create_account`, before the e621
verification, and refused outright otherwise — with no key, or a wrong one, that
account cannot be linked at all).

`V35` records the grant on the device link (`admin_granted_at`,
`admin_key_fingerprint`), where the guard can test it with one indexed lookup
(`db::is_instance_admin`). The fingerprint is what makes the grant **revocable by
configuration**: it is the domain-separated SHA-256 of the key that wrote it, so
rotating `[runtime].admin_api` retires every grant made under the old value and
re-opens the tier (`db::admin_available`) without anyone revoking anything by
hand. `[instance].owner_secret` cannot do this, which is exactly why the console
is not behind it.

The tier gates the operator surfaces — the local catalog (`/catalog`, including
the instance-wide media pause/resume/kick/clear) and the console
(`/admin`, `GET /api/admin/overview`) — through
`auth::require_instance_admin`.

The credential is proved in **Settings → Owner claim**
(`parser-web/src/components/owner_claim_card.rs`), and that card is the only
place the tier is advertised: no nav entry, no drawer entry and no onboarding
step points at it. The tour's account steps walk to `/account`, which is the
general linking form and not this card. It renders only while
`admin_available` is true and this browser is not already the operator.

`GET /api/session/state` says whether the tier is open (`admin_available`) and
whether this browser holds it (`is_admin`) — nothing else about it. In particular
it does **not** name `[runtime].admin_user`: a browser that has not proved the key
is never told which account holds the machine, so the claim form always asks for
the account id, the username and the key. Three fields instead of one is the
price, and it buys a `/session/state` that discloses nothing about who operates
the instance.

`/account` is the general account-linking page, not a route into this tier: it
links whatever account the reader cares about, with or without a key, and the
server refuses `[runtime].admin_user` to anyone who has not proved
`[runtime].admin_api`. When `/catalog` and `/admin` are refused they explain the
rule rather than offering a call to action.

## Owner console — `/admin`

`parser-web/src/pages/admin.rs` over `GET /api/admin/overview`
(`parser-api/src/routes/admin.rs`). It is a view over the **instance** — the
media queue, the job registry, the bucket pins — so the endpoint requires the
administration grant (`require_instance_admin`) and answers 403 otherwise. The
rows come from the console's own scope (`db::instance::console_accounts_sql`) —
every account on the instance — which is deliberately **wider** than the account
picker's: an operator sees the whole instance without gaining the ability to act
as accounts it holds no link to. Because of that gap each row carries two flags,
not one: `linked` (unlink would find something to remove) and `addressable` (the
account is in the scope `account_in_scope` recognises, so sync, export,
`/process` and the bucket pin would actually be accepted). The console renders
every per-row control from `addressable`, because each of those routes re-checks
it — a row that is merely *visible* offers nothing to click and says `view only`.
Rotating `[runtime].admin_api` narrows the scope back to the links. The nav entry
follows the same rule
as the Catalog tab: `models::ownership::is_admin` is the single predicate behind
both `catalog_entry_visible` and `admin_entry_visible`, and a browser that is not
the operator is told which credential opens the console rather than being sent to
link an account or to claim the instance, neither of which would help.

The overview carries the totals (accounts in scope, **distinct** saved posts,
accounts with a stored key, running jobs), the instance-wide media counters, the
configured `[buckets.*]` names, and one page of accounts with their per-account
job state. Three details are deliberate:

- `saved_posts` counts **distinct** posts, not `accounts_post` rows: one post
  saved by two accounts is one post.
- the media counters are **instance-wide** and the panel says so —
  `db::media::queue_stats()` has no tenant filter yet (TODO §4.5).
- the console scope is the whole instance for any operator, so `totals.accounts`
  and `instance_accounts` agree. They stay separate fields because they answer
  different questions — "how many are in scope" and "how many exist here" — and
  they agree by construction rather than by coincidence.

### A/B bucket assignment

`PUT /api/account/<id>/experiment_bucket` with `{"bucket": "variant"}` pins an
account to one arm; `DELETE` clears the pin so it falls back to the
deterministic hash of `account_id` (`Config::pick_bucket`). Before this route
the buckets existed only in config and were assigned by that hash, with no way
to say "put *this* account in *that* arm" — the field was read-only, exposed
through `/feed_settings` and the export.

Two behaviours are load-bearing:

- **An unknown name is refused with 400 and the list of valid names.**
  `pick_bucket` silently ignores an explicit name that is not in
  `[buckets.*]`, so storing one would look like success and change nothing.
  On an instance with no buckets configured the refusal says exactly that.
- **An ownership miss is 404, not 500.** The route checks
  `db::accounts::account_in_scope` and maps the miss itself, because
  `ApiError::from_string` files any unrecognised error string under `Internal`
  — see the note on that pattern in `db/accounts.rs`.

The overview reports both `bucket_explicit` (what the database holds) and
`bucket` (what scoring actually uses). They differ when a pin names a bucket
the instance no longer configures; the console renders that as
`Stale pin: legacy-arm → control` and offers it as a
`legacy-arm (not configured)` option, because a `<select>` with nothing
selected displays its first option and would otherwise read "Hash (no pin)"
while the pin is still there.

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

Keys are **account-scoped**, not device-scoped: every device the account is in
scope for — the ones linked to it, plus the instance owner — can read/manage it
and run sync with it (`require_account_scope`, enforced on every key accessor).
This is deliberate so direct sync works from whichever device is active. The
**admin_user account is special-cased in sync**: it syncs with the shared
`admin_api` directly and needs no stored per-account key (see *Read-only direct
sync*).

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
`export` / interactions / …) by account **scope** — the `account_device_links`
link, or the instance claim — so an anonymous token cannot claim an arbitrary
public account and read its accumulated parser data (closes the piolium M2
finding — ownership proof is offered as an option, not imposed as a hard
requirement).

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
`height/width` frame ratio). Cards render via `PostCard`
(`post_card.rs`), which reserves the media box with
`aspect-ratio: {w} / {h}`.

### Extreme-tall media handling

Nothing is cropped by us, and the ratio the layout charges is the ratio the card
renders. e621 caps previews at 2:1, though: for an ultra-tall post the preview is
a *crop*, so "render the preview" and "render the frame" are different pictures.
Three rules keep both ends of the layout in agreement:

- `post_grid.rs::frame_dimensions()` returns the **source frame** (original →
  sample → preview → 4:3), and `card_media_ratio()` charges exactly its
  `height/width` — never the preview's. Charging 2.0 for a post that renders at
  14 is what makes the greedy balancer optimise for heights that never reach the
  screen, and the posts after the comic then pile into a column that is already
  a dozen viewports tall while its neighbours end early.
- `post_card.rs::proportional_candidates()` drops every file whose own aspect
  ratio differs from the frame by more than `CROP_TOLERANCE` (2%), so a card only
  renders a faithful resize of the frame; a cropped file comes back only when
  nothing else is displayable. Among those, the smallest candidate at least as
  wide as the measured column wins, so sharpness still follows the column width.
- `post_card.rs::media_reserve_style()` reserves the pre-load height with that
  same frame ratio and **no `min-height` floor**. The old 300px floor reserved
  more than the image rendered for every landscape post (a 3:1 pano reserved
  300px and rendered at 119px), which was its own source of column drift.
  `assign_to_columns()` is greedy shortest-column and walks the feed in order,
  so each column keeps the feed order — a longest-first pass would balance
  marginally better and scramble the reading order.

Measured on the live app (real DB, tag `tall_image`, 6 columns, 160 cards):
**0 / 160 cards cropped** (media box ratio == image natural ratio), **0 / 143
lazily-loading cards with a wrong reserve**, and the live column membership
matches the greedy assignment over frame ratios exactly (0 / 160 mismatches)
while the old preview-ratio weighting lands **133 / 160** cards in a different
column. Column-height spread on that page: **3 069 px** with frame ratios versus
**6 402 px** with preview ratios.

The trade-off is explicit: a 1:50 post occupies ~50 column-widths of vertical
space (≈15 000 px in a 300 px column), so its neighbours can only be evened out
by posts that come after it, and a page that ends right after one comic keeps a
gap at the bottom of the other columns. Showing the whole frame is the intended
behaviour — a cap would have to crop (or letterbox) the media again.
