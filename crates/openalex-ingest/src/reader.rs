//! JSONL partition iteration.
//!
//! OpenAlex snapshot layout:
//! ```text
//! data/<entity>/updated_date=YYYY-MM-DD/part_NNNN.jsonl[.gz]
//! ```
//! `decompress_snapshot.py` already unpacks `.gz` files in place, so the common
//! case is plain `.jsonl`. We still handle `.gz` so this works on a fresh
//! snapshot where decompression hasn't happened yet.
//!
//! Failure policy: a record that does not parse is counted against an explicit
//! `max_parse_errors` budget (the snapshot has the occasional malformed line;
//! a drifted schema must not be read as "a few bad lines"), and the read
//! fails once the budget is exceeded. Anything else — an unreadable directory
//! entry, an IO error mid-file, a truncated `.gz` stream, a callback error —
//! aborts the read immediately and is returned.

use anyhow::{bail, Context, Result};
use flate2::read::MultiGzDecoder;
use serde::de::DeserializeOwned;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

const READ_BUF: usize = 256 * 1024;

/// List `updated_date=*` partition directories under an entity dir, sorted.
pub fn list_partitions(entity_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(entity_dir)
        .with_context(|| format!("read_dir {}", entity_dir.display()))?
    {
        let path = entry
            .with_context(|| format!("read_dir entry under {}", entity_dir.display()))?
            .path();
        let is_partition = path.is_dir()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("updated_date="))
                .unwrap_or(false);
        if is_partition {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// The data files of one partition, sorted: regular files whose extension is
/// exactly `jsonl` or `gz`. The strict extension match keeps rclone's
/// partial-upload leftovers (`part_0049.gz.19B6a7d8`) out of the load; they
/// are not data and reading them as JSONL logs millions of garbage lines.
///
/// A partition with no data file is an error, not an empty partition:
/// logging it as loaded would silently drop everything it was supposed to
/// contain (an interrupted download that left only partial uploads looks
/// exactly like this).
pub fn partition_files(partition_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(partition_dir)
        .with_context(|| format!("read_dir {}", partition_dir.display()))?
    {
        let path = entry
            .with_context(|| format!("read_dir entry under {}", partition_dir.display()))?
            .path();
        let is_data = path.is_file()
            && matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("jsonl") | Some("gz")
            );
        if is_data {
            files.push(path);
        }
    }
    if files.is_empty() {
        bail!(
            "partition {} contains no .jsonl/.gz data file (incomplete download?)",
            partition_dir.display()
        );
    }
    files.sort();
    Ok(files)
}

/// Read every data file in a partition directory (see [`partition_files`]),
/// calling `on_record` for each successfully-parsed record. `max_parse_errors`
/// is the budget for the whole partition: the read fails as soon as more
/// records than that have failed to parse.
pub fn read_partition<T, F>(
    partition_dir: &Path,
    max_parse_errors: u64,
    mut on_record: F,
) -> Result<PartitionStats>
where
    T: DeserializeOwned,
    F: FnMut(T) -> Result<()>,
{
    let mut stats = PartitionStats::default();
    for f in partition_files(partition_dir)? {
        let remaining = max_parse_errors.saturating_sub(stats.parse_errors);
        let file_stats = read_jsonl_file(&f, remaining, &mut on_record)?;
        stats.total_lines += file_stats.total_lines;
        stats.parsed += file_stats.parsed;
        stats.parse_errors += file_stats.parse_errors;
    }
    Ok(stats)
}

/// Read a single `.jsonl` or `.jsonl.gz` file line by line.
///
/// Lines are read as bytes and handed to `serde_json::from_slice`, so a line
/// that is not valid UTF-8 is an ordinary parse error (the bytes are consumed
/// and the read moves on). A failing *read* is different: an IO error or a
/// truncated/corrupt gzip stream repeats on every retry, so it is returned
/// instead of being counted and retried.
///
/// Fails once more than `max_parse_errors` lines have failed to parse.
pub fn read_jsonl_file<T, F>(
    path: &Path,
    max_parse_errors: u64,
    on_record: &mut F,
) -> Result<PartitionStats>
where
    T: DeserializeOwned,
    F: FnMut(T) -> Result<()>,
{
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader: Box<dyn Read> = if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        Box::new(MultiGzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut buf = BufReader::with_capacity(READ_BUF, reader);

    let mut stats = PartitionStats::default();
    let mut line = Vec::new();
    let mut lineno = 0u64;
    loop {
        line.clear();
        let n = buf
            .read_until(b'\n', &mut line)
            .with_context(|| format!("read {} after line {}", path.display(), lineno))?;
        if n == 0 {
            break;
        }
        lineno += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        stats.total_lines += 1;
        match serde_json::from_slice::<T>(&line) {
            Ok(rec) => {
                on_record(rec).with_context(|| format!("{} line {}", path.display(), lineno))?;
                stats.parsed += 1;
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), lineno, error = %e, "parse error");
                stats.parse_errors += 1;
                if stats.parse_errors > max_parse_errors {
                    bail!(
                        "{}: more than {} records failed to parse (latest: line {}: {})",
                        path.display(),
                        max_parse_errors,
                        lineno,
                        e
                    );
                }
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
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    use std::time::Duration;

    const C1: &str = r#"{"id":"https://openalex.org/C1","display_name":"Foo","level":0,"works_count":1,"cited_by_count":2}"#;
    const C2: &str = r#"{"id":"https://openalex.org/C2","display_name":"Bar","level":1,"works_count":3,"cited_by_count":4}"#;

    #[test]
    fn reads_jsonl_concepts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part_0000.jsonl");
        std::fs::write(&path, format!("{C1}\n{C2}\nthis-is-not-json\n")).unwrap();

        let mut got: Vec<Concept> = Vec::new();
        let stats = read_jsonl_file::<Concept, _>(&path, 1, &mut |c| {
            got.push(c);
            Ok(())
        })
        .unwrap();
        assert_eq!(stats.total_lines, 3);
        assert_eq!(stats.parsed, 2);
        assert_eq!(stats.parse_errors, 1);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "https://openalex.org/C1");
        assert_eq!(got[1].display_name.as_deref(), Some("Bar"));
    }

    #[test]
    fn invalid_utf8_line_is_one_parse_error_and_reading_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part_0000.jsonl");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(C1.as_bytes());
        bytes.extend_from_slice(b"\n\xff\xfe not utf-8 \xc3\n");
        bytes.extend_from_slice(C2.as_bytes());
        bytes.push(b'\n');
        std::fs::write(&path, bytes).unwrap();

        let mut ids = Vec::new();
        let stats = read_jsonl_file::<Concept, _>(&path, 5, &mut |c| {
            ids.push(c.id);
            Ok(())
        })
        .unwrap();
        assert_eq!(stats.parse_errors, 1);
        assert_eq!(stats.parsed, 2);
        assert_eq!(ids, ["https://openalex.org/C1", "https://openalex.org/C2"]);
    }

    #[test]
    fn parse_error_budget_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part_0000.jsonl");
        std::fs::write(&path, format!("{C1}\nbad one\nbad two\n{C2}\n")).unwrap();

        let ok = read_jsonl_file::<Concept, _>(&path, 2, &mut |_| Ok(())).unwrap();
        assert_eq!(ok.parse_errors, 2, "exactly at the budget is tolerated");

        let err = read_jsonl_file::<Concept, _>(&path, 1, &mut |_| Ok(())).unwrap_err();
        assert!(
            format!("{err:#}").contains("failed to parse"),
            "one past the budget must fail: {err:#}"
        );
    }

    #[test]
    fn callback_error_aborts_the_read_with_its_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part_0000.jsonl");
        std::fs::write(&path, format!("{C1}\n{C2}\n")).unwrap();

        let mut seen = 0;
        let err = read_jsonl_file::<Concept, _>(&path, 0, &mut |_| {
            seen += 1;
            anyhow::bail!("sink is full")
        })
        .unwrap_err();
        assert_eq!(seen, 1, "no record after the failing one is delivered");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("line 1") && msg.contains("sink is full"),
            "{msg}"
        );
    }

    /// RA-112 (unverified in the audit): the old loop `continue`d on every
    /// line-level IO error, and a truncated gzip stream errors on every
    /// retry. Runs the read on a worker thread so a regression shows up as a
    /// timeout instead of hanging the suite.
    #[test]
    fn truncated_gz_is_an_error_not_an_endless_retry() {
        let dir = tempfile::tempdir().unwrap();
        let full = dir.path().join("full.gz");
        let mut enc = GzEncoder::new(File::create(&full).unwrap(), Compression::default());
        for i in 0..20_000 {
            writeln!(enc, r#"{{"id":"https://openalex.org/C{i}"}}"#).unwrap();
        }
        enc.finish().unwrap();
        let bytes = std::fs::read(&full).unwrap();
        let trunc = dir.path().join("trunc.gz");
        std::fs::write(&trunc, &bytes[..bytes.len() / 2]).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = read_jsonl_file::<Concept, _>(&trunc, u64::MAX, &mut |_| Ok(()));
            let _ = tx.send(r.map(|s| s.parsed).map_err(|e| format!("{e:#}")));
        });
        let outcome = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("read of a truncated gz must terminate");
        let msg = outcome.expect_err("a truncated gz must be an error, not a short success");
        assert!(msg.contains("trunc.gz"), "error names the file: {msg}");
    }

    #[test]
    fn complete_gz_reads_like_plain_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part_0000.gz");
        let mut enc = GzEncoder::new(File::create(&path).unwrap(), Compression::default());
        writeln!(enc, "{C1}").unwrap();
        writeln!(enc, "{C2}").unwrap();
        enc.finish().unwrap();
        let stats = read_jsonl_file::<Concept, _>(&path, 0, &mut |_| Ok(())).unwrap();
        assert_eq!((stats.parsed, stats.parse_errors), (2, 0));
    }

    /// `cat a.gz b.gz` is a valid gzip file of two members; a first-member-
    /// only decoder would stop after `a` and report a clean, short read.
    #[test]
    fn concatenated_gz_members_are_all_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part_0000.gz");
        let mut bytes = Vec::new();
        for line in [C1, C2] {
            let mut enc = GzEncoder::new(Vec::new(), Compression::default());
            writeln!(enc, "{line}").unwrap();
            bytes.extend(enc.finish().unwrap());
        }
        std::fs::write(&path, bytes).unwrap();
        let stats = read_jsonl_file::<Concept, _>(&path, 0, &mut |_| Ok(())).unwrap();
        assert_eq!((stats.parsed, stats.parse_errors), (2, 0));
    }

    #[test]
    fn partition_files_skip_partial_uploads_and_reject_an_empty_partition() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "part_0001.jsonl",
            "part_0000.jsonl",
            "part_0002.gz",
            "part_0049.gz.19B6a7d8",
            "notes.txt",
        ] {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        std::fs::create_dir(dir.path().join("sub.jsonl")).unwrap();
        let names: Vec<String> = partition_files(dir.path())
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["part_0000.jsonl", "part_0001.jsonl", "part_0002.gz"]
        );

        let only_partials = tempfile::tempdir().unwrap();
        std::fs::write(only_partials.path().join("part_0000.gz.19B6a7d8"), "x").unwrap();
        let err = partition_files(only_partials.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("no .jsonl/.gz data file"),
            "{err:#}"
        );
    }

    #[test]
    fn list_partitions_returns_only_sorted_updated_date_dirs() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "updated_date=2024-02-01",
            "updated_date=2023-12-31",
            "manifest",
        ] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        std::fs::write(dir.path().join("updated_date=2025-01-01"), "a file").unwrap();
        let got: Vec<String> = list_partitions(dir.path())
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(got, ["updated_date=2023-12-31", "updated_date=2024-02-01"]);
    }
}
