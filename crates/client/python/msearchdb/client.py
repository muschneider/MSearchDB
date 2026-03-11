"""Synchronous HTTP client for MSearchDB.

Example::

    from msearchdb import MSearchDB

    db = MSearchDB(hosts=["http://localhost:9200"])
    db.create_collection("products")
    db.index("products", {"id": "1", "fields": {"name": "Laptop"}})
    results = db.search("products", q="laptop")
"""

from __future__ import annotations

import itertools
import logging
import threading
import time
from typing import Any

import requests
from requests.adapters import HTTPAdapter
from urllib3.util.retry import Retry

from .bulk import build_ndjson, documents_to_ndjson
from .exceptions import (
    ConnectionError,
    MSearchDBError,
    TimeoutError,
    raise_for_status,
)
from .models import (
    BulkResult,
    CollectionInfo,
    IndexResult,
    SearchResult,
)

logger = logging.getLogger("msearchdb")


class MSearchDB:
    """Synchronous MSearchDB client with round-robin host selection and
    automatic failover.

    Args:
        hosts: List of base URLs (e.g. ``["http://localhost:9200"]``).
        api_key: Optional API key for the ``X-API-Key`` header.
        timeout: Request timeout in seconds.
        max_retries: Maximum number of retry attempts per request.
        retry_on_timeout: Whether to retry on timeout errors.
        sniff_on_start: Attempt to discover cluster nodes on startup.
    """

    def __init__(
        self,
        hosts: list[str] | None = None,
        api_key: str | None = None,
        timeout: int = 30,
        max_retries: int = 3,
        retry_on_timeout: bool = True,
        sniff_on_start: bool = False,
    ) -> None:
        self._hosts = [h.rstrip("/") for h in (hosts or ["http://localhost:9200"])]
        self._api_key = api_key
        self._timeout = timeout
        self._max_retries = max_retries
        self._retry_on_timeout = retry_on_timeout

        # Thread-safe round-robin iterator
        self._host_cycle = itertools.cycle(self._hosts)
        self._host_lock = threading.Lock()

        # Shared session with connection pooling
        self._session = requests.Session()
        retry_strategy = Retry(
            total=0,  # we handle retries ourselves
            backoff_factor=0,
        )
        adapter = HTTPAdapter(
            max_retries=retry_strategy,
            pool_connections=len(self._hosts),
            pool_maxsize=10,
        )
        self._session.mount("http://", adapter)
        self._session.mount("https://", adapter)

        if self._api_key:
            self._session.headers["X-API-Key"] = self._api_key

        if sniff_on_start:
            self._sniff()

    # -- Context manager -----------------------------------------------------

    def __enter__(self) -> MSearchDB:
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def close(self) -> None:
        """Close the underlying HTTP session."""
        self._session.close()

    # -- Internal helpers ----------------------------------------------------

    def _next_host(self) -> str:
        with self._host_lock:
            return next(self._host_cycle)

    def _request(
        self,
        method: str,
        path: str,
        *,
        json_body: Any | None = None,
        data: str | bytes | None = None,
        params: dict[str, str] | None = None,
        headers: dict[str, str] | None = None,
    ) -> dict[str, Any] | list[Any]:
        """Perform an HTTP request with round-robin failover.

        Tries each host up to *max_retries* total attempts across all hosts.
        """
        last_exc: Exception | None = None
        attempts = 0
        tried_hosts: set[str] = set()

        while attempts < self._max_retries:
            host = self._next_host()
            tried_hosts.add(host)
            url = f"{host}{path}"
            attempts += 1

            try:
                resp = self._session.request(
                    method,
                    url,
                    json=json_body,
                    data=data,
                    params=params,
                    headers=headers,
                    timeout=self._timeout,
                )

                # Parse JSON body (may be empty for some responses)
                try:
                    body = resp.json()
                except ValueError:
                    body = resp.text

                # Raise typed exception for error status codes
                if resp.status_code >= 400:
                    raise_for_status(resp.status_code, body)

                return body

            except requests.exceptions.ConnectionError as exc:
                logger.warning("Connection failed to %s: %s", host, exc)
                last_exc = exc
                # Brief pause before trying next host
                if attempts < self._max_retries:
                    time.sleep(0.1 * attempts)
                continue

            except requests.exceptions.Timeout as exc:
                logger.warning("Request timed out to %s: %s", host, exc)
                last_exc = exc
                if self._retry_on_timeout and attempts < self._max_retries:
                    time.sleep(0.1 * attempts)
                    continue
                raise TimeoutError(f"Request timed out after {self._timeout}s") from exc

            except MSearchDBError:
                # Application-level errors (404, 409, etc.) are NOT retried
                raise

            except Exception as exc:
                logger.warning("Unexpected error from %s: %s", host, exc)
                last_exc = exc
                if attempts < self._max_retries:
                    time.sleep(0.1 * attempts)
                continue

        hosts_str = ", ".join(sorted(tried_hosts))
        raise ConnectionError(
            f"Failed to connect after {attempts} attempts. "
            f"Tried hosts: {hosts_str}"
        ) from last_exc

    def _sniff(self) -> None:
        """Discover cluster nodes via the ``/_nodes`` endpoint and update the
        host list."""
        try:
            nodes = self._request("GET", "/_nodes")
            if isinstance(nodes, list) and nodes:
                new_hosts: list[str] = []
                for node in nodes:
                    addr = node.get("address", {})
                    host_str = addr.get("host", "127.0.0.1")
                    port = addr.get("port", 9200)
                    new_hosts.append(f"http://{host_str}:{port}")
                if new_hosts:
                    self._hosts = new_hosts
                    self._host_cycle = itertools.cycle(self._hosts)
                    logger.info("Sniffed %d node(s): %s", len(new_hosts), new_hosts)
        except Exception as exc:
            logger.debug("Sniffing failed: %s", exc)

    # -----------------------------------------------------------------------
    # Collection management
    # -----------------------------------------------------------------------

    def create_collection(
        self, name: str, settings: dict[str, Any] | None = None
    ) -> CollectionInfo:
        """Create a new collection.

        Args:
            name: Collection name.
            settings: Optional schema / settings dict.

        Returns:
            :class:`~msearchdb.models.CollectionInfo` for the new collection.
        """
        body: dict[str, Any] = {}
        if settings:
            body["schema"] = settings
        resp = self._request("PUT", f"/collections/{name}", json_body=body)
        return CollectionInfo.from_dict(resp)

    def delete_collection(self, name: str) -> dict[str, Any]:
        """Delete a collection.

        Args:
            name: Collection name.

        Returns:
            ``{"acknowledged": True}`` on success.
        """
        return self._request("DELETE", f"/collections/{name}")

    def list_collections(self) -> list[CollectionInfo]:
        """List all collections.

        Returns:
            List of :class:`~msearchdb.models.CollectionInfo`.
        """
        resp = self._request("GET", "/collections")
        if isinstance(resp, list):
            return [CollectionInfo.from_dict(c) for c in resp]
        return []

    def get_collection_info(self, name: str) -> CollectionInfo:
        """Get metadata about a collection.

        Args:
            name: Collection name.

        Returns:
            :class:`~msearchdb.models.CollectionInfo`.
        """
        resp = self._request("GET", f"/collections/{name}")
        return CollectionInfo.from_dict(resp)

    # -----------------------------------------------------------------------
    # Document CRUD
    # -----------------------------------------------------------------------

    def index(
        self,
        collection: str,
        document: dict[str, Any],
        doc_id: str | None = None,
    ) -> IndexResult:
        """Index (create) a document.

        Args:
            collection: Target collection name.
            document: Document fields dict.
            doc_id: Optional document ID (auto-generated if omitted).

        Returns:
            :class:`~msearchdb.models.IndexResult`.
        """
        body: dict[str, Any] = {"fields": document}
        if doc_id is not None:
            body["id"] = doc_id
        resp = self._request("POST", f"/collections/{collection}/docs", json_body=body)
        return IndexResult.from_dict(resp)

    def get(self, collection: str, doc_id: str) -> dict[str, Any]:
        """Retrieve a document by ID.

        Args:
            collection: Collection name.
            doc_id: Document ID.

        Returns:
            Dict with ``_id``, ``_source``, and ``found`` keys.
        """
        return self._request("GET", f"/collections/{collection}/docs/{doc_id}")

    def update(
        self,
        collection: str,
        doc_id: str,
        document: dict[str, Any],
    ) -> IndexResult:
        """Update (upsert) a document.

        Args:
            collection: Collection name.
            doc_id: Document ID.
            document: Updated fields dict.

        Returns:
            :class:`~msearchdb.models.IndexResult`.
        """
        body: dict[str, Any] = {"fields": document}
        resp = self._request(
            "PUT", f"/collections/{collection}/docs/{doc_id}", json_body=body
        )
        return IndexResult.from_dict(resp)

    def delete(self, collection: str, doc_id: str) -> IndexResult:
        """Delete a document.

        Args:
            collection: Collection name.
            doc_id: Document ID.

        Returns:
            :class:`~msearchdb.models.IndexResult`.
        """
        resp = self._request("DELETE", f"/collections/{collection}/docs/{doc_id}")
        return IndexResult.from_dict(resp)

    # -----------------------------------------------------------------------
    # Search
    # -----------------------------------------------------------------------

    def search(
        self,
        collection: str,
        query: dict[str, Any] | None = None,
        *,
        q: str | None = None,
        size: int = 10,
        from_: int | None = None,
        highlight: dict[str, Any] | None = None,
        sort: list[dict[str, str]] | None = None,
    ) -> SearchResult:
        """Search a collection.

        Either ``query`` (DSL dict) or ``q`` (simple text) should be provided.

        Args:
            collection: Collection name.
            query: Query DSL dict (for POST search).
            q: Simple query string (for GET search).
            size: Maximum hits to return.
            from_: Pagination offset.
            highlight: Highlight configuration dict.
            sort: List of ``{"field": ..., "order": ...}`` dicts.

        Returns:
            :class:`~msearchdb.models.SearchResult`.
        """
        if q is not None and query is None:
            # Simple GET search
            params: dict[str, str] = {"q": q, "size": str(size)}
            resp = self._request(
                "GET", f"/collections/{collection}/_search", params=params
            )
        else:
            # Full DSL POST search
            body: dict[str, Any] = {"query": query or {"match_all": {}}, "size": size}
            if from_ is not None:
                body["from"] = from_
            if highlight is not None:
                body["highlight"] = highlight
            if sort is not None:
                body["sort"] = sort
            resp = self._request(
                "POST", f"/collections/{collection}/_search", json_body=body
            )
        return SearchResult.from_dict(resp)

    # -----------------------------------------------------------------------
    # Bulk operations
    # -----------------------------------------------------------------------

    def bulk(self, collection: str, operations: list[dict[str, Any]]) -> BulkResult:
        """Execute bulk operations on a collection.

        Args:
            collection: Target collection name.
            operations: List of operation dicts, each with ``"action"``,
                optional ``"_id"``, and optional ``"doc"`` keys.

        Returns:
            :class:`~msearchdb.models.BulkResult`.
        """
        ndjson = build_ndjson(operations)
        resp = self._request(
            "POST",
            f"/collections/{collection}/docs/_bulk",
            data=ndjson,
            headers={"Content-Type": "application/x-ndjson"},
        )
        return BulkResult.from_dict(resp)

    def bulk_index(
        self, collection: str, documents: list[dict[str, Any]]
    ) -> BulkResult:
        """Convenience method to bulk-index a list of documents.

        Each document may contain an ``"id"`` key that is used as the document
        ID.  All other keys are indexed as fields.

        Args:
            collection: Target collection name.
            documents: List of document dicts.

        Returns:
            :class:`~msearchdb.models.BulkResult`.
        """
        ndjson = documents_to_ndjson(documents)
        resp = self._request(
            "POST",
            f"/collections/{collection}/docs/_bulk",
            data=ndjson,
            headers={"Content-Type": "application/x-ndjson"},
        )
        return BulkResult.from_dict(resp)

    # -----------------------------------------------------------------------
    # Cluster
    # -----------------------------------------------------------------------

    def cluster_health(self) -> dict[str, Any]:
        """Get cluster health status.

        Returns:
            Dict with ``status``, ``number_of_nodes``, ``leader_id``,
            ``is_leader``.
        """
        return self._request("GET", "/_cluster/health")

    def cluster_state(self) -> dict[str, Any]:
        """Get full cluster state including node list.

        Returns:
            Dict with ``nodes`` list and ``leader`` ID.
        """
        return self._request("GET", "/_cluster/state")

    def list_nodes(self) -> list[dict[str, Any]]:
        """List all nodes in the cluster.

        Returns:
            List of node info dicts.
        """
        resp = self._request("GET", "/_nodes")
        return resp if isinstance(resp, list) else []

    def node_stats(self) -> dict[str, Any]:
        """Get statistics for the connected node.

        Returns:
            Dict with ``node_id``, ``collections``, ``is_leader``,
            ``current_term``.
        """
        return self._request("GET", "/_stats")

    def refresh(self, collection: str) -> dict[str, Any]:
        """Force a search index refresh for a collection.

        Args:
            collection: Collection name.

        Returns:
            ``{"acknowledged": True, "collection": "..."}``
        """
        return self._request("POST", f"/collections/{collection}/_refresh")
