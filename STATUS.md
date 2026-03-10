# MSearchDB — Status Report

> **Audit date:** 2026-03-07
> **Auditor:** Read-only codebase analysis (no modifications made)
> **Scope:** All 7 workspace crates, binary entry point, proto definitions, test infrastructure

---

## Overall Health

| Metric | Value |
|--------|-------|
| **Overall Score** | **78 / 100** |
| **Crates fully implemented** | 6 / 7 |
| **HTTP endpoints** | 17 / 17 wired with real backends |
| **gRPC RPCs** | 9 / 9 implemented (but never started at runtime) |
| **Test functions** | ~220+ across 28 unit test modules and 9 integration test files |
| **TODO / FIXME / HACK markers** | 0 |
| **Production `unwrap()` calls** | 33 (31 in handlers, 1 in middleware, 1 in cluster manager) |

**Verdict:** MSearchDB is a well-architected, cleanly written Rust project with real implementations across all layers. The single-node HTTP path is fully functional end-to-end (HTTP -> Raft -> state machine -> storage + index). The primary gaps are: (1) no persistence at the binary level (in-memory storage), (2) gRPC server never started (blocking multi-node), and (3) the client crate is a stub.

---

## 1. Endpoints Status

### 1.1 HTTP REST API (port 9200)

All 17 endpoints are wired through axum with real backend calls. Writes go through Raft consensus; reads go directly to storage/index backends.

| Status | Method | Path | Handler | Description |
|--------|--------|------|---------|-------------|
| ✅ Working | `PUT` | `/collections/:name` | `create_collection` | Creates a collection. Checks for duplicates (409). Proposes through Raft. Supports optional `schema` in body. |
| ✅ Working | `GET` | `/collections/:name` | `get_collection` | Returns collection name and doc_count. 404 if not found. |
| ✅ Working | `GET` | `/collections` | `list_collections` | Lists all collections with doc counts from in-memory registry. |
| ✅ Working | `DELETE` | `/collections/:name` | `delete_collection` | Deletes a collection. Proposes through Raft. Removes from in-memory registry. |
| ✅ Working | `POST` | `/collections/:name/docs` | `index_document` | Indexes a document. Auto-generates UUID if no `id` given. Proposes `InsertDocument` through Raft. |
| ✅ Working | `PUT` | `/collections/:name/docs/:id` | `upsert_document` | Updates a document by ID. Proposes `UpdateDocument` through Raft. |
| ✅ Working | `GET` | `/collections/:name/docs/:id` | `get_document` | Gets a document by ID. Reads directly from `StorageBackend` (bypasses Raft). |
| ✅ Working | `DELETE` | `/collections/:name/docs/:id` | `delete_document` | Deletes a document by ID. Proposes `DeleteDocument` through Raft. |
| ⚠️ Partial | `POST` | `/collections/:name/_search` | `search_documents` | Executes search with Query DSL (match/term/range/bool/fuzzy). **Limitation:** `size`, `from`, `highlight`, and `sort` fields are parsed from the request body but silently ignored — the `IndexBackend::search` trait only accepts a `&Query`. |
| ⚠️ Partial | `GET` | `/collections/:name/_search?q=` | `simple_search` | Simple full-text search via query string. Same limitation: no pagination support. |
| ✅ Working | `POST` | `/collections/:name/docs/_bulk` | `bulk_index` | NDJSON bulk indexing. Supports `index` and `delete` actions. Per-item error reporting. Batches of 100 (but each command gets its own Raft round-trip). |
| ⚠️ Partial | `POST` | `/collections/:name/_refresh` | `refresh_collection` | Validates collection exists, returns `{"acknowledged": true}`. **Does not actually call any index commit/refresh operation** — effectively a no-op. |
| ⚠️ Partial | `GET` | `/_cluster/health` | `cluster_health` | Returns cluster health. **Limitations:** `number_of_nodes` hardcoded to `1`. Status is only `"green"` or `"red"` (never `"yellow"` despite doc comments mentioning it). |
| ⚠️ Partial | `GET` | `/_cluster/state` | `cluster_state` | Returns cluster state. **Limitation:** Only reports the local node with hardcoded address `127.0.0.1:9200`. No actual multi-node discovery. |
| ⚠️ Partial | `GET` | `/_nodes` | `list_nodes` | Lists cluster nodes. **Same limitation:** returns only the local node with hardcoded address. |
| ✅ Working | `POST` | `/_nodes/:id/_join` | `join_node` | Adds a node as a Raft learner. Calls `raft_node.add_learner()`. Accepts `address` in body (defaults gRPC port to 9300). |
| ⚠️ Partial | `GET` | `/_stats` | `get_stats` | Returns node stats. **Limitation:** `current_term` field is actually populated with the `leader_id` (not the real Raft term). Comment says "approximate". |

**Summary:** 10 fully working, 7 partially working (functional but with documented limitations). Zero broken or unimplemented endpoints.

### 1.2 gRPC Services (port 9300 — NOT STARTED)

All 9 RPCs across 2 services are fully implemented in code but the gRPC server is **never started** from the binary.

| Status | Service | RPC | Description |
|--------|---------|-----|-------------|
| 🔧 Not started | `NodeService` | `AppendEntries` | Raft log replication. Fully implemented. |
| 🔧 Not started | `NodeService` | `RequestVote` | Raft leader election. Fully implemented. |
| 🔧 Not started | `NodeService` | `InstallSnapshot` | Client-streaming snapshot transfer. Fully implemented. |
| 🔧 Not started | `NodeService` | `ForwardWrite` | Write forwarding to leader. Fully implemented. |
| 🔧 Not started | `NodeService` | `HealthCheck` | Node health + metrics. Fully implemented. |
| 🔧 Not started | `NodeService` | `JoinCluster` | Cluster membership join. Fully implemented. |
| 🔧 Not started | `QueryService` | `Search` | Execute search on local node. Fully implemented. |
| 🔧 Not started | `QueryService` | `Get` | Retrieve document by ID. Fully implemented. |
| 🔧 Not started | `QueryService` | `Scatter` | Server-streaming scatter-gather. Fully implemented. |

**Blocking issue:** `main.rs` never calls `start_grpc_server()`. Additionally, the Raft node uses `StubNetworkFactory` (returns `Unreachable` for all RPCs) instead of a gRPC-backed `RaftNetworkFactory`. Both pieces need to be wired for multi-node operation.

---

## 2. Crate Implementation Status

| Crate | Package | Status | Description |
|-------|---------|--------|-------------|
| `core` | `msearchdb-core` | ✅ Fully implemented | Domain types, error handling, Query DSL, traits, consistent hashing, cluster routing, rebalancing |
| `storage` | `msearchdb-storage` | ✅ Fully implemented | RocksDB backend with WAL, memtable, column families, Snappy compression, bloom filters |
| `index` | `msearchdb-index` | ✅ Fully implemented | Tantivy full-text search (BM25, fuzzy, boolean, range, highlighting, CJK support) |
| `consensus` | `msearchdb-consensus` | ✅ Fully implemented | Raft consensus via openraft (state machine, log store, snapshot, quorum) |
| `network` | `msearchdb-network` | ✅ Fully implemented | gRPC server/client, circuit breaker, connection pool, scatter-gather |
| `node` | `msearchdb` | ✅ Fully implemented | HTTP REST API (axum), middleware, cluster manager, handlers |
| `client` | `msearchdb-client` | ❌ Stub | Only `pub use msearchdb_core;` — no HTTP client, no tests, no functionality |

---

## 3. Dependencies & Integrations

### 3.1 Environment Variables

| Variable | Required | Used In | Description |
|----------|----------|---------|-------------|
| `MSEARCHDB_API_KEY` | No | `main.rs:66` | Optional API key for X-API-Key header authentication. If unset, auth is disabled entirely. |

**Issues:**
- No `.env` or `.env.example` file exists — the env var is undocumented outside of code inspection
- No other env vars are used (not even for port configuration)

### 3.2 External Service Dependencies

| Service | Status | Notes |
|---------|--------|-------|
| **RocksDB** (librocksdb) | ⚠️ Available but unused at runtime | The `msearchdb-storage` crate fully implements `RocksDbStorage`, but `main.rs` uses `InMemoryStorage` instead. RocksDB is compiled as a dependency (requires C++ toolchain). |
| **Tantivy** (full-text search) | ✅ Connected | `TantivyIndex::new_in_ram()` is used at startup. Fully functional for search operations. Index is in-memory (lost on restart). |
| **openraft** (Raft consensus) | ✅ Connected | Single-node Raft cluster is initialized and functional. Writes go through `propose()` and are committed by the state machine. |
| **tonic/gRPC** | ⚠️ Compiled but not started | All gRPC code compiles. Server and client are complete. The binary never starts the gRPC listener. |
| **No external databases** | N/A | No PostgreSQL, MySQL, Redis, or other external DB connections. Self-contained. |
| **No external APIs** | N/A | No third-party API integrations. |
| **No TLS/SSL** | N/A | HTTP and gRPC are plaintext. No TLS configuration exists. |

### 3.3 Configuration System

| Feature | Status | Notes |
|---------|--------|-------|
| `NodeConfig` with TOML loading | ✅ Implemented in core | `from_toml_file()`, `from_toml_str()`, `to_toml_string()`, validation |
| Config loading in binary | ❌ Not used | `main.rs` calls `NodeConfig::default()` — no CLI args, no config file path |
| CLI argument parsing | ❌ Missing | No `clap`, `structopt`, or manual args parsing. Zero runtime configurability. |
| Default values | ✅ Hardcoded | `node_id=1`, `http_port=9200`, `grpc_port=9300`, bind=`0.0.0.0` |

### 3.4 Missing Integrations

| Integration | Impact | Details |
|-------------|--------|---------|
| **RocksDB not wired into binary** | High | `InMemoryStorage` (HashMap) is used instead. All data lost on restart. The `scan()` method ignores range and limit parameters — returns all documents regardless. |
| **gRPC server not started** | High | Prevents multi-node clustering. The `start_grpc_server()` helper exists but is never called. |
| **No `GrpcNetworkFactory`** | High | Raft uses `StubNetworkFactory` (returns `Unreachable`). Even if the gRPC server were started, the Raft engine has no way to send RPCs to peers. |
| **Client crate is empty** | Medium | Only re-exports `msearchdb_core`. No HTTP client for programmatic API access. |
| **No TLS** | Low | Acceptable for development; would need TLS for production. |
| **No metrics endpoint** | Low | No Prometheus `/metrics` or similar observability endpoint. |

---

## 4. Known Issues

### 4.1 Bugs / Incorrect Behavior

| # | Severity | File | Line(s) | Description |
|---|----------|------|---------|-------------|
| 1 | **High** | `node/src/main.rs` | 128-135 | `InMemoryStorage::scan()` ignores `range` and `limit` parameters — returns ALL documents. Breaks range-based operations (rebalancing, pagination). |
| 2 | **Medium** | `node/src/handlers/admin.rs` | 63 | `get_stats` reports `current_term` but the value is actually the `leader_id` (wrong semantics). |
| 3 | **Medium** | `node/src/handlers/cluster.rs` | 60, 86 | Cluster health and state responses hardcode the node address to `127.0.0.1:9200` instead of reading from config. |
| 4 | **Medium** | `node/src/handlers/search.rs` | — | `SearchRequest` accepts `size`, `from`, `highlight`, `sort` fields, but the handler ignores all of them — only the `query` is passed to the index backend. |
| 5 | **Low** | `node/src/handlers/cluster.rs` | 30-35 | Cluster health status is only `"green"` or `"red"` — the `"yellow"` state (documented in comments) is never emitted. |
| 6 | **Low** | `node/src/handlers/documents.rs` | 66, 117 | Document `version` is always `1` — no actual version tracking or optimistic concurrency. |

### 4.2 Potential Panics in Production Code

| # | Severity | File | Line(s) | Description |
|---|----------|------|---------|-------------|
| 7 | **Medium** | `node/src/handlers/*.rs` | (31 sites) | All handlers use `serde_json::to_value(resp).unwrap()` for response serialization. While this rarely fails on well-typed structs, it is a potential panic in production. |
| 8 | **Medium** | `node/src/cluster_manager.rs` | 556 | `unwrap()` on `.find()` in `ReadRepairCoordinator::compare_replicas()`. Can panic if the replicas vector has inconsistent state. |
| 9 | **Low** | `node/src/middleware.rs` | 135 | `serde_json::to_string(&body).unwrap()` in auth rejection — on a static JSON literal, unlikely to fail. |

### 4.3 Architectural Gaps

| # | Severity | Description |
|---|----------|-------------|
| 10 | **High** | **No data persistence in the binary.** `main.rs` uses `InMemoryStorage` (HashMap) and `TantivyIndex::new_in_ram()`. All data is lost on restart. The `RocksDbStorage` implementation exists but is not wired in. |
| 11 | **High** | **gRPC server never started.** `start_grpc_server()` exists in `network/src/server.rs` but `main.rs` never calls it. Multi-node Raft is impossible without it. |
| 12 | **High** | **No `GrpcNetworkFactory` for Raft.** The Raft node uses `StubNetworkFactory` where every RPC returns `Unreachable`. A factory that delegates to `NodeClient` needs to be written. |
| 13 | **High** | **No CLI argument parsing.** Port, node ID, data directory, and config file path are all hardcoded. There is no way to run multiple nodes or customize the deployment. |
| 14 | **Medium** | **Collection metadata is in-memory only.** The `AppState.collections` HashMap is not persisted. Collections are lost on restart even if documents were persisted (in a hypothetical RocksDB mode). |
| 15 | **Medium** | **`CreateCollection` / `DeleteCollection` are no-ops in the Raft state machine.** They log the action but do not create or destroy physical storage or index structures. |
| 16 | **Medium** | **Bulk indexing is not atomic.** Each document in a batch gets its own Raft round-trip. A partial failure leaves the batch in a half-committed state. |
| 17 | **Low** | **Client crate is a stub.** Only re-exports `msearchdb_core`. No HTTP client, no tests. Dependencies are declared but unused (`tokio`, `serde`, etc.). |
| 18 | **Low** | **Snapshot format is uncompressed JSON.** All documents are serialized as a JSON array. Not viable for large datasets. |

### 4.4 Security Considerations

| # | Severity | Description |
|---|----------|-------------|
| 19 | **Medium** | **No TLS.** HTTP and gRPC are plaintext. API key (if set) is transmitted in the clear. |
| 20 | **Low** | **API key auth is optional and simple.** Single shared key via env var, compared with constant-time equality. No RBAC, no JWT, no per-user auth. |
| 21 | **Low** | **Bind address hardcoded to `0.0.0.0`.** Always listens on all interfaces, even in development. |

---

## 5. Test Coverage Summary

| Crate | Unit Test Modules | Integration Test Files | Approx. Test Functions | Notable Coverage |
|-------|:-:|:-:|:-:|------|
| `core` | 8 | 1 | ~40 | Hash ring balance, rebalance disruption, serde roundtrips |
| `storage` | 3 | 2 | ~35 | RocksDB persistence, WAL corruption, concurrent access, p99 latency |
| `index` | 5 | 2 | ~61 | 100K-doc bulk, BM25 scoring, fuzzy search, CJK, concurrent indexing |
| `consensus` | 5 | 1 | ~20 | 3-node leader election, failover, follower rejection |
| `network` | 4 | 1 | ~25 | Full gRPC roundtrip, circuit breaker, scatter-gather with timeouts |
| `node` | 4 | 2 | ~40 | Full HTTP stack (axum oneshot), API key auth, cluster manager |
| `client` | 0 | 0 | 0 | **No tests** |
| **Total** | **29** | **9** | **~220+** | |

**Quality highlights:**
- Performance assertions (p99 read <10ms, p99 search <50ms over 100K docs)
- Concurrent safety tests (multi-thread memtable, multi-task indexing)
- Graceful degradation tests (scatter-gather with timed-out nodes)
- Serde roundtrip tests on every major type
- Error path coverage (NotFound, invalid JSON, circuit breaker trips, follower proposals)

---

## 6. Summary

### Top 3 Blockers Preventing a Fully Working Application

| Priority | Blocker | Impact | Effort to Fix |
|----------|---------|--------|---------------|
| **#1** | **No data persistence in the binary** — `InMemoryStorage` and in-RAM Tantivy index are used. `RocksDbStorage` exists but is not wired in. | All data lost on restart. Unusable for any real workload. | Low — swap `InMemoryStorage::new()` with `RocksDbStorage::new(path)` and `TantivyIndex::new(path)`. Add CLI args for data directory. |
| **#2** | **gRPC server not started & no `GrpcNetworkFactory`** — multi-node clustering is impossible. All Raft network calls return `Unreachable`. | Single-node only. Defeats the purpose of a distributed database. | Medium — call `start_grpc_server()` from `main.rs`, write a `GrpcNetworkFactory` that delegates to `NodeClient`, add CLI args for peer addresses. |
| **#3** | **No CLI argument parsing** — node ID, ports, data directory, config file path, and peer addresses are all hardcoded to defaults. | Cannot run multiple nodes, cannot configure deployments, cannot use the existing TOML config infrastructure. | Low — add `clap` with args for `--config`, `--port`, `--node-id`, `--data-dir`, `--peers`. |

### What Works Well

- **Clean architecture**: 7-crate workspace with clear separation of concerns and well-defined trait boundaries
- **Real Raft consensus**: Single-node write path is fully functional (HTTP -> propose -> commit -> apply to storage + index)
- **Comprehensive search**: Tantivy integration with BM25, fuzzy, boolean, range, term, and highlighting support
- **Production-grade networking code**: Circuit breaker, connection pooling, scatter-gather with dedup and re-ranking
- **Strong test suite**: ~220+ tests with performance assertions, concurrency testing, and error path coverage
- **Zero TODOs/FIXMEs**: No deferred work markers in the codebase — code is either implemented or not present
- **Consistent error handling**: Unified `DbError` enum with `thiserror`, proper HTTP status code mapping

### Application Readiness

| Use Case | Ready? |
|----------|--------|
| Local development / demo | ✅ Yes (single-node, in-memory) |
| Single-node with persistence | ❌ No (requires wiring RocksDB + disk-based Tantivy) |
| Multi-node cluster | ❌ No (requires gRPC startup + network factory + CLI args) |
| Production deployment | ❌ No (requires persistence, TLS, metrics, CLI configuration) |
