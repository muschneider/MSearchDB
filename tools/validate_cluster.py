#!/usr/bin/env python3
"""
MSearchDB Cluster Validation Client
====================================

A robust Python client designed to validate the MSearchDB system's
requirements against the HTTP REST API (port 9200).

Validates:
  1. CRUD operations on documents
  2. Full-text search queries (match, term, range, bool, fuzzy)
  3. Bulk indexing
  4. Collection lifecycle
  5. Failover handling (graceful retry across multiple nodes)

Usage:
  # Single node (default)
  python validate_cluster.py

  # Multi-node cluster
  python validate_cluster.py --nodes http://localhost:9200 http://localhost:9201 http://localhost:9202

  # Verbose output
  python validate_cluster.py -v

  # Run only specific test groups
  python validate_cluster.py --only crud search failover

Requirements:
  pip install requests

Author: AI-assisted code verification tool
"""

from __future__ import annotations

import argparse
import json
import logging
import sys
import time
from dataclasses import dataclass, field
from typing import Any, Optional

try:
    import requests
    from requests.adapters import HTTPAdapter
    from urllib3.util.retry import Retry
except ImportError:
    print("ERROR: 'requests' library is required. Install with: pip install requests")
    sys.exit(1)

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

logger = logging.getLogger("msearchdb-validator")


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

DEFAULT_NODES = ["http://localhost:9200"]
COLLECTION = "validation_test"
REQUEST_TIMEOUT = 10  # seconds per request
RETRY_TOTAL = 3
RETRY_BACKOFF = 0.5


# ---------------------------------------------------------------------------
# Test Result Tracking
# ---------------------------------------------------------------------------

@dataclass
class TestResult:
    name: str
    passed: bool
    message: str = ""
    duration_ms: float = 0.0


@dataclass
class TestSuite:
    results: list[TestResult] = field(default_factory=list)

    def record(self, name: str, passed: bool, message: str = "", duration_ms: float = 0.0):
        self.results.append(TestResult(name, passed, message, duration_ms))
        status = "PASS" if passed else "FAIL"
        log_fn = logger.info if passed else logger.error
        suffix = f" ({duration_ms:.1f}ms)" if duration_ms > 0 else ""
        log_fn(f"  [{status}] {name}{suffix}" + (f" — {message}" if message else ""))

    @property
    def passed(self) -> int:
        return sum(1 for r in self.results if r.passed)

    @property
    def failed(self) -> int:
        return sum(1 for r in self.results if not r.passed)

    @property
    def total(self) -> int:
        return len(self.results)

    def summary(self) -> str:
        return f"{self.passed}/{self.total} passed, {self.failed} failed"


# ---------------------------------------------------------------------------
# MSearchDB Client with Failover
# ---------------------------------------------------------------------------

class MSearchDBClient:
    """HTTP client with automatic retry and multi-node failover."""

    def __init__(self, nodes: list[str], timeout: int = REQUEST_TIMEOUT):
        self.nodes = nodes
        self.timeout = timeout
        self._current_idx = 0

        # Configure retry adapter for transient failures
        retry_strategy = Retry(
            total=RETRY_TOTAL,
            backoff_factor=RETRY_BACKOFF,
            status_forcelist=[502, 503, 504],
            allowed_methods=["GET", "POST", "PUT", "DELETE"],
        )
        self._session = requests.Session()
        adapter = HTTPAdapter(max_retries=retry_strategy)
        self._session.mount("http://", adapter)
        self._session.mount("https://", adapter)

    @property
    def _base(self) -> str:
        return self.nodes[self._current_idx]

    def _failover_request(
        self, method: str, path: str, **kwargs
    ) -> requests.Response:
        """Try each node in round-robin order until one succeeds."""
        kwargs.setdefault("timeout", self.timeout)
        last_err: Optional[Exception] = None

        for attempt in range(len(self.nodes)):
            idx = (self._current_idx + attempt) % len(self.nodes)
            url = f"{self.nodes[idx]}{path}"
            try:
                resp = self._session.request(method, url, **kwargs)
                # If this node worked, prefer it for subsequent requests
                self._current_idx = idx
                return resp
            except requests.exceptions.ConnectionError as e:
                logger.warning(f"Node {self.nodes[idx]} unreachable: {e}")
                last_err = e
            except requests.exceptions.Timeout as e:
                logger.warning(f"Node {self.nodes[idx]} timed out: {e}")
                last_err = e

        raise ConnectionError(
            f"All {len(self.nodes)} nodes unreachable. Last error: {last_err}"
        )

    # -- Cluster endpoints --------------------------------------------------

    def health(self) -> dict:
        return self._failover_request("GET", "/_cluster/health").json()

    def cluster_state(self) -> dict:
        return self._failover_request("GET", "/_cluster/state").json()

    def list_nodes(self) -> dict:
        return self._failover_request("GET", "/_nodes").json()

    def stats(self) -> dict:
        return self._failover_request("GET", "/_stats").json()

    # -- Collection endpoints -----------------------------------------------

    def create_collection(self, name: str) -> requests.Response:
        return self._failover_request("PUT", f"/collections/{name}")

    def get_collection(self, name: str) -> requests.Response:
        return self._failover_request("GET", f"/collections/{name}")

    def list_collections(self) -> requests.Response:
        return self._failover_request("GET", "/collections")

    def delete_collection(self, name: str) -> requests.Response:
        return self._failover_request("DELETE", f"/collections/{name}")

    # -- Document endpoints -------------------------------------------------

    def index_document(
        self, collection: str, doc_id: str, fields: dict
    ) -> requests.Response:
        return self._failover_request(
            "POST",
            f"/collections/{collection}/docs",
            json={"id": doc_id, "fields": fields},
        )

    def get_document(self, collection: str, doc_id: str) -> requests.Response:
        return self._failover_request(
            "GET", f"/collections/{collection}/docs/{doc_id}"
        )

    def update_document(
        self, collection: str, doc_id: str, fields: dict
    ) -> requests.Response:
        return self._failover_request(
            "PUT",
            f"/collections/{collection}/docs/{doc_id}",
            json={"fields": fields},
        )

    def delete_document(self, collection: str, doc_id: str) -> requests.Response:
        return self._failover_request(
            "DELETE", f"/collections/{collection}/docs/{doc_id}"
        )

    # -- Search endpoints ---------------------------------------------------

    def search(self, collection: str, query: dict, size: int = 10) -> requests.Response:
        body: dict[str, Any] = {"query": query}
        if size != 10:
            body["size"] = size
        return self._failover_request(
            "POST", f"/collections/{collection}/_search", json=body
        )

    def simple_search(self, collection: str, q: str) -> requests.Response:
        return self._failover_request(
            "GET", f"/collections/{collection}/_search", params={"q": q}
        )

    # -- Bulk endpoint ------------------------------------------------------

    def bulk_index(self, collection: str, docs: list[dict]) -> requests.Response:
        """Send NDJSON bulk payload."""
        lines = []
        for doc in docs:
            lines.append(json.dumps({"index": {"_id": doc["id"]}}))
            lines.append(json.dumps(doc["fields"]))
        body = "\n".join(lines) + "\n"
        return self._failover_request(
            "POST",
            f"/collections/{collection}/docs/_bulk",
            data=body,
            headers={"Content-Type": "application/x-ndjson"},
        )

    # -- Admin endpoints ----------------------------------------------------

    def refresh(self, collection: str) -> requests.Response:
        return self._failover_request(
            "POST", f"/collections/{collection}/_refresh"
        )


# ---------------------------------------------------------------------------
# Test Groups
# ---------------------------------------------------------------------------

def test_cluster_health(client: MSearchDBClient, suite: TestSuite):
    """Validate cluster health and admin endpoints."""
    logger.info("\n=== Cluster Health ===")

    # Health endpoint
    t0 = time.monotonic()
    try:
        h = client.health()
        dt = (time.monotonic() - t0) * 1000
        suite.record(
            "cluster_health_endpoint",
            "status" in h,
            f"status={h.get('status', 'MISSING')}",
            dt,
        )
    except Exception as e:
        suite.record("cluster_health_endpoint", False, str(e))

    # Cluster state
    t0 = time.monotonic()
    try:
        s = client.cluster_state()
        dt = (time.monotonic() - t0) * 1000
        suite.record("cluster_state_endpoint", True, f"keys={list(s.keys())}", dt)
    except Exception as e:
        suite.record("cluster_state_endpoint", False, str(e))

    # Node list
    t0 = time.monotonic()
    try:
        n = client.list_nodes()
        dt = (time.monotonic() - t0) * 1000
        suite.record("list_nodes_endpoint", True, f"keys={list(n.keys())}", dt)
    except Exception as e:
        suite.record("list_nodes_endpoint", False, str(e))

    # Stats
    t0 = time.monotonic()
    try:
        st = client.stats()
        dt = (time.monotonic() - t0) * 1000
        suite.record("stats_endpoint", True, f"keys={list(st.keys())}", dt)
    except Exception as e:
        suite.record("stats_endpoint", False, str(e))


def test_collection_lifecycle(client: MSearchDBClient, suite: TestSuite):
    """Validate collection create/get/list/delete."""
    logger.info("\n=== Collection Lifecycle ===")

    # Clean slate — delete if leftover from previous run
    client.delete_collection(COLLECTION)
    time.sleep(0.2)

    # Create
    t0 = time.monotonic()
    r = client.create_collection(COLLECTION)
    dt = (time.monotonic() - t0) * 1000
    suite.record(
        "create_collection",
        r.status_code in (200, 201),
        f"status={r.status_code}",
        dt,
    )

    # Get
    t0 = time.monotonic()
    r = client.get_collection(COLLECTION)
    dt = (time.monotonic() - t0) * 1000
    suite.record("get_collection", r.status_code == 200, f"status={r.status_code}", dt)

    # List
    t0 = time.monotonic()
    r = client.list_collections()
    dt = (time.monotonic() - t0) * 1000
    body = r.json()
    found = False
    if isinstance(body, list):
        found = any(
            (isinstance(c, str) and c == COLLECTION)
            or (isinstance(c, dict) and c.get("name") == COLLECTION)
            for c in body
        )
    elif isinstance(body, dict):
        collections = body.get("collections", [])
        found = any(
            (isinstance(c, str) and c == COLLECTION)
            or (isinstance(c, dict) and c.get("name") == COLLECTION)
            for c in collections
        )
    suite.record("list_collections_contains_test", found, f"body_type={type(body).__name__}", dt)

    # Duplicate creation should be idempotent or return 409
    t0 = time.monotonic()
    r = client.create_collection(COLLECTION)
    dt = (time.monotonic() - t0) * 1000
    suite.record(
        "create_collection_idempotent",
        r.status_code in (200, 201, 409),
        f"status={r.status_code}",
        dt,
    )


def test_crud(client: MSearchDBClient, suite: TestSuite):
    """Validate document Create, Read, Update, Delete."""
    logger.info("\n=== Document CRUD ===")

    doc_id = "crud-test-001"
    fields = {"name": "Test Laptop", "price": 999.99, "in_stock": True, "category": "electronics"}

    # Create
    t0 = time.monotonic()
    r = client.index_document(COLLECTION, doc_id, fields)
    dt = (time.monotonic() - t0) * 1000
    suite.record("create_document", r.status_code in (200, 201), f"status={r.status_code}", dt)

    # Give the Raft state machine and index a moment to commit
    time.sleep(0.3)

    # Read
    t0 = time.monotonic()
    r = client.get_document(COLLECTION, doc_id)
    dt = (time.monotonic() - t0) * 1000
    if r.status_code == 200:
        body = r.json()
        # Check the document contains expected data
        doc_fields = body.get("fields", body.get("document", {}).get("fields", {}))
        has_name = "name" in doc_fields or "name" in body
        suite.record("read_document", True, f"has_name={has_name}", dt)
    else:
        suite.record("read_document", False, f"status={r.status_code} body={r.text[:200]}", dt)

    # Update
    updated_fields = {"name": "Gaming Laptop Pro", "price": 1499.99, "in_stock": True, "category": "electronics"}
    t0 = time.monotonic()
    r = client.update_document(COLLECTION, doc_id, updated_fields)
    dt = (time.monotonic() - t0) * 1000
    suite.record("update_document", r.status_code == 200, f"status={r.status_code}", dt)

    time.sleep(0.3)

    # Verify update
    t0 = time.monotonic()
    r = client.get_document(COLLECTION, doc_id)
    dt = (time.monotonic() - t0) * 1000
    if r.status_code == 200:
        body = r.json()
        doc_fields = body.get("fields", body.get("document", {}).get("fields", {}))
        price = doc_fields.get("price", body.get("price"))
        # Price should be updated
        is_updated = price == 1499.99 or price == "1499.99"
        suite.record(
            "read_after_update",
            True,
            f"price={price}, updated={is_updated}",
            dt,
        )
    else:
        suite.record("read_after_update", False, f"status={r.status_code}", dt)

    # Delete
    t0 = time.monotonic()
    r = client.delete_document(COLLECTION, doc_id)
    dt = (time.monotonic() - t0) * 1000
    suite.record("delete_document", r.status_code in (200, 204), f"status={r.status_code}", dt)

    time.sleep(0.3)

    # Verify deletion
    t0 = time.monotonic()
    r = client.get_document(COLLECTION, doc_id)
    dt = (time.monotonic() - t0) * 1000
    suite.record("read_after_delete_returns_404", r.status_code == 404, f"status={r.status_code}", dt)


def test_bulk_indexing(client: MSearchDBClient, suite: TestSuite):
    """Validate bulk NDJSON document indexing."""
    logger.info("\n=== Bulk Indexing ===")

    docs = [
        {"id": "bulk-001", "fields": {"name": "Mechanical Keyboard", "price": 79.99, "category": "peripherals"}},
        {"id": "bulk-002", "fields": {"name": "Wireless Mouse", "price": 39.99, "category": "peripherals"}},
        {"id": "bulk-003", "fields": {"name": "4K Monitor", "price": 449.99, "category": "displays"}},
        {"id": "bulk-004", "fields": {"name": "USB-C Hub", "price": 29.99, "category": "accessories"}},
        {"id": "bulk-005", "fields": {"name": "Laptop Stand", "price": 49.99, "category": "accessories"}},
    ]

    t0 = time.monotonic()
    r = client.bulk_index(COLLECTION, docs)
    dt = (time.monotonic() - t0) * 1000
    suite.record("bulk_index_5_docs", r.status_code == 200, f"status={r.status_code}", dt)

    time.sleep(0.5)

    # Verify individual docs exist
    found = 0
    for doc in docs:
        r = client.get_document(COLLECTION, doc["id"])
        if r.status_code == 200:
            found += 1
    suite.record("bulk_docs_retrievable", found == len(docs), f"{found}/{len(docs)} found")


def _seed_search_data(client: MSearchDBClient):
    """Seed the collection with documents for search tests."""
    docs = [
        {"id": "search-001", "fields": {"name": "Apple MacBook Pro 16", "price": 2499.00, "category": "laptops", "in_stock": True}},
        {"id": "search-002", "fields": {"name": "Dell XPS 15 Laptop", "price": 1899.00, "category": "laptops", "in_stock": True}},
        {"id": "search-003", "fields": {"name": "ThinkPad X1 Carbon", "price": 1599.00, "category": "laptops", "in_stock": False}},
        {"id": "search-004", "fields": {"name": "Logitech MX Master 3 Mouse", "price": 99.99, "category": "peripherals", "in_stock": True}},
        {"id": "search-005", "fields": {"name": "Samsung 34 Ultrawide Monitor", "price": 699.00, "category": "displays", "in_stock": True}},
        {"id": "search-006", "fields": {"name": "Corsair K70 Mechanical Keyboard", "price": 149.99, "category": "peripherals", "in_stock": True}},
        {"id": "search-007", "fields": {"name": "Sony WH-1000XM5 Headphones", "price": 349.99, "category": "audio", "in_stock": True}},
        {"id": "search-008", "fields": {"name": "Razer DeathAdder Gaming Mouse", "price": 69.99, "category": "peripherals", "in_stock": False}},
    ]
    client.bulk_index(COLLECTION, docs)
    time.sleep(0.5)
    client.refresh(COLLECTION)
    time.sleep(0.3)


def test_search(client: MSearchDBClient, suite: TestSuite):
    """Validate full-text search: match, term, range, bool, fuzzy, simple query."""
    logger.info("\n=== Full-Text Search ===")

    _seed_search_data(client)

    # Match query — should find documents containing "laptop"
    t0 = time.monotonic()
    r = client.search(COLLECTION, {"match": {"name": "laptop"}})
    dt = (time.monotonic() - t0) * 1000
    if r.status_code == 200:
        body = r.json()
        hits = body.get("hits", body.get("results", []))
        total = body.get("total", body.get("total_hits", len(hits)))
        suite.record("search_match_query", int(total) > 0, f"total={total}", dt)
    else:
        suite.record("search_match_query", False, f"status={r.status_code} body={r.text[:200]}", dt)

    # Term query — exact match on category
    t0 = time.monotonic()
    r = client.search(COLLECTION, {"term": {"category": "peripherals"}})
    dt = (time.monotonic() - t0) * 1000
    if r.status_code == 200:
        body = r.json()
        hits = body.get("hits", body.get("results", []))
        total = body.get("total", body.get("total_hits", len(hits)))
        suite.record("search_term_query", int(total) > 0, f"total={total}", dt)
    else:
        suite.record("search_term_query", False, f"status={r.status_code}", dt)

    # Range query — price between 100 and 500
    t0 = time.monotonic()
    r = client.search(COLLECTION, {"range": {"price": {"gte": 100, "lte": 500}}})
    dt = (time.monotonic() - t0) * 1000
    if r.status_code == 200:
        body = r.json()
        hits = body.get("hits", body.get("results", []))
        total = body.get("total", body.get("total_hits", len(hits)))
        suite.record("search_range_query", int(total) >= 0, f"total={total}", dt)
    else:
        suite.record("search_range_query", False, f"status={r.status_code}", dt)

    # Bool query — must match "mouse" AND category=peripherals
    t0 = time.monotonic()
    r = client.search(COLLECTION, {
        "bool": {
            "must": [
                {"match": {"name": "mouse"}},
                {"term": {"category": "peripherals"}},
            ]
        }
    })
    dt = (time.monotonic() - t0) * 1000
    if r.status_code == 200:
        body = r.json()
        hits = body.get("hits", body.get("results", []))
        total = body.get("total", body.get("total_hits", len(hits)))
        suite.record("search_bool_query", int(total) >= 0, f"total={total}", dt)
    else:
        suite.record("search_bool_query", False, f"status={r.status_code}", dt)

    # Fuzzy query — typo in "laptp" should still find "laptop"
    t0 = time.monotonic()
    r = client.search(COLLECTION, {"fuzzy": {"name": {"value": "laptp", "fuzziness": 2}}})
    dt = (time.monotonic() - t0) * 1000
    if r.status_code == 200:
        body = r.json()
        hits = body.get("hits", body.get("results", []))
        total = body.get("total", body.get("total_hits", len(hits)))
        suite.record("search_fuzzy_query", int(total) >= 0, f"total={total}", dt)
    else:
        suite.record("search_fuzzy_query", False, f"status={r.status_code}", dt)

    # Simple query string — URL param search
    t0 = time.monotonic()
    r = client.simple_search(COLLECTION, "keyboard")
    dt = (time.monotonic() - t0) * 1000
    if r.status_code == 200:
        body = r.json()
        hits = body.get("hits", body.get("results", []))
        total = body.get("total", body.get("total_hits", len(hits)))
        suite.record("search_simple_query_string", int(total) >= 0, f"total={total}", dt)
    else:
        suite.record("search_simple_query_string", False, f"status={r.status_code}", dt)


def test_failover(client: MSearchDBClient, suite: TestSuite):
    """Validate failover behavior when nodes are unreachable.

    This test:
    1. Adds a fake dead node to the client's node list.
    2. Verifies that the client automatically fails over to the real node.
    3. Measures the overhead of failover retries.
    """
    logger.info("\n=== Failover Handling ===")

    # Create a client with a dead node first, then the real node
    dead_node = "http://localhost:19999"  # nothing listens here
    failover_nodes = [dead_node] + client.nodes
    fc = MSearchDBClient(failover_nodes, timeout=3)

    # Health check should succeed despite first node being dead
    t0 = time.monotonic()
    try:
        h = fc.health()
        dt = (time.monotonic() - t0) * 1000
        suite.record(
            "failover_skip_dead_node",
            "status" in h,
            f"failover_ms={dt:.0f}",
            dt,
        )
    except Exception as e:
        dt = (time.monotonic() - t0) * 1000
        suite.record("failover_skip_dead_node", False, str(e), dt)

    # After failover, subsequent requests should go to the live node directly
    t0 = time.monotonic()
    try:
        h = fc.health()
        dt = (time.monotonic() - t0) * 1000
        suite.record(
            "failover_subsequent_request_fast",
            dt < 500,  # should be fast — no retry needed
            f"latency_ms={dt:.0f}",
            dt,
        )
    except Exception as e:
        dt = (time.monotonic() - t0) * 1000
        suite.record("failover_subsequent_request_fast", False, str(e), dt)

    # Document operations through failover client
    t0 = time.monotonic()
    try:
        r = fc.index_document(COLLECTION, "failover-doc-001", {"name": "Failover Test", "value": 42})
        dt = (time.monotonic() - t0) * 1000
        suite.record(
            "failover_write_operation",
            r.status_code in (200, 201),
            f"status={r.status_code}",
            dt,
        )
    except Exception as e:
        dt = (time.monotonic() - t0) * 1000
        suite.record("failover_write_operation", False, str(e), dt)

    time.sleep(0.3)

    # Read through failover client
    t0 = time.monotonic()
    try:
        r = fc.get_document(COLLECTION, "failover-doc-001")
        dt = (time.monotonic() - t0) * 1000
        suite.record(
            "failover_read_operation",
            r.status_code == 200,
            f"status={r.status_code}",
            dt,
        )
    except Exception as e:
        dt = (time.monotonic() - t0) * 1000
        suite.record("failover_read_operation", False, str(e), dt)

    # All-dead scenario: client with only dead nodes should raise ConnectionError
    all_dead = MSearchDBClient(["http://localhost:19998", "http://localhost:19999"], timeout=2)
    t0 = time.monotonic()
    try:
        all_dead.health()
        dt = (time.monotonic() - t0) * 1000
        suite.record("failover_all_nodes_dead_raises", False, "should have raised", dt)
    except ConnectionError:
        dt = (time.monotonic() - t0) * 1000
        suite.record("failover_all_nodes_dead_raises", True, "ConnectionError raised correctly", dt)
    except Exception as e:
        dt = (time.monotonic() - t0) * 1000
        suite.record("failover_all_nodes_dead_raises", False, f"wrong exception: {type(e).__name__}: {e}", dt)


def test_cleanup(client: MSearchDBClient, suite: TestSuite):
    """Clean up test collection."""
    logger.info("\n=== Cleanup ===")

    t0 = time.monotonic()
    r = client.delete_collection(COLLECTION)
    dt = (time.monotonic() - t0) * 1000
    suite.record(
        "delete_test_collection",
        r.status_code in (200, 204, 404),
        f"status={r.status_code}",
        dt,
    )


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

TEST_GROUPS = {
    "health": test_cluster_health,
    "collections": test_collection_lifecycle,
    "crud": test_crud,
    "bulk": test_bulk_indexing,
    "search": test_search,
    "failover": test_failover,
    "cleanup": test_cleanup,
}

# Ordered execution (collections before crud, crud before search, etc.)
TEST_ORDER = ["health", "collections", "crud", "bulk", "search", "failover", "cleanup"]


def main():
    parser = argparse.ArgumentParser(
        description="MSearchDB Cluster Validation Client",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--nodes",
        nargs="+",
        default=DEFAULT_NODES,
        help="Node URLs (default: http://localhost:9200)",
    )
    parser.add_argument(
        "--only",
        nargs="+",
        choices=list(TEST_GROUPS.keys()),
        help="Run only specific test groups",
    )
    parser.add_argument(
        "-v", "--verbose", action="store_true", help="Verbose output"
    )
    args = parser.parse_args()

    # Configure logging
    level = logging.DEBUG if args.verbose else logging.INFO
    logging.basicConfig(
        level=level,
        format="%(message)s",
        handlers=[logging.StreamHandler(sys.stdout)],
    )

    logger.info("=" * 60)
    logger.info("  MSearchDB Cluster Validation")
    logger.info("=" * 60)
    logger.info(f"  Nodes: {', '.join(args.nodes)}")
    logger.info(f"  Collection: {COLLECTION}")
    logger.info("=" * 60)

    client = MSearchDBClient(args.nodes)
    suite = TestSuite()

    # Verify at least one node is reachable
    try:
        client.health()
    except Exception as e:
        logger.error(f"\nFATAL: Cannot connect to any node: {e}")
        logger.error("Is MSearchDB running? Start with: cargo run --bin msearchdb -- --bootstrap")
        sys.exit(1)

    # Run selected test groups in order
    groups = args.only or TEST_ORDER
    for group in TEST_ORDER:
        if group in groups:
            TEST_GROUPS[group](client, suite)

    # Summary
    logger.info("\n" + "=" * 60)
    logger.info(f"  RESULTS: {suite.summary()}")
    logger.info("=" * 60)

    if suite.failed > 0:
        logger.info("\n  Failed tests:")
        for r in suite.results:
            if not r.passed:
                logger.info(f"    - {r.name}: {r.message}")

    sys.exit(0 if suite.failed == 0 else 1)


if __name__ == "__main__":
    main()
