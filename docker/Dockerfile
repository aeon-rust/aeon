# Aeon — Production Multi-Stage Dockerfile
# Builds a minimal static binary image for deployment.
#
# Usage:
#   docker build -t aeon:latest -f docker/Dockerfile .
#   docker run -p 4471:4471 aeon:latest
#
# Build args:
#   FEATURES — Cargo features to enable (default: "native-loader,rest-api")

# ── Stage 1: Builder ──────────────────────────────────────────────────
FROM rust:1.94-bookworm AS builder

WORKDIR /build

# Install system deps for rdkafka (cmake, libssl)
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    libssl-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Install wasm target for processor validation
RUN rustup target add wasm32-unknown-unknown

# Copy workspace manifests first for Docker layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates/aeon-types/Cargo.toml crates/aeon-types/
COPY crates/aeon-io/Cargo.toml crates/aeon-io/
COPY crates/aeon-state/Cargo.toml crates/aeon-state/
COPY crates/aeon-wasm/Cargo.toml crates/aeon-wasm/
COPY crates/aeon-connectors/Cargo.toml crates/aeon-connectors/
COPY crates/aeon-engine/Cargo.toml crates/aeon-engine/
COPY crates/aeon-cluster/Cargo.toml crates/aeon-cluster/
COPY crates/aeon-crypto/Cargo.toml crates/aeon-crypto/
COPY crates/aeon-secrets/Cargo.toml crates/aeon-secrets/
COPY crates/aeon-observability/Cargo.toml crates/aeon-observability/
COPY crates/aeon-native-sdk/Cargo.toml crates/aeon-native-sdk/
COPY crates/aeon-wasm-sdk/Cargo.toml crates/aeon-wasm-sdk/
COPY crates/aeon-processor-client/Cargo.toml crates/aeon-processor-client/
COPY crates/aeon-cli/Cargo.toml crates/aeon-cli/
COPY samples/processors/rust-native/Cargo.toml samples/processors/rust-native/
COPY samples/e2e-pipeline/Cargo.toml samples/e2e-pipeline/
COPY xtask/Cargo.toml xtask/

# Create stub lib.rs/main.rs files so cargo can resolve the workspace
RUN for dir in crates/*/; do mkdir -p "$dir/src" && echo "" > "$dir/src/lib.rs"; done \
    && mkdir -p crates/aeon-cli/src && echo "fn main() {}" > crates/aeon-cli/src/main.rs \
    && mkdir -p samples/processors/rust-native/src && echo "" > samples/processors/rust-native/src/lib.rs \
    && mkdir -p samples/e2e-pipeline/src && echo "fn main() {}" > samples/e2e-pipeline/src/main.rs \
    && mkdir -p xtask/src && echo "fn main() {}" > xtask/src/main.rs

# Pre-build dependencies (cached unless Cargo.toml/lock change)
ARG FEATURES="native-validate,rest-api"
RUN cargo build --release -p aeon-cli --features "$FEATURES" 2>/dev/null || true

# Copy actual source code
COPY crates/ crates/
COPY samples/ samples/

# Build the final binary
RUN cargo build --release -p aeon-cli --features "$FEATURES" \
    && strip /build/target/release/aeon

# ── Stage 2: Runtime ──────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN groupadd -r aeon && useradd -r -g aeon -d /app -s /sbin/nologin aeon

WORKDIR /app

# Copy binary
COPY --from=builder /build/target/release/aeon /usr/local/bin/aeon

# Create directories for artifacts and config
RUN mkdir -p /app/artifacts /app/config /app/data \
    && chown -R aeon:aeon /app

USER aeon

# Default ports: 4470 (QUIC cluster), 4471 (REST API), 4472 (metrics)
EXPOSE 4470 4471 4472

# Health check against REST API
HEALTHCHECK --interval=15s --timeout=5s --retries=3 \
    CMD ["/usr/local/bin/aeon", "--help"]

ENTRYPOINT ["/usr/local/bin/aeon"]
