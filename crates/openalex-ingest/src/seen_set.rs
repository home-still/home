//! In-RAM seen-set for streaming pre-dedupe of the OpenAlex works ingest.
//!
//! The works snapshot emits the same `openalex_id` in every `updated_date`
//! partition where a record changed. We walk partitions newest-first and
//! gate each row through this set: the first sighting of an ID is the
//! canonical (newest) version, every later sighting is a stale duplicate to
//! skip. This is how we hold the PRIMARY KEY invariant on the live
//! `works`/`work_topics`/`work_concepts`/`work_references` tables without
//! ever paying for `ON CONFLICT` or a separate dedupe phase.
//!
//! Storage shape: `HashSet<u64>` keyed on the integer portion of the work-ID
//! (`W2741809807` → `2741809807`). At 250M unique works that's ~2 GB raw
//! plus ~3× HashSet overhead, ~6 GB resident. `big` has 32 GB total with
//! ~10 GB pinned by services and ~10 GB to DuckDB during ingest, leaving
//! the budget tight but feasible.
//!
//! Crash recovery: the set checkpoints to a sidecar binary file every N
//! partitions (`maybe_checkpoint`). On restart, `load_or_default` rehydrates
//! it. The `_ingest_log` table independently records which partitions
//! committed; both must agree on resume — if the seen-set file is missing
//! or shorter than the log implies, `rebuild_from_db` reconstructs the set
//! from the live `works` table (slow but correct).

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use duckdb::Connection;

use crate::parser::parse_work_id_u64;

/// Default partitions-between-checkpoints. At 311 works partitions and ~5s
/// per checkpoint write, 10 means ≤30 checkpoint writes per full ingest =
/// ~150s of overhead, ~0.5% of total runtime.
pub const DEFAULT_CHECKPOINT_EVERY_N: u32 = 10;

pub struct SeenSet {
    ids: HashSet<u64>,
    path: PathBuf,
    checkpoint_every_n_partitions: u32,
    partitions_since_checkpoint: u32,
}

impl SeenSet {
    /// Open the seen-set file at `path`. If it exists, rehydrate the set
    /// from it; if not, return an empty set. The file format is just a
    /// stream of little-endian u64s — one per ID, no framing.
    pub fn load_or_default(path: PathBuf) -> Result<Self> {
        let mut ids = HashSet::new();
        if path.exists() {
            let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
            let mut reader = BufReader::with_capacity(1 << 20, f);
            let mut buf = [0u8; 8];
            loop {
                match reader.read_exact(&mut buf) {
                    Ok(()) => {
                        ids.insert(u64::from_le_bytes(buf));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => {
                        return Err(anyhow!("read {}: {}", path.display(), e));
                    }
                }
            }
            tracing::info!(loaded_ids = ids.len(), path = %path.display(), "seen-set rehydrated");
        }
        Ok(Self {
            ids,
            path,
            checkpoint_every_n_partitions: DEFAULT_CHECKPOINT_EVERY_N,
            partitions_since_checkpoint: 0,
        })
    }

    /// Try to insert a work-ID by its raw string form. Returns `Ok(true)` if
    /// the ID is newly seen (caller should emit the row), `Ok(false)` if
    /// it's a duplicate (caller should skip), and `Err` if the ID couldn't
    /// be parsed as a `W<digits>` u64 (caller decides whether to error or
    /// log+skip).
    pub fn insert(&mut self, work_id_str: &str) -> Result<bool> {
        let id = parse_work_id_u64(work_id_str)
            .ok_or_else(|| anyhow!("malformed work id: {}", work_id_str))?;
        Ok(self.ids.insert(id))
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Increment the partition counter; flush to disk if the threshold
    /// is hit. Call after each partition completes.
    pub fn maybe_checkpoint(&mut self) -> Result<()> {
        self.partitions_since_checkpoint += 1;
        if self.partitions_since_checkpoint >= self.checkpoint_every_n_partitions {
            self.force_checkpoint()?;
            self.partitions_since_checkpoint = 0;
        }
        Ok(())
    }

    /// Write the entire set to disk, overwriting any prior checkpoint.
    /// Atomic-ish: write to `<path>.tmp` then `rename` over the live file
    /// so a crash mid-write leaves the prior checkpoint intact.
    pub fn force_checkpoint(&self) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        if let Some(parent) = tmp.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let f = File::create(&tmp)
            .with_context(|| format!("create checkpoint at {}", tmp.display()))?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        for id in &self.ids {
            w.write_all(&id.to_le_bytes())
                .context("write seen-set id")?;
        }
        w.flush().context("flush seen-set checkpoint")?;
        drop(w);
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), self.path.display()))?;
        tracing::info!(ids = self.ids.len(), path = %self.path.display(), "seen-set checkpointed");
        Ok(())
    }

    /// Recovery path: if the on-disk checkpoint is missing or stale relative
    /// to `_ingest_log`, rebuild the set by scanning `SELECT openalex_id FROM
    /// works`. Slow at corpus scale (~minutes for 250M rows) but exact.
    pub fn rebuild_from_db(conn: &Connection, path: PathBuf) -> Result<Self> {
        let mut stmt = conn.prepare("SELECT openalex_id FROM works")?;
        let mut rows = stmt.query([])?;
        let mut ids = HashSet::new();
        while let Some(row) = rows.next()? {
            let id_str: String = row.get(0)?;
            if let Some(id) = parse_work_id_u64(&id_str) {
                ids.insert(id);
            }
        }
        tracing::info!(rebuilt_ids = ids.len(), "seen-set rebuilt from works table");
        Ok(Self {
            ids,
            path,
            checkpoint_every_n_partitions: DEFAULT_CHECKPOINT_EVERY_N,
            partitions_since_checkpoint: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_returns_true_for_first_sighting_false_for_dup() {
        let dir = std::env::temp_dir().join(format!("oa-seenset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seen.bin");
        let mut s = SeenSet::load_or_default(path).unwrap();
        assert!(s.insert("W123").unwrap());
        assert!(!s.insert("W123").unwrap());
        assert!(s.insert("https://openalex.org/W456").unwrap());
        assert!(!s.insert("W456").unwrap());
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn checkpoint_roundtrip_preserves_ids() {
        let dir = std::env::temp_dir().join(format!("oa-seenset-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seen.bin");

        let mut s1 = SeenSet::load_or_default(path.clone()).unwrap();
        s1.insert("W100").unwrap();
        s1.insert("W200").unwrap();
        s1.insert("W300").unwrap();
        s1.force_checkpoint().unwrap();

        let mut s2 = SeenSet::load_or_default(path).unwrap();
        assert_eq!(s2.len(), 3);
        assert!(!s2.insert("W100").unwrap());
        assert!(!s2.insert("W200").unwrap());
        assert!(!s2.insert("W300").unwrap());
        assert!(s2.insert("W400").unwrap());
    }

    #[test]
    fn malformed_id_errors_does_not_insert() {
        let dir = std::env::temp_dir().join(format!("oa-seenset-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = SeenSet::load_or_default(dir.join("seen.bin")).unwrap();
        assert!(s.insert("A123").is_err());
        assert!(s.insert("Wabc").is_err());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn maybe_checkpoint_flushes_at_threshold() {
        let dir = std::env::temp_dir().join(format!("oa-seenset-thresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seen.bin");
        let mut s = SeenSet::load_or_default(path.clone()).unwrap();
        s.checkpoint_every_n_partitions = 3;
        s.insert("W1").unwrap();
        s.maybe_checkpoint().unwrap();
        assert!(!path.exists(), "no checkpoint after partition 1");
        s.insert("W2").unwrap();
        s.maybe_checkpoint().unwrap();
        assert!(!path.exists(), "no checkpoint after partition 2");
        s.insert("W3").unwrap();
        s.maybe_checkpoint().unwrap();
        assert!(path.exists(), "checkpoint after partition 3");
    }
}
