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
//! # The invariant
//!
//! **The set holds exactly the IDs whose `works` row is committed.** An ID
//! enters the set only through [`SeenSet::commit`], which the loader calls
//! after the transaction that inserted the rows has committed, never before.
//! (The original loader inserted on first sighting, i.e. before its row was
//! written; any failure afterwards left an ID the database never received,
//! and every later checkpoint and re-run treated that work as already
//! loaded.) Because of this, every checkpoint is a snapshot of committed
//! IDs and the database can only be *ahead* of a checkpoint (a crash between
//! a commit and the next checkpoint), never behind it.
//!
//! # Crash recovery
//!
//! The set checkpoints to a sidecar binary file every N partitions
//! (`maybe_checkpoint`). [`SeenSet::open`] does not trust that file: it
//! reads it strictly and reconciles it against the `works` table, which is
//! the authority:
//!
//! * a `works` row missing from the checkpoint (the crash window) is adopted;
//! * a checkpoint ID with no `works` row means the checkpoint was written
//!   ahead of the database (written by the pre-fix loader). Continuing would
//!   permanently skip those works, so `open` fails and says what to do.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
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
    /// Load the checkpoint at `path` (empty set if the file does not exist)
    /// and reconcile it with the `works` table of `conn`. See the module
    /// docs for the reconciliation rules. Scans `works.openalex_id` once
    /// (about a minute at corpus scale).
    pub fn open(conn: &Connection, path: PathBuf) -> Result<Self> {
        let mut ids = read_checkpoint(&path)?;
        let checkpointed = ids.len() as u64;

        let mut stmt = conn.prepare("SELECT openalex_id FROM works")?;
        let mut rows = stmt.query([])?;
        let mut matched = 0u64;
        let mut adopt: Vec<u64> = Vec::new();
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let id = parse_work_id_u64(&raw)
                .ok_or_else(|| anyhow!("works row has a non-W<digits> openalex_id: {raw:?}"))?;
            if ids.contains(&id) {
                matched += 1;
            } else {
                adopt.push(id);
            }
        }

        let absent_from_db = checkpointed.saturating_sub(matched);
        if absent_from_db > 0 {
            bail!(
                "seen-set checkpoint {} lists {absent_from_db} work IDs that have no row in \
                 `works` (checkpoint: {checkpointed} IDs, works rows found in it: {matched}). \
                 It was written ahead of the database, so loading on would skip those works \
                 for good. Move the file aside and re-run: the set is then rebuilt from \
                 `works`, and every partition without an 'ok' row in `_ingest_log` is loaded \
                 again.",
                path.display()
            );
        }
        if !adopt.is_empty() {
            tracing::warn!(
                adopted = adopt.len(),
                checkpointed,
                "works table is ahead of the seen-set checkpoint (crash window or missing \
                 checkpoint); adopting its IDs"
            );
            ids.extend(adopt);
        }
        tracing::info!(ids = ids.len(), path = %path.display(), "seen-set ready");
        Ok(Self {
            ids,
            path,
            checkpoint_every_n_partitions: DEFAULT_CHECKPOINT_EVERY_N,
            partitions_since_checkpoint: 0,
        })
    }

    /// Whether `id` already has a committed `works` row.
    pub fn contains(&self, id: u64) -> bool {
        self.ids.contains(&id)
    }

    /// Record IDs whose rows have been committed. Call only after the
    /// database transaction that inserted them returned success.
    pub fn commit(&mut self, committed: HashSet<u64>) {
        self.ids.extend(committed);
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
    /// Crash-safe: the bytes go to `<path>.tmp`, are fsynced, and only then
    /// renamed over the live file, so a crash leaves either the old or the
    /// new checkpoint, never a torn one. The parent directory is fsynced so
    /// the rename itself survives a power loss.
    pub fn force_checkpoint(&self) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        let parent = match self.path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
        let f = File::create(&tmp)
            .with_context(|| format!("create checkpoint at {}", tmp.display()))?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        for id in &self.ids {
            w.write_all(&id.to_le_bytes())
                .context("write seen-set id")?;
        }
        let f = w
            .into_inner()
            .map_err(|e| anyhow!("flush seen-set checkpoint: {e}"))?;
        f.sync_all().context("fsync seen-set checkpoint")?;
        drop(f);
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), self.path.display()))?;
        File::open(parent)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("fsync directory {}", parent.display()))?;
        tracing::info!(ids = self.ids.len(), path = %self.path.display(), "seen-set checkpointed");
        Ok(())
    }
}

/// Read a checkpoint file: a stream of little-endian u64s, no framing. A
/// missing file is an empty set; anything else that is not a whole number of
/// records (a torn write) is an error.
fn read_checkpoint(path: &Path) -> Result<HashSet<u64>> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(e) => return Err(anyhow!("open {}: {e}", path.display())),
    };
    let len = f
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if len % 8 != 0 {
        bail!(
            "seen-set checkpoint {} is {len} bytes, not a whole number of 8-byte IDs \
             (torn write?); move it aside to rebuild the set from `works`",
            path.display()
        );
    }
    let count = (len / 8) as usize;
    let mut ids = HashSet::with_capacity(count);
    let mut reader = BufReader::with_capacity(1 << 20, f);
    let mut buf = [0u8; 8];
    for _ in 0..count {
        reader
            .read_exact(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        ids.insert(u64::from_le_bytes(buf));
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SCHEMA_DDL;

    fn db_with_works(ids: &[u64]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_DDL).unwrap();
        for id in ids {
            conn.execute(
                "INSERT INTO works(openalex_id) VALUES (?)",
                duckdb::params![format!("W{id}")],
            )
            .unwrap();
        }
        conn
    }

    fn open_empty(dir: &Path) -> SeenSet {
        SeenSet::open(&db_with_works(&[]), dir.join("seen.bin")).unwrap()
    }

    fn set(ids: &[u64]) -> HashSet<u64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn committed_ids_are_seen_and_uncommitted_ids_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open_empty(dir.path());
        assert!(!s.contains(123));
        s.commit(set(&[123, 456]));
        assert!(s.contains(123) && s.contains(456));
        assert!(!s.contains(789));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn checkpoint_roundtrip_preserves_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seen.bin");
        let mut s1 = open_empty(dir.path());
        s1.commit(set(&[100, 200, 300]));
        s1.force_checkpoint().unwrap();
        assert!(
            !path.with_extension("tmp").exists(),
            "temp file is renamed away"
        );

        let s2 = SeenSet::open(&db_with_works(&[100, 200, 300]), path).unwrap();
        assert_eq!(s2.len(), 3);
        for id in [100, 200, 300] {
            assert!(s2.contains(id));
        }
        assert!(!s2.contains(400));
    }

    #[test]
    fn maybe_checkpoint_flushes_at_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seen.bin");
        let mut s = open_empty(dir.path());
        s.checkpoint_every_n_partitions = 3;
        s.commit(set(&[1]));
        s.maybe_checkpoint().unwrap();
        assert!(!path.exists(), "no checkpoint after partition 1");
        s.commit(set(&[2]));
        s.maybe_checkpoint().unwrap();
        assert!(!path.exists(), "no checkpoint after partition 2");
        s.commit(set(&[3]));
        s.maybe_checkpoint().unwrap();
        assert!(path.exists(), "checkpoint after partition 3");
    }

    #[test]
    fn open_adopts_works_rows_missing_from_the_checkpoint() {
        // Crash window: rows committed, checkpoint not yet rewritten.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seen.bin");
        let mut old = open_empty(dir.path());
        old.commit(set(&[1, 2]));
        old.force_checkpoint().unwrap();

        let s = SeenSet::open(&db_with_works(&[1, 2, 3, 4]), path).unwrap();
        assert_eq!(s.len(), 4);
        assert!(s.contains(3) && s.contains(4));
    }

    #[test]
    fn open_without_a_checkpoint_rebuilds_from_works() {
        let dir = tempfile::tempdir().unwrap();
        let s = SeenSet::open(&db_with_works(&[7, 8, 9]), dir.path().join("seen.bin")).unwrap();
        assert_eq!(s.len(), 3);
        assert!(s.contains(8));
    }

    #[test]
    fn open_rejects_a_checkpoint_that_is_ahead_of_works() {
        // The pre-fix loader marked IDs before their INSERT: W1/W2 are in
        // the checkpoint, only W1 ever reached `works`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seen.bin");
        let mut poisoned = open_empty(dir.path());
        poisoned.commit(set(&[1, 2]));
        poisoned.force_checkpoint().unwrap();

        assert!(
            SeenSet::open(&db_with_works(&[1]), path.clone()).is_err(),
            "a checkpoint with an uncommitted ID must be refused"
        );
        assert!(
            path.exists(),
            "the refused file is left in place for the operator"
        );
    }

    #[test]
    fn open_rejects_a_torn_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seen.bin");
        std::fs::write(&path, [1u8, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0]).unwrap();
        assert!(
            SeenSet::open(&db_with_works(&[]), path).is_err(),
            "trailing partial bytes are not an ID"
        );
    }

    #[test]
    fn checkpoint_fails_when_the_directory_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        let s = SeenSet::open(&db_with_works(&[]), sub.join("seen.bin")).unwrap();
        std::fs::write(&sub, "a file where the directory should be").unwrap();
        assert!(s.force_checkpoint().is_err());
    }
}
