# Aeon Publishing Guide

> How to publish Aeon to crates.io, Docker Hub, and GitHub Releases.
> Covers crate ordering, metadata requirements, Docker image strategy,
> pre-built binaries, and CI/CD automation.
>
> **Status**: Pre-publish. All crates compile and test (820 tests passing).
> Publishing pipeline is designed and documented but has not been executed yet.
> Crate names (`aeon-*`) and Docker Hub org (`aeonrust`) should be reserved
> before first public release.

---

## Table of Contents

1. [Overview — Distribution Channels](#1-overview--distribution-channels)
2. [crates.io — Rust Crate Registry](#2-cratesio--rust-crate-registry)
3. [Docker Hub — Container Images](#3-docker-hub--container-images)
4. [GitHub Releases — Pre-built Binaries](#4-github-releases--pre-built-binaries)
5. [CI/CD Automation](#5-cicd-automation)
6. [Versioning Strategy](#6-versioning-strategy)
7. [Pre-Publish Checklist](#7-pre-publish-checklist)

---

## 1. Overview — Distribution Channels

Aeon is distributed through three channels, serving different users:

| Channel | What | Who | Install Method |
|---------|------|-----|---------------|
| **crates.io** | Library crates + CLI | Rust developers building on Aeon | `cargo add aeon-types`, `cargo install aeon-cli` |
| **Docker Hub** | Container images | Operators deploying Aeon | `docker pull aeonrust/aeon:latest` |
| **GitHub Releases** | Pre-built binaries | Users who want the CLI without compiling | Download from releases page |

All three channels are versioned in lockstep (e.g., v0.1.0 everywhere).

### User Personas and Channels

```
Rust developer building a custom processor:
  → cargo add aeon-types aeon-wasm-sdk
  → Writes processor, compiles to .wasm
  → Deploys to running Aeon instance

Rust developer building a custom connector:
  → cargo add aeon-types aeon-connectors
  → Implements Source/Sink traits
  → Integrates into their own pipeline binary

Operator deploying Aeon:
  → docker pull aeonrust/aeon:latest
  → docker compose up
  → Manages via REST API or CLI

Developer running Aeon locally:
  → cargo install aeon-cli
  → aeon dev up (starts Redpanda)
  → aeon apply -f pipeline.yaml
```

---

## 2. crates.io — Rust Crate Registry

### 2.1 Crate Inventory

All publishable crates share the `aeon-` prefix for discoverability. They all
live in the same crates.io registry (there is only one public Cargo registry).

#### Published as Libraries (12 crates)

| Crate | Description | Internal Deps | Key Users |
|-------|-------------|---------------|-----------|
| `aeon-types` | Event, Output, traits, errors | None | Every Aeon user |
| `aeon-wasm-sdk` | Wasm processor SDK (`no_std`) | None | Wasm processor authors |
| `aeon-native-sdk` | Native processor SDK (FFI) | `aeon-types` | Native processor authors |
| `aeon-io` | Tokio I/O abstraction | `aeon-types` | Advanced users |
| `aeon-state` | Multi-tier state store | `aeon-types` | Stateful processor authors |
| `aeon-wasm` | Wasmtime host runtime | `aeon-types` | Pipeline operators |
| `aeon-crypto` | Encryption, signing, Merkle | `aeon-types` | Security-critical deployments |
| `aeon-observability` | OTLP, logging, metrics | `aeon-types` | All production deployments |
| `aeon-connectors` | 22 source/sink implementations | `aeon-types` | Pipeline builders |
| `aeon-cluster` | Raft consensus + QUIC | `aeon-types` | Multi-node clusters |
| `aeon-processor-client` | T3/T4 network processor SDK | `aeon-types` | Rust T3/T4 processor authors |
| `aeon-engine` | Pipeline orchestrator | `aeon-types`, `aeon-native-sdk` (opt) | Pipeline operators |

#### Published as Binary (1 crate)

| Crate | Binary Name | Install | Deps |
|-------|-------------|---------|------|
| `aeon-cli` | `aeon` | `cargo install aeon-cli` | `aeon-types`, `aeon-engine` |

#### NOT Published (samples/examples)

| Crate | Reason |
|-------|--------|
| `aeon-e2e-pipeline` | Example application (users build their own) |
| `aeon-sample-rust-native` | Sample processor (reference only) |

### 2.2 Dependency Graph and Publishing Order

Crates must be published bottom-up (dependencies before dependents). Each tier
can be published in parallel within the tier.

```
Tier 1 (no internal deps — publish first):
  ├── aeon-types           ← foundation, everything depends on this
  └── aeon-wasm-sdk        ← zero dependencies, no_std

Tier 2 (depends on aeon-types only):
  ├── aeon-native-sdk
  ├── aeon-processor-client
  ├── aeon-crypto
  ├── aeon-io
  ├── aeon-state
  ├── aeon-wasm
  ├── aeon-observability
  ├── aeon-connectors
  └── aeon-cluster

Tier 3 (depends on Tier 1 + 2):
  └── aeon-engine          ← depends on aeon-types, optionally aeon-native-sdk

Tier 4 (binary, depends on Tier 1-3):
  └── aeon-cli             ← depends on aeon-types, aeon-engine
```

### 2.3 Required Cargo.toml Metadata for crates.io

Each crate's `Cargo.toml` needs these fields for crates.io acceptance:

```toml
[package]
name = "aeon-types"
description = "Core types, traits, and error definitions for the Aeon stream processing engine"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

# Required for crates.io
repository = "https://github.com/aeon-rust/aeon"
homepage = "https://github.com/aeon-rust/aeon"
documentation = "https://docs.rs/aeon-types"
readme = "../../README.md"
keywords = ["stream-processing", "event-driven", "pipeline", "kafka", "wasm"]
categories = ["data-structures", "asynchronous"]

# Exclude non-essential files from the published crate
[package.metadata.docs.rs]
all-features = true
```

The workspace `Cargo.toml` should define shared metadata:

```toml
[workspace.package]
version = "0.1.0"
edition = "2024"
license = "Apache-2.0"
rust-version = "1.85"
repository = "https://github.com/aeon-rust/aeon"
homepage = "https://github.com/aeon-rust/aeon"
```

### 2.4 Workspace Dependency References for Publishing

When publishing to crates.io, `path` dependencies must also specify a `version`
so that the published crate resolves from the registry, not from local paths.

Update workspace `Cargo.toml`:

```toml
[workspace.dependencies]
# Internal crates — version required for crates.io publishing
aeon-types = { path = "crates/aeon-types", version = "0.1.0" }
aeon-io = { path = "crates/aeon-io", version = "0.1.0" }
aeon-state = { path = "crates/aeon-state", version = "0.1.0" }
aeon-engine = { path = "crates/aeon-engine", version = "0.1.0" }
aeon-connectors = { path = "crates/aeon-connectors", version = "0.1.0" }
aeon-native-sdk = { path = "crates/aeon-native-sdk", version = "0.1.0" }
```

And in individual crate `Cargo.toml` files that reference siblings:

```toml
# aeon-engine/Cargo.toml
[dependencies]
aeon-types = { workspace = true }
# This resolves to { path = "../aeon-types", version = "0.1.0" }
# Locally: uses the path. On crates.io: uses the version from registry.
```

### 2.5 Publishing Commands

```bash
# Authenticate (one-time)
cargo login

# Dry run (validate without publishing)
cargo publish --dry-run -p aeon-types
cargo publish --dry-run -p aeon-wasm-sdk

# Publish Tier 1
cargo publish -p aeon-types
cargo publish -p aeon-wasm-sdk

# Wait for crates.io index update (~60 seconds)
sleep 60

# Publish Tier 2
cargo publish -p aeon-native-sdk
cargo publish -p aeon-crypto
cargo publish -p aeon-io
cargo publish -p aeon-state
cargo publish -p aeon-wasm
cargo publish -p aeon-observability
cargo publish -p aeon-connectors
cargo publish -p aeon-cluster

# Wait for index update
sleep 60

# Publish Tier 3
cargo publish -p aeon-engine

# Wait for index update
sleep 60

# Publish Tier 4 (binary)
cargo publish -p aeon-cli
```

### 2.6 Excluding Samples from Publishing

Samples should be marked as not publishable:

```toml
# samples/e2e-pipeline/Cargo.toml
[package]
publish = false

# samples/processors/rust-native/Cargo.toml
[package]
publish = false
```

### 2.7 Keyword and Category Allocation

Different crates target different search terms on crates.io:

| Crate | Keywords | Categories |
|-------|----------|------------|
| `aeon-types` | stream-processing, event-driven, pipeline, kafka, wasm | data-structures, asynchronous |
| `aeon-wasm-sdk` | wasm, processor, stream-processing, no-std, sdk | wasm, no-std |
| `aeon-connectors` | kafka, redpanda, nats, mqtt, connector | asynchronous, network-programming |
| `aeon-engine` | stream-processing, pipeline, orchestration, spsc | concurrency, asynchronous |
| `aeon-cli` | stream-processing, cli, pipeline, devops | command-line-utilities |

---

## 3. Docker Hub — Container Images

### 3.1 Image Strategy

| Image | Tag | Contents | Size Target | Use Case |
|-------|-----|----------|-------------|----------|
| `aeonrust/aeon` | `latest`, `0.1.0` | CLI + all connectors + REST API | ~80MB | Production |
| `aeonrust/aeon` | `slim` | CLI + kafka + memory + blackhole only | ~50MB | Minimal production |
| `aeonrust/aeon-bench` | `latest`, `0.1.0` | Benchmark binaries | ~100MB | Performance testing |

### 3.2 Production Dockerfile

The existing `Dockerfile` needs to be updated to build the CLI instead of just
the E2E pipeline sample. The production image should include:

```dockerfile
# docker/Dockerfile.release — Production Aeon image
# Usage:
#   docker build -t aeonrust/aeon:latest -f docker/Dockerfile.release .
#   docker build -t aeonrust/aeon:0.1.0 -f docker/Dockerfile.release .

# ── Stage 1: Rust builder ─────────────────────────────────────────────
FROM rust:1.85-bookworm AS builder

RUN apt-get update && apt-get install -y \
    cmake libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace manifests first (Docker layer caching)
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY samples/ samples/

# Build the CLI binary with all features
ARG FEATURES="default"
RUN cargo build --release -p aeon-cli && \
    cp target/release/aeon /build/aeon

# Also build the E2E pipeline binaries
RUN cargo build --release -p aeon-e2e-pipeline && \
    cp target/release/aeon-pipeline /build/aeon-pipeline && \
    cp target/release/aeon-producer /build/aeon-producer

# ── Stage 2: Runtime ──────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Non-root user (OWASP A05)
RUN groupadd -r aeon && useradd -r -g aeon -d /app aeon

WORKDIR /app

# Binaries
COPY --from=builder /build/aeon /usr/local/bin/aeon
COPY --from=builder /build/aeon-pipeline /usr/local/bin/aeon-pipeline
COPY --from=builder /build/aeon-producer /usr/local/bin/aeon-producer

# Directories for runtime data
RUN mkdir -p /app/artifacts /app/data/checkpoints && \
    chown -R aeon:aeon /app

USER aeon

# Default: start the REST API server
ENTRYPOINT ["aeon"]
CMD ["--help"]

EXPOSE 4470/udp 4471/tcp 4472/udp

LABEL org.opencontainers.image.source="https://github.com/aeon-rust/aeon"
LABEL org.opencontainers.image.description="Aeon real-time data processing engine"
LABEL org.opencontainers.image.licenses="Apache-2.0"
```

### 3.3 Docker Hub Publishing

```bash
# Build and tag
docker build -t aeonrust/aeon:latest -t aeonrust/aeon:0.1.0 \
  -f docker/Dockerfile.release .

# Multi-platform build (linux/amd64 + linux/arm64)
docker buildx build --platform linux/amd64,linux/arm64 \
  -t aeonrust/aeon:latest -t aeonrust/aeon:0.1.0 \
  -f docker/Dockerfile.release --push .

# Benchmark image
docker build -t aeonrust/aeon-bench:latest -t aeonrust/aeon-bench:0.1.0 \
  -f docker/Dockerfile.bench .
docker push aeonrust/aeon-bench:latest
docker push aeonrust/aeon-bench:0.1.0
```

### 3.4 Docker Hub Repository Structure

```
aeonrust/
  aeon            — Main engine image (CLI + connectors + REST API)
    :latest       — Latest stable release
    :0.1.0        — Specific version
    :0.1          — Minor version (latest patch)
    :slim         — Minimal connectors
    :nightly      — Built from main branch nightly (CI)
  aeon-bench      — Benchmark runner image
    :latest
    :0.1.0
```

### 3.5 Docker Compose for End Users

Users pull from Docker Hub instead of building locally:

```yaml
# docker-compose.prod.yml (updated for Docker Hub)
services:
  aeon:
    image: aeonrust/aeon:0.1.0   # <-- pull from Docker Hub
    container_name: aeon-engine
    ports:
      - "4471:4471"
    environment:
      AEON_API_TOKEN: ${AEON_API_TOKEN:?Set AEON_API_TOKEN}
      AEON_BROKERS: "redpanda:9092"
      AEON_CHECKPOINT_DIR: /app/data/checkpoints
      AEON_LOG_FORMAT: json
    volumes:
      - aeon-data:/app/data
```

---

## 4. GitHub Releases — Pre-built Binaries

### 4.1 Binary Matrix

Pre-built binaries for the `aeon` CLI, published as GitHub Release assets:

| Platform | Architecture | Binary Name | Notes |
|----------|-------------|-------------|-------|
| Linux | x86_64 | `aeon-x86_64-unknown-linux-gnu` | Most common server platform |
| Linux | aarch64 | `aeon-aarch64-unknown-linux-gnu` | ARM servers (AWS Graviton, etc.) |
| macOS | x86_64 | `aeon-x86_64-apple-darwin` | Intel Macs |
| macOS | aarch64 | `aeon-aarch64-apple-darwin` | Apple Silicon (M1/M2/M3/M4) |
| Windows | x86_64 | `aeon-x86_64-pc-windows-msvc.exe` | Windows servers/dev |

### 4.2 Release Asset Structure

Each GitHub Release (e.g., `v0.1.0`) includes:

```
aeon-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
aeon-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
aeon-v0.1.0-aarch64-unknown-linux-gnu.tar.gz
aeon-v0.1.0-aarch64-unknown-linux-gnu.tar.gz.sha256
aeon-v0.1.0-x86_64-apple-darwin.tar.gz
aeon-v0.1.0-x86_64-apple-darwin.tar.gz.sha256
aeon-v0.1.0-aarch64-apple-darwin.tar.gz
aeon-v0.1.0-aarch64-apple-darwin.tar.gz.sha256
aeon-v0.1.0-x86_64-pc-windows-msvc.zip
aeon-v0.1.0-x86_64-pc-windows-msvc.zip.sha256
```

Each archive contains:
- `aeon` binary (or `aeon.exe` on Windows)
- `LICENSE`
- `README.md`

### 4.3 Install Methods Summary

```bash
# Option 1: cargo install (build from source)
cargo install aeon-cli

# Option 2: Docker (pre-built image)
docker pull aeonrust/aeon:latest
docker run aeonrust/aeon:latest --help

# Option 3: GitHub Release (pre-built binary)
curl -L https://github.com/aeon-rust/aeon/releases/latest/download/aeon-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo mv aeon /usr/local/bin/

# Option 4: Homebrew (future, macOS/Linux)
# brew install aeonrust/tap/aeon

# Option 5: OS package managers (future)
# apt install aeon        # Debian/Ubuntu
# dnf install aeon        # Fedora/RHEL
# pacman -S aeon          # Arch Linux
# scoop install aeon      # Windows
```

---

## 5. CI/CD Automation

### 5.1 GitHub Actions — Release Workflow

```yaml
# .github/workflows/release.yml
name: Release

on:
  push:
    tags:
      - 'v*'

permissions:
  contents: write

jobs:
  # ── Step 1: Validate ────────────────────────────────────────────────
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --workspace
      - run: cargo clippy --workspace -- -D warnings
      - run: cargo fmt --all -- --check

  # ── Step 2: Publish to crates.io ────────────────────────────────────
  publish-crates:
    needs: validate
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Publish crates (tier order)
        env:
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
        run: |
          # Tier 1
          cargo publish -p aeon-types --no-verify
          cargo publish -p aeon-wasm-sdk --no-verify
          sleep 30
          # Tier 2
          cargo publish -p aeon-native-sdk --no-verify
          cargo publish -p aeon-crypto --no-verify
          cargo publish -p aeon-io --no-verify
          cargo publish -p aeon-state --no-verify
          cargo publish -p aeon-wasm --no-verify
          cargo publish -p aeon-observability --no-verify
          cargo publish -p aeon-connectors --no-verify
          cargo publish -p aeon-cluster --no-verify
          sleep 30
          # Tier 3-4
          cargo publish -p aeon-engine --no-verify
          sleep 30
          cargo publish -p aeon-cli --no-verify

  # ── Step 3: Build binaries for all platforms ────────────────────────
  build-binaries:
    needs: validate
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            artifact: aeon
          - os: ubuntu-latest
            target: aarch64-unknown-linux-gnu
            artifact: aeon
          - os: macos-latest
            target: x86_64-apple-darwin
            artifact: aeon
          - os: macos-latest
            target: aarch64-apple-darwin
            artifact: aeon
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            artifact: aeon.exe
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}
      - name: Install cross-compilation deps (Linux aarch64)
        if: matrix.target == 'aarch64-unknown-linux-gnu'
        run: |
          sudo apt-get update
          sudo apt-get install -y gcc-aarch64-linux-gnu
      - name: Build
        run: cargo build --release --target ${{ matrix.target }} -p aeon-cli
      - name: Package
        run: |
          mkdir -p dist
          cp target/${{ matrix.target }}/release/${{ matrix.artifact }} dist/
          cp LICENSE README.md dist/
      - uses: actions/upload-artifact@v4
        with:
          name: aeon-${{ matrix.target }}
          path: dist/

  # ── Step 4: Create GitHub Release ───────────────────────────────────
  github-release:
    needs: build-binaries
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
      - name: Create release archives and checksums
        run: |
          for dir in aeon-*/; do
            target="${dir%/}"
            if [[ "$target" == *windows* ]]; then
              zip -j "${target}.zip" "$dir"/*
              sha256sum "${target}.zip" > "${target}.zip.sha256"
            else
              tar czf "${target}.tar.gz" -C "$dir" .
              sha256sum "${target}.tar.gz" > "${target}.tar.gz.sha256"
            fi
          done
      - uses: softprops/action-gh-release@v2
        with:
          files: |
            aeon-*.tar.gz
            aeon-*.tar.gz.sha256
            aeon-*.zip
            aeon-*.zip.sha256

  # ── Step 5: Publish Docker images ───────────────────────────────────
  publish-docker:
    needs: validate
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-buildx-action@v3
      - uses: docker/login-action@v3
        with:
          username: ${{ secrets.DOCKERHUB_USERNAME }}
          password: ${{ secrets.DOCKERHUB_TOKEN }}
      - name: Extract version from tag
        id: version
        run: echo "VERSION=${GITHUB_REF#refs/tags/v}" >> $GITHUB_OUTPUT
      - uses: docker/build-push-action@v5
        with:
          context: .
          file: docker/Dockerfile.release
          platforms: linux/amd64,linux/arm64
          push: true
          tags: |
            aeonrust/aeon:latest
            aeonrust/aeon:${{ steps.version.outputs.VERSION }}
```

### 5.2 CI — Pull Request Checks

```yaml
# .github/workflows/ci.yml
name: CI

on:
  pull_request:
  push:
    branches: [main]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --workspace
      - run: cargo clippy --workspace -- -D warnings
      - run: cargo fmt --all -- --check

  # Verify all crates can be published (dry run)
  publish-check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: |
          cargo publish --dry-run -p aeon-types
          cargo publish --dry-run -p aeon-wasm-sdk
```

---

## 6. Versioning Strategy

### Semantic Versioning

All crates use **SemVer** and are versioned in lockstep:

```
v0.1.0 → v0.1.1 → v0.2.0 → v1.0.0
```

- **Patch** (0.1.0 → 0.1.1): Bug fixes, no API changes
- **Minor** (0.1.x → 0.2.0): New features, backward-compatible API changes
- **Major** (0.x → 1.0): Breaking API changes

### Lockstep vs Independent Versioning

**Recommendation: lockstep versioning** (all crates share the same version).

Rationale:
- Aeon is a cohesive system, not a collection of unrelated libraries
- Users expect `aeon-types 0.2.0` to work with `aeon-engine 0.2.0`
- Eliminates version matrix complexity
- Simpler release process

The workspace already uses `version.workspace = true` which enforces lockstep.

### Version Lifecycle

```
0.1.0  — Initial public release (current)
0.2.0  — Based on community feedback, API refinements
0.x.y  — Pre-1.0: API may change between minor versions
1.0.0  — Stable API guarantee (SemVer contract begins)
```

---

## 7. Pre-Publish Checklist

### Before First Publish

- [ ] Choose GitHub org name (e.g., `aeonrust`)
- [ ] Reserve crate names on crates.io (publish placeholder 0.0.1 if needed)
- [ ] Create Docker Hub organization (`aeonrust`)
- [ ] Set up GitHub repository secrets:
  - `CARGO_REGISTRY_TOKEN` — crates.io API token
  - `DOCKERHUB_USERNAME` — Docker Hub username
  - `DOCKERHUB_TOKEN` — Docker Hub access token
- [ ] Add `repository`, `homepage`, `keywords`, `categories` to all crate Cargo.tomls
- [ ] Add `version = "0.1.0"` alongside `path` for all workspace internal deps
- [ ] Mark sample crates with `publish = false`
- [ ] Verify `cargo publish --dry-run` passes for all 12 crates
- [ ] Verify Docker multi-platform build works
- [ ] Write crates.io README (can be workspace README or per-crate)

### Per-Release Checklist

- [ ] Update version in workspace `Cargo.toml` (single place, propagates to all)
- [ ] Update CHANGELOG.md (if maintained)
- [ ] Run full test suite: `cargo test --workspace`
- [ ] Run clippy: `cargo clippy --workspace -- -D warnings`
- [ ] Run fmt check: `cargo fmt --all -- --check`
- [ ] Tag the release: `git tag v0.1.0 && git push --tags`
- [ ] CI workflow handles the rest (crates.io, Docker Hub, GitHub Release)

### Metadata Updates Needed

The following files need one-time updates before first publish:

**Workspace Cargo.toml** — add repository, homepage, and version to internal deps:

```toml
[workspace.package]
repository = "https://github.com/aeon-rust/aeon"
homepage = "https://github.com/aeon-rust/aeon"

[workspace.dependencies]
aeon-types = { path = "crates/aeon-types", version = "0.1.0" }
aeon-io = { path = "crates/aeon-io", version = "0.1.0" }
aeon-state = { path = "crates/aeon-state", version = "0.1.0" }
aeon-engine = { path = "crates/aeon-engine", version = "0.1.0" }
aeon-connectors = { path = "crates/aeon-connectors", version = "0.1.0" }
aeon-native-sdk = { path = "crates/aeon-native-sdk", version = "0.1.0" }
```

**Each crate's Cargo.toml** — add keywords and categories:

```toml
[package]
keywords = ["stream-processing", "event-driven", "pipeline", "kafka", "wasm"]
categories = ["asynchronous"]
repository.workspace = true
homepage.workspace = true
```

**Sample crates** — mark as not publishable:

```toml
[package]
publish = false
```
