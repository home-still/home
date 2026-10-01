//! Live-snapshot parse checks. Ignored by default — run with the snapshot's
//! `data` directory (the one holding `concepts/`, `topics/`, `works/`, …)
//! in `OPENALEX_SNAPSHOT_DIR`:
//!   OPENALEX_SNAPSHOT_DIR=/path/to/openalex-snapshot/data \
//!     cargo test -p openalex-ingest --test snapshot_live -- --ignored
//!
//! Confirms the serde structs survive contact with real OpenAlex JSONL. If a
//! field shape drifts in a future snapshot release, this test fails fast and
//! tells us where.

use openalex_ingest::{model, reader};
use std::path::PathBuf;

/// Read-only: the tests only parse; nothing here writes to the snapshot.
fn snapshot_dir() -> PathBuf {
    std::env::var_os("OPENALEX_SNAPSHOT_DIR")
        .map(PathBuf::from)
        .expect("set OPENALEX_SNAPSHOT_DIR to the OpenAlex snapshot `data` directory")
}

/// These tests count parse errors themselves; the reader's own budget is off.
const UNLIMITED: u64 = u64::MAX;

fn first_partition(entity: &str) -> PathBuf {
    let entity_dir = snapshot_dir().join(entity);
    reader::list_partitions(&entity_dir)
        .expect("list partitions")
        .into_iter()
        .next()
        .expect("at least one partition")
}

#[test]
#[ignore]
fn parses_concepts_partition() {
    let mut count = 0u32;
    let stats =
        reader::read_partition::<model::Concept, _>(&first_partition("concepts"), UNLIMITED, |c| {
            assert!(c.id.starts_with("https://openalex.org/C"));
            count += 1;
            Ok(())
        })
        .unwrap();
    assert!(count > 0, "got at least one concept");
    assert_eq!(stats.parse_errors, 0, "no parse errors expected");
    eprintln!(
        "concepts: {} parsed, {} lines",
        stats.parsed, stats.total_lines
    );
}

#[test]
#[ignore]
fn parses_topics_partition() {
    let mut count = 0u32;
    let stats =
        reader::read_partition::<model::Topic, _>(&first_partition("topics"), UNLIMITED, |t| {
            assert!(t.id.starts_with("https://openalex.org/T"));
            count += 1;
            Ok(())
        })
        .unwrap();
    assert!(count > 0);
    assert_eq!(stats.parse_errors, 0);
    eprintln!("topics: {} parsed", stats.parsed);
}

#[test]
#[ignore]
fn parses_first_works_partition() {
    let mut count = 0u32;
    let stats =
        reader::read_partition::<model::Work, _>(&first_partition("works"), UNLIMITED, |w| {
            if count < 5 {
                assert!(w.id.starts_with("https://openalex.org/W"));
            }
            count += 1;
            Ok(())
        })
        .unwrap();
    assert!(count > 0);
    // Allow some parse errors on works (occasional malformed lines have been
    // observed in past snapshots) but flag if it's anything dramatic.
    let err_rate = stats.parse_errors as f64 / stats.total_lines.max(1) as f64;
    assert!(
        err_rate < 0.001,
        "parse error rate {:.4} too high ({} errors / {} lines)",
        err_rate,
        stats.parse_errors,
        stats.total_lines
    );
    eprintln!(
        "works: {} parsed, {} errors, {} lines",
        stats.parsed, stats.parse_errors, stats.total_lines
    );
}
