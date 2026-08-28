# Local Catalog & Offline Media Serving

The app can keep a **local copy** of the posts you care about — their metadata
in SQLite and their full-size media on disk — so the feed, the post viewer and
actual image/video bytes keep working even with **no e621 network access**.

This page is about *using and configuring* the feature. Collection is opt-in
and **scoped**: `save_favourites` controls the favourites sources, `save_all`
additionally the "every post you encounter" sources. The media cache follows
whatever is collected.

## The idea in one line

**Local-first, not fallback.** If a post is already stored locally, it is served
locally — the app never asks e621 for it. e621 is only contacted for posts that
aren't in the local catalog.

## What it gives you

- **A searchable local catalog** of a selected account's saved posts, with
  fast local tag search and autocomplete (no e621 round-trips).
- **Local media bytes**: full-size originals are downloaded to disk in the
  background and served by the app itself, so a cached post shows its real file
  even offline.
- **Local pool navigation**: pool membership is stored locally so the post
  viewer's next/prev pool navigation works from local data.
- **Media queue controls** from the UI: pause/resume the downloader, run it
  now, or wipe the whole media cache.

---

## Enabling

The feature lives in the `[catalog]` section of `config.toml`. Two toggles
control **which sources are collected** into the local DB (posts, tags,
`accounts_post`); the media cache then follows what was collected:

```toml
[catalog]
save_favourites = true     # collect favourites: /favourites + /process (+ direct sync)
save_all        = false    # ALSO collect every post the owner encounters: /digest /search /trending (and feed)
media_cache_max_bytes = 0  # 0 = unlimited; else LRU-evict oldest beyond this many bytes
```

### A. Collection scopes

* **`save_favourites = true`** — the favourites scope: posts served by
  `/browse/favorites` and imported by `/process` (and the direct sync) are
  persisted (posts + `accounts_post` + tags) and become the searchable local
  catalog.
* **`save_all = true`** — additionally the encountered scope: posts shown on
  `/digest`, `/search` and `/trending` (and the feed) are collected too.
  `save_all` implies the favourites scope.
* **Both off** — nothing is collected: `/process` refuses with a clear job
  error, favourites/encountered sources don't persist, and the `/catalog` page
  stays empty.

### B. Media cache (download full-size originals)

The media folder is hardcoded to `media/` (relative to the working
directory) and always enabled — link/symlink it wherever you need it. Only the
size cap is configurable:

```toml
[catalog]
media_cache_max_bytes = 0         # 0 = unlimited; else LRU-evict oldest beyond this many bytes
```

A background worker downloads the **full-size original** of every saved post
that doesn't have one yet, in small rate-gated idle batches. A favourites sync
(`POST /account/<id>/sync`) or `/process` that saves a post automatically
queues its original for download — no manual step. It only ever downloads
*saved* posts, never the whole recommendation corpus.

Transient failures (network, 5xx) are retried with a short backoff and the post
stays queued. When the CDN answers **404/410** for an original, the post is
considered deleted upstream: it is **purged from the local catalog** (the post
row and its tag/interaction links, via FK cascade) and logged to the audit
stream (`catalog.media.post_deleted`), so the worker never retries it forever.

### Full reference

```toml
[catalog]
save_favourites       = false   # favourites scope: collect /favourites + /process (+ direct sync)
media_cache_max_bytes = 0       # hard size cap (bytes); 0 = unlimited (folder is fixed at media/)
pool_membership       = false   # store pool membership locally for offline pool navigation
save_all              = false   # additionally collect encountered posts: /digest /search /trending (and feed)
```

All fields default to off/0/false. The media folder itself is not
configurable — it is always `media/` (relative to the working directory). With
both toggles off nothing is collected and the media cache stays empty.

---

## Using it

Open the **Catalog** page (`/catalog`). All controls are owner-gated and always
available.

### Search
- Type tag terms in the search box (e.g. `wolf rating:s`). Results match
  **all** whitespace-separated terms, case-insensitively, and only across that
  account's saved posts.
- Autocomplete comes from the **local DB** (no e621 request).
- You can save a query as a **named grouping** (a name + tag query, kept in the
  browser) and re-run it later with one click.

### Media queue
The toolbar shows the downloader state (`Pending`, `Stored`, bytes on disk) with:
- **Pause / Resume** — pause or resume the background media worker (Resume also
  kicks a run immediately).
- **Run now** — make the worker run a pass immediately instead of waiting.
- **Clear cache** — delete all downloaded originals **and** their index. Confirms
  first. After clearing, saved posts re-download on the next pass.

---

## API reference

All endpoints are under `/api` and owner-gated (owner-token cookie + per-IP rate
limits).

### Catalog (search)
- `GET /catalog/<account_id>/search?query=<tags>&page=&limit=`
  Posts in the account's saved catalog matching all tag terms (AND,
  case-insensitive). An empty `query` returns the whole saved catalog. Paginated
  like browse (`?page=` is 1-based).
- `GET /catalog/<account_id>/tag/suggest?prefix=&limit=`
  Local tag autocomplete: tag names the account's saved posts carry,
  prefix-matched, ordered by frequency.

### Media (raw files)
- `GET /api/media/<post_id>?size=original`
  Streams the locally stored original. **Read-only**: a post with no local
  original returns 404 — it never fetches e621 on demand. Only originals are
  stored; requesting any other `size` 404s.

  Responses are aggressively cacheable: `Cache-Control: public,
  max-age=31536000, immutable` plus a strong `ETag` (file mtime + size). A
  matching `If-None-Match` returns `304 Not Modified` without reading the file.

  Byte ranges are supported (single range only): `Range: bytes=0-99` returns
  `206 Partial Content` with `Content-Range`, and unsatisfiable ranges return
  `416` — so `<video>`/`<audio>` seeking works on the originals. Multi-range
  headers are ignored (full 200).

  Per-IP rate limit: **240 requests/minute, burst 480** (high enough for a full
  catalog grid; still bounded against scraping).

### Media queue / catalog manage
- `GET  /catalog/<account_id>/media/status` → `{paused, pending, stored, bytes}`
- `POST /catalog/<account_id>/media/pause` — pause the background worker
- `POST /catalog/<account_id>/media/resume` — resume (and kick) it
- `POST /catalog/<account_id>/media/kick` — run a pass now
- `GET  /catalog/<account_id>/media/queue?limit=` — list queued (un-downloaded) saved posts
- `DELETE /catalog/<account_id>/media` — wipe all originals + the media index
  (the UI’s **Clear cache**)
- `DELETE /catalog/<account_id>/post/<post_id>` — remove a post from the
  account's catalog. The on-disk original + index row are deleted **only when
  this was the last account** still saving the post — other owners keep the
  shared file (audit event records `media_removed`).

---

## How serving works

When a post has a stored original, the backend rewrites its media URLs
(`preview`, `sample`, `original`) to point at the local file
(`/api/media/<id>?size=original`). So locally-available posts render from your
server, no e621 involved. The single post view (`get_single_post`) and pool view
are local-first: they use local data and only fall back to e621 for posts/pools
that aren't present.

---

## Backup & restore

The database is one SQLite file; media is a folder on disk.

```sh
# 1. Stop the server (clean stop checkpoints the WAL into database.db)
# 2. Copy the DB and the media folder
cp database.db  backup-$(date +%s).db
cp -r media     media-backup-$(date +%s)/   # if you use the media cache
```

After a **hard kill** (not a clean stop), un-checkpointed pages may still be in
`database.db-wal` — copy it too, or open the file once and run
`PRAGMA wal_checkpoint(FULL)` before copying.

---

## Docker notes

`docker-compose.yml` bind-mounts `./database.db` and `./media` into the
container, so both persist on the host under `parser-api/`. The container runs
as the launching host user (uid/gid from `id -u`/`id -g` via `./compose.sh`, or
from `E621_UID`/`E621_GID` in `.env`) so bind-mounted files stay host-owned.
The hardcoded `media/` folder resolves inside the container to `/app/media`
(the mounted host folder).

The config file sets paths (`db_path`, ...), interpreted relative to the
launch directory. The env overrides that exist are:
- `CONFIG_PATH` — which `config.toml` to load (default `./config.toml`).
- `ROCKET_PORT` — the listening port (compose sets `8181`).

There is **no** `DB_PATH` or `MEDIA_CACHE_DIR` env override — the DB path
comes from the config and the media folder is always `media/`, both relative
to wherever the app is launched (bind `database.db` / `media` there if you
want them elsewhere).

---

## Limitations & security notes

- **`/api/media/<id>` is unauthenticated** (so plain `<img>`/`<video>` tags
  work), but per-IP rate-limited (240/min, burst 480). Anyone with network
  access to the server can read stored originals — an accepted tradeoff for
  the offline-serve feature.
- **Offline pool navigation** only works when *every* pool member is already
  local; otherwise it falls back to a live fetch (and saves membership for next
  time).
- A **0-byte stored original** is treated as missing (404).
- If a post is re-downloaded with a changed file extension, the prior file stays
  on disk as an orphan until the cache is cleared (LRU eviction only walks
  indexed rows).

## Out of scope
- Storing/proxying preview & sample sizes (only full originals are cached).
- Comments offline (live-only).
- Writing favourites back to e621.
- Media bytes inside SQLite (files live on disk; the DB only indexes them).
