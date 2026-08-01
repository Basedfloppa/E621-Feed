# SQLite load testing

Concurrency behaviour of the SQLite-backed API under read/write load,
measured with [locust](https://locust.io/) against the production
`database.db` (198k tags, 307k posts, 75 GB file).

## Setup

> Companion docs: [`memory-profile.md`](memory-profile.md) explains where
> the process RSS goes; `cargo bench --bench scoring` measures the scoring
> hot path (≈3.5 µs/post).

```bash
# 1. Build the release binary
cd parser-api && cargo build --release

# 2. Load-test config (separate dir so the real config.toml is untouched)
cd parser-api/loadtest   # config.toml + locustfile.py live here
ROCKET_PORT=8088 ../target/release/e621-account-parser-api

# 3. Hammer it
locust -f loadtest/locustfile.py --host http://127.0.0.1:8088 \
    --headless -u 50 --spawn-rate 10 -t 2m --only-summary
```

The load-test config points at the real DB but **disables the background
workers** (`prefetch_interval_secs=0`, `tag_alias_import_interval_secs=0`,
`cache_validate_interval_secs=0`) so the test isolates DB behaviour from
e621-bound traffic. `loadtest/config.toml` is gitignored.

The locustfile has three user classes:

- `SessionUser` — light reads (tag_counts, profile, tag resolve, accounts)
- `RecHeavyUser` — the scoring hot path (recommendations)
- `WriteHeavy` — feedback interactions (feed_interactions inserts)

Auth uses real `owner_token`s from `account_device_links` (cookie
`owner_token`). POSTs send a same-origin `Origin` header because the
release build rejects cross-origin unsafe methods (CSRF guard).

## Results

### Write path (feed interaction) — isolated

16 parallel writers, each its own token, ~45 s:

```
TOTALS: {200: 1214}          # 1214/1214 = 100% success
```

The **dedicated single writer** design (`with_write_tx` + `Mutex` +
`IMMEDIATE` transactions in `db/mod.rs`) works: SQLite's one-writer rule
is serialised at the application layer, readers stay parallel in WAL mode,
no `database is locked` errors under 16+ concurrent writers.

### Mixed load (25 readers + 25 writers, 40 s)

```
TOTALS: {200: 2102, 429: 102}   # 0 × 500
```

The only errors are **429 rate limits** (120 writes/min per `owner_token`
in `routes/feed.rs`) — expected defensive behaviour when many virtual
users share a small pool of real tokens. No SQLite errors.

### Heavy reads (recommendations)

| Scenario | Latency |
|----------|---------|
| Single request, cold IDF build | 5.7 s |
| Under 50-user load | **36–55 s** |

The scoring path (`ScoringContext::score` over 12 channels against
thousands of candidate posts) is the system's true bottleneck — a CPU /
query-shape problem, not a SQLite write-contention problem.

### Degradation of light reads under heavy scoring

When several 40 s recommendations run concurrently, light reads
(tag_counts, profile) degrade from ~4 ms / ~120 ms to **~30 s**. The
16-connection pool is monopolised by long blocking scoring queries. This
is the closest thing to a real finding in this test.

### WAL growth

With the cache-pruner disabled (interval=0) the WAL file grew to
**333 MB** over ~30 min of writes. In production `cache_pruner` runs
`PRAGMA wal_checkpoint(TRUNCATE)` every 300 s, which bounds this. The
load-test config disabling the pruner is the only reason WAL grew — a
reminder that the checkpoint cadence is load-bearing.

## Caveats

- locust's gevent client reported ~50 % **500** on `/interaction` during
  headless runs that plain Python (urllib, same paths, same auth, same
  rates) did not reproduce (0 × 500). Treat locust's 500s as a harness
  artifact; the Python cross-check is the reliable number.
- All writes used a handful of shared post_ids — real-world writes touch a
  spread of posts, but the FK-heavy `UPDATE account_tag_feedback` with the
  `tags_posts` join is the dominant write cost regardless.
- The test ran against the developer's real DB. `loadtest/config.toml`
  disables background workers so nothing e621-bound fired.

## Recommendations

1. **Bump the pool** (`Pool::builder().max_size(16)` in `db/mod.rs`) or
   bound recommendation query time — long scoring queries starving the
   pool is the only real degradation observed.
2. **Keep the dedicated writer.** It is what makes 25 parallel writers
   error-free; do not switch to pool-based writes.
3. **Keep the WAL checkpoint cadence** (300 s in the cache-pruner) — with
   it disabled the WAL grows hundreds of MB in minutes.
4. **Rate limits are the binding constraint for writes**, not SQLite —
   per-token interaction caps (120/min) are hit first under multi-user
   load with few tokens.
5. Recommendations at 36–55 s are the scaling ceiling: consider caching
   per-account scored lists or narrowing the candidate query (the scoring
   benchmark in `benches/scoring.rs` shows a single `score()` is only
   3.5 µs — the 36 s is query/hydration time, not the math).

## Re-running

```bash
cd parser-api/loadtest
ROCKET_PORT=8088 ../target/release/e621-account-parser-api &
locust -f locustfile.py --host http://127.0.0.1:8088 --headless \
    -u 50 --spawn-rate 10 -t 2m --only-summary
```
