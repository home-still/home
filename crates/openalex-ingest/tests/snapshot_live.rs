//! Live-snapshot parse checks. Ignored by default — run with:
//!   cargo test -p openalex-ingest -- --ignored
//!
//! Confirms the serde structs survive contact with real OpenAlex JSONL on
//! `big`. If a field shape drifts in a future snapshot release, this test
//! fails fast and tells us where.

use openalex_ingest::{model, reader};
use std::path::PathBuf;

const SNAPSHOT: &str = "/home/ladvien/data/academic_papers/openalex-snapshot/data";

fn first_partition(entity: &str) -> PathBuf {
    let entity_dir = PathBuf::from(SNAPSHOT).join(entity);
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
    let stats = reader::read_partition::<model::Concept, _>(&first_partition("concepts"), |c| {
        assert!(c.id.starts_with("https://openalex.org/C"));
        count += 1;
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
    let stats = reader::read_partition::<model::Topic, _>(&first_partition("topics"), |t| {
        assert!(t.id.starts_with("https://openalex.org/T"));
        count += 1;
    })
    .unwrap();
    assert!(count > 0);
    assert_eq!(stats.parse_errors, 0);
    eprintln!("topics: {} parsed", stats.parsed);
}

#[test]
#[ignore]
fn parses_first_works_partition() {
    // Works partitions are big — bail after first 1000 records.
    let mut count = 0u32;
    let stats = reader::read_partition::<model::Work, _>(&first_partition("works"), |w| {
        if count < 5 {
            assert!(w.id.starts_with("https://openalex.org/W"));
        }
        count += 1;
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
