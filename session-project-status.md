# MSearchDB — Project Status & Architecture Verification

**Date:** 2026-03-28  
**Scope:** Full code audit of architectural premises vs. actual implementation  
**Methodology:** Line-by-line reading of all 7 crates, full `cargo test --workspace`, build verification

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Architectural Premise Verification](#2-architectural-premise-verification)
3. [Detailed Findings](#3-detailed-findings)
   - [3.1 Raft Consensus](#31-raft-consensus)
   - [3.2 Consistent Hashing & Routing](#32-consistent-hashing--routing)
   - [3.3 Replication (Factor 3)](#33-replication-factor-3)
   - [3.4 gRPC Inter-Node Communication](#34-grpc-inter-node-communication)
   - [3.5 Fault Tolerance & Failover](#35-fault-tolerance--failover)
   - [3.6 Full-Text Search](#36-full-text-search)
   - [3.7 Storage Engine](#37-storage-engine)
4. [Critical Gap Analysis](#4-critical-gap-analysis)
5. [Test Suite Status](#5-test-suite-status)
6. [Build, Run & Test Instructions](#6-build-run--test-instructions)
   - [6.1 Prerequisites](#61-prerequisites)
   - [6.2 Building the Rust Cluster](#62-building-the-rust-cluster)
   - [6.3 Running a Single Node](#63-running-a-single-node)
   - [6.4 Running a Multi-Node Cluster](#64-running-a-multi-node-cluster)
   - [6.5 Manual API Testing](#65-manual-api-testing)
   - [6.6 Running the Rust Test Suite](#66-running-the-rust-test-suite)
7. [Python Validation Client](#7-python-validation-client)
   - [7.1 Installation](#71-installation)
   - [7.2 Running the Client](#72-running-the-client)
   - [7.3 Test Coverage](#73-test-coverage)
8. [Recommendations](#8-recommendations)

---

## 1. Executive Summary

MSearchDB is a Rust-based distributed NoSQL database with 7 crates and approximately 15,000+ lines of production code. The **library layer** is impressively complete — Raft consensus, consistent hashing, gRPC transport, circuit breakers, scatter-gather, failure detection, and gossip protocols are all implemented with thorough test coverage (650+ unit and integration tests).

However, there is a **critical integration gap**: the distributed machinery (routing, forwarding, scatter-gather, gossip networking, read repair) is not wired into the HTTP request handlers. The system currently operates as a **single-node database** that stores all data locally, regardless of what the hash ring says. Raft consensus works for write ordering but does not distribute data across nodes.

### Verdict by Architectural Premise

| Premise                             | Status                                  | Assessment                                                                                                                                                                                                                                                                       |
| ----------------------------------- | --------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Rust with full-text search          | **WORKING**                             | Tantivy index with BM25, fuzzy, boolean queries. 17 HTTP endpoints.                                                                                                                                                                                                              |
| Consistent hashing for partitioning | **LIBRARY ONLY**                        | `ConsistentHashRing` + `ClusterRouter` implemented and tested, but never consulted in the HTTP data path. Every node stores everything locally.                                                                                                                                  |
| Replication factor 3                | **CONFIGURED, NOT ENFORCED**            | `replication_factor` field exists in config (default 3). The router's `route_document()` returns 3 replica nodes. But writes go only to the local node — no replication to peers occurs.                                                                                         |
| Raft consensus for writes           | **PARTIALLY WORKING**                   | All HTTP writes go through `raft_node.propose()`. openraft manages leader election and log replication. But: (a) log store is in-memory (non-durable), (b) collection operations are no-ops in the state machine, (c) documents are double-written (Raft apply + handler write). |
| gRPC for node-to-node               | **TRANSPORT COMPLETE, NOT FULLY WIRED** | 9 RPCs implemented (client + server). Raft uses gRPC for AppendEntries/Vote/Snapshot. But: write-forwarding from followers not used by HTTP handlers; scatter-gather not used for search.                                                                                        |
| Fault tolerance (survive node loss) | **PARTIAL**                             | Raft handles leader election and log replication for committed entries. Circuit breaker and health checks exist. But: in-memory log store means crash recovery is impossible; no data redistribution on node failure; gossip sends are no-ops.                                   |

---

## 2. Architectural Premise Verification

### Premise 1: "Core written in Rust with full-text search"

**FULLY IMPLEMENTED.**

- **Storage**: RocksDB backend with Snappy compression, bloom filters, column families per collection (`crates/storage/src/rocksdb_backend.rs`).
- **Full-text search**: Tantivy engine with BM25 scoring, configurable analyzers (standard, keyword, ngram, CJK), search result highlighting (`crates/index/`).
- **HTTP API**: 17 REST endpoints via axum on port 9200 (`crates/node/src/handlers/`).
- **Query DSL**: Match, term, range, boolean (must/should/must_not), fuzzy queries — all serializable as JSON.

### Premise 2: "Data partitioned via consistent hashing"

**IMPLEMENTED AS LIBRARY, NOT INTEGRATED INTO DATA PATH.**

- `ConsistentHashRing<T>` in `crates/core/src/consistent_hash.rs`: SHA-256 ring with 150 virtual nodes per physical node. Algorithmic correctness verified by tests showing <7% standard deviation across 100K keys with 3 nodes.
- `ClusterRouter` in `crates/core/src/cluster_router.rs`: `route_document()` maps document IDs to N replica nodes; `route_query()` distinguishes targeted reads from scatter-gather.
- **Gap**: `ClusterRouter` is instantiated in `main.rs:258` and passed to `ClusterManager`, but it is **not part of `AppState`** (see `state.rs:52-131`). HTTP handlers have no access to the router. `route_document()` and `route_query()` are never called outside tests.

### Premise 3: "Replication factor of 3"

**CONFIGURED, NOT ENFORCED.**

- `NodeConfig.replication_factor` defaults to 3 (`crates/core/src/config.rs`).
- `ClusterRouter::new(nodes, replication_factor)` stores this value and uses it in `route_document()`.
- **Gap**: When a document is written via the HTTP API, the handler writes to local storage only. There is no code that says "write this document to the N nodes returned by `route_document()`". The Raft state machine applies to whichever backends were injected at startup — on each node, that's its own local storage. In a multi-node cluster, Raft log entries are replicated (the _commands_ are on all nodes), but the state machine's `apply()` writes using global `StorageBackend::put()` which goes to the default RocksDB column family, not the collection-specific one. Meanwhile, the HTTP handler writes again to the collection-specific column family. This dual-write means the Raft-replicated path and the HTTP-handler path store data in different places.

### Premise 4: "Raft consensus for all write operations"

**PARTIALLY WORKING — SEE SECTION 3.1 FOR DETAILS.**

All HTTP write handlers call `state.raft_node.propose(cmd)` before writing locally. openraft manages leader election, log replication, and state machine application. The gRPC transport (`GrpcNetworkFactory`) handles AppendEntries, Vote, and InstallSnapshot RPCs between nodes.

Critical issues:

- **In-memory log store**: `MemLogStore` uses a `BTreeMap` in memory. Violates Raft's durability guarantee. A node crash loses all log entries and the vote record.
- **Double writes**: HTTP handlers write to collection-scoped storage after the Raft proposal, while the state machine `apply()` writes to global storage. Different storage paths.
- **Collection operations are no-ops in the state machine**: `CreateCollection` and `DeleteCollection` log a message and return success without creating/deleting anything. The actual work happens in the HTTP handler on the leader only, meaning follower nodes never create the collection's RocksDB column family or Tantivy index.
- **No write-forwarding from followers**: If a follower receives an HTTP write, `raft_node.propose()` fails. The handler does not catch this and forward to the leader via gRPC `ForwardWrite`.

### Premise 5: "gRPC for node-to-node communication"

**TRANSPORT COMPLETE, PARTIALLY INTEGRATED.**

See section 3.4 for the full breakdown. The gRPC layer is production-quality: 2 services (NodeService, QueryService), 9 RPCs, full client library with retry/backoff, connection pooling with circuit breaker. Raft consensus uses gRPC for all inter-node communication.

Gaps: HTTP handlers don't use `scatter_search()` for distributed queries or `forward_write()` for leader forwarding.

### Premise 6: "Cluster remains available if a node goes down"

**PARTIAL — RAFT HANDLES IT, BUT DATA PATH DOESN'T.**

- Raft leader election works: if the leader dies, a new leader is elected within ~2 seconds (tested).
- Committed Raft entries survive leader failover (within the same process lifetime).
- **But**: since data isn't actually distributed (every node only has its own local data), losing a node means losing whatever data was only on that node. There's no cross-node replication of actual document bytes.
- The gossip protocol's send path is a no-op (messages built but never transmitted).
- Read repair is local-only (remote repairs are logged but not sent).

---

## 3. Detailed Findings

### 3.1 Raft Consensus

**Files**: `crates/consensus/src/`

| Component               | Status                    | File                       | Notes                                                             |
| ----------------------- | ------------------------- | -------------------------- | ----------------------------------------------------------------- |
| State machine `apply()` | Working for documents     | `state_machine.rs:241-280` | Writes to injected StorageBackend + IndexBackend                  |
| State machine `apply()` | **No-op for collections** | `state_machine.rs:168-176` | Logs a message, returns success                                   |
| Log store               | **In-memory only**        | `log_store.rs:42-51`       | `BTreeMap<u64, Entry>`, no fsync                                  |
| Vote persistence        | **In-memory only**        | `log_store.rs:48`          | Lost on restart, violates Raft safety                             |
| Snapshot store          | **In-memory only**        | `state_machine.rs:78,88`   | `snapshot_dir` field exists but unused                            |
| Leader election         | Working                   | `raft_node.rs:240`         | openraft + gRPC transport, tested with 3 nodes                    |
| Log replication         | Working                   | `raft_network.rs:79-197`   | All 3 RPCs wired to gRPC                                          |
| Write proposal path     | Working                   | `raft_node.rs:240-277`     | `propose()` calls `raft.client_write()`                           |
| Error handling in apply | **Asymmetric**            | `state_machine.rs:130-141` | Storage failure → fail response; index failure → success response |

**Double-Write Problem** (Critical):

Every document write happens twice through different paths:

1. Raft `apply()` → `StorageBackend::put()` (global/default column family)
2. HTTP handler → `StorageBackend::put_in_collection()` (collection-specific column family)

On the leader, both paths execute. On followers, only path 1 executes. This creates inconsistency: followers have documents in the default column family but not in the collection column family that GET handlers query.

### 3.2 Consistent Hashing & Routing

**Files**: `crates/core/src/consistent_hash.rs`, `cluster_router.rs`, `rebalancer.rs`

| Component             | Status           | Notes                                                         |
| --------------------- | ---------------- | ------------------------------------------------------------- |
| Hash ring             | Complete         | SHA-256, 150 vnodes, BTreeMap-based clockwise walk            |
| Document routing      | Complete library | `route_document()` returns N replica nodes                    |
| Query routing         | Complete library | Distinguishes targeted vs scatter queries                     |
| Rebalance planning    | Complete library | Probe-based sampling (10K keys)                               |
| Rebalance execution   | **Dry-run stub** | `Rebalancer::execute()` logs moves, returns `Completed`       |
| Integration with HTTP | **NOT WIRED**    | `ClusterRouter` not in `AppState`, never called from handlers |

### 3.3 Replication (Factor 3)

**Status: Configuration exists, enforcement does not.**

The `replication_factor` value flows through:

1. `NodeConfig` → `ClusterRouter::new()` → stored as `self.replication_factor`
2. `ClusterRouter::route_document()` calls `self.ring.get_nodes(key, replication_factor as usize)`
3. `ClusterRouter::can_satisfy_quorum()` checks if enough healthy nodes exist

But none of this is called from the write path. A document indexed via HTTP is stored on exactly one node — the one that received the request.

### 3.4 gRPC Inter-Node Communication

**Files**: `crates/network/src/`, `proto/msearchdb.proto`

| RPC               | Server          | Client                | Used in Production                     |
| ----------------- | --------------- | --------------------- | -------------------------------------- |
| `AppendEntries`   | `server.rs:56`  | `raft_network.rs:79`  | Yes (Raft)                             |
| `RequestVote`     | `server.rs:80`  | `raft_network.rs:168` | Yes (Raft)                             |
| `InstallSnapshot` | `server.rs:103` | `raft_network.rs:111` | Yes (Raft)                             |
| `ForwardWrite`    | `server.rs:151` | `client.rs:183`       | **No** (not called from HTTP handlers) |
| `HealthCheck`     | `server.rs:178` | `client.rs:278`       | Yes (ClusterManager)                   |
| `JoinCluster`     | `server.rs:203` | `client.rs:359`       | Yes (main.rs:311)                      |
| `Search`          | `server.rs:277` | `client.rs:218`       | **No** (handlers search locally)       |
| `Get`             | `server.rs:303` | `client.rs:248`       | **No** (handlers read locally)         |
| `Scatter`         | `server.rs:336` | scatter_gather.rs     | **No** (handlers don't fan out)        |

Additional components:

- **Connection Pool** (`connection_pool.rs`): Per-node client caching with tonic channel reuse.
- **Circuit Breaker** (`connection_pool.rs:55`): 3-state FSM (Closed/Open/HalfOpen), 3-failure threshold, 30s open duration. Wired into scatter-gather and health checks. **Not** used for Raft RPCs.
- **Scatter-Gather** (`scatter_gather.rs`): `FuturesUnordered`-based fan-out with per-node timeouts, deduplication, and global re-ranking. Fully implemented but never called from HTTP search handlers.
- **TLS** (`tls.rs`): Helper functions for server/client TLS + mTLS exist. **Not wired** into the gRPC server startup.

### 3.5 Fault Tolerance & Failover

**Files**: `crates/node/src/cluster_manager.rs`

| Component                    | Status           | Notes                                                                             |
| ---------------------------- | ---------------- | --------------------------------------------------------------------------------- |
| Phi Accrual failure detector | Working          | phi > 8.0 suspect, phi > 16.0 dead                                                |
| Health check loop            | Working          | 1s interval, 3 consecutive failures → Offline                                     |
| Gossip protocol (logic)      | Complete         | 2s interval, 2 random peers, latest-timestamp merge                               |
| Gossip protocol (network)    | **Stub**         | Messages built but `_message_clone` is unused (`cluster_manager.rs:831`)          |
| Gossip receive handler       | **Never called** | `handle_gossip()` has no HTTP/gRPC endpoint                                       |
| Read repair (comparison)     | Complete         | Version comparison, stale detection                                               |
| Read repair (remote writes)  | **Stub**         | Only local repairs execute; remote is logged                                      |
| Cluster health endpoint      | **Hardcoded**    | Reports 1 node, 1 active regardless of actual state (`handlers/cluster.rs:35-36`) |
| Cluster state endpoint       | **Hardcoded**    | Single local node, doesn't read from ClusterManager                               |

### 3.6 Full-Text Search

**FULLY WORKING.**

The Tantivy-based search engine is the strongest component:

- **BM25 scoring** with configurable boost factors
- **Query types**: match (full-text), term (exact), range (numeric/date), boolean (must/should/must_not), fuzzy (edit distance)
- **Analyzers**: Standard (lowercase + stop words), keyword (no tokenization), ngram, CJK
- **Highlighting**: `<em>` tag wrapping with configurable fragment size
- **Collection-scoped indexes**: Each collection gets its own Tantivy index with independent schema
- **Dynamic schema evolution**: New fields are added to the schema as documents with new fields are indexed
- **`spawn_blocking`**: CPU-intensive Tantivy operations run on Tokio's blocking thread pool

### 3.7 Storage Engine

**FULLY WORKING.**

- **RocksDB** with Snappy compression, bloom filters (10 bits per key), configurable write buffer
- **Column families** per collection for data isolation
- **WAL** (`crates/storage/src/wal.rs`): Custom write-ahead log with CRC32 checksums
- **Memtable** (`crates/storage/src/memtable.rs`): In-memory write buffer with lock-free skip list

---

## 4. Critical Gap Analysis

### Gap 1: The Data Path Does Not Use the Hash Ring (CRITICAL)

**Impact**: Data is not distributed. Every node stores all documents locally. The system cannot partition data across the cluster.

**Fix required**: Add `ClusterRouter` to `AppState`. Modify write handlers to call `route_document()`, determine if the local node is in the replica set, and forward writes via gRPC `ForwardWrite` to the correct nodes. Modify read handlers to use `route_document()` for targeted reads and `scatter_search()` for full-text queries.

### Gap 2: In-Memory Raft Log Store (CRITICAL)

**Impact**: A node crash loses all Raft log entries, the vote record, and uncommitted state. The cluster cannot recover after restart. This violates Raft's core durability guarantee.

**Fix required**: Replace `MemLogStore` with a RocksDB-backed log store. Store the vote and committed index in RocksDB. Call `fsync` before acknowledging log appends.

### Gap 3: Double Writes — State Machine vs. HTTP Handlers (HIGH)

**Impact**: Documents are written to two different storage locations (global column family via Raft apply, collection column family via HTTP handler). On follower nodes, only the global write occurs, so collection-scoped reads on followers return nothing.

**Fix required**: Choose one path. Recommended: have the state machine's `apply()` handle collection-scoped writes (pass collection name in `RaftCommand`), and remove the duplicate writes from HTTP handlers.

### Gap 4: No Write-Forwarding from Followers (HIGH)

**Impact**: In a multi-node cluster, only the Raft leader can accept HTTP writes. Followers return errors.

**Fix required**: In write handlers, catch the "not leader" error from `raft_node.propose()`, determine the current leader via `raft_node.current_leader()`, and forward the write via `NodeClient::forward_write()`.

### Gap 5: Collection Operations Are No-Ops in State Machine (HIGH)

**Impact**: Collection creation/deletion is leader-only. Follower nodes never create RocksDB column families or Tantivy indexes for collections. After leader failover, the new leader has no collection metadata.

**Fix required**: Implement actual collection creation/deletion in the state machine's `apply()` method. Persist collection metadata through Raft so all nodes have it.

### Gap 6: Gossip Protocol Sends Into the Void (MEDIUM)

**Impact**: Cluster membership changes don't propagate. Failure detection results are local-only.

**Fix required**: Add a gRPC RPC for gossip message exchange, or use UDP. Wire the receive path into `handle_gossip()`.

### Gap 7: Cluster Endpoints Return Hardcoded Values (MEDIUM)

**Impact**: `/_cluster/health`, `/_cluster/state`, and `/_nodes` always report a single-node cluster.

**Fix required**: Read from `ClusterManager`'s router and health data instead of hardcoding.

---

## 5. Test Suite Status

**Build**: `cargo check --workspace` — compiles cleanly with zero warnings.

**Tests**: 650+ tests across all crates.

```
cargo test --workspace

  msearchdb-consensus:     40 passed, 0 failed
  consensus integration:   13 passed, 0 failed, 1 ignored (endurance)
  consensus chaos:         10-11 passed, 0-1 flaky (leader_failover timing)
  consensus write-path:     6 passed
  consensus cluster:        6 passed
  msearchdb-core:         198 passed, 0 failed
  core integration:        12 passed
  msearchdb-index:         68 passed, 0 failed
  index integration:        4 passed
  msearchdb-storage:       34 passed, 0 failed
  storage integration:     10 passed
  msearchdb-network:       16 passed, 0 failed
  network integration:      7 passed
  msearchdb-node:         117 passed, 0 failed
  node integration:        24+ passed
  Doc-tests:               26 passed
```

**Known Flaky Test**: `chaos_02_leader_failure_new_leader_within_2s` occasionally fails due to the 2-second deadline for leader election under CI load. This is a timing sensitivity issue, not a logic bug.

**Clippy**: `cargo clippy --workspace -- -D warnings` passes cleanly.

---

## 6. Build, Run & Test Instructions

### 6.1 Prerequisites

- **Rust** (stable, 1.75+): Install via [rustup](https://rustup.rs/)
- **Clang/LLVM**: Required by RocksDB compilation (install via `apt install clang` or `brew install llvm`)
- **CMake**: Required by some native dependencies (`apt install cmake` or `brew install cmake`)
- **Protobuf compiler**: Required for gRPC proto compilation (`apt install protobuf-compiler` or `brew install protobuf`)

Verify prerequisites:

```bash
rustc --version     # 1.75.0 or newer
cargo --version
protoc --version    # libprotoc 3.x or 4.x
clang --version
cmake --version
```

### 6.2 Building the Rust Cluster

```bash
# Clone the repository
git clone <repository_url>
cd MSearchDB

# Debug build (faster compilation, slower runtime)
cargo build --workspace

# Release build (slower compilation, optimized runtime)
cargo build --release

# Type-check only (fastest feedback loop)
cargo check --workspace
```

### 6.3 Running a Single Node

```bash
# Start with default config (HTTP on :9200, gRPC on :9300, --bootstrap creates a single-node Raft cluster)
cargo run --bin msearchdb -- --bootstrap

# With custom ports
cargo run --bin msearchdb -- --bootstrap --http-port 9200 --grpc-port 9300 --node-id 1

# With a TOML config file
cargo run --bin msearchdb -- --config config.toml --bootstrap

# With custom data directory
cargo run --bin msearchdb -- --bootstrap --data-dir /tmp/msearchdb-data

# Release mode (recommended for testing throughput)
cargo run --release --bin msearchdb -- --bootstrap
```

The node creates these directories under `--data-dir` (default: `data/`):

```
data/
  storage/    # RocksDB files
  index/      # Tantivy index files
  logs/       # Log files
  snapshots/  # Backup snapshots
```

### 6.4 Running a Multi-Node Cluster

**Note**: Due to the integration gaps documented above, multi-node operation has significant limitations. Each node will have its own independent data. Raft consensus will replicate the _log entries_, but the state machine write path and the HTTP handler write path diverge.

```bash
# Terminal 1: Bootstrap the first node as cluster leader
cargo run --bin msearchdb -- \
  --bootstrap \
  --node-id 1 \
  --http-port 9200 \
  --grpc-port 9300 \
  --data-dir /tmp/node1

# Terminal 2: Start second node and join the cluster
cargo run --bin msearchdb -- \
  --node-id 2 \
  --http-port 9201 \
  --grpc-port 9301 \
  --data-dir /tmp/node2 \
  --peers "127.0.0.1:9300"

# Terminal 3: Start third node
cargo run --bin msearchdb -- \
  --node-id 3 \
  --http-port 9202 \
  --grpc-port 9302 \
  --data-dir /tmp/node3 \
  --peers "127.0.0.1:9300"
```

### 6.5 Manual API Testing

Using `curl` (or `httpie`):

```bash
# Health check
curl -s http://localhost:9200/_cluster/health | python3 -m json.tool

# Create a collection
curl -s -X PUT http://localhost:9200/collections/products

# Index a document
curl -s -X POST http://localhost:9200/collections/products/docs \
  -H 'Content-Type: application/json' \
  -d '{"id": "doc1", "fields": {"name": "Laptop", "price": 999.99, "in_stock": true}}'

# Get a document
curl -s http://localhost:9200/collections/products/docs/doc1 | python3 -m json.tool

# Search with Query DSL
curl -s -X POST http://localhost:9200/collections/products/_search \
  -H 'Content-Type: application/json' \
  -d '{"query": {"match": {"name": "laptop"}}}'

# Simple URL search
curl -s "http://localhost:9200/collections/products/_search?q=laptop"

# Fuzzy search (handles typos)
curl -s -X POST http://localhost:9200/collections/products/_search \
  -H 'Content-Type: application/json' \
  -d '{"query": {"fuzzy": {"name": {"value": "lptop", "fuzziness": 2}}}}'

# Boolean query
curl -s -X POST http://localhost:9200/collections/products/_search \
  -H 'Content-Type: application/json' \
  -d '{"query": {"bool": {"must": [{"match": {"name": "laptop"}}, {"term": {"in_stock": true}}]}}}'

# Range query
curl -s -X POST http://localhost:9200/collections/products/_search \
  -H 'Content-Type: application/json' \
  -d '{"query": {"range": {"price": {"gte": 100, "lte": 500}}}}'

# Update a document
curl -s -X PUT http://localhost:9200/collections/products/docs/doc1 \
  -H 'Content-Type: application/json' \
  -d '{"fields": {"name": "Gaming Laptop", "price": 1299.99, "in_stock": true}}'

# Delete a document
curl -s -X DELETE http://localhost:9200/collections/products/docs/doc1

# Bulk index (NDJSON format)
curl -s -X POST http://localhost:9200/collections/products/docs/_bulk \
  -H 'Content-Type: application/x-ndjson' \
  -d '{"index":{"_id":"b1"}}
{"name":"Keyboard","price":49.99}
{"index":{"_id":"b2"}}
{"name":"Mouse","price":29.99}
'

# List collections
curl -s http://localhost:9200/collections | python3 -m json.tool

# Delete collection
curl -s -X DELETE http://localhost:9200/collections/products

# Cluster state
curl -s http://localhost:9200/_cluster/state | python3 -m json.tool

# Node stats
curl -s http://localhost:9200/_stats | python3 -m json.tool
```

### 6.6 Running the Rust Test Suite

```bash
# Run ALL tests (unit + integration)
cargo test --workspace

# Run tests for a single crate
cargo test -p msearchdb-core
cargo test -p msearchdb-consensus
cargo test -p msearchdb-network
cargo test -p msearchdb-node
cargo test -p msearchdb-storage
cargo test -p msearchdb-index

# Run a specific test by name
cargo test -p msearchdb-core consistent_hash
cargo test -p msearchdb-consensus chaos_01

# Show stdout from tests
cargo test --workspace -- --nocapture

# Run clippy linter
cargo clippy --workspace -- -D warnings

# Check formatting
cargo fmt --all -- --check
```

---

## 7. Python Validation Client

### 7.1 Installation

```bash
# Ensure Python 3.9+ is installed
python3 --version

# Install dependencies
pip install requests

# Or use a virtual environment
python3 -m venv .venv
source .venv/bin/activate
pip install requests
```

### 7.2 Running the Client

First, start MSearchDB:

```bash
cargo run --bin msearchdb -- --bootstrap
```

Then in another terminal:

```bash
# Run all validation tests against localhost:9200
python3 tools/validate_cluster.py

# Verbose output
python3 tools/validate_cluster.py -v

# Against multiple nodes
python3 tools/validate_cluster.py --nodes http://localhost:9200 http://localhost:9201 http://localhost:9202

# Run only specific test groups
python3 tools/validate_cluster.py --only health crud search

# Available test groups: health, collections, crud, bulk, search, failover, cleanup
```

### 7.3 Test Coverage

The Python client validates:

| Test Group      | Tests  | What It Validates                                                                         |
| --------------- | ------ | ----------------------------------------------------------------------------------------- |
| **health**      | 4      | `/_cluster/health`, `/_cluster/state`, `/_nodes`, `/_stats` endpoints                     |
| **collections** | 4      | Create, get, list, idempotent create                                                      |
| **crud**        | 6      | Index, get, update, read-after-update, delete, read-after-delete (404)                    |
| **bulk**        | 2      | NDJSON bulk indexing, verify all docs retrievable                                         |
| **search**      | 6      | Match, term, range, boolean, fuzzy, simple query string                                   |
| **failover**    | 5      | Dead node skip, fast subsequent requests, write/read through failover, all-dead detection |
| **cleanup**     | 1      | Delete test collection                                                                    |
| **Total**       | **28** |                                                                                           |

The failover tests specifically validate:

1. Client automatically skips an unreachable node and routes to a live one.
2. After failover, subsequent requests go directly to the live node (no retry overhead).
3. CRUD operations succeed through the failover path.
4. When all nodes are unreachable, a `ConnectionError` is raised promptly.

---

## 8. Recommendations

### Priority 1: Fix the Data Path (Required for Distribution)

1. **Add `ClusterRouter` to `AppState`**. This is the single most impactful change — it makes routing decisions available to every handler.

2. **Wire `route_document()` into write handlers**. Before writing, determine the replica set. If the local node is in it, write locally. Forward to other replicas via gRPC `ForwardWrite`.

3. **Wire `route_query()` into search handlers**. Use `scatter_search_with_pool()` for full-text queries. Use `route_document()` for `_id`-based reads.

4. **Implement write-forwarding from followers**. Catch "not leader" errors in handlers, determine the leader, and forward via gRPC.

### Priority 2: Fix Raft Durability (Required for Crash Recovery)

5. **Replace `MemLogStore` with a RocksDB-backed log store**. Store log entries, vote, and committed index in a dedicated RocksDB column family. Call `fsync` on the write callback.

6. **Persist snapshots to disk**. Wire the `snapshot_dir` field to actual file I/O in `build_snapshot()` and `install_snapshot()`.

### Priority 3: Fix State Machine Consistency (Required for Multi-Node)

7. **Eliminate double writes**. Have the state machine's `apply()` use collection-scoped methods (`put_in_collection`, `index_document_in_collection`). Pass the collection name inside `RaftCommand`. Remove the duplicate writes from HTTP handlers.

8. **Implement collection operations in the state machine**. `CreateCollection` and `DeleteCollection` must actually create/delete RocksDB column families and Tantivy indexes so that followers have them.

### Priority 4: Complete Cluster Management (Required for Production)

9. **Wire gossip networking**. Add a gRPC RPC for gossip exchange. Connect the send path to actual network calls and the receive path to `handle_gossip()`.

10. **Fix cluster endpoints**. Read from `ClusterManager` instead of hardcoding single-node values.

11. **Implement remote read repair**. Wire `ReadRepairCoordinator` to forward repair writes via gRPC.

12. **Implement actual rebalancing**. Replace the dry-run `Rebalancer::execute()` with real data transfer via gRPC streaming.
