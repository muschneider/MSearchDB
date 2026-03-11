"""Integration tests for the MSearchDB Python client.

These tests require a running MSearchDB cluster. They are skipped automatically
if no cluster is reachable at the default addresses.

Run with::

    pytest tests/test_integration.py -v

Or to force-run even without a cluster (expect failures)::

    pytest tests/test_integration.py -v --run-integration
"""

from __future__ import annotations

import time
import uuid

import pytest
import requests

from msearchdb import MSearchDB, Q, Search
from msearchdb.exceptions import ConflictError, NotFoundError
from msearchdb.models import BulkResult, CollectionInfo, SearchResult

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

HOSTS = [
    "http://localhost:9200",
    "http://localhost:9201",
    "http://localhost:9202",
]


def cluster_is_available() -> bool:
    """Check if at least one MSearchDB node is reachable."""
    for host in HOSTS:
        try:
            resp = requests.get(f"{host}/_cluster/health", timeout=2)
            if resp.status_code == 200:
                return True
        except Exception:
            continue
    return False


# Skip all tests in this module if no cluster is available
pytestmark = pytest.mark.skipif(
    not cluster_is_available(),
    reason="MSearchDB cluster not available",
)


@pytest.fixture(scope="module")
def db():
    """Module-scoped client connected to the cluster."""
    client = MSearchDB(hosts=HOSTS, timeout=10, max_retries=3)
    yield client
    client.close()


@pytest.fixture()
def collection_name():
    """Generate a unique collection name per test to avoid collisions."""
    return f"test_{uuid.uuid4().hex[:12]}"


@pytest.fixture()
def setup_collection(db, collection_name):
    """Create a collection before the test and delete it after."""
    db.create_collection(collection_name)
    yield collection_name
    try:
        db.delete_collection(collection_name)
    except Exception:
        pass


# ---------------------------------------------------------------------------
# Collection management tests
# ---------------------------------------------------------------------------

class TestCollectionIntegration:

    def test_create_and_delete_collection(self, db, collection_name):
        info = db.create_collection(collection_name)
        assert isinstance(info, CollectionInfo)
        assert info.name == collection_name
        assert info.doc_count == 0

        result = db.delete_collection(collection_name)
        assert result["acknowledged"] is True

    def test_create_duplicate_collection(self, db, setup_collection):
        with pytest.raises(ConflictError):
            db.create_collection(setup_collection)

    def test_list_collections(self, db, setup_collection):
        collections = db.list_collections()
        names = [c.name for c in collections]
        assert setup_collection in names

    def test_get_collection_info(self, db, setup_collection):
        info = db.get_collection_info(setup_collection)
        assert info.name == setup_collection


# ---------------------------------------------------------------------------
# Document CRUD tests
# ---------------------------------------------------------------------------

class TestDocumentIntegration:

    def test_index_and_get(self, db, setup_collection):
        result = db.index(
            setup_collection,
            {"name": "Test Doc", "value": 42},
            doc_id="test-doc-1",
        )
        assert result.id == "test-doc-1"
        assert result.result == "created"

        doc = db.get(setup_collection, "test-doc-1")
        assert doc["found"] is True
        assert doc["_source"]["name"] == "Test Doc"
        assert doc["_source"]["value"] == 42

    def test_index_auto_id(self, db, setup_collection):
        result = db.index(setup_collection, {"name": "Auto ID"})
        assert result.id  # Non-empty ID generated
        assert result.result == "created"

    def test_update_document(self, db, setup_collection):
        db.index(setup_collection, {"name": "Original"}, doc_id="upd-1")
        result = db.update(setup_collection, "upd-1", {"name": "Updated"})
        assert result.result == "updated"

        doc = db.get(setup_collection, "upd-1")
        assert doc["_source"]["name"] == "Updated"

    def test_delete_document(self, db, setup_collection):
        db.index(setup_collection, {"name": "ToDelete"}, doc_id="del-1")
        result = db.delete(setup_collection, "del-1")
        assert result.result == "deleted"

        with pytest.raises(NotFoundError):
            db.get(setup_collection, "del-1")

    def test_get_nonexistent_document(self, db, setup_collection):
        with pytest.raises(NotFoundError):
            db.get(setup_collection, "does-not-exist")


# ---------------------------------------------------------------------------
# Bulk operations tests
# ---------------------------------------------------------------------------

class TestBulkIntegration:

    def test_bulk_index(self, db, setup_collection):
        documents = [
            {"id": f"bulk-{i}", "name": f"Item {i}", "value": i}
            for i in range(50)
        ]
        result = db.bulk_index(setup_collection, documents)
        assert isinstance(result, BulkResult)
        assert result.errors is False
        assert len(result.succeeded) == 50

    def test_bulk_mixed_operations(self, db, setup_collection):
        # First create a doc to delete
        db.index(setup_collection, {"name": "PreExisting"}, doc_id="pre-1")

        result = db.bulk(setup_collection, [
            {"action": "index", "_id": "mix-1", "doc": {"name": "New 1"}},
            {"action": "index", "_id": "mix-2", "doc": {"name": "New 2"}},
            {"action": "delete", "_id": "pre-1"},
        ])
        assert isinstance(result, BulkResult)
        assert len(result.items) == 3

    def test_bulk_index_large_batch(self, db, setup_collection):
        """Index 1000 documents to test batch handling."""
        documents = [
            {"id": f"large-{i}", "name": f"Document {i}", "category": "test", "value": i}
            for i in range(1000)
        ]
        result = db.bulk_index(setup_collection, documents)
        assert result.errors is False
        assert len(result.succeeded) == 1000


# ---------------------------------------------------------------------------
# Search tests
# ---------------------------------------------------------------------------

class TestSearchIntegration:

    @pytest.fixture(autouse=True)
    def _seed_data(self, db, setup_collection):
        """Seed the collection with test data before each search test."""
        self.collection = setup_collection
        documents = [
            {"id": "s1", "name": "Rust Programming Language", "category": "book",
             "price": 39.99, "in_stock": True,
             "description": "Learn systems programming with Rust"},
            {"id": "s2", "name": "Python Crash Course", "category": "book",
             "price": 29.99, "in_stock": True,
             "description": "A hands-on introduction to Python programming"},
            {"id": "s3", "name": "Go in Action", "category": "book",
             "price": 44.99, "in_stock": False,
             "description": "Practical guide to Go programming language"},
            {"id": "s4", "name": "Wireless Mouse", "category": "electronics",
             "price": 24.99, "in_stock": True,
             "description": "Ergonomic wireless mouse for productivity"},
            {"id": "s5", "name": "Mechanical Keyboard", "category": "electronics",
             "price": 89.99, "in_stock": True,
             "description": "Cherry MX switches mechanical keyboard"},
        ]
        db.bulk_index(setup_collection, documents)
        db.refresh(setup_collection)
        # Brief pause for index to settle
        time.sleep(0.3)

    def test_simple_text_search(self, db):
        results = db.search(self.collection, q="programming")
        assert isinstance(results, SearchResult)
        assert results.total >= 1

    def test_match_query(self, db):
        results = db.search(self.collection, query=Q.match("name", "rust"))
        assert results.total >= 1
        names = [h.source.get("name", "") for h in results.hits]
        assert any("Rust" in n for n in names)

    def test_term_query(self, db):
        results = db.search(self.collection, query=Q.term("category", "electronics"))
        assert results.total >= 1
        for hit in results.hits:
            assert hit.source.get("category") == "electronics"

    def test_range_query(self, db):
        results = db.search(
            self.collection,
            query=Q.range("price", gte=30, lte=50),
        )
        assert results.total >= 1
        for hit in results.hits:
            price = hit.source.get("price", 0)
            assert 30 <= price <= 50

    def test_bool_query(self, db):
        results = db.search(
            self.collection,
            query=Q.bool(
                must=[Q.term("category", "book")],
                must_not=[Q.term("in_stock", False)],
            ),
        )
        for hit in results.hits:
            assert hit.source.get("category") == "book"
            assert hit.source.get("in_stock") is True

    def test_search_with_size(self, db):
        results = db.search(self.collection, query=Q.match_all(), size=2)
        assert len(results.hits) <= 2

    def test_search_builder_integration(self, db):
        body = (
            Search()
            .query(Q.bool(
                must=[Q.match("description", "programming")],
                filter=[Q.range("price", lte=40)],
            ))
            .size(10)
            .highlight("name", "description")
            .to_dict()
        )
        results = db.search(self.collection, **body)
        assert isinstance(results, SearchResult)
        assert results.total >= 1


# ---------------------------------------------------------------------------
# Cluster tests
# ---------------------------------------------------------------------------

class TestClusterIntegration:

    def test_cluster_health(self, db):
        health = db.cluster_health()
        assert "status" in health
        assert health["status"] in ("green", "yellow", "red")
        assert "number_of_nodes" in health

    def test_cluster_state(self, db):
        state = db.cluster_state()
        assert "nodes" in state
        assert isinstance(state["nodes"], list)

    def test_list_nodes(self, db):
        nodes = db.list_nodes()
        assert isinstance(nodes, list)
        assert len(nodes) >= 1

    def test_node_stats(self, db):
        stats = db.node_stats()
        assert "node_id" in stats


# ---------------------------------------------------------------------------
# Failover test
# ---------------------------------------------------------------------------

class TestFailoverIntegration:

    def test_operations_with_multiple_hosts(self):
        """Verify client works when given multiple host addresses."""
        available_hosts = []
        for host in HOSTS:
            try:
                requests.get(f"{host}/_cluster/health", timeout=2)
                available_hosts.append(host)
            except Exception:
                continue

        if len(available_hosts) < 2:
            pytest.skip("Need at least 2 reachable nodes for failover test")

        # Add a bad host to the mix — client should skip it
        test_hosts = ["http://localhost:19999"] + available_hosts
        db = MSearchDB(hosts=test_hosts, max_retries=len(test_hosts), timeout=3)
        try:
            health = db.cluster_health()
            assert health["status"] in ("green", "yellow", "red")
        finally:
            db.close()
