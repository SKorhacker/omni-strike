# syntax=docker/dockerfile:1.7
#
# Omni Strike API server (oa-server) — production image.
#
#   Mainnet read-only (default):
#     docker build -t omni-strike/oa-server:latest .
#   Testnet build (testnet market data + testnet execution code compiled in), separate tag:
#     docker build --build-arg CARGO_FEATURES=testnet -t omni-strike/oa-server:testnet .
#
# The image contains NO configuration and NO secrets: everything comes from the environment at
# run time (see deploy/.env.example). Database migrations are embedded in the binary
# (sqlx::migrate!) and applied at startup.

ARG RUST_IMAGE=rust:1.92-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim
# Semver requirement; pin an exact version (e.g. =0.1.71) for fully reproducible builds.
ARG CARGO_CHEF_VERSION=0.1.78

# ---------------------------------------------------------------------------------------------
# chef: base with cargo-chef for dependency-layer caching
# ---------------------------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS chef
ARG CARGO_CHEF_VERSION
# rust-toolchain.toml pins 1.92.0 (+ rustfmt/clippy). Pin the image's toolchain explicitly so
# rustup never tries to download components during a release build.
ENV RUSTUP_TOOLCHAIN=1.92.0 \
    CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked --version "${CARGO_CHEF_VERSION}"
WORKDIR /src

# ---------------------------------------------------------------------------------------------
# planner: compute the dependency recipe (changes only when Cargo.toml/Cargo.lock change)
# ---------------------------------------------------------------------------------------------
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

# ---------------------------------------------------------------------------------------------
# builder: build deps (cached layer), then the workspace
# ---------------------------------------------------------------------------------------------
FROM chef AS builder
# Empty = mainnet read-only build. "testnet" = TESTNET build. "live" does not compile, by design.
ARG CARGO_FEATURES=""
COPY --from=planner /src/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo chef cook --release --locked --recipe-path recipe.json -p oa-app --bin oa-server \
      ${CARGO_FEATURES:+--features ${CARGO_FEATURES}}
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Embedded at compile time (include_str!): the integrator guide served at /docs and the geo policy.
COPY docs/integrators.md ./docs/integrators.md
COPY config ./config
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo build --release --locked -p oa-app --bin oa-server \
      ${CARGO_FEATURES:+--features ${CARGO_FEATURES}} \
 && install -Dm755 target/release/oa-server /out/oa-server

# ---------------------------------------------------------------------------------------------
# runtime: minimal Debian, non-root, tini as PID 1
# ---------------------------------------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime
ARG CARGO_FEATURES=""
LABEL org.opencontainers.image.title="omni-strike-oa-server" \
      org.opencontainers.image.description="Omni Strike options aggregator API (oa-server)" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="https://github.com/SKorhacker/omni-strike" \
      io.omnistrike.cargo-features="${CARGO_FEATURES}"
# libssl3: reqwest / tokio-tungstenite use native-tls (OpenSSL).
# curl: HEALTHCHECK only. tini: signal forwarding + zombie reaping.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3 curl tini \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 oa \
 && useradd --system --uid 10001 --gid oa --home-dir /app --shell /usr/sbin/nologin oa \
 && mkdir -p /app \
 && chown oa:oa /app
COPY --from=builder /out/oa-server /usr/local/bin/oa-server
WORKDIR /app
USER 10001:10001
ENV RUST_LOG=info \
    OA_LOG_FORMAT=json \
    OA_BIND=0.0.0.0:8080 \
    OA_ENV_FILE=/nonexistent/.env
EXPOSE 8080
# oa-server drains gracefully on SIGINT or SIGTERM (25 s max, then flushes observations).
STOPSIGNAL SIGTERM
# /health/ready = database reachable AND at least one venue catalog READY. The first catalogs
# take a few seconds after boot, hence the start period. /health/* never requires a token.
HEALTHCHECK --interval=15s --timeout=5s --start-period=60s --retries=4 \
  CMD curl -fsS -o /dev/null http://127.0.0.1:8080/health/ready || exit 1
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/oa-server"]
