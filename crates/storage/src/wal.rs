//! Write-Ahead Log (WAL) for crash recovery.
//!
//! The WAL provides durability by persisting every mutation to an append-only
//! file *before* it is applied to the main storage engine. On recovery,
//! [`WalReader`] replays committed entries so no acknowledged writes are lost.
//!
//! Each entry is framed as:
//!
//! ```text
//! ┌──────────┬──────────┬──────────────────┬──────────┐
//! │ len (4B) │ CRC (4B) │ payload (len B)  │  …next…  │
//! └──────────┴──────────┴──────────────────┴──────────┘
//! ```
//!
//! - **len**: little-endian `u32` — byte length of the serialized payload.
//! - **CRC**: CRC32 (Castagnoli) checksum of the payload bytes.
//! - **payload**: MessagePack-encoded [`WalEntry`].
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//! use msearchdb_storage::wal::{WalWriter, WalReader, WalOperation};
//!
//! # fn example() -> msearchdb_core::error::DbResult<()> {
//! let mut writer = WalWriter::new(Path::new("/tmp/wal.log"))?;
//! writer.append(WalOperation::Put {
//!     key: b"doc-1".to_vec(),
//!     value: b"hello".to_vec(),
//! })?;
//! writer.sync()?;
//!
//! let entries = WalReader::read_all(Path::new("/tmp/wal.log"))?;
//! assert_eq!(entries.len(), 1);
//! # Ok(())
//! # }
//! ```

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use msearchdb_core::error::{DbError, DbResult};

/// A key-value pair from WAL replay.
pub type KeyValue = (Vec<u8>, Vec<u8>);

/// Result of replaying a WAL: `(puts, deletes)`.
pub type ReplayResult = (Vec<KeyValue>, Vec<Vec<u8>>);

// ---------------------------------------------------------------------------
// WalOperation
// ---------------------------------------------------------------------------

/// The kind of mutation recorded in a WAL entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum WalOperation {
    /// Insert or update a key-value pair.
    Put {
        /// Raw key bytes.
        key: Vec<u8>,
        /// Raw value bytes.
        value: Vec<u8>,
    },
    /// Delete a key.
    Delete {
        /// Raw key bytes.
        key: Vec<u8>,
    },
    /// A checkpoint marker — indicates all preceding entries are durable.
    Checkpoint,
}

// ---------------------------------------------------------------------------
// WalEntry
// ---------------------------------------------------------------------------

/// A single entry in the write-ahead log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WalEntry {
    /// Monotonically increasing sequence number.
    pub sequence: u64,
    /// The operation that was performed.
    pub operation: WalOperation,
    /// Unix timestamp in milliseconds when the entry was created.
    pub timestamp: u64,
}

impl WalEntry {
    /// Create a new WAL entry with the current wall-clock time.
    pub fn new(sequence: u64, operation: WalOperation) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            sequence,
            operation,
            timestamp,
        }
    }
}

// ---------------------------------------------------------------------------
// WalWriter
// ---------------------------------------------------------------------------

/// Appends [`WalEntry`] records to an append-only WAL file.
///
/// Each entry is framed with a 4-byte length prefix and a 4-byte CRC32
/// checksum for corruption detection.
pub struct WalWriter {
    writer: BufWriter<File>,
    next_sequence: u64,
}

impl WalWriter {
    /// Open (or create) a WAL file at `path`.
    ///
    /// If the file already exists, new entries are appended. The writer
    /// scans existing entries to determine the next sequence number.
    pub fn new(path: &Path) -> DbResult<Self> {
        // Determine next sequence by reading existing entries.
        let next_sequence = if path.exists() {
            let entries = WalReader::read_all(path)?;
            entries.last().map_or(0, |e| e.sequence + 1)
        } else {
            0
        };

        let file = OpenOptions::new().create(true).append(true).open(path)?;

        Ok(Self {
            writer: BufWriter::new(file),
            next_sequence,
        })
    }

    /// Append a new operation to the WAL, returning the assigned sequence number.
    pub fn append(&mut self, operation: WalOperation) -> DbResult<u64> {
        let seq = self.next_sequence;
        let entry = WalEntry::new(seq, operation);
        self.next_sequence += 1;
        self.write_entry(&entry)?;
        Ok(seq)
    }

    /// Flush and fsync the underlying file, ensuring durability.
    pub fn sync(&mut self) -> DbResult<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }

    /// Write a single framed entry: `[len:4][crc:4][payload:len]`.
    fn write_entry(&mut self, entry: &WalEntry) -> DbResult<()> {
        let payload =
            rmp_serde::to_vec(entry).map_err(|e| DbError::SerializationError(e.to_string()))?;

        let crc = crc32fast::hash(&payload);

        let len = payload.len() as u32;
        self.writer.write_all(&len.to_le_bytes())?;
        self.writer.write_all(&crc.to_le_bytes())?;
        self.writer.write_all(&payload)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WalReader
// ---------------------------------------------------------------------------

/// Reads and validates WAL entries from a file.
///
/// Used during recovery to replay committed operations.
pub struct WalReader;

impl WalReader {
    /// Read all valid entries from a WAL file.
    ///
    /// Entries whose CRC checksum does not match are reported as errors,
    /// indicating file corruption.
    pub fn read_all(path: &Path) -> DbResult<Vec<WalEntry>> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut entries = Vec::new();

        loop {
            // Read length prefix.
            let mut len_buf = [0u8; 4];
            match reader.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            let len = u32::from_le_bytes(len_buf) as usize;

            // Read CRC.
            let mut crc_buf = [0u8; 4];
            reader.read_exact(&mut crc_buf)?;
            let stored_crc = u32::from_le_bytes(crc_buf);

            // Read payload.
            let mut payload = vec![0u8; len];
            reader.read_exact(&mut payload)?;

            // Validate CRC.
            let computed_crc = crc32fast::hash(&payload);
            if computed_crc != stored_crc {
                return Err(DbError::StorageError(format!(
                    "WAL entry CRC mismatch: stored={:#010x}, computed={:#010x}",
                    stored_crc, computed_crc
                )));
            }

            // Deserialize.
            let entry: WalEntry = rmp_serde::from_slice(&payload)
                .map_err(|e| DbError::SerializationError(e.to_string()))?;
            entries.push(entry);
        }

        Ok(entries)
    }

    /// Replay WAL entries as raw key-value operations.
    ///
    /// Returns `(puts, deletes)` where puts is a vec of `(key, value)` and
    /// deletes is a vec of keys. Only entries up to and including the last
    /// `Checkpoint` are included. If no checkpoint exists, all entries are
    /// replayed.
    pub fn replay(path: &Path) -> DbResult<ReplayResult> {
        let entries = Self::read_all(path)?;

        // Find the last checkpoint index.
        let last_checkpoint = entries
            .iter()
            .rposition(|e| matches!(e.operation, WalOperation::Checkpoint));

        let replay_entries = match last_checkpoint {
            Some(idx) => &entries[..=idx],
            None => &entries,
        };

        let mut puts = Vec::new();
        let mut deletes = Vec::new();

        for entry in replay_entries {
            match &entry.operation {
                WalOperation::Put { key, value } => {
                    puts.push((key.clone(), value.clone()));
                }
                WalOperation::Delete { key } => {
                    deletes.push(key.clone());
                }
                WalOperation::Checkpoint => {}
            }
        }

        Ok((puts, deletes))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn wal_path(dir: &TempDir) -> std::path::PathBuf {
        dir.path().join("test.wal")
    }

    #[test]
    fn write_and_read_entries() {
        let dir = TempDir::new().unwrap();
        let path = wal_path(&dir);

        {
            let mut writer = WalWriter::new(&path).unwrap();
            writer
                .append(WalOperation::Put {
                    key: b"k1".to_vec(),
                    value: b"v1".to_vec(),
                })
                .unwrap();
            writer
                .append(WalOperation::Delete {
                    key: b"k1".to_vec(),
                })
                .unwrap();
            writer.append(WalOperation::Checkpoint).unwrap();
            writer.sync().unwrap();
        }

        let entries = WalReader::read_all(&path).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sequence, 0);
        assert_eq!(entries[1].sequence, 1);
        assert_eq!(entries[2].sequence, 2);
        assert!(matches!(entries[0].operation, WalOperation::Put { .. }));
        assert!(matches!(entries[1].operation, WalOperation::Delete { .. }));
        assert!(matches!(entries[2].operation, WalOperation::Checkpoint));
    }

    #[test]
    fn crc_corruption_detected() {
        let dir = TempDir::new().unwrap();
        let path = wal_path(&dir);

        {
            let mut writer = WalWriter::new(&path).unwrap();
            writer
                .append(WalOperation::Put {
                    key: b"k".to_vec(),
                    value: b"v".to_vec(),
                })
                .unwrap();
            writer.sync().unwrap();
        }

        // Corrupt the payload by flipping a byte in the CRC field.
        {
            let mut data = std::fs::read(&path).unwrap();
            // CRC is at bytes 4..8. Flip bit in byte 4.
            data[4] ^= 0xFF;
            std::fs::write(&path, &data).unwrap();
        }

        let result = WalReader::read_all(&path);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("CRC mismatch"), "got: {}", err_str);
    }

    #[test]
    fn replay_respects_checkpoint() {
        let dir = TempDir::new().unwrap();
        let path = wal_path(&dir);

        {
            let mut writer = WalWriter::new(&path).unwrap();
            writer
                .append(WalOperation::Put {
                    key: b"k1".to_vec(),
                    value: b"v1".to_vec(),
                })
                .unwrap();
            writer.append(WalOperation::Checkpoint).unwrap();
            // This entry is after the checkpoint, so replay should NOT
            // include it when a checkpoint is present.
            writer
                .append(WalOperation::Put {
                    key: b"k2".to_vec(),
                    value: b"v2".to_vec(),
                })
                .unwrap();
            writer.sync().unwrap();
        }

        let (puts, deletes) = WalReader::replay(&path).unwrap();
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].0, b"k1");
        assert!(deletes.is_empty());
    }

    #[test]
    fn recovery_after_simulated_crash() {
        let dir = TempDir::new().unwrap();
        let path = wal_path(&dir);

        // Phase 1: write some entries, then "crash" (drop writer without checkpoint).
        {
            let mut writer = WalWriter::new(&path).unwrap();
            writer
                .append(WalOperation::Put {
                    key: b"doc-1".to_vec(),
                    value: b"data-1".to_vec(),
                })
                .unwrap();
            writer
                .append(WalOperation::Put {
                    key: b"doc-2".to_vec(),
                    value: b"data-2".to_vec(),
                })
                .unwrap();
            writer.sync().unwrap();
            // drop without checkpoint — simulates crash.
        }

        // Phase 2: recover by reading the WAL.
        let entries = WalReader::read_all(&path).unwrap();
        assert_eq!(entries.len(), 2);

        // Phase 3: resume writing after recovery.
        {
            let mut writer = WalWriter::new(&path).unwrap();
            assert_eq!(
                writer
                    .append(WalOperation::Put {
                        key: b"doc-3".to_vec(),
                        value: b"data-3".to_vec(),
                    })
                    .unwrap(),
                2 // sequence continues from where we left off
            );
            writer.sync().unwrap();
        }

        let entries = WalReader::read_all(&path).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn empty_wal_file() {
        let dir = TempDir::new().unwrap();
        let path = wal_path(&dir);

        // Create an empty WAL.
        {
            let _writer = WalWriter::new(&path).unwrap();
        }

        let entries = WalReader::read_all(&path).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn wal_entry_timestamps_are_recent() {
        let entry = WalEntry::new(0, WalOperation::Checkpoint);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        // Should be within 1 second.
        assert!(entry.timestamp <= now_ms);
        assert!(entry.timestamp > now_ms - 1000);
    }
}
