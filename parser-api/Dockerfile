# ===================== BuildKit =====================
# Requires BuildKit (`docker buildx` or Docker ≥ 23 with BuildKit enabled).
# `--mount=type=cache` persists build caches (cargo registry/target, npm)
# on the host between separate `docker build` runs, so toolchain installs,
# dependency downloads and compilation are only redone when inputs change.
# Without BuildKit these cache mounts are a hard error — see docs/deployment.md.

# ===================== Base =====================
# Shared base for every build stage (keeps the apt/toolchain layer cached).
#
# IMPORTANT: the rust version is PINNED, not floating. The frontend dist
# filenames + SRI integrity hashes are derived from the compiled wasm, and
# wasm-bindgen's JS glue is version-sensitive — a floating `rust:slim-trixie`
# tag was how two builds ended up with the same bundle URL but different
# bytes, breaking SRI on the live site. Keep this tag in sync with
# `rust-toolchain.toml` at the repo root (same rustc release).
FROM rust:1.96.0-slim-trixie AS base
WORKDIR /app

# make is needed by tikv-jemalloc-sys; gcc comes with the rust toolchain.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    ca-certificates \
    make \
    && rm -rf /var/lib/apt/lists/*

# ===================== Frontend (WASM) =====================
# Compiles parser-web (Rust -> WASM via trunk) into parser-web/dist. The dist
# is then embedded into the backend binary (build.rs -> serve_embedded.rs).
#
# Caching:
#   * `npm ci` runs only when package.json / package-lock.json change, and its
#     downloads reuse the persistent `/root/.npm` cache mount.
#   * `trunk build --release` reuses the persistent cargo registry + target
#     cache mounts, so unchanged wasm deps are not rebuilt.
FROM base AS frontend
RUN rustup target add wasm32-unknown-unknown

# trunk (wasm bundler) + node (tailwind hook in Trunk.toml).
# With the cache mount below, `cargo install trunk` only re-runs when the
# pinned version changes.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install trunk@0.21.14 --locked --root /usr/local
# wasm-bindgen CLI must match the `wasm-bindgen` crate pinned in
# parser-web/Cargo.lock (0.2.126) — trunk generates the JS glue with this
# binary, and a version mismatch between the crate and the CLI is a build
# error. trunk can auto-install the matching version, but pinning it here
# keeps the toolchain explicit and reproducible.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install wasm-bindgen-cli@0.2.126 --locked --root /usr/local
RUN apt-get update && apt-get install -y --no-install-recommends nodejs npm \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /web

# Pin the toolchain for every rustup invocation in this stage (must match the
# base image tag above).
COPY rust-toolchain.toml ./

# -- JS deps --
COPY parser-web/package.json parser-web/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm \
    npm ci

# -- Frontend sources --
COPY parser-web/src ./src
# Cargo.toml + Cargo.lock are REQUIRED: without them trunk logs
# `no rust project found` and builds CSS/static only — no wasm/.js is emitted
# and the embedded SPA comes out as a blank page. (Regression from 699b544.)
COPY parser-web/Cargo.toml parser-web/Cargo.lock ./
COPY parser-web/index.html ./
COPY parser-web/style.css ./
COPY parser-web/static ./static
COPY parser-web/.well-known ./.well-known
COPY parser-web/scripts ./scripts
COPY parser-web/Trunk.toml ./

# `--release` also runs the post-build compress hook (compress-dist.sh).
RUN --mount=type=cache,id=wasm-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=wasm-target,target=/web/target \
    trunk build --release
# Remove pre-compressed siblings; build.rs excludes them from the embed anyway.
RUN find dist -name '*.gz' -delete -o -name '*.br' -delete

# ===================== Backend =====================
FROM base AS builder
ARG FEATURES=""
# cwd is /app/parser-api so build.rs resolves ../parser-web/dist.
WORKDIR /app/parser-api
# Pin the toolchain for the backend build too (see rust-toolchain.toml note).
COPY rust-toolchain.toml /app/rust-toolchain.toml
COPY --from=frontend /web/dist /app/parser-web/dist
COPY parser-api/ .
RUN --mount=type=cache,id=backend-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=backend-target,target=/app/parser-api/target \
    cargo build --release --bin e621-account-parser-api ${FEATURES:+--features "$FEATURES"} \
    && cp /app/parser-api/target/release/e621-account-parser-api /app/e621-account-parser-api \
    && strip /app/e621-account-parser-api

# ===================== Runtime =====================
# Single runtime image: the binary serves BOTH the embedded frontend (/) and
# the API (/api), so nginx/Caddy are no longer needed to serve static files.
FROM debian:bookworm-slim AS runtime

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system app \
    && useradd --system --gid app --create-home --home-dir /app app

COPY --from=builder /app/e621-account-parser-api /usr/local/bin/e621-account-parser-api
COPY parser-api/migrations ./migrations

ENV RUST_BACKTRACE=1

# The binary binds to Rocket.toml's :8080 (the actual listening port).
EXPOSE 8080

CMD ["/usr/local/bin/e621-account-parser-api"]
