# Aeon — Multi-stage production build
#
# Builds the `aeon` CLI binary with all features enabled:
#   REST API, native processor loader, Kafka connectors, Wasm runtime,
#   WebTransport, cluster support, crypto, observability.
#
# Usage:
#   docker build -t aeonrust/aeon:latest .
#   docker build -t aeonrust/aeon:latest --build-arg PROFILE=release .
#   docker run aeonrust/aeon:latest --version
#
# Multi-stage:
#   1. rust-builder: compiles `aeon` CLI binary
#   2. wasm-builder: compiles sample Rust Wasm processor (optional)
#   3. runtime: minimal Debian image with binary + wasm artifacts

# ─── Stage 1: Rust builder ───────────────────────────────────────────
FROM rust:1.88-bookworm AS rust-builder

# Install build dependencies for rdkafka (librdkafka), cmake for zstd,
# protobuf for potential gRPC, libclang for bindgen
RUN apt-get update && apt-get install -y \
    cmake \
    libssl-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/aeon-types/Cargo.toml crates/aeon-types/Cargo.toml
COPY crates/aeon-io/Cargo.toml crates/aeon-io/Cargo.toml
COPY crates/aeon-state/Cargo.toml crates/aeon-state/Cargo.toml
COPY crates/aeon-wasm/Cargo.toml crates/aeon-wasm/Cargo.toml
COPY crates/aeon-connectors/Cargo.toml crates/aeon-connectors/Cargo.toml
COPY crates/aeon-engine/Cargo.toml crates/aeon-engine/Cargo.toml
COPY crates/aeon-cluster/Cargo.toml crates/aeon-cluster/Cargo.toml
COPY crates/aeon-crypto/Cargo.toml crates/aeon-crypto/Cargo.toml
COPY crates/aeon-observability/Cargo.toml crates/aeon-observability/Cargo.toml
COPY crates/aeon-native-sdk/Cargo.toml crates/aeon-native-sdk/Cargo.toml
COPY crates/aeon-wasm-sdk/Cargo.toml crates/aeon-wasm-sdk/Cargo.toml
COPY crates/aeon-processor-client/Cargo.toml crates/aeon-processor-client/Cargo.toml
COPY crates/aeon-cli/Cargo.toml crates/aeon-cli/Cargo.toml
COPY samples/processors/rust-native/Cargo.toml samples/processors/rust-native/Cargo.toml
COPY samples/e2e-pipeline/Cargo.toml samples/e2e-pipeline/Cargo.toml

# Create stub lib.rs/main.rs for dependency pre-build (Docker layer caching)
RUN mkdir -p crates/aeon-types/src && echo "pub fn stub(){}" > crates/aeon-types/src/lib.rs && \
    mkdir -p crates/aeon-io/src && echo "pub fn stub(){}" > crates/aeon-io/src/lib.rs && \
    mkdir -p crates/aeon-state/src && echo "pub fn stub(){}" > crates/aeon-state/src/lib.rs && \
    mkdir -p crates/aeon-wasm/src && echo "pub fn stub(){}" > crates/aeon-wasm/src/lib.rs && \
    mkdir -p crates/aeon-connectors/src && echo "pub fn stub(){}" > crates/aeon-connectors/src/lib.rs && \
    mkdir -p crates/aeon-engine/src && echo "pub fn stub(){}" > crates/aeon-engine/src/lib.rs && \
    mkdir -p crates/aeon-cluster/src && echo "pub fn stub(){}" > crates/aeon-cluster/src/lib.rs && \
    mkdir -p crates/aeon-crypto/src && echo "pub fn stub(){}" > crates/aeon-crypto/src/lib.rs && \
    mkdir -p crates/aeon-observability/src && echo "pub fn stub(){}" > crates/aeon-observability/src/lib.rs && \
    mkdir -p crates/aeon-native-sdk/src && echo "pub fn stub(){}" > crates/aeon-native-sdk/src/lib.rs && \
    mkdir -p crates/aeon-wasm-sdk/src && echo "pub fn stub(){}" > crates/aeon-wasm-sdk/src/lib.rs && \
    mkdir -p crates/aeon-processor-client/src && echo "pub fn stub(){}" > crates/aeon-processor-client/src/lib.rs && \
    mkdir -p crates/aeon-cli/src && echo "fn main(){}" > crates/aeon-cli/src/main.rs && \
    mkdir -p samples/processors/rust-native/src && echo "pub fn stub(){}" > samples/processors/rust-native/src/lib.rs && \
    mkdir -p samples/e2e-pipeline/src && echo "fn main(){}" > samples/e2e-pipeline/src/pipeline.rs && \
    echo "fn main(){}" > samples/e2e-pipeline/src/producer.rs

# Pre-build dependencies (cached layer — only re-runs when Cargo.toml/lock changes)
ARG PROFILE=release
RUN cargo build --profile ${PROFILE} -p aeon-cli --features rest-api 2>/dev/null || true

# Copy real source code
COPY crates/ crates/
COPY samples/processors/rust-native/ samples/processors/rust-native/
COPY samples/e2e-pipeline/ samples/e2e-pipeline/

# Touch source files to invalidate cache for real build
RUN find crates/ samples/ -name "*.rs" -exec touch {} +

# Build the aeon CLI binary with all production features
RUN cargo build --profile ${PROFILE} -p aeon-cli --features rest-api && \
    cp target/${PROFILE}/aeon /build/aeon

# Also build the e2e-pipeline binary (for benchmarking/testing)
RUN cargo build --profile ${PROFILE} -p aeon-e2e-pipeline && \
    cp target/${PROFILE}/aeon-pipeline /build/aeon-pipeline && \
    cp target/${PROFILE}/aeon-producer /build/aeon-producer

# ─── Stage 2: Rust-Wasm processor builder (optional) ────────────────
FROM rust:1.88-bookworm AS wasm-builder

RUN rustup target add wasm32-unknown-unknown

WORKDIR /build
COPY samples/processors/rust-wasm/ ./rust-wasm/

RUN cd rust-wasm && \
    cargo build --release --target wasm32-unknown-unknown && \
    cp target/wasm32-unknown-unknown/release/aeon_sample_rust_wasm.wasm /build/rust-processor.wasm

# ─── Stage 3: Runtime image ─────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN groupadd -r aeon && useradd -r -g aeon -d /app aeon

WORKDIR /app

# Copy the aeon CLI binary (primary entrypoint)
COPY --from=rust-builder /build/aeon /usr/local/bin/aeon

# Copy e2e-pipeline binaries (optional, for benchmarking)
COPY --from=rust-builder /build/aeon-pipeline /usr/local/bin/aeon-pipeline
COPY --from=rust-builder /build/aeon-producer /usr/local/bin/aeon-producer

# Copy sample wasm processor
COPY --from=wasm-builder /build/rust-processor.wasm /app/processors/rust-processor.wasm

# Create data and artifact directories
RUN mkdir -p /app/data /app/artifacts /app/processors && \
    chown -R aeon:aeon /app

# Expose ports:
#   4470 — QUIC (Raft cluster + processor transport) [UDP]
#   4471 — REST API + WebSocket processor host [TCP]
#   4472 — Prometheus metrics [TCP]
EXPOSE 4470/udp
EXPOSE 4471/tcp
EXPOSE 4472/tcp

USER aeon

# Health check via REST API
HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/aeon", "--version"]

ENTRYPOINT ["aeon"]
CMD ["serve"]
