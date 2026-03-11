"""Tests for the synchronous MSearchDB client (mocked HTTP)."""

from __future__ import annotations

import json

import pytest
import responses

from msearchdb.client import MSearchDB
from msearchdb.exceptions import (
    ConflictError,
    ConnectionError,
    NotFoundError,
    InvalidInputError,
    AuthenticationError,
    ServiceUnavailableError,
)
from msearchdb.models import (
    BulkResult,
    CollectionInfo,
    IndexResult,
    SearchResult,
)

BASE = "http://localhost:9200"


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def db():
    client = MSearchDB(hosts=[BASE], max_retries=1)
    yield client
    client.close()


@pytest.fixture
def multi_db():
    """Client with multiple hosts for failover tests."""
    client = MSearchDB(
        hosts=["http://host1:9200", "http://host2:9200"],
        max_retries=2,
    )
    yield client
    client.close()


# ---------------------------------------------------------------------------
# Collection management
# ---------------------------------------------------------------------------

class TestCollections:

    @responses.activate
    def test_create_collection(self, db):
        responses.add(
            responses.PUT,
            f"{BASE}/collections/products",
            json={"name": "products", "doc_count": 0},
            status=200,
        )
        result = db.create_collection("products")
        assert isinstance(result, CollectionInfo)
        assert result.name == "products"
        assert result.doc_count == 0

    @responses.activate
    def test_create_collection_with_settings(self, db):
        responses.add(
            responses.PUT,
            f"{BASE}/collections/products",
            json={"name": "products", "doc_count": 0},
            status=200,
        )
        result = db.create_collection("products", settings={"analyzer": "standard"})
        assert result.name == "products"
        # Verify schema was sent in request body
        body = json.loads(responses.calls[0].request.body)
        assert body["schema"] == {"analyzer": "standard"}

    @responses.activate
    def test_create_collection_conflict(self, db):
        responses.add(
            responses.PUT,
            f"{BASE}/collections/products",
            json={
                "error": {"type": "resource_already_exists", "reason": "already exists"},
                "status": 409,
            },
            status=409,
        )
        with pytest.raises(ConflictError):
            db.create_collection("products")

    @responses.activate
    def test_delete_collection(self, db):
        responses.add(
            responses.DELETE,
            f"{BASE}/collections/products",
            json={"acknowledged": True},
            status=200,
        )
        result = db.delete_collection("products")
        assert result["acknowledged"] is True

    @responses.activate
    def test_delete_collection_not_found(self, db):
        responses.add(
            responses.DELETE,
            f"{BASE}/collections/products",
            json={"error": {"type": "not_found", "reason": "not found"}, "status": 404},
            status=404,
        )
        with pytest.raises(NotFoundError):
            db.delete_collection("products")

    @responses.activate
    def test_list_collections(self, db):
        responses.add(
            responses.GET,
            f"{BASE}/collections",
            json=[
                {"name": "products", "doc_count": 42},
                {"name": "articles", "doc_count": 100},
            ],
            status=200,
        )
        result = db.list_collections()
        assert len(result) == 2
        assert all(isinstance(c, CollectionInfo) for c in result)
        assert result[0].name == "products"
        assert result[1].doc_count == 100

    @responses.activate
    def test_get_collection_info(self, db):
        responses.add(
            responses.GET,
            f"{BASE}/collections/products",
            json={"name": "products", "doc_count": 42},
            status=200,
        )
        result = db.get_collection_info("products")
        assert result.name == "products"
        assert result.doc_count == 42


# ---------------------------------------------------------------------------
# Document CRUD
# ---------------------------------------------------------------------------

class TestDocuments:

    @responses.activate
    def test_index_document(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/docs",
            json={"_id": "doc1", "result": "created", "version": 1},
            status=201,
        )
        result = db.index("products", {"name": "Laptop"}, doc_id="doc1")
        assert isinstance(result, IndexResult)
        assert result.id == "doc1"
        assert result.result == "created"

    @responses.activate
    def test_index_document_auto_id(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/docs",
            json={"_id": "auto-uuid", "result": "created", "version": 1},
            status=201,
        )
        result = db.index("products", {"name": "Laptop"})
        assert result.id == "auto-uuid"
        # Verify no "id" in request body
        body = json.loads(responses.calls[0].request.body)
        assert "id" not in body

    @responses.activate
    def test_get_document(self, db):
        responses.add(
            responses.GET,
            f"{BASE}/collections/products/docs/doc1",
            json={
                "_id": "doc1",
                "_source": {"name": "Laptop", "price": 999.99},
                "found": True,
            },
            status=200,
        )
        result = db.get("products", "doc1")
        assert result["_id"] == "doc1"
        assert result["_source"]["name"] == "Laptop"
        assert result["found"] is True

    @responses.activate
    def test_get_document_not_found(self, db):
        responses.add(
            responses.GET,
            f"{BASE}/collections/products/docs/missing",
            json={"error": {"type": "not_found", "reason": "not found"}, "status": 404},
            status=404,
        )
        with pytest.raises(NotFoundError):
            db.get("products", "missing")

    @responses.activate
    def test_update_document(self, db):
        responses.add(
            responses.PUT,
            f"{BASE}/collections/products/docs/doc1",
            json={"_id": "doc1", "result": "updated", "version": 1},
            status=200,
        )
        result = db.update("products", "doc1", {"name": "Gaming Laptop"})
        assert result.result == "updated"
        body = json.loads(responses.calls[0].request.body)
        assert body["fields"]["name"] == "Gaming Laptop"

    @responses.activate
    def test_delete_document(self, db):
        responses.add(
            responses.DELETE,
            f"{BASE}/collections/products/docs/doc1",
            json={"_id": "doc1", "result": "deleted"},
            status=200,
        )
        result = db.delete("products", "doc1")
        assert result.result == "deleted"


# ---------------------------------------------------------------------------
# Search
# ---------------------------------------------------------------------------

class TestSearch:

    @responses.activate
    def test_simple_search(self, db):
        responses.add(
            responses.GET,
            f"{BASE}/collections/products/_search",
            json={
                "hits": {
                    "total": {"value": 1, "relation": "eq"},
                    "hits": [
                        {"_id": "1", "_score": 1.5, "_source": {"name": "Laptop"}},
                    ],
                },
                "took": 5,
                "timed_out": False,
            },
            status=200,
        )
        result = db.search("products", q="laptop")
        assert isinstance(result, SearchResult)
        assert result.total == 1
        assert len(result.hits) == 1
        assert result.hits[0].id == "1"
        assert result.hits[0].score == 1.5
        assert result.hits[0].source["name"] == "Laptop"

    @responses.activate
    def test_dsl_search(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/_search",
            json={
                "hits": {
                    "total": {"value": 2, "relation": "eq"},
                    "hits": [
                        {"_id": "1", "_score": 2.0, "_source": {"name": "A"}},
                        {"_id": "2", "_score": 1.0, "_source": {"name": "B"}},
                    ],
                },
                "took": 3,
                "timed_out": False,
            },
            status=200,
        )
        result = db.search(
            "products",
            query={"match": {"name": {"query": "test", "operator": "or"}}},
            size=20,
        )
        assert result.total == 2
        assert len(result.hits) == 2

    @responses.activate
    def test_search_with_highlight(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/_search",
            json={
                "hits": {
                    "total": {"value": 1, "relation": "eq"},
                    "hits": [
                        {
                            "_id": "1",
                            "_score": 1.0,
                            "_source": {"name": "Laptop"},
                            "highlight": {"name": ["<em>Laptop</em>"]},
                        },
                    ],
                },
                "took": 2,
                "timed_out": False,
            },
            status=200,
        )
        result = db.search(
            "products",
            query={"match_all": {}},
            highlight={"fields": ["name"]},
        )
        assert result.hits[0].highlight == {"name": ["<em>Laptop</em>"]}

    @responses.activate
    def test_search_with_sort(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/_search",
            json={
                "hits": {
                    "total": {"value": 0, "relation": "eq"},
                    "hits": [],
                },
                "took": 1,
                "timed_out": False,
            },
            status=200,
        )
        db.search(
            "products",
            query={"match_all": {}},
            sort=[{"field": "price", "order": "asc"}],
        )
        body = json.loads(responses.calls[0].request.body)
        assert body["sort"] == [{"field": "price", "order": "asc"}]

    @responses.activate
    def test_search_with_pagination(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/_search",
            json={
                "hits": {
                    "total": {"value": 0, "relation": "eq"},
                    "hits": [],
                },
                "took": 1,
                "timed_out": False,
            },
            status=200,
        )
        db.search("products", query={"match_all": {}}, size=5, from_=10)
        body = json.loads(responses.calls[0].request.body)
        assert body["size"] == 5
        assert body["from"] == 10

    @responses.activate
    def test_search_result_iteration(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/_search",
            json={
                "hits": {
                    "total": {"value": 2, "relation": "eq"},
                    "hits": [
                        {"_id": "1", "_score": 2.0, "_source": {"x": 1}},
                        {"_id": "2", "_score": 1.0, "_source": {"x": 2}},
                    ],
                },
                "took": 1,
                "timed_out": False,
            },
            status=200,
        )
        result = db.search("products", query={"match_all": {}})
        # Test __len__
        assert len(result) == 2
        # Test __iter__
        ids = [hit.id for hit in result]
        assert ids == ["1", "2"]


# ---------------------------------------------------------------------------
# Bulk operations
# ---------------------------------------------------------------------------

class TestBulk:

    @responses.activate
    def test_bulk_operations(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/docs/_bulk",
            json={
                "took": 50,
                "errors": False,
                "items": [
                    {"action": "index", "_id": "1", "status": 201},
                    {"action": "delete", "_id": "2", "status": 200},
                ],
            },
            status=200,
        )
        result = db.bulk("products", [
            {"action": "index", "_id": "1", "doc": {"name": "Laptop"}},
            {"action": "delete", "_id": "2"},
        ])
        assert isinstance(result, BulkResult)
        assert result.took == 50
        assert result.errors is False
        assert len(result.items) == 2
        assert len(result.succeeded) == 2
        assert len(result.failed) == 0

    @responses.activate
    def test_bulk_index_convenience(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/docs/_bulk",
            json={
                "took": 30,
                "errors": False,
                "items": [
                    {"action": "index", "_id": "1", "status": 201},
                    {"action": "index", "_id": "2", "status": 201},
                ],
            },
            status=200,
        )
        result = db.bulk_index("products", [
            {"id": "1", "name": "Keyboard"},
            {"id": "2", "name": "Mouse"},
        ])
        assert result.errors is False
        assert len(result.succeeded) == 2

        # Verify NDJSON body format
        body = responses.calls[0].request.body
        lines = body.strip().split("\n")
        assert len(lines) == 4  # 2 action lines + 2 doc lines
        # First action line
        action = json.loads(lines[0])
        assert action == {"index": {"_id": "1"}}
        # First doc line
        doc = json.loads(lines[1])
        assert doc == {"name": "Keyboard"}

    @responses.activate
    def test_bulk_with_errors(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/docs/_bulk",
            json={
                "took": 50,
                "errors": True,
                "items": [
                    {"action": "index", "_id": "1", "status": 201},
                    {"action": "index", "_id": "2", "status": 400, "error": "parse error"},
                ],
            },
            status=200,
        )
        result = db.bulk_index("products", [
            {"id": "1", "name": "OK"},
            {"id": "2", "name": "Bad"},
        ])
        assert result.errors is True
        assert len(result.succeeded) == 1
        assert len(result.failed) == 1
        assert result.failed[0].error == "parse error"


# ---------------------------------------------------------------------------
# Cluster
# ---------------------------------------------------------------------------

class TestCluster:

    @responses.activate
    def test_cluster_health(self, db):
        responses.add(
            responses.GET,
            f"{BASE}/_cluster/health",
            json={
                "status": "green",
                "number_of_nodes": 3,
                "leader_id": 1,
                "is_leader": True,
            },
            status=200,
        )
        result = db.cluster_health()
        assert result["status"] == "green"
        assert result["number_of_nodes"] == 3

    @responses.activate
    def test_cluster_state(self, db):
        responses.add(
            responses.GET,
            f"{BASE}/_cluster/state",
            json={
                "nodes": [
                    {"id": 1, "address": {"host": "127.0.0.1", "port": 9200}, "status": "Leader"},
                ],
                "leader": 1,
            },
            status=200,
        )
        result = db.cluster_state()
        assert result["leader"] == 1
        assert len(result["nodes"]) == 1

    @responses.activate
    def test_list_nodes(self, db):
        responses.add(
            responses.GET,
            f"{BASE}/_nodes",
            json=[
                {"id": 1, "address": {"host": "127.0.0.1", "port": 9200}, "status": "Leader"},
                {"id": 2, "address": {"host": "127.0.0.1", "port": 9201}, "status": "Follower"},
            ],
            status=200,
        )
        result = db.list_nodes()
        assert len(result) == 2

    @responses.activate
    def test_node_stats(self, db):
        responses.add(
            responses.GET,
            f"{BASE}/_stats",
            json={"node_id": 1, "collections": 3, "is_leader": True, "current_term": 1},
            status=200,
        )
        result = db.node_stats()
        assert result["node_id"] == 1

    @responses.activate
    def test_refresh(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/_refresh",
            json={"acknowledged": True, "collection": "products"},
            status=200,
        )
        result = db.refresh("products")
        assert result["acknowledged"] is True


# ---------------------------------------------------------------------------
# Failover & error handling
# ---------------------------------------------------------------------------

class TestFailover:

    @responses.activate
    def test_failover_to_second_host(self, multi_db):
        # First host fails, second succeeds
        responses.add(
            responses.GET,
            "http://host1:9200/_cluster/health",
            body=requests_connection_error(),
        )
        responses.add(
            responses.GET,
            "http://host2:9200/_cluster/health",
            json={"status": "green", "number_of_nodes": 2},
            status=200,
        )
        result = multi_db.cluster_health()
        assert result["status"] == "green"

    @responses.activate
    def test_all_hosts_fail(self, multi_db):
        responses.add(
            responses.GET,
            "http://host1:9200/_cluster/health",
            body=requests_connection_error(),
        )
        responses.add(
            responses.GET,
            "http://host2:9200/_cluster/health",
            body=requests_connection_error(),
        )
        with pytest.raises(ConnectionError):
            multi_db.cluster_health()

    @responses.activate
    def test_authentication_error(self, db):
        responses.add(
            responses.GET,
            f"{BASE}/_cluster/health",
            json={"error": {"type": "unauthorized", "reason": "invalid key"}, "status": 401},
            status=401,
        )
        with pytest.raises(AuthenticationError):
            db.cluster_health()

    @responses.activate
    def test_invalid_input_error(self, db):
        responses.add(
            responses.POST,
            f"{BASE}/collections/products/_search",
            json={"error": {"type": "invalid_input", "reason": "bad query"}, "status": 400},
            status=400,
        )
        with pytest.raises(InvalidInputError):
            db.search("products", query={"bad": "query"})

    @responses.activate
    def test_service_unavailable_error(self, db):
        responses.add(
            responses.GET,
            f"{BASE}/_cluster/health",
            json={"error": {"type": "service_unavailable", "reason": "no leader"}, "status": 503},
            status=503,
        )
        with pytest.raises(ServiceUnavailableError):
            db.cluster_health()


class TestClientConfiguration:

    def test_default_host(self):
        db = MSearchDB()
        assert "http://localhost:9200" in db._hosts
        db.close()

    def test_custom_hosts(self):
        db = MSearchDB(hosts=["http://a:1", "http://b:2"])
        assert db._hosts == ["http://a:1", "http://b:2"]
        db.close()

    def test_trailing_slash_stripped(self):
        db = MSearchDB(hosts=["http://localhost:9200/"])
        assert db._hosts == ["http://localhost:9200"]
        db.close()

    @responses.activate
    def test_api_key_header(self):
        responses.add(
            responses.GET,
            f"{BASE}/_cluster/health",
            json={"status": "green"},
            status=200,
        )
        db = MSearchDB(hosts=[BASE], api_key="my-secret-key", max_retries=1)
        db.cluster_health()
        assert responses.calls[0].request.headers["X-API-Key"] == "my-secret-key"
        db.close()

    def test_context_manager(self):
        with MSearchDB(hosts=[BASE]) as db:
            assert db is not None
        # Session should be closed (no assertion needed, just no error)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def requests_connection_error():
    """Return an exception instance for responses mock."""
    import requests.exceptions
    return requests.exceptions.ConnectionError("Connection refused")
