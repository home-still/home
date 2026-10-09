//! `hs openalex …` — local OpenAlex catalog management.
//!
//! Commands, in the order a catalog is built:
//!   load           Bulk-load a snapshot entity into DuckDB. Every entity
//!                  (dimensions and authors included) follows the works rule:
//!                  partitions are walked newest-first, a seen-set of integer
//!                  IDs gates each record, and the newest copy wins.
//!   load-works     Bulk-load works partitions via streaming pre-dedupe.
//!                  Walks partitions newest-first; a SeenSet of seen
//!                  integer work-IDs gates each row so the live tables
//!                  hold their PRIMARY KEY invariants without ON CONFLICT.
//!                  Exits non-zero on the first failed partition.
//!   build-indexes  Build the post-load secondary indexes.
//!   install-fts    Download + load the DuckDB `fts` extension (needs network
//!                  once per host and DuckDB version; touches no database).
//!   build-fts      Build the BM25 FTS index over works.title + abstract_text.
//!                  Only LOADs `fts`: run `install-fts` first.
//! Read-only:
//!   status         Print row counts per table and the ingest-log entries
//!                  that failed or skipped records.
//!   query          Run an ad-hoc SQL query and print rows as JSON.
//!
//! `status` and `query` open the database read-only: they never create it and
//! can run next to hs-mcp's read-only handle. `install-fts` does not open the
//! database. Every other command is a writer and takes the file exclusively.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use openalex_ingest::reader::list_partitions;
use openalex_ingest::{
    LoadOptions, OpenAlexDb, SeenSet, SimpleEntity, DEFAULT_MAX_PARSE_ERRORS_PER_PARTITION,
};

#[derive(Subcommand, Debug)]
pub enum OpenAlexCmd {
    /// Load a "simple" entity (concepts, topics, authors, sources, …). Partitions
    /// are walked newest-first and the newest copy of each ID wins.
    Load {
        /// Entity name. One of: concepts, topics, domains, fields, subfields,
        /// sources, institutions, funders, publishers, authors.
        entity: String,
        /// authors only: fail a partition once more than this many of its
        /// records are unusable (unparseable, or without an A<digits> id;
        /// default 100). The SQL-ingested entities fail on the first
        /// malformed line or id and take no limit.
        #[arg(long)]
        max_parse_errors: Option<u64>,
    },
    /// Load works partitions via streaming pre-dedupe (parses Rust-side,
    /// fans out to 5 appenders, gates each row through a SeenSet). Fails on
    /// the first failed partition; re-run to resume.
    LoadWorks {
        /// Optional partition name (e.g. updated_date=2024-01-01) to load
        /// just one. The SeenSet is rebuilt from the live `works` table so
        /// existing rows still dedupe correctly.
        #[arg(long)]
        partition: Option<String>,
        /// Fail a partition once more than this many of its records are
        /// unusable (unparseable, or without a W<digits> id). Records within
        /// the limit are skipped and counted in `_ingest_log.parse_errors`.
        #[arg(long, default_value_t = DEFAULT_MAX_PARSE_ERRORS_PER_PARTITION)]
        max_parse_errors: u64,
    },
    /// Print row counts per table and any failed / lossy ingest-log entries.
    Status,
    /// Run an ad-hoc SELECT (read-only connection) and print row tuples.
    Query { sql: String },
    /// Build the BM25 FTS index over works.title + abstract_text (needs the
    /// extension: run `install-fts` first; this never downloads it).
    BuildFts,
    /// Build the post-load secondary indexes.
    BuildIndexes,
    /// Download and load the DuckDB `fts` extension that `build-fts` needs.
    /// Needs network access once per host and DuckDB version; opens no
    /// database.
    InstallFts,
}

impl OpenAlexCmd {
    /// The subcommands that open the database read-write.
    fn writes(&self) -> bool {
        matches!(
            self,
            Self::Load { .. } | Self::LoadWorks { .. } | Self::BuildFts | Self::BuildIndexes
        )
    }

    /// The subcommands refused on Windows: the writers, and `install-fts`
    /// (its only consumer, `build-fts`, is refused there, so installing the
    /// extension for it would be a step toward nothing).
    fn refused_on_windows(&self) -> bool {
        self.writes() || matches!(self, Self::InstallFts)
    }
}

pub async fn dispatch(cmd: OpenAlexCmd) -> Result<()> {
    let cfg = load_config()?;
    // Every subcommand is synchronous DuckDB work; keep it off the async workers.
    tokio::task::spawn_blocking(move || run(&cfg, cmd)).await?
}

fn run(cfg: &Config, cmd: OpenAlexCmd) -> Result<()> {
    // On Windows the next DuckDB call after a failed write never returns
    // (windows-2022 CI: a duplicate key or a failed merge, then a hang), so a
    // bad partition would wedge the loader instead of failing it (RA-149).
    if cfg!(windows) && cmd.refused_on_windows() {
        bail!(
            "`hs openalex` load and build commands are not supported on Windows: DuckDB hangs \
             after a failed write there (RA-149). Run OpenAlex ingest on Linux or macOS; \
             `status` and `query` work here"
        );
    }
    match cmd {
        OpenAlexCmd::Load {
            entity,
            max_parse_errors,
        } => {
            let db = OpenAlexDb::open(&cfg.db_path)?;
            // `authors` goes through the Rust Appender path because the SQL
            // path OOMs DuckDB on the giant 02-01 partition (176 GB).
            let stats = if entity == "authors" {
                let opts = LoadOptions {
                    max_parse_errors_per_partition: max_parse_errors
                        .unwrap_or(DEFAULT_MAX_PARSE_ERRORS_PER_PARTITION),
                };
                db.load_authors(&cfg.snapshot_dir, &opts)?
            } else {
                if max_parse_errors.is_some() {
                    bail!("--max-parse-errors only applies to `authors`");
                }
                let e = SimpleEntity::parse(&entity)?;
                db.load_simple_entity(e, &cfg.snapshot_dir)?
            };
            println!(
                "{}: {} partitions loaded, {} skipped, {} rows inserted, {} records skipped",
                entity,
                stats.partitions_loaded,
                stats.skipped_partitions,
                stats.rows_inserted,
                stats.parse_errors
            );
        }
        OpenAlexCmd::LoadWorks {
            partition,
            max_parse_errors,
        } => {
            let db = OpenAlexDb::open(&cfg.db_path)?;
            let opts = LoadOptions {
                max_parse_errors_per_partition: max_parse_errors,
            };
            load_works(&db, cfg, partition, &opts)?;
        }
        OpenAlexCmd::Status => {
            let db = OpenAlexDb::open_read_only(&cfg.db_path)?;
            for (table, n) in db.row_counts()? {
                println!("  {:>20}: {}", table, n);
            }
            let attention = db.ingest_log_attention()?;
            if !attention.is_empty() {
                println!("\ningest log entries that failed or skipped records:");
                for e in attention {
                    println!(
                        "  {:>10} {:<28} {:<6} rows={} skipped_records={}",
                        e.entity, e.partition, e.status, e.rows, e.parse_errors
                    );
                }
            }
        }
        OpenAlexCmd::Query { sql } => {
            let db = OpenAlexDb::open_read_only(&cfg.db_path)?;
            let conn = db.raw();
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query([])?;
            // duckdb-rs requires .query() before column metadata is valid.
            let col_names: Vec<String> = rows
                .as_ref()
                .map(|s| s.column_names().into_iter().collect())
                .ok_or_else(|| anyhow!("statement metadata unavailable after query"))?;
            // The row is a JSON object keyed by column name: a repeated name
            // would silently overwrite the earlier column's value.
            for (i, name) in col_names.iter().enumerate() {
                if col_names[..i].contains(name) {
                    bail!("the query returns the column name {name:?} twice; alias one of them");
                }
            }
            let mut count = 0u64;
            while let Some(row) = rows.next()? {
                let mut out = serde_json::Map::with_capacity(col_names.len());
                for (idx, name) in col_names.iter().enumerate() {
                    let v: duckdb::types::Value = row.get(idx)?;
                    out.insert(name.clone(), value_to_json(v)?);
                }
                println!("{}", serde_json::to_string(&out)?);
                count += 1;
            }
            eprintln!("({} rows)", count);
        }
        OpenAlexCmd::BuildFts => {
            OpenAlexDb::open(&cfg.db_path)?.build_fts()?;
            println!("FTS index built.");
        }
        OpenAlexCmd::BuildIndexes => {
            OpenAlexDb::open(&cfg.db_path)?.build_post_load_indexes()?;
            println!("Post-load indexes built.");
        }
        OpenAlexCmd::InstallFts => {
            OpenAlexDb::install_fts()?;
            println!("fts extension installed and loaded. Next: `hs openalex build-fts`.");
        }
    }

    Ok(())
}

/// `hs openalex load-works`. Returns `Err` (so the process exits non-zero) as
/// soon as a partition fails; the failed partition is stamped `error` in
/// `_ingest_log` by the loader.
fn load_works(
    db: &OpenAlexDb,
    cfg: &Config,
    partition: Option<String>,
    opts: &LoadOptions,
) -> Result<()> {
    let seen_path = seen_set_path(&cfg.db_path)?;
    let works_dir = cfg.snapshot_dir.join("works");
    let parts = list_partitions(&works_dir)?;
    if let Some(part_name) = partition {
        // Single-partition path (testing/debug). The SeenSet is rebuilt from
        // the live `works` table so we don't re-emit rows that previous runs
        // already loaded; otherwise PK violations would surface on the bulk
        // INSERT.
        let part_path = parts
            .iter()
            .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(part_name.as_str()))
            .ok_or_else(|| {
                anyhow!(
                    "partition not found: {}",
                    works_dir.join(&part_name).display()
                )
            })?;
        println!("bootstrapping seen-set from existing works table...");
        let mut seen = SeenSet::open(db.raw(), seen_path)?;
        let s = db.load_works_partition(part_path, &mut seen, opts)?;
        seen.force_checkpoint()?;
        println!(
            "works/{}: {} rows inserted, {} records skipped (skipped_partition={})",
            part_name, s.rows_inserted, s.parse_errors, s.skipped_partitions
        );
    } else {
        // Full-corpus walk: load_works walks newest-first and manages
        // SeenSet reconcile / checkpoint / final flush internally.
        println!(
            "loading {} works partitions newest-first (streaming pre-dedupe)...",
            parts.len()
        );
        let s = db.load_works(&cfg.snapshot_dir, seen_path, opts)?;
        println!(
            "works: {} partitions loaded, {} skipped, {} rows inserted, {} records skipped",
            s.partitions_loaded, s.skipped_partitions, s.rows_inserted, s.parse_errors
        );
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
fn seen_set_path(db_path: &Path) -> Result<PathBuf> {
    db_path
        .parent()
        .map(|p| p.join("seen_set.bin"))
        .ok_or_else(|| {
            anyhow!(
                "openalex.db_path {} has no parent directory",
                db_path.display()
            )
        })
}

/// One DuckDB value as JSON for `hs openalex query`. Every variant has a
/// faithful rendering (dates and timestamps as ISO text, lists/structs/maps
/// as JSON arrays/objects, floats at their own precision); a value that
/// cannot be rendered is an error, never a placeholder or a `null`.
fn value_to_json(v: duckdb::types::Value) -> Result<serde_json::Value> {
    use duckdb::types::Value as V;
    use serde_json::Value as J;
    Ok(match v {
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
        // A REAL column is f32: widening it to f64 would print 0.4 as
        // 0.4000000059604645, so go through its shortest decimal form.
        V::Float(f) => float_json(f.to_string().parse::<f64>()?, &f.to_string()),
        V::Double(f) => float_json(f, &f.to_string()),
        V::Decimal(d) => J::String(d.to_string()),
        V::Text(s) | V::Enum(s) => J::String(s),
        V::Blob(b) => J::String(format!("<{} bytes>", b.len())),
        V::Date32(days) => {
            let date = days
                .checked_add(719_163) // days from 0001-01-01 to 1970-01-01
                .and_then(chrono::NaiveDate::from_num_days_from_ce_opt)
                .ok_or_else(|| anyhow!("date {days} days from the Unix epoch is out of range"))?;
            J::String(date.to_string())
        }
        V::Timestamp(unit, ticks) => {
            let ts = chrono::DateTime::from_timestamp_micros(unit.to_micros(ticks))
                .ok_or_else(|| anyhow!("timestamp {ticks} ({unit:?}) is out of range"))?;
            J::String(ts.naive_utc().format("%Y-%m-%d %H:%M:%S%.f").to_string())
        }
        V::Time64(unit, ticks) => {
            let micros = unit.to_micros(ticks);
            let time = chrono::NaiveTime::from_num_seconds_from_midnight_opt(
                u32::try_from(micros.div_euclid(1_000_000))?,
                u32::try_from(micros.rem_euclid(1_000_000) * 1000)?,
            )
            .ok_or_else(|| anyhow!("time {ticks} ({unit:?}) is out of range"))?;
            J::String(time.format("%H:%M:%S%.f").to_string())
        }
        V::Interval {
            months,
            days,
            nanos,
        } => serde_json::json!({ "months": months, "days": days, "nanos": nanos }),
        V::List(items) | V::Array(items) => J::Array(
            items
                .into_iter()
                .map(value_to_json)
                .collect::<Result<Vec<_>>>()?,
        ),
        V::Struct(fields) => {
            let mut out = serde_json::Map::new();
            for (name, value) in fields.iter() {
                out.insert(name.clone(), value_to_json(value.clone())?);
            }
            J::Object(out)
        }
        V::Map(entries) => J::Array(
            entries
                .iter()
                .map(|(k, val)| {
                    Ok(serde_json::json!({
                        "key": value_to_json(k.clone())?,
                        "value": value_to_json(val.clone())?,
                    }))
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        V::Union(inner) => value_to_json(*inner)?,
    })
}

/// A float as a JSON number, or its text (`NaN`, `inf`) when JSON has no
/// number for it.
fn float_json(f: f64, text: &str) -> serde_json::Value {
    serde_json::Number::from_f64(f)
        .map(serde_json::Value::Number)
        .unwrap_or_else(|| serde_json::Value::String(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEW: &str = "updated_date=2024-02-01";

    fn work(id: u64, topic: &str) -> String {
        format!(
            r#"{{"id":"https://openalex.org/W{id}","title":"t{id}","publication_year":2020,"publication_date":"2020-01-01","topics":[{{"id":"https://openalex.org/{topic}","score":0.5}}]}}"#
        )
    }

    struct Env {
        _tmp: tempfile::TempDir,
        cfg: Config,
    }

    fn env_with_partition(lines: &[String]) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot_dir = tmp.path().join("snap");
        let part = snapshot_dir.join("works").join(NEW);
        std::fs::create_dir_all(&part).unwrap();
        std::fs::write(part.join("part_0000.jsonl"), lines.join("\n") + "\n").unwrap();
        let cfg = Config {
            db_path: tmp.path().join("data").join("oa.duckdb"),
            snapshot_dir,
        };
        Env { _tmp: tmp, cfg }
    }

    fn load_works_cmd(partition: Option<&str>) -> OpenAlexCmd {
        OpenAlexCmd::LoadWorks {
            partition: partition.map(str::to_string),
            max_parse_errors: DEFAULT_MAX_PARSE_ERRORS_PER_PARTITION,
        }
    }

    /// RA-35: `hs openalex load-works` must exit non-zero (main maps every
    /// `Err` from `dispatch` to a failure exit code) when a partition fails,
    /// and `status` must then show the partition as failed.
    #[cfg(not(windows))]
    #[test]
    fn load_works_fails_loudly_and_status_reports_the_failed_partition() {
        let env = env_with_partition(&[work(1, "T1")]);
        {
            // A stray edge row makes the merge of W1 fail inside the transaction.
            let db = OpenAlexDb::open(&env.cfg.db_path).unwrap();
            db.raw()
                .execute_batch("INSERT INTO work_topics VALUES ('W1','T1',0.1)")
                .unwrap();
        }
        assert!(run(&env.cfg, load_works_cmd(None)).is_err());
        assert!(run(&env.cfg, load_works_cmd(Some(NEW))).is_err());

        let db = OpenAlexDb::open_read_only(&env.cfg.db_path).unwrap();
        let attention = db.ingest_log_attention().unwrap();
        assert_eq!(attention.len(), 1);
        assert_eq!(
            (
                attention[0].partition.as_str(),
                attention[0].status.as_str()
            ),
            (NEW, "error")
        );
        let works: u64 = db
            .raw()
            .query_row("SELECT COUNT(*) FROM works", [], |r| r.get(0))
            .unwrap();
        assert_eq!(works, 0);
        run(&env.cfg, OpenAlexCmd::Status).expect("status works on a failed database");
    }

    #[cfg(not(windows))]
    #[test]
    fn load_works_succeeds_and_a_rerun_is_a_no_op() {
        let env = env_with_partition(&[work(1, "T1"), work(2, "T1")]);
        run(&env.cfg, load_works_cmd(None)).unwrap();
        run(&env.cfg, load_works_cmd(None)).unwrap();
        let db = OpenAlexDb::open_read_only(&env.cfg.db_path).unwrap();
        let works: u64 = db
            .raw()
            .query_row("SELECT COUNT(*) FROM works", [], |r| r.get(0))
            .unwrap();
        assert_eq!(works, 2);
        assert!(db.ingest_log_attention().unwrap().is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn single_partition_load_rejects_names_that_are_not_listed_partitions() {
        let env = env_with_partition(&[work(1, "T1")]);
        for bad in [
            "updated_date=1999-01-01",
            "../snap/works/updated_date=2024-02-01",
            "",
        ] {
            assert!(run(&env.cfg, load_works_cmd(Some(bad))).is_err(), "{bad:?}");
        }
    }

    /// RA-109: reading commands never create the database or its directory,
    /// and `query` cannot write.
    #[test]
    fn status_and_query_are_read_only_and_never_create_the_database() {
        let env = env_with_partition(&[]);
        assert!(run(&env.cfg, OpenAlexCmd::Status).is_err());
        assert!(run(
            &env.cfg,
            OpenAlexCmd::Query {
                sql: "SELECT 1".into()
            }
        )
        .is_err());
        assert!(!env.cfg.db_path.exists());
        assert!(!env.cfg.db_path.parent().unwrap().exists());

        OpenAlexDb::open(&env.cfg.db_path).unwrap();
        run(
            &env.cfg,
            OpenAlexCmd::Query {
                sql: "SELECT COUNT(*) FROM works".into(),
            },
        )
        .unwrap();
        for sql in [
            "CREATE TABLE t(x INT)",
            "INSERT INTO works(openalex_id) VALUES ('W1')",
            "DROP TABLE works",
        ] {
            assert!(
                run(&env.cfg, OpenAlexCmd::Query { sql: sql.into() }).is_err(),
                "{sql}"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn max_parse_errors_is_rejected_for_entities_that_take_no_limit() {
        let env = env_with_partition(&[]);
        let cmd = OpenAlexCmd::Load {
            entity: "concepts".into(),
            max_parse_errors: Some(5),
        };
        assert!(run(&env.cfg, cmd).is_err());
    }

    #[test]
    fn seen_set_path_has_no_tmp_fallback() {
        assert_eq!(
            seen_set_path(Path::new("/data/oa/oa.duckdb")).unwrap(),
            Path::new("/data/oa/seen_set.bin")
        );
        assert!(seen_set_path(Path::new("/")).is_err());
    }

    #[derive(clap::Parser, Debug)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: OpenAlexCmd,
    }

    /// `install-fts` is its own step in front of `build-fts`, and it is not a
    /// database writer: it must not create the catalog or take its lock. It is
    /// still refused on Windows, like the commands that need it.
    #[test]
    fn install_fts_is_a_subcommand_that_never_opens_the_database() {
        use clap::Parser;
        let install = Wrapper::try_parse_from(["openalex", "install-fts"])
            .unwrap()
            .cmd;
        assert!(matches!(install, OpenAlexCmd::InstallFts));
        assert!(install.refused_on_windows());
        assert!(!install.writes());
        let build = Wrapper::try_parse_from(["openalex", "build-fts"])
            .unwrap()
            .cmd;
        assert!(matches!(build, OpenAlexCmd::BuildFts));
        assert!(build.writes());
    }

    /// `query` prints what DuckDB holds: dates as dates, lists as arrays, a
    /// REAL at its own precision. (These used to print as `Date32(18262)`,
    /// `List([Int(1)])` and 0.4000000059604645.)
    #[test]
    fn query_values_render_as_their_json_form() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let v: duckdb::types::Value = conn
            .query_row(
                "SELECT {'date': DATE '2020-01-01', 'at': TIMESTAMP '2020-01-02 03:04:05', \
                 'list': [1, 2], 'score': 0.4::FLOAT, 'nan': 'NaN'::DOUBLE, \
                 'dec': 1.50::DECIMAL(4,2), 'map': MAP {'k': 1}}",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            value_to_json(v).unwrap(),
            serde_json::json!({
                "date": "2020-01-01",
                "at": "2020-01-02 03:04:05",
                "list": [1, 2],
                "score": 0.4,
                "nan": "NaN",
                "dec": "1.50",
                "map": [{"key": "k", "value": 1}],
            })
        );
    }

    /// A row is a JSON object keyed by column name; two columns of one name
    /// must be an error, not one silently overwriting the other.
    #[test]
    fn query_with_a_repeated_column_name_is_refused() {
        let env = env_with_partition(&[]);
        OpenAlexDb::open(&env.cfg.db_path).unwrap();
        let err = run(
            &env.cfg,
            OpenAlexCmd::Query {
                sql: "SELECT 1 AS id, 2 AS id".into(),
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("twice"), "{err:#}");
        run(
            &env.cfg,
            OpenAlexCmd::Query {
                sql: "SELECT 1 AS a, 2 AS b".into(),
            },
        )
        .unwrap();
    }

    #[cfg(not(windows))]
    fn concept(id: u64, name: &str) -> String {
        format!(
            r#"{{"id":"https://openalex.org/C{id}","display_name":"{name}","level":0,"description":"d","wikidata":"https://www.wikidata.org/wiki/Q{id}","works_count":1,"cited_by_count":2,"ancestors":[{{"id":"https://openalex.org/C9","display_name":"Root","level":0}}]}}"#
        )
    }

    /// `hs openalex load <dimension>` follows the works rule: the copy in the
    /// newest partition wins, an older copy is skipped without error.
    #[cfg(not(windows))]
    #[test]
    fn load_keeps_the_newest_copy_of_a_dimension_record() {
        let env = env_with_partition(&[]);
        for (part, lines) in [
            (NEW, vec![concept(1, "new")]),
            (
                "updated_date=2024-01-01",
                vec![concept(1, "old"), concept(2, "old")],
            ),
        ] {
            let dir = env.cfg.snapshot_dir.join("concepts").join(part);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("part_0000.jsonl"), lines.join("\n") + "\n").unwrap();
        }
        let load = OpenAlexCmd::Load {
            entity: "concepts".into(),
            max_parse_errors: None,
        };
        run(&env.cfg, load).unwrap();

        let db = OpenAlexDb::open_read_only(&env.cfg.db_path).unwrap();
        let rows: Vec<(String, String)> = db
            .raw()
            .prepare("SELECT openalex_id, display_name FROM concepts ORDER BY 1")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![
                ("C1".to_string(), "new".to_string()),
                ("C2".to_string(), "old".to_string())
            ]
        );
    }

    /// RA-149: every read-write subcommand is refused on Windows before the
    /// database is touched; `status` and `query` stay available.
    #[cfg(windows)]
    #[test]
    fn write_commands_are_refused_on_windows_without_touching_the_database() {
        let env = env_with_partition(&[work(1, "T1")]);
        let writes = [
            OpenAlexCmd::Load {
                entity: "concepts".into(),
                max_parse_errors: None,
            },
            load_works_cmd(None),
            OpenAlexCmd::BuildFts,
            OpenAlexCmd::BuildIndexes,
            OpenAlexCmd::InstallFts,
        ];
        for cmd in writes {
            let err = run(&env.cfg, cmd).unwrap_err();
            assert!(
                format!("{err:#}").contains("not supported on Windows"),
                "{err:#}"
            );
        }
        assert!(!env.cfg.db_path.exists());
    }
}
