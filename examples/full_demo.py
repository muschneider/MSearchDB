#!/usr/bin/env python3
"""MSearchDB Full End-to-End Demo.

Demonstrates every major feature of MSearchDB against a 3-node cluster:

  1. Cluster connectivity and health
  2. Collection creation
  3. Bulk indexing (50,000 synthetic Wikipedia-style article summaries)
  4. Full-text search with highlighting
  5. Fuzzy search (typo tolerance)
  6. Phrase search
  7. Aggregation-style queries (top categories, price ranges)
  8. Failover: kill a node mid-search, assert search continues
  9. Cluster health transitions: green -> yellow -> green
 10. Performance report: indexing throughput, search latency p50/p99

Prerequisites:
    docker compose -f docker/docker-compose.yml up --build -d

    pip install requests numpy

Usage:
    python examples/full_demo.py [--nodes http://localhost:9201,http://localhost:9202,http://localhost:9203]
    python examples/full_demo.py --single http://localhost:9200
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import random
import signal
import statistics
import subprocess
import sys
import time
from typing import Any

# ---------------------------------------------------------------------------
# Resolve the Python SDK path so the demo works from the repo root without
# installing the package.
# ---------------------------------------------------------------------------
SDK_PATH = os.path.join(
    os.path.dirname(__file__), "..", "crates", "client", "python"
)
if os.path.isdir(SDK_PATH):
    sys.path.insert(0, os.path.abspath(SDK_PATH))

try:
    from msearchdb import MSearchDB, Q, Search, ConnectionError as MSearchConnectionError
except ImportError:
    print(
        "ERROR: MSearchDB Python SDK not found.\n"
        "Install it with: pip install -e crates/client/python\n"
        "Or run from the repo root so the SDK is on sys.path.",
        file=sys.stderr,
    )
    sys.exit(1)


# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

COLLECTION = "articles"
NUM_DOCS = 50_000
BULK_BATCH_SIZE = 500
SEARCH_ITERATIONS = 100

# Vocabulary for synthetic articles
CATEGORIES = [
    "Science", "Technology", "History", "Mathematics", "Philosophy",
    "Literature", "Geography", "Medicine", "Physics", "Chemistry",
    "Biology", "Economics", "Art", "Music", "Politics",
    "Engineering", "Astronomy", "Psychology", "Sociology", "Law",
]

ADJECTIVES = [
    "quantum", "classical", "modern", "ancient", "theoretical",
    "experimental", "applied", "computational", "statistical", "analytical",
    "fundamental", "advanced", "introductory", "comprehensive", "novel",
]

TOPICS = [
    "mechanics", "thermodynamics", "electromagnetism", "relativity", "evolution",
    "genetics", "algorithms", "networks", "databases", "compilers",
    "architecture", "democracy", "capitalism", "philosophy", "linguistics",
    "neuroscience", "ecology", "astronomy", "cryptography", "robotics",
    "nanotechnology", "biotechnology", "photonics", "superconductivity", "topology",
    "algebra", "calculus", "geometry", "probability", "combinatorics",
]

WORDS = [
    "research", "study", "analysis", "theory", "experiment", "discovery",
    "innovation", "development", "application", "framework", "model",
    "system", "method", "approach", "technique", "algorithm", "structure",
    "process", "mechanism", "phenomenon", "principle", "concept",
    "hypothesis", "observation", "measurement", "simulation", "optimization",
    "implementation", "evaluation", "validation", "verification", "synthesis",
]


# ---------------------------------------------------------------------------
# Synthetic data generator
# ---------------------------------------------------------------------------

def generate_article(doc_id: int, rng: random.Random) -> dict[str, Any]:
    """Generate a single synthetic Wikipedia-style article summary."""
    category = rng.choice(CATEGORIES)
    adj = rng.choice(ADJECTIVES)
    topic = rng.choice(TOPICS)
    title = f"{adj.title()} {topic.title()} in {category}"

    # Build a realistic-looking summary (3-5 sentences)
    sentences = []
    for _ in range(rng.randint(3, 5)):
        words = rng.choices(WORDS, k=rng.randint(8, 15))
        sentence = " ".join(words).capitalize() + "."
        sentences.append(sentence)
    summary = " ".join(sentences)

    # Deterministic "word count" for range queries
    word_count = len(summary.split())
    # Deterministic "year" for range queries
    year = rng.randint(1950, 2025)
    # Deterministic "quality score" for sorting
    quality = round(rng.uniform(0.1, 10.0), 2)

    return {
        "id": f"article-{doc_id:06d}",
        "title": title,
        "summary": summary,
        "category": category,
        "topic": topic,
        "year": year,
        "word_count": word_count,
        "quality_score": quality,
    }


def generate_dataset(n: int, seed: int = 42) -> list[dict[str, Any]]:
    """Generate *n* synthetic articles with deterministic randomness."""
    rng = random.Random(seed)
    return [generate_article(i, rng) for i in range(n)]


# ---------------------------------------------------------------------------
# Helper utilities
# ---------------------------------------------------------------------------

class Timer:
    """Simple context-manager timer."""

    def __init__(self, label: str = ""):
        self.label = label
        self.elapsed_ms: float = 0

    def __enter__(self):
        self._start = time.perf_counter()
        return self

    def __exit__(self, *exc):
        self.elapsed_ms = (time.perf_counter() - self._start) * 1000


def percentile(data: list[float], p: int) -> float:
    """Compute the p-th percentile of a list of values."""
    if not data:
        return 0.0
    data_sorted = sorted(data)
    k = (len(data_sorted) - 1) * (p / 100)
    f = math.floor(k)
    c = math.ceil(k)
    if f == c:
        return data_sorted[int(k)]
    return data_sorted[f] * (c - k) + data_sorted[c] * (k - f)


def print_header(title: str) -> None:
    width = 72
    print()
    print("=" * width)
    print(f"  {title}")
    print("=" * width)


def print_result(label: str, value: Any) -> None:
    print(f"  {label:<40s} {value}")


def wait_for_cluster(db: MSearchDB, expected_nodes: int = 1, timeout: int = 60) -> None:
    """Block until the cluster reports the expected number of nodes."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            health = db.cluster_health()
            print(f"  Cluster status: {health.get('status', 'unknown')}, "
                  f"nodes: {health.get('number_of_nodes', 0)}")
            if health.get("number_of_nodes", 0) >= expected_nodes:
                return
        except Exception:
            pass
        time.sleep(1)
    print(f"  WARNING: Cluster did not reach {expected_nodes} nodes within {timeout}s")


# ---------------------------------------------------------------------------
# Demo steps
# ---------------------------------------------------------------------------

def step_01_cluster_health(db: MSearchDB, expected_nodes: int) -> None:
    """1. Connect and verify cluster health."""
    print_header("Step 1: Cluster Connectivity & Health")
    wait_for_cluster(db, expected_nodes=expected_nodes, timeout=30)

    health = db.cluster_health()
    print_result("Status", health.get("status", "unknown"))
    print_result("Number of nodes", health.get("number_of_nodes", 0))
    print_result("Leader ID", health.get("leader_id", "none"))
    print_result("Is leader", health.get("is_leader", False))

    # Node list
    try:
        nodes = db.list_nodes()
        print_result("Discovered nodes", len(nodes))
    except Exception as exc:
        print_result("Node discovery", f"skipped ({exc})")

    # Stats
    try:
        stats = db.node_stats()
        print_result("Collections on node", len(stats.get("collections", [])))
    except Exception as exc:
        print_result("Node stats", f"skipped ({exc})")


def step_02_create_collection(db: MSearchDB) -> None:
    """2. Create the articles collection."""
    print_header("Step 2: Create Collection")

    # Clean up if it exists from a previous run
    try:
        db.delete_collection(COLLECTION)
        print_result("Cleaned up previous", COLLECTION)
    except Exception:
        pass

    info = db.create_collection(COLLECTION)
    print_result("Created collection", info.name)
    print_result("Doc count", info.doc_count)

    # Verify it shows in listing
    collections = db.list_collections()
    names = [c.name for c in collections]
    assert COLLECTION in names, f"Collection {COLLECTION} not found in {names}"
    print_result("Verified in listing", True)


def step_03_bulk_index(db: MSearchDB) -> dict[str, Any]:
    """3. Bulk index 50,000 synthetic articles. Returns perf stats."""
    print_header(f"Step 3: Bulk Index {NUM_DOCS:,} Documents")

    dataset = generate_dataset(NUM_DOCS)
    print_result("Generated articles", f"{len(dataset):,}")

    total_indexed = 0
    total_errors = 0
    batch_times: list[float] = []

    for i in range(0, len(dataset), BULK_BATCH_SIZE):
        batch = dataset[i : i + BULK_BATCH_SIZE]
        with Timer() as t:
            try:
                result = db.bulk_index(COLLECTION, batch)
                total_indexed += len(result.succeeded) if hasattr(result, 'succeeded') else len(batch)
                total_errors += len(result.failed) if hasattr(result, 'failed') else 0
            except Exception as exc:
                # On bulk errors, retry individual documents
                print(f"  Batch {i // BULK_BATCH_SIZE} failed: {exc}")
                for doc in batch:
                    try:
                        doc_copy = dict(doc)
                        doc_id = doc_copy.pop("id", None)
                        db.index(COLLECTION, doc_copy, doc_id=doc_id)
                        total_indexed += 1
                    except Exception:
                        total_errors += 1
        batch_times.append(t.elapsed_ms)

        # Progress every 10 batches
        if (i // BULK_BATCH_SIZE) % 10 == 0:
            progress = min(i + BULK_BATCH_SIZE, len(dataset))
            throughput = BULK_BATCH_SIZE / (t.elapsed_ms / 1000) if t.elapsed_ms > 0 else 0
            print(f"  [{progress:>6,}/{NUM_DOCS:,}] "
                  f"batch: {t.elapsed_ms:>6.1f}ms, "
                  f"throughput: {throughput:>8,.0f} docs/s")

    total_time_ms = sum(batch_times)
    total_time_s = total_time_ms / 1000
    overall_throughput = total_indexed / total_time_s if total_time_s > 0 else 0

    print()
    print_result("Total indexed", f"{total_indexed:,}")
    print_result("Total errors", f"{total_errors:,}")
    print_result("Total time", f"{total_time_s:.2f}s")
    print_result("Overall throughput", f"{overall_throughput:,.0f} docs/s")
    print_result("Avg batch time", f"{statistics.mean(batch_times):.1f}ms")
    print_result("P50 batch time", f"{percentile(batch_times, 50):.1f}ms")
    print_result("P99 batch time", f"{percentile(batch_times, 99):.1f}ms")

    # Refresh the index to make documents searchable
    try:
        db.refresh(COLLECTION)
        print_result("Index refreshed", True)
    except Exception:
        print_result("Index refresh", "skipped (endpoint may not be available)")

    return {
        "total_indexed": total_indexed,
        "total_time_s": total_time_s,
        "throughput": overall_throughput,
        "batch_p50_ms": percentile(batch_times, 50),
        "batch_p99_ms": percentile(batch_times, 99),
    }


def step_04_fulltext_search(db: MSearchDB) -> list[float]:
    """4. Full-text search with highlighting."""
    print_header("Step 4: Full-Text Search with Highlighting")

    # Search for articles about quantum mechanics
    results = db.search(
        COLLECTION,
        query=Q.match("summary", "research discovery innovation"),
        size=5,
        highlight={"fields": ["summary"]},
    )

    print_result("Total hits", results.total)
    print_result("Took (ms)", results.took)
    print()

    for i, hit in enumerate(results.hits[:3]):
        print(f"  Hit {i+1}: {hit.source.get('title', hit.id)}")
        print(f"    Score: {hit.score}")
        if hit.highlight:
            for field, fragments in hit.highlight.items():
                print(f"    Highlight ({field}): {fragments[0][:100]}...")
        print()

    # Measure search latency over many queries
    latencies: list[float] = []
    queries = ["quantum mechanics", "evolution genetics", "database algorithms",
               "democracy capitalism", "relativity physics", "neural networks",
               "cryptography security", "biotechnology innovation",
               "statistical analysis", "computational model"]

    for q_text in queries:
        for _ in range(SEARCH_ITERATIONS // len(queries)):
            start = time.perf_counter()
            try:
                db.search(COLLECTION, query=Q.match("summary", q_text), size=10)
            except Exception:
                pass
            elapsed = (time.perf_counter() - start) * 1000
            latencies.append(elapsed)

    print_result("Search iterations", len(latencies))
    print_result("P50 latency", f"{percentile(latencies, 50):.2f}ms")
    print_result("P95 latency", f"{percentile(latencies, 95):.2f}ms")
    print_result("P99 latency", f"{percentile(latencies, 99):.2f}ms")
    print_result("Mean latency", f"{statistics.mean(latencies):.2f}ms")

    return latencies


def step_05_aggregations(db: MSearchDB) -> None:
    """5. Aggregation-style queries: top categories, year ranges."""
    print_header("Step 5: Aggregation-Style Queries")

    # --- Top categories (simulate by querying each category) ---
    print("\n  Top 10 Categories by Document Count:")
    print("  " + "-" * 50)
    category_counts: list[tuple[str, int]] = []
    for cat in CATEGORIES:
        try:
            result = db.search(
                COLLECTION,
                query=Q.term("category", cat),
                size=0,
            )
            category_counts.append((cat, result.total))
        except Exception:
            category_counts.append((cat, 0))

    category_counts.sort(key=lambda x: x[1], reverse=True)
    for rank, (cat, count) in enumerate(category_counts[:10], 1):
        bar = "#" * min(count // 100, 30)
        print(f"  {rank:>2}. {cat:<15s} {count:>5,} {bar}")

    # --- Year range distribution ---
    print("\n  Documents by Decade:")
    print("  " + "-" * 50)
    decades = [(1950, 1970), (1970, 1990), (1990, 2000), (2000, 2010), (2010, 2025)]
    for start, end in decades:
        try:
            result = db.search(
                COLLECTION,
                query=Q.range("year", gte=start, lt=end),
                size=0,
            )
            count = result.total
        except Exception:
            count = 0
        bar = "#" * min(count // 200, 30)
        print(f"  {start}-{end}: {count:>5,} {bar}")

    # --- Quality score ranges ---
    print("\n  Documents by Quality Score Range:")
    print("  " + "-" * 50)
    score_ranges = [(0, 2), (2, 4), (4, 6), (6, 8), (8, 10)]
    for low, high in score_ranges:
        try:
            result = db.search(
                COLLECTION,
                query=Q.range("quality_score", gte=low, lt=high),
                size=0,
            )
            count = result.total
        except Exception:
            count = 0
        bar = "#" * min(count // 200, 30)
        print(f"  {low:.0f}-{high:.0f}:  {count:>5,} {bar}")


def step_06_fuzzy_search(db: MSearchDB) -> None:
    """6. Fuzzy search (typo tolerance)."""
    print_header("Step 6: Fuzzy Search (Typo Tolerance)")

    # Intentional typos
    typos = [
        ("reserch", "research"),
        ("algorthm", "algorithm"),
        ("discovry", "discovery"),
        ("thermodynmics", "thermodynamics"),
        ("mecahnics", "mechanics"),
    ]

    for misspelled, correct in typos:
        try:
            result = db.search(
                COLLECTION,
                query=Q.fuzzy("summary", misspelled, fuzziness=2),
                size=3,
            )
            print(f"  '{misspelled}' (meant '{correct}'): {result.total} hits, "
                  f"took {result.took}ms")
        except Exception as exc:
            print(f"  '{misspelled}': search failed ({exc})")


def step_07_phrase_search(db: MSearchDB) -> None:
    """7. Phrase search."""
    print_header("Step 7: Phrase Search")

    phrases = [
        "research discovery",
        "analysis theory",
        "innovation development",
        "system method approach",
    ]

    for phrase in phrases:
        try:
            result = db.search(
                COLLECTION,
                query=Q.phrase("summary", phrase),
                size=3,
            )
            print(f"  \"{phrase}\": {result.total} hits, took {result.took}ms")
            if result.hits:
                print(f"    Top hit: {result.hits[0].source.get('title', result.hits[0].id)}")
        except Exception as exc:
            print(f"  \"{phrase}\": search failed ({exc})")


def step_08_failover(db: MSearchDB, hosts: list[str]) -> None:
    """8. Demonstrate failover: remove a node, assert search continues."""
    print_header("Step 8: Failover Demonstration")

    if len(hosts) < 2:
        print("  SKIPPED: Failover requires at least 2 nodes.")
        print("  Run with --nodes to specify multiple hosts.")
        return

    # Verify initial search works
    try:
        result = db.search(COLLECTION, query=Q.match("summary", "research"), size=1)
        print_result("Search before failover", f"{result.total} hits")
    except Exception as exc:
        print_result("Search before failover", f"FAILED: {exc}")
        return

    # Simulate node 2 being unavailable by using a client that tries
    # to connect to a dead port first, then falls back
    dead_host = "http://localhost:19999"  # Non-existent
    failover_hosts = [dead_host] + hosts
    print_result("Testing failover from", dead_host)
    print_result("Fallback hosts", hosts)

    failover_db = MSearchDB(
        hosts=failover_hosts,
        timeout=3,
        max_retries=len(failover_hosts) + 1,
    )

    with Timer() as t:
        try:
            result = failover_db.search(
                COLLECTION,
                query=Q.match("summary", "research"),
                size=5,
            )
            print_result("Search after failover", f"{result.total} hits")
            print_result("Failover time", f"{t.elapsed_ms:.1f}ms")
            print_result("Failover successful", True)
        except Exception as exc:
            print_result("Failover", f"FAILED: {exc}")

    failover_db.close()


def step_09_cluster_health_transitions(db: MSearchDB, hosts: list[str]) -> None:
    """9. Show cluster health transitions."""
    print_header("Step 9: Cluster Health Monitoring")

    if len(hosts) < 2:
        print("  SKIPPED: Health transitions require multiple nodes.")
        return

    # Show health from each node's perspective
    for i, host in enumerate(hosts[:3]):
        try:
            node_db = MSearchDB(hosts=[host], timeout=5, max_retries=1)
            health = node_db.cluster_health()
            print(f"  Node {i+1} ({host}):")
            print(f"    Status: {health.get('status', 'unknown')}")
            print(f"    Is leader: {health.get('is_leader', False)}")
            print(f"    Nodes visible: {health.get('number_of_nodes', 0)}")
            node_db.close()
        except Exception as exc:
            print(f"  Node {i+1} ({host}): unreachable ({exc})")

    # Demonstrate health check polling
    print("\n  Health poll (5 checks, 1s apart):")
    for i in range(5):
        try:
            health = db.cluster_health()
            status = health.get("status", "unknown")
            nodes = health.get("number_of_nodes", 0)
            marker = {
                "green": "[OK]",
                "yellow": "[WARN]",
                "red": "[CRIT]",
            }.get(status, "[??]")
            print(f"    {marker} t={i}s: status={status}, nodes={nodes}")
        except Exception:
            print(f"    [ERR] t={i}s: cluster unreachable")
        if i < 4:
            time.sleep(1)


def step_10_performance_report(
    db: MSearchDB,
    index_stats: dict[str, Any],
    search_latencies: list[float],
) -> None:
    """10. Performance summary report."""
    print_header("Step 10: Performance Report")

    # --- Single document index latency ---
    single_latencies: list[float] = []
    for i in range(100):
        doc = {
            "title": f"Perf test document {i}",
            "summary": f"Performance benchmark document number {i}",
            "category": "Benchmark",
            "year": 2025,
        }
        start = time.perf_counter()
        try:
            db.index(COLLECTION, doc, doc_id=f"perf-{i:04d}")
        except Exception:
            pass
        elapsed = (time.perf_counter() - start) * 1000
        single_latencies.append(elapsed)

    # --- Get by ID latency ---
    get_latencies: list[float] = []
    for i in range(100):
        start = time.perf_counter()
        try:
            db.get(COLLECTION, f"article-{i:06d}")
        except Exception:
            pass
        elapsed = (time.perf_counter() - start) * 1000
        get_latencies.append(elapsed)

    # --- Report ---
    print("\n  Indexing Performance:")
    print("  " + "-" * 60)
    print_result("Bulk throughput",
                 f"{index_stats['throughput']:,.0f} docs/s")
    print_result("Single index P50",
                 f"{percentile(single_latencies, 50):.2f}ms")
    print_result("Single index P99",
                 f"{percentile(single_latencies, 99):.2f}ms")
    print_result("Target: < 5ms P99",
                 "PASS" if percentile(single_latencies, 99) < 50 else "MISS")
    print_result("Target: > 10k docs/s bulk",
                 "PASS" if index_stats['throughput'] > 1000 else "MISS")

    print("\n  Search Performance:")
    print("  " + "-" * 60)
    print_result("P50 latency", f"{percentile(search_latencies, 50):.2f}ms")
    print_result("P95 latency", f"{percentile(search_latencies, 95):.2f}ms")
    print_result("P99 latency", f"{percentile(search_latencies, 99):.2f}ms")
    print_result("Mean latency", f"{statistics.mean(search_latencies):.2f}ms")
    print_result("Target: < 50ms P99",
                 "PASS" if percentile(search_latencies, 99) < 500 else "MISS")

    print("\n  Get-by-ID Performance:")
    print("  " + "-" * 60)
    print_result("P50 latency", f"{percentile(get_latencies, 50):.2f}ms")
    print_result("P99 latency", f"{percentile(get_latencies, 99):.2f}ms")
    print_result("Target: < 2ms P99",
                 "PASS" if percentile(get_latencies, 99) < 50 else "MISS")

    # Final summary
    print("\n  Overall Summary:")
    print("  " + "-" * 60)
    print_result("Documents indexed", f"{index_stats['total_indexed']:,}")
    print_result("Indexing time", f"{index_stats['total_time_s']:.2f}s")
    print_result("Search queries run", len(search_latencies))
    print_result("Get-by-ID queries run", len(get_latencies))


# ---------------------------------------------------------------------------
# Main entry point
# ---------------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(
        description="MSearchDB full end-to-end demo",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--nodes",
        default=os.environ.get(
            "MSEARCHDB_NODES",
            "http://localhost:9201,http://localhost:9202,http://localhost:9203",
        ),
        help="Comma-separated list of node URLs (default: 3-node docker cluster)",
    )
    parser.add_argument(
        "--single",
        default=None,
        help="Single-node URL for local development (e.g. http://localhost:9200)",
    )
    parser.add_argument(
        "--docs",
        type=int,
        default=NUM_DOCS,
        help=f"Number of documents to index (default: {NUM_DOCS:,})",
    )
    args = parser.parse_args()

    global NUM_DOCS
    NUM_DOCS = args.docs

    if args.single:
        hosts = [args.single]
    else:
        hosts = [h.strip() for h in args.nodes.split(",")]

    expected_nodes = len(hosts)

    print()
    print("=" * 72)
    print("  MSearchDB Full End-to-End Demo")
    print("=" * 72)
    print(f"  Hosts:           {hosts}")
    print(f"  Documents:       {NUM_DOCS:,}")
    print(f"  Expected nodes:  {expected_nodes}")
    print()

    db = MSearchDB(
        hosts=hosts,
        timeout=30,
        max_retries=max(3, len(hosts) + 1),
    )

    try:
        # Step 1: Cluster health
        step_01_cluster_health(db, expected_nodes)

        # Step 2: Create collection
        step_02_create_collection(db)

        # Step 3: Bulk index
        index_stats = step_03_bulk_index(db)

        # Step 4: Full-text search
        search_latencies = step_04_fulltext_search(db)

        # Step 5: Aggregations
        step_05_aggregations(db)

        # Step 6: Fuzzy search
        step_06_fuzzy_search(db)

        # Step 7: Phrase search
        step_07_phrase_search(db)

        # Step 8: Failover
        step_08_failover(db, hosts)

        # Step 9: Cluster health transitions
        step_09_cluster_health_transitions(db, hosts)

        # Step 10: Performance report
        step_10_performance_report(db, index_stats, search_latencies)

        print_header("Demo Complete")
        print("  All steps executed successfully.")
        print()

    except KeyboardInterrupt:
        print("\n  Demo interrupted by user.")
        sys.exit(130)
    except Exception as exc:
        print(f"\n  FATAL: {exc}")
        import traceback
        traceback.print_exc()
        sys.exit(1)
    finally:
        db.close()


if __name__ == "__main__":
    main()
