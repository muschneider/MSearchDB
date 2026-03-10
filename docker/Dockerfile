# ---------------------------------------------------------------------------
# MSearchDB — Multi-stage Docker build
# ---------------------------------------------------------------------------
# Stage 1: Build the release binary
# Stage 2: Minimal runtime image
# ---------------------------------------------------------------------------

# ---- Builder stage ----
FROM rust:1.75-bookworm AS builder

WORKDIR /build

# Install system dependencies for RocksDB (libclang, librocksdb)
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang \
    libclang-dev \
    librocksdb-dev \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/Cargo.toml
COPY crates/storage/Cargo.toml crates/storage/Cargo.toml
COPY crates/index/Cargo.toml crates/index/Cargo.toml
COPY crates/consensus/Cargo.toml crates/consensus/Cargo.toml
COPY crates/network/Cargo.toml crates/network/Cargo.toml
COPY crates/node/Cargo.toml crates/node/Cargo.toml
COPY crates/client/Cargo.toml crates/client/Cargo.toml

# Create dummy source files for dependency caching
RUN mkdir -p crates/core/src && echo "pub fn _dummy() {}" > crates/core/src/lib.rs && \
    mkdir -p crates/storage/src && echo "pub fn _dummy() {}" > crates/storage/src/lib.rs && \
    mkdir -p crates/index/src && echo "pub fn _dummy() {}" > crates/index/src/lib.rs && \
    mkdir -p crates/consensus/src && echo "pub fn _dummy() {}" > crates/consensus/src/lib.rs && \
    mkdir -p crates/network/src && echo "pub fn _dummy() {}" > crates/network/src/lib.rs && \
    mkdir -p crates/node/src && echo "pub fn _dummy() {}" > crates/node/src/lib.rs && echo "fn main() {}" > crates/node/src/main.rs && \
    mkdir -p crates/client/src && echo "pub fn _dummy() {}" > crates/client/src/lib.rs

# Copy proto files (needed for network crate build script)
COPY proto/ proto/

# Build dependencies only (cached unless Cargo.toml/lock change)
RUN cargo build --release --workspace 2>/dev/null || true

# Copy actual source code
COPY crates/ crates/

# Touch all source files to ensure they're rebuilt
RUN find crates -name "*.rs" -exec touch {} +

# Build the release binary
RUN cargo build --release --bin msearchdb

# ---- Runtime stage ----
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    librocksdb7.8 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN groupadd -r msearchdb && useradd -r -g msearchdb -m msearchdb

# Create data directories
RUN mkdir -p /var/lib/msearchdb && chown msearchdb:msearchdb /var/lib/msearchdb
RUN mkdir -p /var/log/msearchdb && chown msearchdb:msearchdb /var/log/msearchdb

# Copy the binary from builder
COPY --from=builder /build/target/release/msearchdb /usr/local/bin/msearchdb

# Default configuration file
COPY docker/node.toml /etc/msearchdb/node.toml

USER msearchdb
WORKDIR /var/lib/msearchdb

# HTTP API
EXPOSE 9200
# gRPC inter-node
EXPOSE 9300
# Prometheus metrics
EXPOSE 9100

HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD curl -sf http://localhost:9200/_cluster/health || exit 1

ENTRYPOINT ["msearchdb"]
CMD ["--config", "/etc/msearchdb/node.toml", "--bootstrap", "--json-log"]
