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

## Scoring & calibration internals

- Per-knob "lower vs. higher" guide for every scoring variable:
  [scoring.md](scoring.md)
- Offline calibration (`calibrate eval` / `calibrate grid`, holdout
  artifacts, A/B experiment buckets): [calibration.md](calibration.md)
- Background catalog hydration (media/tags/uploader repair):
  [catalog-hydration.md](catalog-hydration.md)
