# MSearchDB

A distributed NoSQL database with full-text search capabilities, written in Rust.

MSearchDB is inspired by Elasticsearch and designed from the ground up to leverage
Rust's ownership model, zero-cost abstractions, and type system to deliver a safe,
performant, and maintainable distributed search engine.

[![Vibe Coded](https://img.shields.io/badge/vibe_coded-✨-purple)](https://vibecoded.dev)

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        HTTP / gRPC API                          │
│                      (crates/network)                           │
├────────────┬──────────────┬──────────────┬──────────────────────┤
│   Query    │   Indexing   │  Replication  │   Cluster Mgmt      │
│   Engine   │   Engine     │  (Raft)       │                     │
│ (crates/   │ (crates/     │ (crates/      │ (crates/consensus)  │
│  index)    │  index)      │  consensus)   │                     │
├────────────┴──────────────┴──────────────┴──────────────────────┤
│                      Storage Engine                             │
│                      (crates/storage)                           │
├─────────────────────────────────────────────────────────────────┤
│                  Core Domain Types & Traits                     │
│                      (crates/core)                              │
└─────────────────────────────────────────────────────────────────┘
```

### Crate Responsibilities

| Crate                     | Purpose                                                                                                                 |
| ------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| **`msearchdb-core`**      | Shared domain types (Document, Query, Error), trait abstractions, and configuration. Every other crate depends on this. |
| **`msearchdb-storage`**   | Persistent key-value storage engine. Implements `StorageBackend` trait.                                                 |
| **`msearchdb-index`**     | Full-text search engine with inverted index, tokenization, and BM25 scoring. Implements `IndexBackend` trait.           |
| **`msearchdb-consensus`** | Raft consensus protocol for leader election and log replication. Implements `ReplicationLog` trait.                     |
| **`msearchdb-network`**   | HTTP REST API for clients and gRPC for inter-node communication.                                                        |
| **`msearchdb-node`**      | Binary that wires everything together into a running database node.                                                     |
| **`msearchdb-client`**    | HTTP client library, designed to be Python-friendly via future PyO3 bindings.                                           |

## Rust Concepts Used

### Newtype Pattern (Zero-Cost Type Safety)

`DocumentId` and `NodeId` wrap primitive types (`String` and `u64`) to prevent
accidental misuse at the type level. The compiler enforces that you cannot pass
a raw `u64` where a `NodeId` is expected, yet at runtime the wrapper has zero
overhead — the newtype is erased during compilation.

```rust
pub struct DocumentId(String);  // Cannot be confused with any other String
pub struct NodeId(u64);         // Cannot be confused with a port number
```

### Ownership & Borrowing

Trait methods like `StorageBackend::get(&self, id: &DocumentId)` borrow data
immutably, while `put(document: Document)` takes ownership — the caller
transfers the document into storage. This makes the API self-documenting:
you can see at the signature level who owns what.

### Trait-Based Abstraction (Not Inheritance)

MSearchDB uses Rust traits instead of class hierarchies. `StorageBackend`,
`IndexBackend`, `ReplicationLog`, and `HealthCheck` are all traits. This means:

- Concrete types implement the trait without inheriting behavior.
- Trait objects (`dyn StorageBackend`) can be used for dynamic dispatch.
- Generics (`impl StorageBackend`) can be used for static dispatch (monomorphized, zero-cost).
- Mock implementations are trivial to write for testing.

### Zero-Cost Serde Serialization

Using `#[derive(Serialize, Deserialize)]` from serde, every domain type gains
JSON (and TOML, MessagePack, etc.) serialization at compile time. The derive
macro generates specialized code — there is no reflection or runtime overhead.

### Error Handling with `thiserror`

The `DbError` enum uses `#[derive(Error)]` from thiserror to generate `Display`
and `std::error::Error` implementations. Each variant carries a context string,
and `From` conversions from `serde_json::Error` and `std::io::Error` enable
seamless use with the `?` operator.

### Forward Compatibility with `#[non_exhaustive]`

Public enums like `FieldValue`, `Query`, `NodeStatus`, and `DbError` are marked
`#[non_exhaustive]`. This means downstream code must include a wildcard arm in
match statements, allowing us to add new variants in minor releases without
breaking semver.

## Dependency Choices

| Dependency             | Why                                                                                                                 |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------- |
| **tokio**              | The de facto async runtime for Rust. "full" features enable timers, I/O, and sync primitives needed for a database. |
| **serde + serde_json** | Industry-standard serialization framework. Zero-cost derive macros, broad ecosystem support.                        |
| **toml**               | Configuration files are written in TOML — human-readable, widely used in the Rust ecosystem.                        |
| **thiserror**          | Generates `Display` and `Error` impls from enum attributes. Keeps error definitions concise.                        |
| **tracing**            | Structured, async-aware logging. Superior to `log` for distributed systems because it supports spans and fields.    |
| **uuid**               | RFC 4122 V4 UUIDs for generating unique document identifiers without coordination.                                  |
| **async-trait**        | Enables async methods in traits until Rust stabilizes native async traits.                                          |

## Getting Started

```bash
# Build the entire workspace
cargo build --workspace

# Run all tests
cargo test --workspace

# Start a node (placeholder)
cargo run --bin msearchdb
```
