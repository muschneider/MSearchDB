# MSearchDB Architecture

This document describes the internal architecture of MSearchDB, a distributed
NoSQL database with full-text search, written in Rust.

## Table of Contents

1. [System Overview](#system-overview)
2. [Write Path](#write-path)
3. [Read Path](#read-path)
4. [Raft Consensus](#raft-consensus)
5. [Consistent Hash Ring](#consistent-hash-ring)
6. [Cluster Management](#cluster-management)
7. [Failure Scenarios and Recovery](#failure-scenarios-and-recovery)
8. [Crate Dependency Graph](#crate-dependency-graph)

---

## System Overview

MSearchDB is organized as a Cargo workspace with 7 crates. Each node runs a
single binary (`msearchdb`) that hosts an HTTP API, a Tantivy search index, a
RocksDB storage engine, and a Raft consensus participant.

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          Node Binary (msearchdb)                       │
│                                                                         │
│  ┌──────────────────────────────────────────────────────────────────┐  │
│  │                     HTTP REST API (axum)                         │  │
│  │  Middleware: Auth -> RateLimit -> Tracing -> RequestId -> CORS   │  │
│  │                                                                  │  │
│  │  Routes:                                                         │  │
│  │    /collections/*           Collection CRUD                      │  │
│  │    /collections/*/docs/*    Document CRUD                        │  │
│  │    /collections/*/_search   Full-text search                     │  │
│  │    /collections/*/_bulk     Bulk indexing                        │  │
│  │    /_cluster/*              Cluster management                   │  │
│  │    /_snapshot/*             Backup / Restore                     │  │
│  │    /_aliases/*              Alias management                     │  │
│  │    /_stats, /metrics        Observability                        │  │
│  └──────────────────┬───────────────────────────────────────────────┘  │
│                     │                                                   │
│  ┌──────────────────▼───────────────────────────────────────────────┐  │
│  │                     Application State (AppState)                  │  │
│  │                                                                   │  │
│  │  ┌─────────────┐  ┌───────────────┐  ┌────────────────────────┐ │  │
│  │  │ ClusterMgr  │  │ WriteBatcher  │  │ DocumentCache (moka)   │ │  │
│  │  │  - Gossip   │  │  100 docs     │  │  10k entries, 60s TTL  │ │  │
│  │  │  - Health   │  │  10ms window  │  │                        │ │  │
│  │  │  - Failover │  │               │  │                        │ │  │
│  │  └──────┬──────┘  └──────┬────────┘  └────────────────────────┘ │  │
│  │         │                │                                        │  │
│  └─────────┼────────────────┼────────────────────────────────────────┘  │
│            │                │                                           │
│  ┌─────────▼────────────────▼────────────────────────────────────────┐  │
│  │                     Raft Node (openraft)                          │  │
│  │                                                                   │  │
│  │  ┌──────────────┐  ┌──────────────┐  ┌────────────────────────┐ │  │
│  │  │ State Machine│  │  Log Store   │  │  Network Factory       │ │  │
│  │  │ (applies     │  │  (in-memory  │  │  (gRPC channels)       │ │  │
│  │  │  commands)   │  │   Raft log)  │  │                        │ │  │
│  │  └──────┬───────┘  └──────────────┘  └────────────────────────┘ │  │
│  │         │                                                         │  │
│  └─────────┼─────────────────────────────────────────────────────────┘  │
│            │                                                             │
│  ┌─────────▼─────────────────────────────────────────────────────────┐  │
│  │  ┌──────────────────────┐    ┌────────────────────────────────┐  │  │
│  │  │   Storage Backend    │    │      Index Backend             │  │  │
│  │  │   (RocksDB)          │    │      (Tantivy)                 │  │  │
│  │  │                      │    │                                │  │  │
│  │  │  - Column families   │    │  - BM25 scoring                │  │  │
│  │  │  - Snappy compress   │    │  - Fuzzy matching              │  │  │
│  │  │  - Bloom filters     │    │  - Boolean queries             │  │  │
│  │  │  - WAL               │    │  - Highlighting                │  │  │
│  │  └──────────────────────┘    └────────────────────────────────┘  │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## Write Path

A document write flows through the following stages:

```
Client POST /collections/{name}/docs
           │
           ▼
   ┌───────────────┐
   │  HTTP Handler  │   Deserialize JSON, validate, assign ID if needed
   │  (documents.rs)│
   └───────┬───────┘
           │
           ▼
   ┌───────────────┐
   │ Write Batcher  │   Buffers up to 100 writes or 10ms window
   │ (optional)     │   Reduces Raft round-trips for burst writes
   └───────┬───────┘
           │
           ▼
   ┌───────────────┐
   │  Raft Node     │   1. Leader receives write
   │  (openraft)    │   2. Appends to log
   │                │   3. Replicates to followers
   │                │   4. Commits after majority ack
   └───────┬───────┘
           │
           ▼ (on commit)
   ┌───────────────┐
   │ State Machine  │   Applies committed entry:
   │ (DbStateMachine│
   └───┬───────┬───┘
       │       │
       ▼       ▼
   ┌───────┐ ┌───────┐
   │RocksDB│ │Tantivy│   Storage: serialize + put (column family per collection)
   │  put  │ │ index │   Index: tokenize + add to inverted index
   └───────┘ └───────┘

   Response: {"_id": "...", "result": "created"}
```

### Write Path Details

1. **HTTP Handler** (`handlers/documents.rs`): Parses the request body into a
   `Document`, assigns a UUID-based `DocumentId` if none provided, validates
   that the target collection exists.

2. **Write Batcher** (`write_batcher.rs`): Accumulates individual writes into
   batches of up to 100. Flushes either when the batch is full or after a 10ms
   deadline. This amortizes the cost of Raft consensus across multiple writes.

3. **Raft Consensus** (`consensus/raft_node.rs`): The leader serializes the
   write as a `RaftCommand::Put` and appends it to the Raft log. The log entry
   is replicated to a majority of nodes before committing.

4. **State Machine** (`consensus/state_machine.rs`): On commit, the
   `DbStateMachine::apply()` method executes the command against both the
   storage backend (RocksDB) and the index backend (Tantivy).

5. **RocksDB Storage** (`storage/rocksdb_backend.rs`): Serializes the document
   with MessagePack and writes to the appropriate column family. Bloom filters
   accelerate negative lookups. Snappy compression reduces disk usage.

6. **Tantivy Index** (`index/tantivy_index.rs`): Tokenizes text fields using
   the configured analyzer (standard, keyword, ngram, or CJK). Adds the document
   to the inverted index. The index is committed periodically or on explicit
   refresh.

### Bulk Write Path

Bulk operations (`/_bulk` endpoint) parse an NDJSON stream of action/document
pairs and submit them as a single `RaftCommand::Batch`. This achieves
significantly higher throughput than individual writes.

```
NDJSON stream  ──►  Parse actions  ──►  Single Raft batch  ──►  Batch apply
                                            (1 round-trip)
```

---

## Read Path

Read operations (search and get-by-ID) do not go through Raft -- they are served
directly from the local node's storage and index.

### Get by ID

```
Client GET /collections/{name}/docs/{id}
           │
           ▼
   ┌───────────────┐
   │  Document      │   1. Check DocumentCache (moka LRU)
   │  Cache         │   2. Cache hit? Return immediately
   └───────┬───────┘
           │ (cache miss)
           ▼
   ┌───────────────┐
   │  RocksDB       │   Direct point lookup by key
   │  get()         │   (bloom filter check → block read → decompress)
   └───────┬───────┘
           │
           ▼
   ┌───────────────┐
   │  Populate      │   Store in cache (10k entries, 60s TTL)
   │  Cache         │
   └───────┬───────┘
           │
           ▼
   Response: {"_id": "...", "_source": {...}, "found": true}
```

### Full-Text Search

```
Client POST /collections/{name}/_search
           │
           ▼
   ┌───────────────┐
   │  Search        │   Parse Query DSL (match, term, range, bool, fuzzy)
   │  Handler       │   Extract SearchOptions (size, from, highlight, sort)
   └───────┬───────┘
           │
           ▼
   ┌───────────────┐
   │  Query Builder  │   Translate MSearchDB Query → Tantivy Query
   │  (query_builder)│   match → QueryParser, term → TermQuery,
   │                 │   range → RangeQuery, bool → BooleanQuery,
   │                 │   fuzzy → FuzzyTermQuery
   └───────┬───────┘
           │
           ▼
   ┌───────────────┐
   │  Tantivy       │   Execute query against inverted index
   │  Searcher      │   BM25 scoring, top-K collection
   └───────┬───────┘
           │
           ▼
   ┌───────────────┐
   │  Highlighting   │   Generate <em>...</em> fragments for matched terms
   │  (highlighting) │
   └───────┬───────┘
           │
           ▼
   ┌───────────────┐
   │  Document       │   Fetch full documents from RocksDB for top-K hits
   │  Retrieval      │   (only _source fields, not the entire index)
   └───────┬───────┘
           │
           ▼
   Response: {"hits": {"total": {...}, "hits": [...]}, "took": 5}
```

### Distributed Search (Scatter-Gather)

When the cluster has multiple nodes, search queries can be distributed:

```
Client ──► Node 1 (coordinator)
               │
      ┌────────┼────────┐
      ▼        ▼        ▼
   Node 1   Node 2   Node 3     ← Scatter: each node searches local shards
      │        │        │
      └────────┼────────┘
               ▼
          Merge + Sort             ← Gather: coordinator merges top-K results
               │
               ▼
           Response
```

The scatter-gather coordinator (`network/scatter_gather.rs`) sends parallel
gRPC requests to all nodes, collects results, merges by score, and returns
the top-K documents.

---

## Raft Consensus

MSearchDB uses the [openraft](https://github.com/databendlabs/openraft) library
for Raft consensus. This provides leader election, log replication, and
linearizable writes.

### State Machine Transitions

```
                    ┌─────────────┐
        ┌───────────│  Follower   │◄──────────────────┐
        │           └──────┬──────┘                    │
        │                  │                           │
        │    election_timeout                          │
        │    (no heartbeat)                            │
        │                  │                           │
        │           ┌──────▼──────┐                    │
        │           │  Candidate  │                    │
        │           └──────┬──────┘                    │
        │                  │                           │
        │         ┌────────┼────────┐                  │
        │         │                 │                  │
        │    majority vote     higher term             │
        │    received          discovered              │
        │         │                 │                  │
        │  ┌──────▼──────┐         │                  │
        │  │   Leader    │─────────┘                  │
        │  └──────┬──────┘                            │
        │         │                                    │
        │    higher term                               │
        │    discovered                                │
        │         │                                    │
        │         └────────────────────────────────────┘
        │
        └── receives valid heartbeat from leader
            (resets election timer)
```

### Raft Command Types

```rust
enum RaftCommand {
    Put {
        collection: String,
        document: Document,
    },
    Delete {
        collection: String,
        id: DocumentId,
    },
    CreateCollection {
        name: String,
    },
    DropCollection {
        name: String,
    },
    Batch {
        commands: Vec<RaftCommand>,
    },
}
```

### Log Replication Flow

```
Client write ──► Leader appends to local log
                     │
                     ├──► Replicate to Follower 1 (gRPC AppendEntries)
                     ├──► Replicate to Follower 2 (gRPC AppendEntries)
                     │
                     │    Wait for majority acknowledgment
                     │
                     ▼
                 Commit entry
                     │
                     ▼
                 Apply to state machine
                 (Storage + Index)
                     │
                     ▼
                 Respond to client
```

---

## Consistent Hash Ring

MSearchDB uses consistent hashing to determine which nodes are responsible for
which documents. This enables automatic data distribution and minimizes data
movement when nodes join or leave.

### Ring Structure

```
                         0 (2^256)
                        ╱          ╲
                    N1-v3          N2-v1
                   ╱                    ╲
               N3-v2                    N1-v1
              ╱                              ╲
          N2-v3        Hash Ring             N3-v3
              ╲        (BTreeMap)            ╱
               N1-v2                    N2-v2
                   ╲                    ╱
                    N3-v1          N1-v4
                        ╲          ╱
                         2^128

   Each physical node has 150 virtual nodes (vnodes) placed on the ring.
   Document placement: SHA-256(doc_id) → find next vnode clockwise → owner node.
```

### Implementation Details

- **Hash function**: SHA-256 of the document ID
- **Virtual nodes**: 150 per physical node (configurable)
- **Ring data structure**: `BTreeMap<[u8; 32], T>` for O(log n) lookup
- **Replication**: The document is stored on N consecutive distinct physical
  nodes clockwise from the hash point (where N = replication_factor)

### Example: Document Placement

```
hash("doc-42") = 0xA3F1...

Ring lookup (clockwise from 0xA3F1...):
  → N2-v47 at 0xA401...  (primary replica: Node 2)
  → N1-v12 at 0xA5B2...  (secondary replica: Node 1)
  → N3-v89 at 0xA7F0...  (tertiary replica: Node 3)
```

### Rebalancing on Topology Change

When a node joins or leaves, the `Rebalancer` computes a migration plan:

```
Before: Nodes [1, 2, 3], RF=3
After:  Nodes [1, 2, 3, 4], RF=3

Rebalancer computes:
  DataMove { doc_range: 0x00..0x40, from: Node 1, to: Node 4 }
  DataMove { doc_range: 0x80..0xC0, from: Node 2, to: Node 4 }
  ...

Status: Planned → InProgress → Completed
```

---

## Cluster Management

The `ClusterManager` handles node membership, health monitoring, and failure
detection.

### Components

```
┌──────────────────────────────────────────────────────────────┐
│                     ClusterManager                            │
│                                                               │
│  ┌─────────────────┐  ┌───────────────────────────────────┐ │
│  │ Failure Detector │  │       Gossip Manager              │ │
│  │ (Phi Accrual)    │  │                                   │ │
│  │                  │  │  - 2s gossip interval             │ │
│  │  phi > 8.0 →     │  │  - 2 random peers per round      │ │
│  │    Suspect       │  │  - Latest-timestamp-wins merge    │ │
│  │  phi > 16.0 →    │  │                                   │ │
│  │    Dead          │  │  Heartbeat + metadata propagation │ │
│  └─────────────────┘  └───────────────────────────────────┘ │
│                                                               │
│  ┌─────────────────┐  ┌───────────────────────────────────┐ │
│  │ Health Monitor   │  │    Read Repair Coordinator        │ │
│  │                  │  │                                   │ │
│  │  1s check loop   │  │  Compare replica versions         │ │
│  │  3 fails →       │  │  Update stale replicas            │ │
│  │    Offline       │  │  Resolve conflicts via vector     │ │
│  │                  │  │    clock comparison                │ │
│  └─────────────────┘  └───────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────┘
```

### Health Check State Machine

```
   ┌──────────┐   health check OK    ┌──────────┐
   │ Starting │ ────────────────────► │  Online  │
   └──────────┘                       └────┬─────┘
                                           │
                                   3 consecutive failures
                                           │
                                      ┌────▼─────┐
                                      │ Offline  │
                                      └────┬─────┘
                                           │
                                    health check OK
                                           │
                                      ┌────▼─────┐
                                      │  Online  │
                                      └──────────┘
```

### Gossip Protocol

Each node periodically (every 2 seconds) selects 2 random peers and exchanges
cluster state:

```
Node A                          Node B
  │                                │
  │  ── Gossip(my_state) ──────►  │
  │                                │  Merge: latest timestamp wins
  │  ◄── Gossip(merged_state) ──  │
  │                                │
  │  Merge: latest timestamp wins  │
  │                                │
```

This ensures eventual consistency of cluster membership information without
requiring a central coordinator.

---

## Failure Scenarios and Recovery

### Scenario 1: Follower Node Failure

```
Timeline:
  t=0s   Node 3 (follower) crashes
  t=1s   Health monitor detects failure (no heartbeat response)
  t=3s   3 consecutive failures → Node 3 marked Offline
  t=3s   Phi accrual detector: phi > 8.0 → Suspect
  t=5s   Phi accrual detector: phi > 16.0 → Dead

Impact: None for writes (leader + remaining follower still form majority)
        Reads may hit stale data if client was reading from Node 3

Recovery:
  1. Node 3 restarts
  2. Raft catches up: replays missed log entries
  3. Health monitor marks Node 3 as Online
  4. Full consistency restored
```

### Scenario 2: Leader Node Failure

```
Timeline:
  t=0s    Node 1 (leader) crashes
  t=150ms Election timeout expires on Node 2 and Node 3
  t=160ms Nodes 2 and 3 become candidates, start election
  t=200ms Node 2 wins election (received majority vote)
  t=200ms Node 2 starts accepting writes as new leader

Impact: Writes fail for ~200ms during election
        In-flight writes that were not committed are lost
        Committed writes are safe (replicated to majority)

Recovery:
  1. Node 1 restarts as follower
  2. Discovers new leader (Node 2) via heartbeat
  3. Catches up by replaying Raft log from Node 2
  4. Cluster returns to full 3-node operation
```

### Scenario 3: Network Partition (Split Brain Prevention)

```
Partition: [Node 1] | [Node 2, Node 3]

Minority side (Node 1):
  - Cannot commit writes (no majority)
  - Reads may return stale data
  - Steps down as leader after election timeout

Majority side (Node 2, Node 3):
  - Elects new leader (say Node 2)
  - Continues normal operation
  - Writes succeed (2/3 = majority)

Partition heals:
  - Node 1 discovers higher term from Node 2
  - Node 1 becomes follower
  - Catches up from Node 2's log
  - Cluster converges to consistent state
```

### Scenario 4: Data Corruption

```
Detection:
  - RocksDB checksums detect block-level corruption on read
  - WAL CRC32 checksums detect log corruption on replay
  - Tantivy segment checksums detect index corruption

Recovery:
  1. Corrupted node shuts down or marks collection as degraded
  2. Read repair coordinator detects version mismatch
  3. Healthy replicas re-send data to corrupted node
  4. Alternatively: restore from snapshot (/_restore API)
```

---

## Crate Dependency Graph

```
msearchdb-node (binary)
    │
    ├── msearchdb-network
    │       ├── msearchdb-consensus
    │       │       ├── msearchdb-storage
    │       │       │       └── msearchdb-core
    │       │       ├── msearchdb-index
    │       │       │       └── msearchdb-core
    │       │       └── msearchdb-core
    │       └── msearchdb-core
    │
    ├── msearchdb-consensus
    ├── msearchdb-storage
    ├── msearchdb-index
    └── msearchdb-core

msearchdb-client (Python SDK)
    └── msearchdb-core (re-export only)
```

### Key External Dependencies

| Dependency | Version | Used By | Purpose |
|---|---|---|---|
| `tokio` | 1.x | All crates | Async runtime |
| `axum` | 0.7 | node | HTTP framework |
| `rocksdb` | 0.22 | storage | Persistent KV store |
| `tantivy` | 0.22 | index | Full-text search engine |
| `openraft` | 0.10 | consensus | Raft consensus |
| `tonic` | 0.12 | network | gRPC framework |
| `prost` | 0.13 | network | Protobuf serialization |
| `serde` | 1.x | All crates | Serialization framework |
| `moka` | 0.12 | node | Concurrent LRU cache |
| `prometheus` | 0.13 | node | Metrics collection |
| `tracing` | 0.1 | All crates | Structured logging |

---

## Performance Characteristics

### Write Performance

| Operation | Expected Latency | Bottleneck |
|---|---|---|
| Single document index | < 5ms P99 | Raft consensus round-trip |
| Bulk index (batch) | > 10,000 docs/s | RocksDB write throughput |
| Collection create | < 10ms | Raft consensus |

### Read Performance

| Operation | Expected Latency | Bottleneck |
|---|---|---|
| Get by ID (cached) | < 0.5ms P99 | In-memory cache lookup |
| Get by ID (uncached) | < 2ms P99 | RocksDB point lookup |
| Full-text search | < 50ms P99 | Tantivy query execution |
| Simple term query | < 10ms P99 | Tantivy term lookup |

### Cluster Performance

| Operation | Expected Time | Notes |
|---|---|---|
| Leader election | < 2 seconds | election_timeout_ms = 150 |
| Follower catch-up | Proportional to log size | Streaming replay |
| Gossip convergence | < 10 seconds | 2s interval, 2 peers |
| Failure detection | < 5 seconds | 3 consecutive 1s checks |
