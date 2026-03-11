"""MSearchDB Python Client SDK.

A full-featured client for the MSearchDB distributed NoSQL database with
full-text search.

Quick start::

    from msearchdb import MSearchDB, Q, Search

    db = MSearchDB(hosts=["http://localhost:9200"])
    db.create_collection("products")
    db.index("products", {"name": "Laptop", "price": 999.99}, doc_id="1")
    results = db.search("products", q="laptop")
    for hit in results.hits:
        print(hit.source)
"""

from __future__ import annotations

__version__ = "0.1.0"

# Core client
from .client import MSearchDB

# Query DSL
from .query_builder import Q, Search

# Models
from .models import (
    BulkItemResult,
    BulkResult,
    CollectionInfo,
    Hit,
    IndexResult,
    SearchResult,
)

# Exceptions
from .exceptions import (
    AuthenticationError,
    BulkError,
    ConflictError,
    ConnectionError,
    InvalidInputError,
    MSearchDBError,
    NotFoundError,
    ServiceUnavailableError,
    TimeoutError,
)

# Async client (lazy import — requires aiohttp)
def __getattr__(name: str):
    if name == "AsyncMSearchDB":
        from .async_client import AsyncMSearchDB
        return AsyncMSearchDB
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


__all__ = [
    # Client
    "MSearchDB",
    "AsyncMSearchDB",
    # Query DSL
    "Q",
    "Search",
    # Models
    "BulkItemResult",
    "BulkResult",
    "CollectionInfo",
    "Hit",
    "IndexResult",
    "SearchResult",
    # Exceptions
    "AuthenticationError",
    "BulkError",
    "ConflictError",
    "ConnectionError",
    "InvalidInputError",
    "MSearchDBError",
    "NotFoundError",
    "ServiceUnavailableError",
    "TimeoutError",
]
