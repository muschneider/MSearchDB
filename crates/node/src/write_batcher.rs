//! Write batching for amortised Raft consensus overhead.
//!
//! The [`WriteBatcher`] buffers incoming write operations in memory and
//! flushes them as a single [`RaftCommand::BatchInsert`] entry when either:
//!
//! - The buffer reaches [`MAX_BATCH_SIZE`] documents (default 100), or
//! - [`MAX_BATCH_DELAY`] has elapsed since the first buffered write (default 10ms).
//!
//! This amortises the per-write Raft proposal overhead, significantly improving
//! throughput for high-volume ingest workloads while keeping tail latency bounded.
//!
//! # Usage
//!
//! ```text
//! let batcher = WriteBatcher::new(raft_node.clone());
//! let handle = batcher.start(); // spawns background flush task
//!
//! // Submit documents — returns a oneshot receiver for the result
//! let rx = batcher.submit("products", doc).await;
//! let result = rx.await.unwrap();
//! ```

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

use msearchdb_consensus::raft_node::RaftNode;
use msearchdb_consensus::types::RaftResponse;
use msearchdb_core::document::Document;
use msearchdb_core::error::DbError;

/// Maximum number of documents per batch before forced flush.
pub const MAX_BATCH_SIZE: usize = 100;

/// Maximum time to wait before flushing a partial batch.
pub const MAX_BATCH_DELAY: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------
// BatchEntry
// ---------------------------------------------------------------------------

/// A single pending write in the batch buffer.
#[allow(dead_code)]
struct BatchEntry {
    /// The collection this document belongs to.
    ///
    /// Stored for future per-collection batch grouping and included in
    /// error diagnostics.
    collection: String,

    /// The document to index.
    document: Document,

    /// Channel to notify the caller of the result.
    reply: oneshot::Sender<Result<RaftResponse, DbError>>,
}

// ---------------------------------------------------------------------------
// WriteBatcher
// ---------------------------------------------------------------------------

/// Buffers writes and flushes them as batched Raft proposals.
///
/// Thread-safe: submissions can come from any handler concurrently.
/// A background task handles timer-based flushing.
pub struct WriteBatcher {
    tx: mpsc::Sender<BatchEntry>,
    rx: Mutex<Option<mpsc::Receiver<BatchEntry>>>,
    raft_node: Arc<RaftNode>,
}

impl WriteBatcher {
    /// Create a new write batcher.
    ///
    /// The batcher is not active until [`start`](WriteBatcher::start) is called.
    pub fn new(raft_node: Arc<RaftNode>) -> Self {
        let (tx, rx) = mpsc::channel(MAX_BATCH_SIZE * 4);
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            raft_node,
        }
    }

    /// Submit a document for batched writing.
    ///
    /// Returns a oneshot receiver that will receive the Raft proposal result
    /// once the batch containing this document is flushed.
    pub async fn submit(
        &self,
        collection: &str,
        document: Document,
    ) -> oneshot::Receiver<Result<RaftResponse, DbError>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let entry = BatchEntry {
            collection: collection.to_owned(),
            document,
            reply: reply_tx,
        };

        if self.tx.send(entry).await.is_err() {
            // Channel closed — return error immediately via a new channel.
            let (err_tx, err_rx) = oneshot::channel();
            let _ = err_tx.send(Err(DbError::ConsensusError(
                "write batcher channel closed".into(),
            )));
            return err_rx;
        }

        reply_rx
    }

    /// Start the background flush loop.
    ///
    /// Takes ownership of the receiver (can only be called once).
    /// Returns a [`JoinHandle`] for the background task.
    pub async fn start(&self) -> JoinHandle<()> {
        let mut rx = self
            .rx
            .lock()
            .await
            .take()
            .expect("WriteBatcher::start called more than once");
        let raft_node = self.raft_node.clone();

        tokio::spawn(async move {
            let mut buffer: Vec<BatchEntry> = Vec::with_capacity(MAX_BATCH_SIZE);

            loop {
                // Wait for the first entry or channel close.
                let entry = match rx.recv().await {
                    Some(e) => e,
                    None => {
                        // Channel closed — flush remaining and exit.
                        if !buffer.is_empty() {
                            flush_batch(&raft_node, &mut buffer).await;
                        }
                        break;
                    }
                };
                buffer.push(entry);

                // Collect more entries up to the batch limit or timeout.
                let deadline = tokio::time::Instant::now() + MAX_BATCH_DELAY;
                while buffer.len() < MAX_BATCH_SIZE {
                    match tokio::time::timeout_at(deadline, rx.recv()).await {
                        Ok(Some(entry)) => {
                            buffer.push(entry);
                        }
                        Ok(None) => {
                            // Channel closed.
                            break;
                        }
                        Err(_timeout) => {
                            // Timer expired.
                            break;
                        }
                    }
                }

                // Flush the batch.
                flush_batch(&raft_node, &mut buffer).await;
            }
        })
    }
}

/// Flush a batch of entries through Raft and notify all callers.
async fn flush_batch(raft_node: &Arc<RaftNode>, buffer: &mut Vec<BatchEntry>) {
    if buffer.is_empty() {
        return;
    }

    let entries: Vec<BatchEntry> = std::mem::take(buffer);

    // Group by collection for potential per-collection batching.
    // For simplicity, submit all documents as a single BatchInsert regardless
    // of collection — the state machine and handlers handle collection routing.
    let documents: Vec<Document> = entries.iter().map(|e| e.document.clone()).collect();

    let result = raft_node.propose_batch(documents).await;

    match result {
        Ok(resp) => {
            for entry in entries {
                let _ = entry.reply.send(Ok(resp.clone()));
            }
        }
        Err(e) => {
            let err_str = e.to_string();
            for entry in entries {
                let _ = entry
                    .reply
                    .send(Err(DbError::ConsensusError(err_str.clone())));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_config_defaults() {
        assert_eq!(MAX_BATCH_SIZE, 100);
        assert_eq!(MAX_BATCH_DELAY, Duration::from_millis(10));
    }
}
