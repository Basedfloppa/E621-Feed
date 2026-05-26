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
