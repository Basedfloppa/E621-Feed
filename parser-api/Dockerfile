# ===================== Base =====================
# Shared base for every build stage (keeps the apt/toolchain layer cached).
FROM rust:slim-trixie AS base
WORKDIR /app

# make is needed by tikv-jemalloc-sys; gcc comes with the rust toolchain.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    ca-certificates \
    make \
    && rm -rf /var/lib/apt/lists/*

# cargo-chef for dependency-layer caching (planner/cacher stages).
RUN cargo install cargo-chef --locked

# ===================== Frontend (WASM) =====================
# Compiles parser-web (Rust -> WASM via trunk) into parser-web/dist. The dist
# is then embedded into the backend binary (build.rs -> serve_embedded.rs).
#
# Layer caching (classic Docker, no BuildKit): each COPY-then-RUN unit only
# re-runs when its inputs change — `npm ci` reruns only when package.json /
# package-lock.json change, and the wasm dependency build below only when
# Cargo.toml / Cargo.lock change.
FROM base AS frontend
RUN rustup target add wasm32-unknown-unknown

# trunk (wasm bundler) + node (tailwind hook in Trunk.toml)
RUN cargo install trunk@0.21.14 --locked --root /usr/local
RUN apt-get update && apt-get install -y --no-install-recommends nodejs npm \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /web

# -- JS deps (cached until package.json / package-lock.json change) --
COPY parser-web/package.json parser-web/package-lock.json ./
RUN npm ci

# -- Rust wasm deps (cached until Cargo.toml / Cargo.lock change) --
# Build only the dependency graph with an empty lib so unchanged deps are
# cached as a Docker layer; the real sources are copied afterwards.
COPY parser-web/Cargo.toml parser-web/Cargo.lock ./
RUN mkdir -p src/empty && printf '' > src/empty/lib.rs \
    && cargo build --release --target wasm32-unknown-unknown \
        --manifest-path Cargo.toml || true
RUN rm -rf src

# -- Actual frontend sources --
COPY parser-web/src ./src
COPY parser-web/index.html ./
COPY parser-web/style.css ./
COPY parser-web/static ./static
COPY parser-web/.well-known ./.well-known
COPY parser-web/scripts ./scripts
COPY parser-web/Trunk.toml ./

# `--release` also runs the post-build compress hook (compress-dist.sh).
RUN trunk build --release
# Remove pre-compressed siblings; build.rs excludes them from the embed anyway.
RUN find dist -name '*.gz' -delete -o -name '*.br' -delete

# ===================== Planner =====================
FROM base AS planner
COPY parser-api ./parser-api
WORKDIR /app/parser-api
RUN cargo chef prepare --recipe-path recipe.json

# ===================== Cacher =====================
# Pre-build all dependency crates so the release build below is incremental.
FROM base AS cacher
WORKDIR /app/parser-api
COPY --from=planner /app/parser-api/recipe.json ./recipe.json
# build.rs reads ../parser-web/dist; make it resolvable in this cwd for cook.
COPY --from=frontend /web/dist /app/parser-web/dist
RUN cargo chef cook --release --recipe-path recipe.json

# ===================== Builder =====================
FROM cacher AS builder
ARG FEATURES=""
# cwd is /app/parser-api (inherited from cacher). dist must exist for build.rs.
COPY --from=frontend /web/dist /app/parser-web/dist
COPY parser-api/ .
RUN cargo build --release --bin e621-account-parser-api ${FEATURES:+--features "$FEATURES"} \
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
COPY --from=planner /app/parser-api/migrations ./migrations

ENV RUST_BACKTRACE=1

# The binary binds to Rocket.toml's :8080 (the actual listening port).
EXPOSE 8080

CMD ["/usr/local/bin/e621-account-parser-api"]
