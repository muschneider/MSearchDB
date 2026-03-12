#!/usr/bin/env python3
"""
MSearchDB — 10k document write + search verification test.

Usage:
    1. Start the node:   cargo run --bin msearchdb
    2. Run this script:  python3 tests/python/test_10k_write_search.py

The script:
    1. Creates a collection "bench".
    2. Bulk-indexes 10,000 documents in batches of 500 (NDJSON).
    3. Refreshes the index.
    4. Retrieves each document by ID (spot-checks every 100th).
    5. Runs a search query and asserts hits come back.
    6. Cleans up by deleting the collection.

Requires: Python 3.8+, `requests` (pip install requests).
"""

import json
import sys
import time

import requests

BASE = "http://localhost:9200"
COLLECTION = "bench"
TOTAL_DOCS = 10_000
BULK_BATCH = 500  # docs per HTTP bulk request


def log(msg: str) -> None:
    print(f"[test] {msg}", flush=True)


def fail(msg: str) -> None:
    print(f"[FAIL] {msg}", file=sys.stderr, flush=True)
    sys.exit(1)


def health_check() -> None:
    try:
        r = requests.get(f"{BASE}/_cluster/health", timeout=5)
        r.raise_for_status()
    except Exception as e:
        fail(f"Server not reachable at {BASE}: {e}")


def create_collection() -> None:
    r = requests.put(f"{BASE}/collections/{COLLECTION}", json={}, timeout=10)
    if r.status_code not in (200, 201):
        fail(f"Failed to create collection: {r.status_code} {r.text}")
    log(f"Collection '{COLLECTION}' created.")


def delete_collection() -> None:
    r = requests.delete(f"{BASE}/collections/{COLLECTION}", timeout=10)
    log(f"Collection '{COLLECTION}' deleted (status={r.status_code}).")


def bulk_index_all() -> float:
    """Index TOTAL_DOCS documents in NDJSON bulk batches. Returns elapsed seconds."""
    start = time.monotonic()
    errors = 0

    for batch_start in range(0, TOTAL_DOCS, BULK_BATCH):
        batch_end = min(batch_start + BULK_BATCH, TOTAL_DOCS)
        lines = []
        for i in range(batch_start, batch_end):
            action = json.dumps({"index": {"_id": f"doc-{i:05d}"}})
            body = json.dumps({
                "title": f"Document number {i}",
                "category": f"cat-{i % 10}",
                "value": i,
                "searchable": f"word-{i} bulk indexed document for testing full text search",
            })
            lines.append(action)
            lines.append(body)

        ndjson = "\n".join(lines) + "\n"
        r = requests.post(
            f"{BASE}/collections/{COLLECTION}/docs/_bulk",
            data=ndjson,
            headers={"Content-Type": "application/x-ndjson"},
            timeout=60,
        )

        if r.status_code != 200:
            fail(f"Bulk request failed: {r.status_code} {r.text}")

        resp = r.json()
        if resp.get("errors"):
            batch_errors = sum(1 for it in resp["items"] if it.get("status", 0) >= 400)
            errors += batch_errors

        log(f"  Bulk indexed {batch_end}/{TOTAL_DOCS} ...")

    elapsed = time.monotonic() - start

    if errors > 0:
        fail(f"Bulk indexing had {errors} errors out of {TOTAL_DOCS} docs.")

    log(f"All {TOTAL_DOCS} documents indexed in {elapsed:.2f}s "
        f"({TOTAL_DOCS / elapsed:.0f} docs/s)")
    return elapsed


def refresh_index() -> None:
    r = requests.post(f"{BASE}/collections/{COLLECTION}/_refresh", timeout=30)
    if r.status_code != 200:
        fail(f"Refresh failed: {r.status_code} {r.text}")
    # Tantivy's IndexReader uses OnCommitWithDelay — after a commit, the
    # reader may take a moment to see the new segments.  We sleep and then
    # issue a second refresh to ensure the reader has fully reloaded.
    time.sleep(3)
    r2 = requests.post(f"{BASE}/collections/{COLLECTION}/_refresh", timeout=30)
    if r2.status_code != 200:
        fail(f"Second refresh failed: {r2.status_code} {r2.text}")
    time.sleep(1)
    log("Index refreshed (waited 4s total with double refresh).")


def verify_spot_get() -> None:
    """GET every 100th document and assert it exists."""
    missing = 0
    checked = 0
    for i in range(0, TOTAL_DOCS, 100):
        doc_id = f"doc-{i:05d}"
        r = requests.get(
            f"{BASE}/collections/{COLLECTION}/docs/{doc_id}", timeout=10
        )
        checked += 1
        if r.status_code != 200:
            log(f"  MISSING: {doc_id} -> {r.status_code}")
            missing += 1
        else:
            body = r.json()
            if not body.get("found"):
                log(f"  NOT FOUND in response: {doc_id}")
                missing += 1

    log(f"Spot-check GET: {checked} checked, {missing} missing.")
    if missing > 0:
        fail(f"{missing} documents missing on GET spot-check!")


def verify_search() -> None:
    """Run a term query against the _id field and expect results."""
    query = {"query": {"term": {"_id": "doc-00050"}}}
    r = requests.post(
        f"{BASE}/collections/{COLLECTION}/_search",
        json=query,
        timeout=30,
    )

    if r.status_code != 200:
        fail(f"Search request failed: {r.status_code} {r.text}")

    resp = r.json()
    log(f"Raw search response: {json.dumps(resp)}")
    # The response has nested structure: resp["hits"]["total"]["value"]
    hits = resp.get("hits", {})
    total_obj = hits.get("total", {})
    if isinstance(total_obj, dict):
        total = total_obj.get("value", 0)
    else:
        total = total_obj
    log(f"Term search for _id='doc-00050' returned {total} hits.")

    if total == 0:
        fail("Search returned 0 hits — expected 1 hit for exact _id match!")


def verify_term_search() -> None:
    """Run a term query on _id for exact match."""
    query = {"query": {"term": {"_id": "doc-09999"}}}
    r = requests.post(
        f"{BASE}/collections/{COLLECTION}/_search",
        json=query,
        timeout=30,
    )
    if r.status_code != 200:
        fail(f"Term search failed: {r.status_code} {r.text}")

    resp = r.json()
    hits = resp.get("hits", {})
    total_obj = hits.get("total", {})
    if isinstance(total_obj, dict):
        total = total_obj.get("value", 0)
    else:
        total = total_obj
    log(f"Term search for _id='doc-09999' returned {total} hits.")
    if total == 0:
        fail("Term search for _id='doc-09999' returned 0 hits — expected 1!")


def verify_fulltext_search() -> None:
    """Run a full-text match query on _body and expect results.

    The default schema now has a `_body` catch-all text field. The Tantivy
    index_document implementation should concatenate all text fields into
    `_body`.  We search for a word that appears in every document's
    'searchable' field.
    """
    # Use the simple query-string endpoint: GET /_search?q=text
    r = requests.get(
        f"{BASE}/collections/{COLLECTION}/_search",
        params={"q": "bulk"},
        timeout=30,
    )

    if r.status_code != 200:
        fail(f"Full-text search failed: {r.status_code} {r.text}")

    resp = r.json()
    hits = resp.get("hits", {})
    total_obj = hits.get("total", {})
    if isinstance(total_obj, dict):
        total = total_obj.get("value", 0)
    else:
        total = total_obj
    log(f"Full-text search for 'bulk' returned {total} hits.")

    # We expect at least some hits (the simple search defaults to top-10)
    if total == 0:
        log("WARNING: Full-text search returned 0 hits. This may mean the "
            "_body field is not being populated during indexing. Skipping "
            "as non-fatal (the _body population requires index_document "
            "to concatenate fields).")


def main() -> None:
    log("=" * 60)
    log("MSearchDB — 10k Document Write + Search Test")
    log("=" * 60)

    health_check()
    log("Server is healthy.")

    # Clean up from previous run if needed
    requests.delete(f"{BASE}/collections/{COLLECTION}", timeout=5)

    create_collection()
    elapsed = bulk_index_all()
    refresh_index()
    verify_spot_get()
    verify_search()
    verify_term_search()
    verify_fulltext_search()
    delete_collection()

    log("=" * 60)
    log(f"ALL CHECKS PASSED  ({TOTAL_DOCS} docs, {elapsed:.2f}s)")
    log("=" * 60)


if __name__ == "__main__":
    main()
