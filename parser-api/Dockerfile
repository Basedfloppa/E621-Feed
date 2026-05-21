# --- Base build image
FROM rust:slim-trixie AS chef-base
WORKDIR /app

# Build features (e.g. "perf_metrics"); override with --build-arg FEATURES=perf_metrics
ARG FEATURES=""

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-chef --locked

FROM chef-base AS planner

COPY . .

RUN cargo chef prepare --recipe-path recipe.json

FROM chef-base AS cacher

COPY --from=planner /app/recipe.json recipe.json

RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=cargo-target,target=/app/target \
    cargo chef cook --release --recipe-path recipe.json

FROM cacher AS builder
ARG FEATURES=""

COPY . .

RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=cargo-target,target=/app/target \
    cargo build --release --bin e621-account-parser-api ${FEATURES:+--features "$FEATURES"} \
    && cp /app/target/release/e621-account-parser-api /app/e621-account-parser-api \
    && strip /app/e621-account-parser-api

FROM debian:bookworm-slim AS runtime

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system app \
    && useradd --system --gid app --create-home --home-dir /app app

COPY --from=builder /app/e621-account-parser-api /usr/local/bin/e621-account-parser-api
COPY --from=builder /app/migrations ./migrations

ENV RUST_BACKTRACE=1

EXPOSE 8181

CMD ["/usr/local/bin/e621-account-parser-api"]