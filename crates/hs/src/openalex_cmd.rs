//! `hs openalex …` — local OpenAlex catalog management.
//!
//! Commands:
//!   load           Bulk-load a snapshot entity into DuckDB.
//!   load-works     Bulk-load works partitions via streaming pre-dedupe.
//!                  Walks partitions newest-first; a SeenSet of seen
//!                  integer work-IDs gates each row so the live tables
//!                  hold their PRIMARY KEY invariants without ON CONFLICT.
//!   status         Print row counts per table and recent ingest log entries.
//!   query          Run an ad-hoc read-only SQL query and print rows as JSON.
//!   build-fts      Build the BM25 FTS index over works.title + abstract_text.
//!   build-indexes  Build the post-load secondary indexes.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use openalex_ingest::reader::list_partitions;
use openalex_ingest::{OpenAlexDb, SimpleEntity};

#[derive(Subcommand, Debug)]
pub enum OpenAlexCmd {
    /// Load a "simple" entity (concepts, topics, authors, sources, …) via DuckDB JSONL ingest.
    Load {
        /// Entity name. One of: concepts, topics, domains, fields, subfields,
        /// sources, institutions, funders, publishers, authors.
        entity: String,
    },
    /// Load works partitions via streaming pre-dedupe (parses Rust-side,
    /// fans out to 5 appenders, gates each row through a SeenSet).
    LoadWorks {
        /// Optional partition name (e.g. updated_date=2024-01-01) to load
        /// just one. The SeenSet is bootstrapped from the live `works`
        /// table so existing rows still dedupe correctly.
        #[arg(long)]
        partition: Option<String>,
    },
    /// Print row counts per table.
    Status,
    /// Run an ad-hoc read-only SELECT and print row tuples.
    Query { sql: String },
    /// Build the BM25 FTS index over works.title + abstract_text.
    BuildFts,
    /// Build the post-load secondary indexes.
    BuildIndexes,
}

pub async fn dispatch(cmd: OpenAlexCmd) -> Result<()> {
    let cfg = load_config()?;
    let db = OpenAlexDb::open(&cfg.db_path)?;

    match cmd {
        OpenAlexCmd::Load { entity } => {
            // `authors` goes through the Rust Appender path because the SQL
            // path OOMs DuckDB on the giant 02-01 partition (176 GB).
            let stats = if entity == "authors" {
                db.load_authors(&cfg.snapshot_dir)?
            } else {
                let e = SimpleEntity::parse(&entity)?;
                db.load_simple_entity(e, &cfg.snapshot_dir)?
            };
            println!(
                "{}: {} partitions loaded, {} skipped, {} rows inserted",
                entity, stats.partitions_loaded, stats.skipped_partitions, stats.rows_inserted
            );
        }
        OpenAlexCmd::LoadWorks { partition } => {
            let seen_path = seen_set_path(&cfg.db_path);
            let works_dir = cfg.snapshot_dir.join("works");
            if let Some(part_name) = partition {
                // Single-partition path (testing/debug). Bootstrap the
                // SeenSet from the live `works` table so we don't re-emit
                // rows that previous runs already loaded; otherwise PK
                // violations would surface on the bulk INSERT.
                let part_path = works_dir.join(&part_name);
                if !part_path.is_dir() {
                    return Err(anyhow!("partition not found: {}", part_path.display()));
                }
                println!("bootstrapping seen-set from existing works table...");
                let mut seen =
                    openalex_ingest::SeenSet::rebuild_from_db(db.raw(), seen_path.clone())?;
                let s = db.load_works_partition(&part_path, &mut seen)?;
                seen.force_checkpoint()?;
                println!(
                    "works/{}: {} rows inserted (skipped_partition={})",
                    part_name, s.rows_inserted, s.skipped_partitions
                );
            } else {
                // Full-corpus walk: load_works walks newest-first and
                // manages SeenSet rehydrate / checkpoint / final flush
                // internally.
                let parts = list_partitions(&works_dir)?;
                println!(
                    "loading {} works partitions newest-first (streaming pre-dedupe)...",
                    parts.len()
                );
                let s = db.load_works(&cfg.snapshot_dir, seen_path)?;
                println!(
                    "works: {} partitions loaded, {} skipped, {} rows inserted",
                    s.partitions_loaded, s.skipped_partitions, s.rows_inserted
                );
            }
        }
        OpenAlexCmd::Status => {
            for (table, n) in db.row_counts()? {
                println!("  {:>20}: {}", table, n);
            }
        }
        OpenAlexCmd::Query { sql } => {
            let conn = db.raw();
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query([])?;
            // duckdb-rs requires .query() before column metadata is valid.
            let col_names: Vec<String> = rows
                .as_ref()
                .map(|s| s.column_names().into_iter().collect())
                .unwrap_or_default();
            let mut count = 0u64;
            while let Some(row) = rows.next()? {
                let mut out = serde_json::Map::with_capacity(col_names.len());
                for (idx, name) in col_names.iter().enumerate() {
                    let v: duckdb::types::Value = row.get(idx)?;
                    out.insert(name.clone(), value_to_json(v));
                }
                println!("{}", serde_json::to_string(&out)?);
                count += 1;
            }
            eprintln!("({} rows)", count);
        }
        OpenAlexCmd::BuildFts => {
            db.build_fts()?;
            println!("FTS index built.");
        }
        OpenAlexCmd::BuildIndexes => {
            db.build_post_load_indexes()?;
            println!("Post-load indexes built.");
        }
    }

    Ok(())
}

struct Config {
    db_path: PathBuf,
    snapshot_dir: PathBuf,
}

/// Resolve `db_path` and `snapshot_dir` from `~/.home-still/config.yaml` under
/// the `openalex:` section. Fail loud if either is missing — ONE PATH.
fn load_config() -> Result<Config> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no $HOME"))?;
    let path = home.join(".home-still").join("config.yaml");
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&raw)?;
    let oa = v
        .get("openalex")
        .ok_or_else(|| anyhow!("missing 'openalex:' section in {}", path.display()))?;
    let db_path = oa
        .get("db_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("openalex.db_path missing"))?;
    let snapshot_dir = oa
        .get("snapshot_dir")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("openalex.snapshot_dir missing"))?;
    Ok(Config {
        db_path: expand_tilde(db_path),
        snapshot_dir: expand_tilde(snapshot_dir),
    })
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

/// SeenSet checkpoint path: lives next to the DuckDB file so wiping the
/// data dir wipes both atomically.
fn seen_set_path(db_path: &std::path::Path) -> PathBuf {
    db_path
        .parent()
        .map(|p| p.join("seen_set.bin"))
        .unwrap_or_else(|| PathBuf::from("/tmp/seen_set.bin"))
}

fn value_to_json(v: duckdb::types::Value) -> serde_json::Value {
    use duckdb::types::Value as V;
    use serde_json::Value as J;
    match v {
        V::Null => J::Null,
        V::Boolean(b) => J::Bool(b),
        V::TinyInt(n) => J::from(n),
        V::SmallInt(n) => J::from(n),
        V::Int(n) => J::from(n),
        V::BigInt(n) => J::from(n),
        V::HugeInt(n) => J::from(n.to_string()),
        V::UTinyInt(n) => J::from(n),
        V::USmallInt(n) => J::from(n),
        V::UInt(n) => J::from(n),
        V::UBigInt(n) => J::from(n),
        V::Float(f) => serde_json::Number::from_f64(f as f64)
            .map(J::Number)
            .unwrap_or(J::Null),
        V::Double(f) => serde_json::Number::from_f64(f)
            .map(J::Number)
            .unwrap_or(J::Null),
        V::Text(s) => J::String(s),
        V::Blob(b) => J::String(format!("<{} bytes>", b.len())),
        other => J::String(format!("{:?}", other)),
    }
}
