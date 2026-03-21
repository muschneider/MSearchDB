# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build --workspace
cargo build --release
cargo check --workspace          # fastest type-check

# Test
cargo test --workspace           # all 220+ tests
cargo test -p msearchdb-<crate>  # single crate
cargo test -- --nocapture        # show stdout
cargo test -- --exact <test_name>  # specific test

# Lint & Format
cargo clippy --workspace -- -D warnings   # warnings are errors
cargo fmt --all
cargo fmt --all -- --check

# Run
cargo run -p msearchdb-node      # starts HTTP on :9200
```

Crate names: `msearchdb-core`, `msearchdb-storage`, `msearchdb-index`, `msearchdb-consensus`, `msearchdb-network`, `msearchdb-node`, `msearchdb-client`.

## Architecture

MSearchDB is a distributed NoSQL database with full-text search (Elasticsearch-like). The workspace has 7 crates in `crates/`:

```
core/       — Domain types, traits, query DSL, cluster routing
storage/    — RocksDB backend, WAL, memtable
index/      — Tantivy BM25 full-text search
consensus/  — Raft via openraft, state machine
network/    — gRPC inter-node (tonic), scatter-gather queries
node/       — HTTP REST API (axum, port 9200) — the binary
client/     — Stub (only re-exports core)
```

### Core Traits (`crates/core/src/traits.rs`)

All storage, indexing, and replication are abstracted behind async traits:
- `StorageBackend` — `put/get/delete/scan`
- `IndexBackend` — `index_document/search/delete`
- `ReplicationLog` — `append/read/commit`
- `HealthCheck`

### Write Path

```
HTTP PUT /collections/{name}/docs/{id}
  → handler (crates/node/src/handlers/documents.rs)
  → WriteBatcher (batches up to 100 writes / 10ms)
  → RaftNode::propose(RaftCommand::InsertDocument)
  → DbStateMachine::apply()
  → storage.put() + index.index_document()  [parallel]
```

### Read Path

Reads go directly to local storage/index (bypassing Raft), with an optional `moka` LRU cache (10k docs, 60s TTL) in `crates/node/src/cache.rs`.

### Cluster Routing

`ClusterRouter` in `crates/core/src/cluster_router.rs` uses a consistent-hash ring (SHA-256, 150 virtual nodes) to determine which nodes own a document. `ScatterGatherCoordinator` in `crates/network/src/scatter_gather.rs` fans out search queries, merges results, and re-ranks by BM25 score.

### AppState (`crates/node/src/state.rs`)

The single `Arc<AppState>` passed to all HTTP handlers holds:
- `RaftNode` — for writes
- `StorageBackend` impl — currently `InMemoryStorage` (not persisted)
- `IndexBackend` impl — currently `TantivyIndex::new_in_ram()`
- `ClusterManager` — gossip, phi-accrual failure detection
- Prometheus metrics

### Known Gaps (from STATUS.md)

- **No persistence**: Binary wires `InMemoryStorage` + in-RAM Tantivy; RocksDB backend exists but is unused
- **No multi-node**: gRPC server is never started; `StubNetworkFactory` returns `Unreachable` for all RPCs
- **No CLI args**: Config is hardcoded; can't run multiple nodes without code changes
- **client crate**: Empty stub — no HTTP client library

## Key Patterns

- **Newtype pattern**: `DocumentId(String)`, `NodeId(u64)` — construct via `.into()` or `::new()`
- **Error handling**: `thiserror` in `core/src/error.rs` → `DbError`/`DbResult`; HTTP mapping in `node/src/errors.rs`
- **Async**: `tokio` runtime, `async_trait` on all backend traits
- **`#[non_exhaustive]`** on public enums — always use `..` in match arms
- **Serde**: All domain types derive `Serialize`/`Deserialize`; DTOs are in `node/src/dto.rs`

## HTTP API (17 endpoints)

- `PUT/GET/DELETE /collections/:name`
- `PUT/GET/DELETE /collections/:name/docs/:id`
- `POST /collections/:name/search`
- `POST /collections/:name/bulk`
- `GET /cluster/health`, `GET /cluster/state`, `POST /cluster/join`
- `GET /admin/stats`, `POST /admin/refresh`, `POST /admin/snapshot`
- `PUT/GET/DELETE /aliases/:name`

Middleware stack (outermost-first): timeout (30s) → gzip compression → tracing → request-id → optional API-key auth.

## gRPC (`proto/msearchdb.proto`)

Two services: `NodeService` (Raft RPCs, join, health) and `QueryService` (search, get, scatter). Server code is complete in `crates/network/src/server.rs` but is never started by the binary.
