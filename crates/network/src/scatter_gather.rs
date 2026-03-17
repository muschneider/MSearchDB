//! Scatter-gather coordinator for distributed search across MSearchDB nodes.
//!
//! When a search query arrives, [`scatter_search`] fans it out to all
//! available nodes concurrently using [`FuturesUnordered`], collects the
//! partial results, deduplicates by document id, and re-ranks globally
//! by BM25 score to produce the final top-N result set.
//!
//! # Timeout handling
//!
//! Each per-node RPC is wrapped in a [`tokio::time::timeout`] of
//! [`NODE_TIMEOUT`].  Nodes that fail or time out are silently skipped —
//! scatter-gather returns the best results available.

use std::collections::HashMap;
use std::time::Duration;

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tokio::time::Instant;

use msearchdb_core::cluster::NodeId;
use msearchdb_core::error::DbResult;
use msearchdb_core::query::{Query, ScoredDocument, SearchResult};

use crate::client::NodeClient;
use crate::connection_pool::ConnectionPool;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Per-node RPC timeout for scatter-gather queries.
const NODE_TIMEOUT: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// scatter_search
// ---------------------------------------------------------------------------

/// Fan out a search query to all given nodes concurrently, then merge,
/// deduplicate, and re-rank the results by BM25 score.
///
/// Uses [`FuturesUnordered`] for non-blocking concurrent fan-out.
/// Nodes that fail or time out are skipped — the function returns
/// partial results rather than failing entirely.
///
/// # Arguments
///
/// * `nodes` — Slice of `(NodeId, NodeClient)` pairs to query.
/// * `query` — The search query to execute.
/// * `limit` — Maximum number of results to return globally.
/// * `timeout` — Per-node RPC timeout (pass `None` for the default 200 ms).
pub async fn scatter_search(
    nodes: &[(NodeId, NodeClient)],
    query: &Query,
    limit: usize,
    timeout: Option<Duration>,
) -> DbResult<SearchResult> {
    let start = Instant::now();
    let per_node_timeout = timeout.unwrap_or(NODE_TIMEOUT);

    if nodes.is_empty() {
        return Ok(SearchResult::empty(0));
    }

    // Fan-out: launch one RPC per node concurrently.
    let mut futures = FuturesUnordered::new();

    for (node_id, client) in nodes {
        let client = client.clone();
        let query = query.clone();
        let nid = *node_id;

        futures.push(async move {
            let result = tokio::time::timeout(per_node_timeout, client.search(&query, limit)).await;

            (nid, result)
        });
    }

    // Gather: collect results from all nodes.
    let mut all_scored: Vec<ScoredDocument> = Vec::new();
    let mut total_hits: u64 = 0;

    while let Some((node_id, result)) = futures.next().await {
        match result {
            Ok(Ok(search_result)) => {
                tracing::debug!(
                    node_id = node_id.as_u64(),
                    hits = search_result.documents.len(),
                    "scatter: received results"
                );
                total_hits += search_result.total;
                all_scored.extend(search_result.documents);
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    node_id = node_id.as_u64(),
                    error = %e,
                    "scatter: node returned error"
                );
            }
            Err(_) => {
                tracing::warn!(
                    node_id = node_id.as_u64(),
                    timeout_ms = per_node_timeout.as_millis() as u64,
                    "scatter: node timed out"
                );
            }
        }
    }

    // Deduplicate by document id (keep the highest-scoring copy).
    let deduped = deduplicate_results(all_scored);

    // Re-rank globally by BM25 score (descending).
    let ranked = rank_results(deduped, limit);

    let took_ms = start.elapsed().as_millis() as u64;

    Ok(SearchResult {
        documents: ranked,
        total: total_hits,
        took_ms,
        aggregations: std::collections::HashMap::new(),
    })
}

// ---------------------------------------------------------------------------
// scatter_search_with_pool  (pool-aware variant)
// ---------------------------------------------------------------------------

/// Like [`scatter_search`] but acquires clients from a [`ConnectionPool`].
///
/// Records success/failure on each node to drive circuit breaker transitions.
pub async fn scatter_search_with_pool(
    node_addrs: &[(NodeId, msearchdb_core::cluster::NodeAddress)],
    query: &Query,
    limit: usize,
    pool: &ConnectionPool,
) -> DbResult<SearchResult> {
    let start = Instant::now();

    if node_addrs.is_empty() {
        return Ok(SearchResult::empty(0));
    }

    let mut futures = FuturesUnordered::new();

    for (node_id, addr) in node_addrs {
        let nid = *node_id;
        let addr = addr.clone();

        // Try to acquire a client from the pool.
        let client = match pool.get(&nid, &addr).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    node_id = nid.as_u64(),
                    error = %e,
                    "scatter: skipping node (pool error)"
                );
                continue;
            }
        };

        let query = query.clone();

        futures.push(async move {
            let result = tokio::time::timeout(NODE_TIMEOUT, client.search(&query, limit)).await;
            (nid, result)
        });
    }

    let mut all_scored: Vec<ScoredDocument> = Vec::new();
    let mut total_hits: u64 = 0;

    while let Some((node_id, result)) = futures.next().await {
        match result {
            Ok(Ok(search_result)) => {
                pool.record_success(&node_id).await;
                total_hits += search_result.total;
                all_scored.extend(search_result.documents);
            }
            Ok(Err(e)) => {
                tracing::warn!(node_id = node_id.as_u64(), error = %e, "scatter: RPC error");
                pool.record_failure(&node_id).await;
            }
            Err(_) => {
                tracing::warn!(node_id = node_id.as_u64(), "scatter: timeout");
                pool.record_failure(&node_id).await;
            }
        }
    }

    let deduped = deduplicate_results(all_scored);
    let ranked = rank_results(deduped, limit);
    let took_ms = start.elapsed().as_millis() as u64;

    Ok(SearchResult {
        documents: ranked,
        total: total_hits,
        took_ms,
        aggregations: std::collections::HashMap::new(),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Deduplicate scored documents by document id, keeping the copy with the
/// highest BM25 score.
pub fn deduplicate_results(docs: Vec<ScoredDocument>) -> Vec<ScoredDocument> {
    let mut best: HashMap<String, ScoredDocument> = HashMap::new();

    for scored in docs {
        let key = scored.document.id.as_str().to_owned();
        let entry = best.entry(key);
        entry
            .and_modify(|existing| {
                if scored.score > existing.score {
                    *existing = scored.clone();
                }
            })
            .or_insert(scored);
    }

    best.into_values().collect()
}

/// Sort documents by score (descending) and truncate to `limit`.
pub fn rank_results(mut docs: Vec<ScoredDocument>, limit: usize) -> Vec<ScoredDocument> {
    docs.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    docs.truncate(limit);
    docs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use msearchdb_core::document::{Document, DocumentId, FieldValue};

    fn make_scored(id: &str, score: f32) -> ScoredDocument {
        ScoredDocument {
            document: Document::new(DocumentId::new(id))
                .with_field("title", FieldValue::Text(format!("doc-{}", id))),
            score,
            sort: Vec::new(),
        }
    }

    // -- deduplicate_results -----------------------------------------------

    #[test]
    fn dedup_empty() {
        let result = deduplicate_results(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn dedup_no_duplicates() {
        let docs = vec![make_scored("a", 1.0), make_scored("b", 2.0)];
        let result = deduplicate_results(docs);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn dedup_keeps_highest_score() {
        let docs = vec![
            make_scored("a", 1.0),
            make_scored("a", 3.0),
            make_scored("a", 2.0),
        ];
        let result = deduplicate_results(docs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].score, 3.0);
    }

    #[test]
    fn dedup_mixed_ids() {
        let docs = vec![
            make_scored("a", 1.0),
            make_scored("b", 5.0),
            make_scored("a", 4.0),
            make_scored("c", 2.0),
            make_scored("b", 3.0),
        ];
        let result = deduplicate_results(docs);
        assert_eq!(result.len(), 3);

        let scores: HashMap<String, f32> = result
            .iter()
            .map(|s| (s.document.id.as_str().to_owned(), s.score))
            .collect();
        assert_eq!(scores["a"], 4.0);
        assert_eq!(scores["b"], 5.0);
        assert_eq!(scores["c"], 2.0);
    }

    // -- rank_results ------------------------------------------------------

    #[test]
    fn rank_sorts_descending() {
        let docs = vec![
            make_scored("a", 1.0),
            make_scored("b", 3.0),
            make_scored("c", 2.0),
        ];
        let ranked = rank_results(docs, 10);
        assert_eq!(ranked[0].score, 3.0);
        assert_eq!(ranked[1].score, 2.0);
        assert_eq!(ranked[2].score, 1.0);
    }

    #[test]
    fn rank_truncates_to_limit() {
        let docs = vec![
            make_scored("a", 1.0),
            make_scored("b", 3.0),
            make_scored("c", 2.0),
            make_scored("d", 4.0),
        ];
        let ranked = rank_results(docs, 2);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].score, 4.0);
        assert_eq!(ranked[1].score, 3.0);
    }

    #[test]
    fn rank_empty_input() {
        let ranked = rank_results(vec![], 10);
        assert!(ranked.is_empty());
    }

    // -- scatter_search (unit, no real network) ----------------------------

    #[tokio::test]
    async fn scatter_search_empty_nodes() {
        use msearchdb_core::query::{FullTextQuery, Operator};

        let query = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "test".into(),
            operator: Operator::Or,
        });
        let result = scatter_search(&[], &query, 10, None).await.unwrap();
        assert_eq!(result.total, 0);
        assert!(result.documents.is_empty());
    }

    // -- Merge three result sets (simulated) --------------------------------

    #[test]
    fn merge_three_result_sets_correct_ranking() {
        // Simulate 3 nodes returning different results.
        let node1 = vec![make_scored("a", 3.0), make_scored("b", 1.0)];
        let node2 = vec![make_scored("c", 4.0), make_scored("a", 2.0)];
        let node3 = vec![make_scored("d", 2.5), make_scored("b", 0.5)];

        let mut all: Vec<ScoredDocument> = Vec::new();
        all.extend(node1);
        all.extend(node2);
        all.extend(node3);

        let deduped = deduplicate_results(all);
        let ranked = rank_results(deduped, 10);

        // Expected order: c(4.0), a(3.0), d(2.5), b(1.0)
        assert_eq!(ranked.len(), 4);
        assert_eq!(ranked[0].document.id.as_str(), "c");
        assert_eq!(ranked[0].score, 4.0);
        assert_eq!(ranked[1].document.id.as_str(), "a");
        assert_eq!(ranked[1].score, 3.0);
        assert_eq!(ranked[2].document.id.as_str(), "d");
        assert_eq!(ranked[2].score, 2.5);
        assert_eq!(ranked[3].document.id.as_str(), "b");
        assert_eq!(ranked[3].score, 1.0);
    }

    #[test]
    fn merge_with_limit_returns_top_n() {
        let node1 = vec![make_scored("a", 5.0), make_scored("b", 1.0)];
        let node2 = vec![make_scored("c", 3.0), make_scored("d", 2.0)];
        let node3 = vec![make_scored("e", 4.0)];

        let mut all: Vec<ScoredDocument> = Vec::new();
        all.extend(node1);
        all.extend(node2);
        all.extend(node3);

        let deduped = deduplicate_results(all);
        let ranked = rank_results(deduped, 3);

        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0].score, 5.0);
        assert_eq!(ranked[1].score, 4.0);
        assert_eq!(ranked[2].score, 3.0);
    }
}
