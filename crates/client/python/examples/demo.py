#!/usr/bin/env python3
"""MSearchDB Python Client Demo.

Demonstrates the full feature set of the MSearchDB Python client against a
running 3-node cluster (default ports: 9200, 9201, 9202).

Usage::

    python examples/demo.py
"""

from __future__ import annotations

import sys
import time

# Allow running from the python/ directory
sys.path.insert(0, ".")

from msearchdb import MSearchDB, Q, Search
from msearchdb.exceptions import ConflictError, NotFoundError

HOSTS = [
    "http://localhost:9200",
    "http://localhost:9201",
    "http://localhost:9202",
]

COLLECTION = "products"


def main() -> None:
    print("=" * 60)
    print("  MSearchDB Python Client Demo")
    print("=" * 60)

    db = MSearchDB(hosts=HOSTS, max_retries=3, timeout=10)

    # ------------------------------------------------------------------
    # 1. Cluster health
    # ------------------------------------------------------------------
    print("\n--- Cluster Health ---")
    health = db.cluster_health()
    print(f"  Status:  {health.get('status')}")
    print(f"  Nodes:   {health.get('number_of_nodes')}")
    print(f"  Leader:  {health.get('leader_id')}")

    # ------------------------------------------------------------------
    # 2. Create collection (idempotent)
    # ------------------------------------------------------------------
    print(f"\n--- Creating collection '{COLLECTION}' ---")
    try:
        info = db.create_collection(COLLECTION, settings={"analyzer": "standard"})
        print(f"  Created: {info.name} (docs: {info.doc_count})")
    except ConflictError:
        print(f"  Collection '{COLLECTION}' already exists (OK)")

    # ------------------------------------------------------------------
    # 3. Index individual documents
    # ------------------------------------------------------------------
    print("\n--- Indexing individual document ---")
    result = db.index(
        COLLECTION,
        {"name": "AirPods Pro 2", "category": "headphones", "price": 249.99,
         "description": "Apple wireless earbuds with ANC", "in_stock": True},
        doc_id="airpods",
    )
    print(f"  Indexed: {result.id} ({result.result})")

    # ------------------------------------------------------------------
    # 4. Bulk index documents
    # ------------------------------------------------------------------
    print("\n--- Bulk indexing products ---")
    products = [
        {"id": "1", "name": "MacBook Pro 16", "category": "laptop",
         "price": 2499.99,
         "description": "Powerful Apple laptop with M3 chip", "in_stock": True},
        {"id": "2", "name": "ThinkPad X1 Carbon", "category": "laptop",
         "price": 1899.99,
         "description": "Ultra-light business laptop by Lenovo", "in_stock": True},
        {"id": "3", "name": "Sony WH-1000XM5", "category": "headphones",
         "price": 349.99,
         "description": "Best noise cancelling headphones", "in_stock": False},
        {"id": "4", "name": "Dell XPS 15", "category": "laptop",
         "price": 1799.99,
         "description": "Premium Dell laptop with OLED display", "in_stock": True},
        {"id": "5", "name": "iPad Air", "category": "tablet",
         "price": 599.99,
         "description": "Apple tablet with M2 chip", "in_stock": True},
    ]
    bulk_result = db.bulk_index(COLLECTION, products)
    print(f"  Took: {bulk_result.took}ms, Errors: {bulk_result.errors}")
    print(f"  Succeeded: {len(bulk_result.succeeded)}, "
          f"Failed: {len(bulk_result.failed)}")

    # ------------------------------------------------------------------
    # 5. Refresh index (make documents searchable)
    # ------------------------------------------------------------------
    print("\n--- Refreshing index ---")
    db.refresh(COLLECTION)
    print("  Done")

    # Brief pause for index to settle
    time.sleep(0.5)

    # ------------------------------------------------------------------
    # 6. Get a document by ID
    # ------------------------------------------------------------------
    print("\n--- Get document by ID ---")
    doc = db.get(COLLECTION, "1")
    print(f"  ID: {doc['_id']}")
    print(f"  Source: {doc['_source']}")

    # ------------------------------------------------------------------
    # 7. Update a document
    # ------------------------------------------------------------------
    print("\n--- Update document ---")
    update_result = db.update(COLLECTION, "1", {
        "name": "MacBook Pro 16 (2024)",
        "category": "laptop",
        "price": 2599.99,
        "description": "Updated Apple laptop with M4 Max chip",
        "in_stock": True,
    })
    print(f"  Updated: {update_result.id} ({update_result.result})")

    # ------------------------------------------------------------------
    # 8. Simple text search
    # ------------------------------------------------------------------
    print("\n--- Simple text search: 'laptop' ---")
    results = db.search(COLLECTION, q="laptop")
    print(f"  Total hits: {results.total}, took: {results.took}ms")
    for hit in results.hits:
        name = hit.source.get("name", "?")
        price = hit.source.get("price", "?")
        print(f"    {hit.id}: {name} (${price}) [score={hit.score:.3f}]")

    # ------------------------------------------------------------------
    # 9. Advanced DSL search with builder
    # ------------------------------------------------------------------
    print("\n--- Advanced query: laptops under $2000 in stock ---")
    query_body = (
        Search()
        .query(Q.bool(
            must=[Q.match("description", "laptop")],
            filter=[
                Q.range("price", lte=2000.0),
                Q.term("in_stock", True),
            ],
        ))
        .sort("price", "asc")
        .highlight("name", "description")
        .size(10)
        .to_dict()
    )
    results = db.search(COLLECTION, **query_body)
    print(f"  Total hits: {results.total}")
    for hit in results.hits:
        name = hit.source.get("name", "?")
        price = hit.source.get("price", "?")
        print(f"    {hit.id}: {name} (${price})")
        if hit.highlight:
            for field, fragments in hit.highlight.items():
                print(f"      highlight[{field}]: {fragments[0]}")

    # ------------------------------------------------------------------
    # 10. Fuzzy search (typo-tolerant)
    # ------------------------------------------------------------------
    print("\n--- Fuzzy search: 'lptop' (typo) ---")
    results = db.search(COLLECTION, query=Q.fuzzy("name", "lptop", fuzziness=2))
    print(f"  Total hits: {results.total}")
    for hit in results.hits:
        print(f"    {hit.id}: {hit.source.get('name')}")

    # ------------------------------------------------------------------
    # 11. Term query (exact match)
    # ------------------------------------------------------------------
    print("\n--- Term query: category='headphones' ---")
    results = db.search(COLLECTION, query=Q.term("category", "headphones"))
    print(f"  Total hits: {results.total}")
    for hit in results.hits:
        print(f"    {hit.id}: {hit.source.get('name')}")

    # ------------------------------------------------------------------
    # 12. Range query
    # ------------------------------------------------------------------
    print("\n--- Range query: price 200..400 ---")
    results = db.search(COLLECTION, query=Q.range("price", gte=200, lte=400))
    print(f"  Total hits: {results.total}")
    for hit in results.hits:
        print(f"    {hit.id}: {hit.source.get('name')} (${hit.source.get('price')})")

    # ------------------------------------------------------------------
    # 13. Delete a document
    # ------------------------------------------------------------------
    print("\n--- Delete document 'airpods' ---")
    del_result = db.delete(COLLECTION, "airpods")
    print(f"  Deleted: {del_result.id} ({del_result.result})")

    # Verify it's gone
    try:
        db.get(COLLECTION, "airpods")
        print("  ERROR: document still exists!")
    except NotFoundError:
        print("  Confirmed: document no longer exists")

    # ------------------------------------------------------------------
    # 14. List collections
    # ------------------------------------------------------------------
    print("\n--- List collections ---")
    collections = db.list_collections()
    for c in collections:
        print(f"  {c.name}: {c.doc_count} docs")

    # ------------------------------------------------------------------
    # 15. Node stats
    # ------------------------------------------------------------------
    print("\n--- Node stats ---")
    stats = db.node_stats()
    for key, val in stats.items():
        print(f"  {key}: {val}")

    # ------------------------------------------------------------------
    # Cleanup
    # ------------------------------------------------------------------
    print(f"\n--- Cleaning up: deleting collection '{COLLECTION}' ---")
    db.delete_collection(COLLECTION)
    print("  Done")

    db.close()
    print("\n" + "=" * 60)
    print("  Demo complete!")
    print("=" * 60)


if __name__ == "__main__":
    main()
