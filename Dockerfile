# Aeon — Multi-stage build for pipeline + producer + wasm processors
# Usage:
#   docker build -t aeon:latest .
#   docker build -t aeon:latest --build-arg PROFILE=release .
#
# Targets: aeon-pipeline, aeon-producer binaries + wasm processor files

# ─── Stage 1: Rust builder ───────────────────────────────────────────
FROM rust:1.85-bookworm AS rust-builder

# Install build dependencies for rdkafka (librdkafka), cmake for zstd
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
COPY crates/aeon-cli/Cargo.toml crates/aeon-cli/Cargo.toml
COPY samples/processors/rust-native/Cargo.toml samples/processors/rust-native/Cargo.toml
COPY samples/e2e-pipeline/Cargo.toml samples/e2e-pipeline/Cargo.toml

# Create stub lib.rs/main.rs for dependency pre-build
RUN mkdir -p crates/aeon-types/src && echo "pub fn stub(){}" > crates/aeon-types/src/lib.rs && \
    mkdir -p crates/aeon-io/src && echo "pub fn stub(){}" > crates/aeon-io/src/lib.rs && \
    mkdir -p crates/aeon-state/src && echo "pub fn stub(){}" > crates/aeon-state/src/lib.rs && \
    mkdir -p crates/aeon-wasm/src && echo "pub fn stub(){}" > crates/aeon-wasm/src/lib.rs && \
    mkdir -p crates/aeon-connectors/src && echo "pub fn stub(){}" > crates/aeon-connectors/src/lib.rs && \
    mkdir -p crates/aeon-engine/src && echo "pub fn stub(){}" > crates/aeon-engine/src/lib.rs && \
    mkdir -p crates/aeon-cluster/src && echo "pub fn stub(){}" > crates/aeon-cluster/src/lib.rs && \
    mkdir -p crates/aeon-crypto/src && echo "pub fn stub(){}" > crates/aeon-crypto/src/lib.rs && \
    mkdir -p crates/aeon-observability/src && echo "pub fn stub(){}" > crates/aeon-observability/src/lib.rs && \
    mkdir -p crates/aeon-cli/src && echo "fn main(){}" > crates/aeon-cli/src/main.rs && \
    mkdir -p samples/processors/rust-native/src && echo "pub fn stub(){}" > samples/processors/rust-native/src/lib.rs && \
    mkdir -p samples/e2e-pipeline/src && echo "fn main(){}" > samples/e2e-pipeline/src/pipeline.rs && \
    echo "fn main(){}" > samples/e2e-pipeline/src/producer.rs

# Pre-build dependencies (cached layer)
ARG PROFILE=release
RUN cargo build --profile ${PROFILE} -p aeon-e2e-pipeline 2>/dev/null || true

# Copy real source code
COPY crates/ crates/
COPY samples/e2e-pipeline/ samples/e2e-pipeline/
COPY samples/processors/rust-native/ samples/processors/rust-native/

# Touch source files to invalidate cache for real build
RUN find crates/ samples/ -name "*.rs" -exec touch {} +

# Build binaries
RUN cargo build --profile ${PROFILE} -p aeon-e2e-pipeline && \
    cp target/${PROFILE}/aeon-pipeline /build/aeon-pipeline && \
    cp target/${PROFILE}/aeon-producer /build/aeon-producer

# ─── Stage 2: Rust-Wasm builder ─────────────────────────────────────
FROM rust:1.85-bookworm AS wasm-builder

RUN rustup target add wasm32-unknown-unknown

WORKDIR /build
COPY samples/processors/rust-wasm/ ./rust-wasm/

RUN cd rust-wasm && \
    cargo build --release --target wasm32-unknown-unknown && \
    cp target/wasm32-unknown-unknown/release/aeon_sample_rust_wasm.wasm /build/rust-processor.wasm

# ─── Stage 3: AssemblyScript builder (optional, uses pre-built) ──────
FROM node:20-slim AS as-builder

WORKDIR /build
COPY samples/processors/assemblyscript-wasm/ ./as-wasm/

# Use pre-built wasm or rebuild
RUN if [ -f as-wasm/build/processor.wasm ]; then \
        cp as-wasm/build/processor.wasm /build/as-processor.wasm; \
    else \
        cd as-wasm && npm install && npx asc assembly/index.ts \
            --target release -o /build/as-processor.wasm; \
    fi

# ─── Stage 4: Runtime image ─────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN groupadd -r aeon && useradd -r -g aeon -d /app aeon

WORKDIR /app

# Copy binaries
COPY --from=rust-builder /build/aeon-pipeline /usr/local/bin/aeon-pipeline
COPY --from=rust-builder /build/aeon-producer /usr/local/bin/aeon-producer

# Copy wasm processors
COPY --from=wasm-builder /build/rust-processor.wasm /app/processors/rust-processor.wasm
COPY --from=as-builder /build/as-processor.wasm /app/processors/as-processor.wasm

RUN chown -R aeon:aeon /app
USER aeon

ENTRYPOINT ["aeon-pipeline"]
CMD ["--help"]
