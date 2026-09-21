//! JSONL partition iteration.
//!
//! OpenAlex snapshot layout:
//! ```text
//! data/<entity>/updated_date=YYYY-MM-DD/part_NNNN.jsonl[.gz]
//! ```
//! `decompress_snapshot.py` already unpacks `.gz` files in place, so the common
//! case is plain `.jsonl`. We still handle `.gz` so this works on a fresh
//! snapshot where decompression hasn't happened yet.

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use serde::de::DeserializeOwned;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

const READ_BUF: usize = 256 * 1024;

/// List `updated_date=*` partition directories under an entity dir, sorted.
pub fn list_partitions(entity_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(entity_dir)
        .with_context(|| format!("read_dir {}", entity_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("updated_date="))
                    .unwrap_or(false)
        })
        .collect();
    out.sort();
    Ok(out)
}

/// Read every JSONL/JSONL.gz file in a partition directory, calling `on_record`
/// for each successfully-parsed record. Parse errors are logged and skipped so
/// one bad line doesn't kill an entire partition.
pub fn read_partition<T, F>(partition_dir: &Path, mut on_record: F) -> Result<PartitionStats>
where
    T: DeserializeOwned,
    F: FnMut(T),
{
    let mut files: Vec<PathBuf> = std::fs::read_dir(partition_dir)
        .with_context(|| format!("read_dir {}", partition_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();

    let mut stats = PartitionStats::default();
    for f in files {
        let file_stats = read_jsonl_file(&f, &mut on_record)?;
        stats.total_lines += file_stats.total_lines;
        stats.parsed += file_stats.parsed;
        stats.parse_errors += file_stats.parse_errors;
    }
    Ok(stats)
}

/// Read a single `.jsonl` or `.jsonl.gz` file line by line.
pub fn read_jsonl_file<T, F>(path: &Path, on_record: &mut F) -> Result<PartitionStats>
where
    T: DeserializeOwned,
    F: FnMut(T),
{
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader: Box<dyn Read> = if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        Box::new(GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let buf = BufReader::with_capacity(READ_BUF, reader);

    let mut stats = PartitionStats::default();
    for (lineno, line) in buf.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(file = %path.display(), lineno, error = %e, "io error reading line");
                stats.parse_errors += 1;
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        stats.total_lines += 1;
        match serde_json::from_str::<T>(&line) {
            Ok(rec) => {
                on_record(rec);
                stats.parsed += 1;
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), lineno = lineno + 1, error = %e, "parse error");
                stats.parse_errors += 1;
            }
        }
    }
    Ok(stats)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PartitionStats {
    pub total_lines: u64,
    pub parsed: u64,
    pub parse_errors: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Concept;
    use std::io::Write;

    #[test]
    fn reads_jsonl_concepts() {
        let dir = tempdir_panicfree();
        let path = dir.join("part_0000.jsonl");
        let mut f = File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"id":"https://openalex.org/C1","display_name":"Foo","level":0,"works_count":1,"cited_by_count":2}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"id":"https://openalex.org/C2","display_name":"Bar","level":1,"works_count":3,"cited_by_count":4}}"#
        )
        .unwrap();
        writeln!(f, "this-is-not-json").unwrap();
        drop(f);

        let mut got: Vec<Concept> = Vec::new();
        let stats = read_jsonl_file::<Concept, _>(&path, &mut |c| got.push(c)).unwrap();
        assert_eq!(stats.total_lines, 3);
        assert_eq!(stats.parsed, 2);
        assert_eq!(stats.parse_errors, 1);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "https://openalex.org/C1");
        assert_eq!(got[1].display_name.as_deref(), Some("Bar"));
    }

    fn tempdir_panicfree() -> PathBuf {
        let p = std::env::temp_dir().join(format!("oa-ingest-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
