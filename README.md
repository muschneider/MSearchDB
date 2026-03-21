# MSearchDB

A distributed NoSQL database with full-text search capabilities, written in Rust.

MSearchDB is inspired by Elasticsearch and designed from the ground up to leverage
Rust's ownership model, zero-cost abstractions, and type system to deliver a safe,
performant, and maintainable distributed search engine.

## Architecture

```
                              ┌──────────────┐
                              │   Clients    │
                              │ (Python SDK, │
                              │  HTTP, curl) │
                              └──────┬───────┘
                                     │
              ┌──────────────────────┼──────────────────────┐
              │                      │                      │
     ┌────────▼────────┐   ┌────────▼────────┐   ┌────────▼────────┐
     │   Node 1 (L)    │   │    Node 2       │   │    Node 3       │
     │   :9201         │   │    :9202        │   │    :9203        │
     │                 │◄──►                 │◄──►                 │
     │  ┌───────────┐  │   │  ┌───────────┐  │   │  ┌───────────┐  │
     │  │ HTTP API  │  │   │  │ HTTP API  │  │   │  │ HTTP API  │  │
     │  │  (axum)   │  │   │  │  (axum)   │  │   │  │  (axum)   │  │
     │  ├───────────┤  │   │  ├───────────┤  │   │  ├───────────┤  │
     │  │ Search    │  │   │  │ Search    │  │   │  │ Search    │  │
     │  │ (Tantivy) │  │   │  │ (Tantivy) │  │   │  │ (Tantivy) │  │
     │  ├───────────┤  │   │  ├───────────┤  │   │  ├───────────┤  │
     │  │ Storage   │  │   │  │ Storage   │  │   │  │ Storage   │  │
     │  │ (RocksDB) │  │   │  │ (RocksDB) │  │   │  │ (RocksDB) │  │
     │  ├───────────┤  │   │  ├───────────┤  │   │  ├───────────┤  │
     │  │ Consensus │  │   │  │ Consensus │  │   │  │ Consensus │  │
     │  │  (Raft)   │  │   │  │  (Raft)   │  │   │  │  (Raft)   │  │
     │  └───────────┘  │   │  └───────────┘  │   │  └───────────┘  │
     └─────────────────┘   └─────────────────┘   └─────────────────┘
              │                      │                      │
              └──────────────────────┼──────────────────────┘
                                     │
                            gRPC inter-node
                            (tonic + protobuf)
```

### Crate Responsibilities

| Crate | Purpose |
|---|---|
| **`msearchdb-core`** | Domain types (`Document`, `Query`, `Error`), trait abstractions, consistent hashing, cluster routing, rebalancing |
| **`msearchdb-storage`** | RocksDB-backed persistent storage with WAL, memtable, column families |
| **`msearchdb-index`** | Tantivy full-text search with BM25 scoring, fuzzy matching, highlighting |
| **`msearchdb-consensus`** | Raft consensus via openraft for leader election and log replication |
| **`msearchdb-network`** | gRPC inter-node communication, connection pooling, scatter-gather |
| **`msearchdb-node`** | HTTP REST API server (axum), the main binary entry point |
| **`msearchdb-client`** | Python SDK with sync/async clients, query builder, bulk helpers |

## Why Rust?

### Zero-Cost Abstractions
Traits like `StorageBackend` and `IndexBackend` define clean interfaces without
any runtime overhead. Generics are monomorphized at compile time -- the compiler
generates specialized machine code for each concrete type, so abstraction is
free.

### Memory Safety Without GC
Rust's ownership system guarantees memory safety at compile time. No garbage
collector pauses, no use-after-free, no data races. For a database that manages
its own memory buffers and handles concurrent requests, this eliminates entire
classes of bugs that plague C/C++ search engines.

### Fearless Concurrency
The type system enforces thread safety. Types that implement `Send + Sync` can
be shared across threads. `Arc<RwLock<T>>` provides shared mutable state with
compile-time guarantees that locks are held correctly. The `tokio` async runtime
efficiently multiplexes thousands of concurrent connections onto a small thread
pool.

## Quick Start

### Option 1: Docker Compose (3-node cluster)

```bash
# Start the full cluster with observability stack
docker compose -f docker/docker-compose.yml up --build -d

# Wait for cluster to form (nodes join automatically)
sleep 15

# Verify cluster health
curl -s http://localhost:9201/_cluster/health | python3 -m json.tool

# Run the full demo
pip install requests
python examples/full_demo.py

# Access observability
# Grafana:    http://localhost:3000  (admin/admin)
# Prometheus: http://localhost:9090
# Jaeger:     http://localhost:16686
```

### Option 2: Single Node (local development)

```bash
# Build and run
cargo build --release
cargo run --release --bin msearchdb -- --bootstrap

# In another terminal, test it
curl -s http://localhost:9200/_cluster/health

# Run demo against single node
python examples/full_demo.py --single http://localhost:9200

# Or with fewer documents for a quick test
python examples/full_demo.py --single http://localhost:9200 --docs 1000
```

### Option 3: Python SDK

```bash
# Install the SDK
pip install -e crates/client/python

# Use in your code
python3 -c "
from msearchdb import MSearchDB, Q

db = MSearchDB(hosts=['http://localhost:9200'])
db.create_collection('products')
db.index('products', {'name': 'Laptop', 'price': 999.99}, doc_id='1')
db.refresh('products')
results = db.search('products', q='laptop')
print(f'Found {results.total} results')
for hit in results.hits:
    print(f'  {hit.id}: {hit.source}')
db.close()
"
```

## Full API Reference

### Collection Management

| Method | Endpoint | Description |
|---|---|---|
| `PUT` | `/collections/{name}` | Create a collection |
| `GET` | `/collections` | List all collections |
| `GET` | `/collections/{name}` | Get collection info |
| `DELETE` | `/collections/{name}` | Delete a collection |

#### Create Collection

```bash
# Simple creation
curl -X PUT http://localhost:9200/collections/products

# With settings
curl -X PUT http://localhost:9200/collections/products \
  -H 'Content-Type: application/json' \
  -d '{"schema": {"fields": {"name": "text", "price": "float"}}}'
```

### Document CRUD

| Method | Endpoint | Description |
|---|---|---|
| `POST` | `/collections/{name}/docs` | Index a document |
| `GET` | `/collections/{name}/docs/{id}` | Get document by ID |
| `PUT` | `/collections/{name}/docs/{id}` | Update (upsert) a document |
| `DELETE` | `/collections/{name}/docs/{id}` | Delete a document |

#### Index a Document

```bash
curl -X POST http://localhost:9200/collections/products/docs \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "laptop-1",
    "fields": {
      "name": "Gaming Laptop",
      "price": 1299.99,
      "category": "electronics",
      "in_stock": true
    }
  }'
```

Response:
```json
{"_id": "laptop-1", "result": "created"}
```

#### Get a Document

```bash
curl http://localhost:9200/collections/products/docs/laptop-1
```

Response:
```json
{
  "_id": "laptop-1",
  "_source": {"name": "Gaming Laptop", "price": 1299.99, ...},
  "found": true
}
```

### Search

| Method | Endpoint | Description |
|---|---|---|
| `POST` | `/collections/{name}/_search` | Query DSL search |
| `GET` | `/collections/{name}/_search?q=term` | Simple text search |
| `POST` | `/collections/{name}/_refresh` | Force index refresh |

#### Full-Text Search (Match Query)

```bash
curl -X POST http://localhost:9200/collections/products/_search \
  -H 'Content-Type: application/json' \
  -d '{"query": {"match": {"name": "gaming laptop"}}}'
```

#### Term Query (Exact Match)

```bash
curl -X POST http://localhost:9200/collections/products/_search \
  -H 'Content-Type: application/json' \
  -d '{"query": {"term": {"category": "electronics"}}}'
```

#### Range Query

```bash
curl -X POST http://localhost:9200/collections/products/_search \
  -H 'Content-Type: application/json' \
  -d '{"query": {"range": {"price": {"gte": 500, "lte": 2000}}}}'
```

#### Boolean Query

```bash
curl -X POST http://localhost:9200/collections/products/_search \
  -H 'Content-Type: application/json' \
  -d '{
    "query": {
      "bool": {
        "must": [
          {"match": {"name": "laptop"}},
          {"term": {"in_stock": true}}
        ],
        "must_not": [
          {"range": {"price": {"gte": 2000}}}
        ]
      }
    },
    "size": 20
  }'
```

#### Fuzzy Search (Typo Tolerance)

```bash
curl -X POST http://localhost:9200/collections/products/_search \
  -H 'Content-Type: application/json' \
  -d '{"query": {"fuzzy": {"name": {"value": "laptpo", "fuzziness": 2}}}}'
```

#### Simple Query String

```bash
curl 'http://localhost:9200/collections/products/_search?q=laptop&size=10'
```

### Bulk Operations

```bash
# NDJSON format: alternating action lines and document lines
curl -X POST http://localhost:9200/collections/products/docs/_bulk \
  -H 'Content-Type: application/x-ndjson' \
  -d '{"index":{"_id":"1"}}
{"name":"Keyboard","price":49.99}
{"index":{"_id":"2"}}
{"name":"Mouse","price":29.99}
{"index":{"_id":"3"}}
{"name":"Monitor","price":399.99}
'
```

Response:
```json
{
  "took": 5,
  "errors": false,
  "items": [
    {"_id": "1", "action": "index", "status": 201},
    {"_id": "2", "action": "index", "status": 201},
    {"_id": "3", "action": "index", "status": 201}
  ]
}
```

### Cluster Management

| Method | Endpoint | Description |
|---|---|---|
| `GET` | `/_cluster/health` | Cluster health status |
| `GET` | `/_cluster/state` | Full cluster state |
| `GET` | `/_nodes` | List cluster nodes |
| `POST` | `/_nodes/{id}/_join` | Join a node to the cluster |
| `GET` | `/_stats` | Node statistics |
| `GET` | `/metrics` | Prometheus metrics |

#### Cluster Health

```bash
curl http://localhost:9200/_cluster/health
```

Response:
```json
{
  "status": "green",
  "number_of_nodes": 3,
  "leader_id": 1,
  "is_leader": true
}
```

### Aliases

| Method | Endpoint | Description |
|---|---|---|
| `PUT` | `/_aliases/{name}` | Create/update an alias |
| `GET` | `/_aliases` | List all aliases |
| `GET` | `/_aliases/{name}` | Get alias info |
| `DELETE` | `/_aliases/{name}` | Delete an alias |

### Snapshots (Backup/Restore)

| Method | Endpoint | Description |
|---|---|---|
| `POST` | `/_snapshot` | Create a snapshot |
| `GET` | `/_snapshot` | List snapshots |
| `GET` | `/_snapshot/{id}` | Download snapshot |
| `POST` | `/_restore` | Restore from snapshot |

## Configuration Reference

Configuration is loaded from a TOML file and can be overridden by CLI flags.

### TOML Configuration File

```toml
[node]
id = 1                              # Unique node ID (u64)
data_dir = "/var/lib/msearchdb"     # Data directory for storage and index

[network]
http_host = "0.0.0.0"              # HTTP API bind address
http_port = 9200                    # HTTP API port
grpc_host = "0.0.0.0"              # gRPC bind address
grpc_port = 9300                    # gRPC port for inter-node communication
peers = []                          # List of peer addresses ["node2:9300"]

[storage]
write_buffer_mb = 64                # RocksDB write buffer size
max_open_files = 1000               # Maximum open file descriptors
compression = true                  # Enable Snappy compression

[index]
heap_size_mb = 128                  # Tantivy writer heap size
merge_policy = "log"                # Index merge policy

[cluster]
replication_factor = 3              # Number of replicas per document
election_timeout_ms = 150           # Raft election timeout
heartbeat_interval_ms = 50          # Raft heartbeat interval

[auth]
api_key = ""                        # API key (empty = disabled)

[observability]
log_level = "info"                  # Log level: trace, debug, info, warn, error
metrics_port = 9100                 # Prometheus metrics port
```

### CLI Flags

```
msearchdb [OPTIONS]

Options:
  --config <FILE>        Path to TOML config file
  --node-id <ID>         Node ID (overrides config)
  --data-dir <DIR>       Data directory (overrides config)
  --http-port <PORT>     HTTP API port (overrides config)
  --grpc-port <PORT>     gRPC port (overrides config)
  --peers <ADDRS>        Comma-separated peer addresses
  --bootstrap            Bootstrap a new single-node cluster
  --json-log             Output logs in JSON format
  --log-level <LEVEL>    Log level: trace, debug, info, warn, error
```

## Operations Guide

### Starting a Cluster

```bash
# Node 1: Bootstrap the cluster
msearchdb --node-id 1 --bootstrap --http-port 9200 --grpc-port 9300

# Node 2: Join the cluster
msearchdb --node-id 2 --http-port 9201 --grpc-port 9301 --peers "node1:9300"

# Node 3: Join the cluster
msearchdb --node-id 3 --http-port 9202 --grpc-port 9302 --peers "node1:9300"
```

### Adding a Node

```bash
# 1. Start the new node with --peers pointing to an existing node
msearchdb --node-id 4 --http-port 9203 --grpc-port 9303 --peers "node1:9300"

# 2. Or use the join API from the new node
curl -X POST http://node1:9200/_nodes/4/_join
```

### Removing a Node

```bash
# Gracefully shut down the node (SIGTERM)
# The node will leave the cluster and transfer leadership if needed
kill -TERM <pid>
```

### Backup (Snapshots)

```bash
# Create a snapshot
curl -X POST http://localhost:9200/_snapshot
# Response: {"id": "snap-20250321-001", "status": "created"}

# List snapshots
curl http://localhost:9200/_snapshot

# Download a snapshot
curl http://localhost:9200/_snapshot/snap-20250321-001 -o backup.tar

# Restore from snapshot
curl -X POST http://localhost:9200/_restore \
  -H 'Content-Type: application/json' \
  -d '{"snapshot_id": "snap-20250321-001"}'
```

### Monitoring

The cluster exposes Prometheus metrics at `/metrics` on each node:

```bash
# Scrape metrics
curl http://localhost:9200/metrics

# Key metrics:
# msearchdb_requests_total{method, path, status}    - Request counter
# msearchdb_request_duration_seconds{method, path}  - Latency histogram
# msearchdb_documents_indexed_total                 - Documents indexed
# msearchdb_search_queries_total                    - Search query counter
# msearchdb_raft_term                               - Current Raft term
# msearchdb_cluster_nodes                           - Number of cluster nodes
```

The Docker Compose setup includes pre-configured Grafana dashboards.

## Troubleshooting

### Common Issues

**Port already in use**
```
Error: Address already in use (os error 98)
```
Solution: Change the port with `--http-port` or stop the conflicting process.

**RocksDB lock file**
```
Error: IO error: lock /var/lib/msearchdb/storage/LOCK: Resource temporarily unavailable
```
Solution: Only one process can open a RocksDB directory. Stop any existing
instance or use a different `--data-dir`.

**Node cannot join cluster**
```
Error: Connection refused
```
Solution: Ensure the bootstrap node is running and the `--peers` address is
correct (hostname:grpc_port, not http_port).

**Search returns no results after indexing**
```bash
# Force a search index refresh
curl -X POST http://localhost:9200/collections/{name}/_refresh
```
The index is eventually consistent. Refresh forces all buffered documents to
become searchable immediately.

**Out of memory**
Reduce the index heap size and RocksDB write buffer:
```toml
[storage]
write_buffer_mb = 32

[index]
heap_size_mb = 64
```

### Log Levels

Set `--log-level debug` for verbose output during troubleshooting:
```bash
msearchdb --bootstrap --log-level debug
```

Use `--json-log` for structured JSON logs suitable for log aggregation.

## Performance Tuning

### Storage (RocksDB)

| Setting | Default | Recommendation |
|---|---|---|
| `write_buffer_mb` | 64 | Increase to 128-256 for write-heavy workloads |
| `max_open_files` | 1000 | Increase for large datasets (10k+) |
| `compression` | true | Disable for max throughput at cost of disk space |

### Index (Tantivy)

| Setting | Default | Recommendation |
|---|---|---|
| `heap_size_mb` | 128 | Increase to 256-512 for large bulk imports |
| `merge_policy` | "log" | Use "log" for balanced read/write performance |

### Cluster (Raft)

| Setting | Default | Recommendation |
|---|---|---|
| `replication_factor` | 3 | 3 for production, 1 for development |
| `election_timeout_ms` | 150 | Increase on high-latency networks (300-500) |
| `heartbeat_interval_ms` | 50 | Should be < election_timeout / 3 |

### OS-Level Tuning

```bash
# Increase open file limit
ulimit -n 65536

# Increase virtual memory areas (for RocksDB mmap)
sysctl -w vm.max_map_count=262144
```

## Comparison with Elasticsearch

| Feature | MSearchDB | Elasticsearch |
|---|---|---|
| **Language** | Rust (compiled, no GC) | Java (JVM, GC pauses) |
| **Memory safety** | Compile-time guaranteed | Runtime (JVM manages) |
| **Search engine** | Tantivy (Rust) | Apache Lucene (Java) |
| **Storage engine** | RocksDB (LSM-tree) | Lucene segments |
| **Consensus** | Raft (openraft) | Zen Discovery / Raft (7.x+) |
| **Serialization** | serde (zero-copy capable) | Jackson (reflection-based) |
| **Binary size** | ~30MB static binary | ~500MB+ (JVM + libs) |
| **Startup time** | < 1 second | 10-30 seconds |
| **Memory footprint** | 50-200MB typical | 1-4GB minimum heap |
| **GC pauses** | None | 10-200ms (G1GC) |

### Where MSearchDB is Better

- **Startup time**: Sub-second cold start vs 10-30s for Elasticsearch
- **Memory efficiency**: No JVM overhead, no GC pauses
- **Binary size**: Single static binary, easy to deploy
- **Resource usage**: Runs well on 512MB RAM; Elasticsearch needs 1GB minimum heap
- **Predictable latency**: No GC pauses means consistent P99 latencies

### Where Elasticsearch is Better

- **Maturity**: 14+ years of production hardening, massive ecosystem
- **Features**: Aggregations, percolator, machine learning, SQL, canvas
- **Horizontal scaling**: Battle-tested at petabyte scale
- **Ecosystem**: Kibana, Logstash, Beats, APM, SIEM
- **Community**: Thousands of plugins, extensive documentation
- **Multi-tenancy**: Index lifecycle management, rollover, shrink

## Development

```bash
# Build the entire workspace
cargo build --workspace

# Run all tests
cargo test --workspace

# Lint with clippy (warnings as errors)
cargo clippy --workspace -- -D warnings

# Format code
cargo fmt --all

# Run benchmarks
cargo bench -p msearchdb-node

# Type-check only (fastest feedback loop)
cargo check --workspace
```

## License

MIT
