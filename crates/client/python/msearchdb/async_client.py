"""Asynchronous HTTP client for MSearchDB using aiohttp.

Example::

    import asyncio
    from msearchdb import AsyncMSearchDB

    async def main():
        async with AsyncMSearchDB(hosts=["http://localhost:9200"]) as db:
            await db.create_collection("products")
            await db.index("products", {"name": "Laptop"}, doc_id="1")
            results = await db.search("products", q="laptop")
            for hit in results.hits:
                print(hit.source)

    asyncio.run(main())
"""

from __future__ import annotations

import asyncio
import itertools
import logging
from typing import Any

try:
    import aiohttp
except ImportError as exc:
    raise ImportError(
        "aiohttp is required for the async client. "
        "Install it with: pip install msearchdb[async]"
    ) from exc

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

logger = logging.getLogger("msearchdb.async")


class AsyncMSearchDB:
    """Asynchronous MSearchDB client with round-robin failover.

    Must be used as an async context manager::

        async with AsyncMSearchDB() as db:
            await db.search(...)

    Args:
        hosts: Base URLs.
        api_key: Optional API key.
        timeout: Request timeout in seconds.
        max_retries: Max retry attempts.
        retry_on_timeout: Retry on timeout errors.
        sniff_on_start: Discover cluster nodes on connect.
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
        self._timeout = aiohttp.ClientTimeout(total=timeout)
        self._max_retries = max_retries
        self._retry_on_timeout = retry_on_timeout
        self._sniff_on_start = sniff_on_start
        self._host_cycle = itertools.cycle(self._hosts)
        self._session: aiohttp.ClientSession | None = None

    # -- Async context manager -----------------------------------------------

    async def __aenter__(self) -> AsyncMSearchDB:
        headers: dict[str, str] = {}
        if self._api_key:
            headers["X-API-Key"] = self._api_key
        self._session = aiohttp.ClientSession(
            timeout=self._timeout,
            headers=headers,
        )
        if self._sniff_on_start:
            await self._sniff()
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.close()

    async def close(self) -> None:
        """Close the underlying aiohttp session."""
        if self._session and not self._session.closed:
            await self._session.close()

    # -- Internal helpers ----------------------------------------------------

    def _next_host(self) -> str:
        return next(self._host_cycle)

    async def _request(
        self,
        method: str,
        path: str,
        *,
        json_body: Any | None = None,
        data: str | bytes | None = None,
        params: dict[str, str] | None = None,
        headers: dict[str, str] | None = None,
    ) -> dict[str, Any] | list[Any]:
        if self._session is None:
            raise RuntimeError(
                "Client session not initialised. "
                "Use 'async with AsyncMSearchDB() as db:' syntax."
            )

        last_exc: Exception | None = None
        attempts = 0
        tried_hosts: set[str] = set()

        while attempts < self._max_retries:
            host = self._next_host()
            tried_hosts.add(host)
            url = f"{host}{path}"
            attempts += 1

            try:
                kwargs: dict[str, Any] = {}
                if json_body is not None:
                    kwargs["json"] = json_body
                if data is not None:
                    kwargs["data"] = data
                if params is not None:
                    kwargs["params"] = params
                if headers is not None:
                    kwargs["headers"] = headers

                async with self._session.request(method, url, **kwargs) as resp:
                    try:
                        body = await resp.json(content_type=None)
                    except Exception:
                        body = await resp.text()

                    if resp.status >= 400:
                        raise_for_status(resp.status, body)

                    return body

            except aiohttp.ClientConnectionError as exc:
                logger.warning("Connection failed to %s: %s", host, exc)
                last_exc = exc
                if attempts < self._max_retries:
                    await asyncio.sleep(0.1 * attempts)
                continue

            except asyncio.TimeoutError as exc:
                logger.warning("Request timed out to %s", host)
                last_exc = exc
                if self._retry_on_timeout and attempts < self._max_retries:
                    await asyncio.sleep(0.1 * attempts)
                    continue
                raise TimeoutError("Request timed out") from exc

            except MSearchDBError:
                raise

            except Exception as exc:
                logger.warning("Unexpected error from %s: %s", host, exc)
                last_exc = exc
                if attempts < self._max_retries:
                    await asyncio.sleep(0.1 * attempts)
                continue

        hosts_str = ", ".join(sorted(tried_hosts))
        raise ConnectionError(
            f"Failed to connect after {attempts} attempts. "
            f"Tried hosts: {hosts_str}"
        ) from last_exc

    async def _sniff(self) -> None:
        try:
            nodes = await self._request("GET", "/_nodes")
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

    async def create_collection(
        self, name: str, settings: dict[str, Any] | None = None
    ) -> CollectionInfo:
        body: dict[str, Any] = {}
        if settings:
            body["schema"] = settings
        resp = await self._request("PUT", f"/collections/{name}", json_body=body)
        return CollectionInfo.from_dict(resp)

    async def delete_collection(self, name: str) -> dict[str, Any]:
        return await self._request("DELETE", f"/collections/{name}")

    async def list_collections(self) -> list[CollectionInfo]:
        resp = await self._request("GET", "/collections")
        if isinstance(resp, list):
            return [CollectionInfo.from_dict(c) for c in resp]
        return []

    async def get_collection_info(self, name: str) -> CollectionInfo:
        resp = await self._request("GET", f"/collections/{name}")
        return CollectionInfo.from_dict(resp)

    # -----------------------------------------------------------------------
    # Document CRUD
    # -----------------------------------------------------------------------

    async def index(
        self,
        collection: str,
        document: dict[str, Any],
        doc_id: str | None = None,
    ) -> IndexResult:
        body: dict[str, Any] = {"fields": document}
        if doc_id is not None:
            body["id"] = doc_id
        resp = await self._request(
            "POST", f"/collections/{collection}/docs", json_body=body
        )
        return IndexResult.from_dict(resp)

    async def get(self, collection: str, doc_id: str) -> dict[str, Any]:
        return await self._request(
            "GET", f"/collections/{collection}/docs/{doc_id}"
        )

    async def update(
        self,
        collection: str,
        doc_id: str,
        document: dict[str, Any],
    ) -> IndexResult:
        body: dict[str, Any] = {"fields": document}
        resp = await self._request(
            "PUT", f"/collections/{collection}/docs/{doc_id}", json_body=body
        )
        return IndexResult.from_dict(resp)

    async def delete(self, collection: str, doc_id: str) -> IndexResult:
        resp = await self._request(
            "DELETE", f"/collections/{collection}/docs/{doc_id}"
        )
        return IndexResult.from_dict(resp)

    # -----------------------------------------------------------------------
    # Search
    # -----------------------------------------------------------------------

    async def search(
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
        if q is not None and query is None:
            params: dict[str, str] = {"q": q, "size": str(size)}
            resp = await self._request(
                "GET", f"/collections/{collection}/_search", params=params
            )
        else:
            body: dict[str, Any] = {
                "query": query or {"match_all": {}},
                "size": size,
            }
            if from_ is not None:
                body["from"] = from_
            if highlight is not None:
                body["highlight"] = highlight
            if sort is not None:
                body["sort"] = sort
            resp = await self._request(
                "POST", f"/collections/{collection}/_search", json_body=body
            )
        return SearchResult.from_dict(resp)

    # -----------------------------------------------------------------------
    # Bulk operations
    # -----------------------------------------------------------------------

    async def bulk(
        self, collection: str, operations: list[dict[str, Any]]
    ) -> BulkResult:
        ndjson = build_ndjson(operations)
        resp = await self._request(
            "POST",
            f"/collections/{collection}/docs/_bulk",
            data=ndjson,
            headers={"Content-Type": "application/x-ndjson"},
        )
        return BulkResult.from_dict(resp)

    async def bulk_index(
        self, collection: str, documents: list[dict[str, Any]]
    ) -> BulkResult:
        ndjson = documents_to_ndjson(documents)
        resp = await self._request(
            "POST",
            f"/collections/{collection}/docs/_bulk",
            data=ndjson,
            headers={"Content-Type": "application/x-ndjson"},
        )
        return BulkResult.from_dict(resp)

    # -----------------------------------------------------------------------
    # Cluster
    # -----------------------------------------------------------------------

    async def cluster_health(self) -> dict[str, Any]:
        return await self._request("GET", "/_cluster/health")

    async def cluster_state(self) -> dict[str, Any]:
        return await self._request("GET", "/_cluster/state")

    async def list_nodes(self) -> list[dict[str, Any]]:
        resp = await self._request("GET", "/_nodes")
        return resp if isinstance(resp, list) else []

    async def node_stats(self) -> dict[str, Any]:
        return await self._request("GET", "/_stats")

    async def refresh(self, collection: str) -> dict[str, Any]:
        return await self._request(
            "POST", f"/collections/{collection}/_refresh"
        )
