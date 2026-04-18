# --- Stage 1: Planner (collect build graph)
FROM rust:slim-trixie AS planner
WORKDIR /app
RUN cargo install cargo-chef --locked
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# --- Stage 2: Cacher (build only dependencies)
FROM rust:slim-trixie AS cacher
WORKDIR /app
RUN cargo install cargo-chef --locked
COPY --from=planner /app/recipe.json recipe.json
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
RUN cargo chef cook --release --recipe-path recipe.json

# --- Stage 3: Builder (compile your binary)
FROM rust:slim-trixie AS builder
ARG BIN_NAME=e621-account-parser-api
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

COPY --from=cacher /app/target target
COPY --from=cacher /usr/local/cargo /usr/local/cargo

COPY . .
RUN cargo build --release --bin ${BIN_NAME}

# --- Stage 4: Runtime (small, non-root image)
FROM debian:bookworm-slim AS runtime
ARG BIN_NAME=e621-account-parser-api
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/${BIN_NAME} /usr/local/bin/${BIN_NAME}
COPY --from=builder /app/migrations ./migrations

EXPOSE 8080

ENV RUST_BACKTRACE=1
ENV CARGO_MANIFEST_DIR="/usr/local/bin"

CMD ["/usr/local/bin/e621-account-parser-api"]