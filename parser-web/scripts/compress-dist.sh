#!/usr/bin/env bash
# Pre-compress static assets to .br + .gz so nginx with brotli_static /
# gzip_static can serve them without on-the-fly compression. Quality is
# pinned high (br=11, gz=9) since this runs once per release build, not
# per request.
#
# Run by Trunk's post_build hook against TRUNK_STAGING_DIR — those files
# end up in dist/ after Trunk's atomic swap.
#
# Behaviour:
#   * Skipped on non-release builds (trunk serve / trunk build) so the
#     dev edit-loop stays sub-second.
#   * Soft-fails if `brotli` / `gzip` aren't installed — compression is an
#     optimisation, not a correctness requirement. The deploy CI is where
#     missing tools should hard-fail; install with `apt install brotli gzip`.

set -euo pipefail

# Trunk only runs hooks on real builds, but the profile gate keeps `trunk
# serve` (debug) instant — at -q 11 brotli on a 1.6 MB wasm is several
# seconds and would dominate every dev rebuild.
profile="${TRUNK_PROFILE:-debug}"
if [[ "$profile" != "release" ]]; then
    echo "compress-dist: skipping (profile=$profile, only compress on release)"
    exit 0
fi

target="${TRUNK_STAGING_DIR:-${1:-dist}}"
if [[ ! -d "$target" ]]; then
    echo "compress-dist: target dir '$target' does not exist" >&2
    exit 1
fi

have_brotli=0
have_gzip=0
command -v brotli >/dev/null 2>&1 && have_brotli=1
command -v gzip   >/dev/null 2>&1 && have_gzip=1

if (( have_brotli == 0 && have_gzip == 0 )); then
    echo "compress-dist: neither 'brotli' nor 'gzip' on PATH; skipping" >&2
    exit 0
fi

# Compress assets that benefit from text/binary compression. WASM is on
# the list because nginx serves it as application/wasm and brotli still
# wins ~10-20% on its LEB128 + interleaved string sections.
mapfile -t files < <(find "$target" -type f \( \
    -name '*.wasm' -o \
    -name '*.js'   -o \
    -name '*.mjs'  -o \
    -name '*.css'  -o \
    -name '*.html' -o \
    -name '*.json' -o \
    -name '*.svg'  -o \
    -name '*.txt'  -o \
    -name '*.xml'  \
\) ! -name '*.br' ! -name '*.gz')

count=0
skipped_small=0
for f in "${files[@]}"; do
    # Below ~1 KB the framing overhead exceeds the savings, and nginx
    # would skip these via brotli_min_length / gzip_min_length anyway.
    size=$(stat -c '%s' "$f")
    if (( size < 1024 )); then
        skipped_small=$((skipped_small + 1))
        continue
    fi

    if (( have_brotli == 1 )); then
        brotli --force --quality=11 --keep -- "$f"
    fi
    if (( have_gzip == 1 )); then
        gzip --force --best --keep -- "$f"
    fi
    count=$((count + 1))
done

echo "compress-dist: precompressed $count files (br=$have_brotli, gz=$have_gzip; skipped $skipped_small under 1 KiB)"
