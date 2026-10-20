# Memory profiling — in-memory data structures

Measured with `parser-api/examples/memory_profile.rs` (synthetic data at
approximate production scale) against the real `database.db` (198,794 tags,
306,888 posts, 2,728,610 co-occurrence pairs with `cooc_count >= 2`).

> **Historical measurements.** The RSS figures below are a point-in-time run
> of the example (this file last revised 2026-08-01); structure sizes follow
> the catalog, so re-run the example after catalog growth rather than
> treating these as current. The commands under [Re-running](#re-running)
> regenerate them.

> Companion docs: [`load-testing.md`](load-testing.md) measures SQLite
> concurrency under load; `cargo bench --bench scoring` times the scoring
> hot path (≈3.5 µs/post).

> **2026-10-19, live instance.** At **44.6M pairs** in `tag_cooccurrence`
> the process sits at **1.7–1.8 GiB RSS**, not the ~73 MB of the 2.7M-pair
> run below: the frozen vec alone is 44.6M × 12 B ≈ **536 MB**, and every
> rebuild stages a second copy before swapping the `Arc`, so a load peaks
> near 1.1 GB. Two full rebuilds were observed minutes apart on the same
> instance — saving favourites marks the global graph dirty
> (`mark_global_relation_dirty`), so browsing `/favorites` costs one
> background rebuild (~10 s CPU, coalesced) and the response is not blocked.

## Methodology

The example builds each structure at increasing scale and reads RSS via
`/proc/self/statm` at each stage. Sizes chosen to approximate the real
catalog:

- IDF index: 200k tags, 2M documents
- TagRelationGraph: 200k tags, ~2M co-occurrence pairs (7 group slots)
- Scoring: 10k synthetic posts scored through a warm `ScoringContext`

## Results

| Stage | RSS (MB) | Δ |
|-------|----------|-----|
| Baseline (app start) | 3.0 | — |
| IDF index (200k tags) | 23.6 | +20.6 |
| TagRelationGraph — HOT (HashMap, 2M pairs) | 117.8 | +94.3 |
| TagRelationGraph — FROZEN (Vec, 2M pairs) | 72.8 | −45.0 |
| ScoringContext + 10k posts | 73.5 | +0.7 |
| After scoring (warm) | 73.8 | +0.3 |
| After drop of everything | 42.7 | −31.1 |

## Per-structure breakdown

### IDF index — `HashMap<String, i64>` in `IdfIndex::df`

- 200k tags → **~21 MB** (≈100 B/tag: 8 B value + string header + heap
  string bytes + HashMap bucket overhead).
- Extrapolation to a larger catalog (say 1M tags, worst case) ≈ **~100 MB**.
- The TODO's "hundreds of MB" estimate was pessimistic at current catalog
  size. The real driver of memory is the **tag-relation graph**, not IDF.

### TagRelationGraph — `PairStorage`

Two shapes matter (see `src/utils/tag_relation.rs`):

| Shape | Bytes/pair | 2M pairs | 2.7M pairs (real) |
|-------|-----------|----------|-------------------|
| **Hot** `HashMap<(TagId,TagId), i64>` | ~40–48 B | ~94 MB | ~127 MB |
| **Frozen** `Vec<(TagId,TagId,u32)>` | ~12 B | ~22 MB | ~30 MB |

Prod already uses the frozen path (`set_pairs_frozen_vec` in
`db/cooccurrence.rs::load_global_tag_relation`), which skips the hot peak
entirely — the comment there documents this choice. **The frozen path is
~4× more memory-efficient** and is the right design for a read-mostly
global graph.

### Scoring

- Building 10k `Post` structs (each ~20 tags, full e621 shape) costs
  **~0.7 MB** — negligible.
- Scoring 10k posts through all 12 channels costs **+0.2 MB** — no
  measurable per-request allocation; the hot path is allocation-light
  (matches the benchmark: 3.46 µs/post).

## Where the production 1.3–1.4 GB actually goes

The structures measured above account for only ~100–150 MB at current
catalog size. The rest comes from:

1. **Catalog scan buffers** — `tag_cooccurrence` table itself is large
   (2.7M rows × 24 B = ~65 MB on disk, but queries build transient
   `Vec` buffers).
2. **API response cache** — `e621_cache_max_entries = 5000` JSON blobs,
   each up to several MB → potentially **hundreds of MB**.
3. **Feed dedup id-sets** — the record itself lives in SQLite
   (`feed_interactions`, `feed_shown_posts`), but each account's recent-seen
   set is also materialised in process memory by `db/hydrate_cache`
   (20–60 s TTL, up to 1024 accounts) so `db_hydrate` does not re-scan the
   tables on every scroll.
4. **Rocket/reqwest connection pools** and general runtime.
5. **SQLite page cache** — SQLite's own cache can grow to hundreds of MB
   under load unless capped.

## Recommendations

1. **Keep the frozen Vec path** for the global graph (already done). The
   hot HashMap form should stay limited to small per-account graphs built
   in calibrate.
2. **Cap the SQLite page cache** — it is bounded per connection today:
   readers get 64 MiB each (16-connection pool), the writer gets 8 GiB by
   default (`E621_DB_CACHE_MB`). Lower the writer value for migration runs.
3. **Measure API cache realistically** — 5000 entries of full post JSON at
   ~50 KB each = 250 MB. Consider lowering `e621_cache_max_entries` or
   capping per-entry size.
4. **jemalloc is optional** — at these sizes glibc returned memory fine in
   the test (73.7→42.5 MB after drop without jemalloc). jemalloc's value
   is mainly avoiding *fragmentation* under sustained churn, not peak RSS.
5. Re-run `memory_profile` after catalog growth (tags → 1M, pairs → 10M)
   to keep the estimates honest.

## Migration peak — `temp_store = MEMORY`

Migrations run on the writer connection, which sets `PRAGMA temp_store = MEMORY`
(`parser-api/src/db/mod.rs`). A `CREATE INDEX` sorts in that store, so a large
build keeps the sorter in the heap instead of spilling to disk.

Measured on a synthetic table of the same shape (4 int columns + rowid, peak
RSS via `VmHWM`):

| rows | `temp_store = MEMORY` | `temp_store = FILE` |
|---:|---:|---:|
| 16.7M | 1095 MB (15.4 s) | **135 MB (9.4 s)** |
| 67.1M | 4167 MB | — |

That is ~60 B/row and linear. V38 (`idx_atc_account_cooc_cover`) built over
**962M rows** took **17 min 17 s** and grew the file by 19 GB; the extrapolated
sorter footprint is tens of GB — during that run the host (container without a
memory limit) pushed its whole desktop into zram.

If a migration ever builds a big index: `PRAGMA temp_store = FILE` for the
migration connection (measured both leaner and faster), and a smaller
`E621_DB_CACHE_MB` for the run. The application does not serve requests while
migrations run, so this needs a maintenance window, not just a restart.

## Re-running

```bash
cd parser-api
cargo run --release --features jemalloc --example memory_profile
# without jemalloc:
cargo run --release --example memory_profile
```
