//! DuckDB connection management + entity loaders.
//!
//! Two ingest patterns:
//!
//! 1. **Simple entities** (concepts, topics, sources, institutions, authors,
//!    domains, fields, subfields, funders, publishers): DuckDB reads the
//!    JSONL files natively via `read_json` and projects / transforms columns
//!    inline. No Rust round-trip — DuckDB handles arrays/structs directly.
//!
//! 2. **Works**: streaming pre-dedupe. Walk partitions newest-first; for
//!    each row, gate on a shared `SeenSet` keyed on integer work-ID. First
//!    sighting → append to staging tables (5 Appenders); duplicate → skip.
//!    At end of file, plain bulk INSERT per live table. No `ON CONFLICT`
//!    is needed because the staging tables only ever contain first-sightings
//!    that don't already exist in the live tables. PRIMARY KEY constraints
//!    on works/work_topics/work_concepts/work_references are safe.
//!
//! Resumability: every partition load consults `_ingest_log` first and skips
//! if status='ok'. The `SeenSet` checkpoints to a sidecar file every N
//! partitions — see `seen_set.rs`. Partial progress within a partition is
//! NOT tracked — a crash mid-partition means re-doing that partition from
//! scratch (the Appender's pending rows are dropped on connection close,
//! so the partial state is invisible).

use anyhow::{anyhow, Context, Result};
use duckdb::{params, Connection};
use std::path::Path;

use crate::model::{Author, Work};
use crate::parser::{reconstruct_abstract, strip_doi, strip_openalex_id};
use crate::reader::{list_partitions, read_partition};
use crate::schema::{POST_LOAD_INDEXES, SCHEMA_DDL};
use crate::seen_set::SeenSet;

pub struct OpenAlexDb {
    conn: Connection,
}

impl OpenAlexDb {
    /// Open (creating if needed) the DuckDB file and ensure the schema is
    /// applied. Idempotent.
    ///
    /// Sets a conservative `memory_limit` (6 GB) and a co-located
    /// `temp_directory` so that loading huge JSONL partitions spills to disk
    /// rather than OOM-killing the process. `big` runs llama-server +
    /// hs-scribe-server simultaneously, so headroom is tight; raise the limit
    /// only after profiling.
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("open duckdb at {}", db_path.display()))?;
        let temp_dir = db_path
            .parent()
            .map(|p| p.join(".duckdb_tmp"))
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/duckdb_tmp"));
        std::fs::create_dir_all(&temp_dir).ok();
        // `preserve_insertion_order=false` is the bulk-load silver bullet:
        // without it, DuckDB buffers entire result sets to keep input ordering
        // visible, which OOMs on huge JSONL partitions. We don't care about
        // physical row order for an OLAP catalog.
        //
        // memory_limit set to 14 GB. At corpus scale (17M+ works → ~50M
        // edges with PK indexes), every ON CONFLICT bulk INSERT pins PK
        // index pages in the buffer pool. 12 GB hit `failed to pin block`
        // errors when re-processing partitions whose works already exist
        // (the recovery case after an OOM kill). 14 GB gives the index
        // pages enough headroom; combined with CHECKPOINT-between-
        // partitions and `--parallel 4 → threads=4`, the working set is
        // bounded.
        // Earlier 14 GB OOM-kill was caused by 2.9 GB of WARN-per-line
        // log spam (rclone partial-upload `.gz.<hex>` files being treated
        // as JSONL). That's now fixed at the source via the .jsonl
        // extension filter in load_works_partition_inner.
        let pragmas = format!(
            "PRAGMA memory_limit='24GB';\n\
             PRAGMA temp_directory='{}';\n\
             PRAGMA threads=4;\n\
             PRAGMA preserve_insertion_order=false;",
            temp_dir.display()
        );
        conn.execute_batch(&pragmas).context("apply pragmas")?;
        conn.execute_batch(SCHEMA_DDL).context("apply schema")?;
        Ok(Self { conn })
    }

    pub fn raw(&self) -> &Connection {
        &self.conn
    }

    /// Apply the post-load secondary indexes (minutes total at full corpus);
    /// call once after bulk load. Each statement runs with its own
    /// `execute_batch` + `CHECKPOINT` so the buffer pool is released between
    /// indexes — `works` has ~56M rows and three indexes on it, so building
    /// them in a single batch keeps tens of GB resident longer than needed.
    pub fn build_post_load_indexes(&self) -> Result<()> {
        for stmt in POST_LOAD_INDEXES
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            tracing::info!(target: "openalex_ingest", sql = %stmt, "building index");
            let started = std::time::Instant::now();
            self.conn
                .execute_batch(&format!("{stmt};"))
                .with_context(|| format!("build index: {stmt}"))?;
            let _ = self.conn.execute_batch("CHECKPOINT;");
            tracing::info!(
                target: "openalex_ingest",
                elapsed_ms = started.elapsed().as_millis() as u64,
                "index built"
            );
        }
        Ok(())
    }

    /// Build the BM25 full-text index over works.title + works.abstract_text,
    /// then write the `openalex_works` readiness sentinel into
    /// `_corpus_state`. The sentinel is the gate hs-mcp checks at startup to
    /// decide whether to expose the 5 `openalex_*` MCP tools — by writing it
    /// only AFTER the FTS index lands, we guarantee that if the tools are
    /// visible, every code path they exercise (search, get, references,
    /// citations, authors_by_topic) has the data it needs.
    ///
    /// Loads the FTS extension if not already present. Slow at full corpus
    /// (~hour+) — run once after works ingest.
    pub fn build_fts(&self) -> Result<()> {
        self.conn
            .execute_batch(
                r#"
                INSTALL fts;
                LOAD fts;
                PRAGMA create_fts_index('works', 'openalex_id', 'title', 'abstract_text', overwrite=1);
                INSERT INTO _corpus_state(component, ready_at, notes)
                VALUES ('openalex_works', CURRENT_TIMESTAMP, 'fts ready')
                ON CONFLICT (component) DO UPDATE
                  SET ready_at = excluded.ready_at,
                      notes    = excluded.notes;
                "#,
            )
            .context("build FTS index on works")
    }

    /// Query the ingest log for a partition; returns true if it's already done.
    fn partition_done(&self, entity: &str, partition: &str) -> Result<bool> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT status FROM _ingest_log WHERE entity = ? AND partition = ?")?;
        let mut rows = stmt.query(params![entity, partition])?;
        if let Some(row) = rows.next()? {
            let status: String = row.get(0)?;
            Ok(status == "ok")
        } else {
            Ok(false)
        }
    }

    fn log_partition(
        &self,
        entity: &str,
        partition: &str,
        status: &str,
        rows: u64,
        parse_errors: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO _ingest_log(entity, partition, status, rows, parse_errors) \
             VALUES (?, ?, ?, ?, ?)",
            params![entity, partition, status, rows, parse_errors],
        )?;
        Ok(())
    }

    /// Bulk-load a "simple" entity by letting DuckDB read the JSONL natively.
    /// `entity` is the snapshot subdir name (e.g. "concepts"). Each partition
    /// is a separate INSERT so resumability is per-partition.
    pub fn load_simple_entity(
        &self,
        entity: SimpleEntity,
        snapshot_root: &Path,
    ) -> Result<EntityStats> {
        let entity_dir = snapshot_root.join(entity.snapshot_dir());
        let partitions = list_partitions(&entity_dir)?;
        let mut stats = EntityStats::default();
        for part in partitions {
            let part_name = part
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow!("partition name not utf-8: {}", part.display()))?
                .to_string();
            if self.partition_done(entity.table_name(), &part_name)? {
                stats.skipped_partitions += 1;
                continue;
            }
            let glob = format!("{}/*.jsonl", part.display());
            let sql = entity.insert_sql(&glob);
            let count_before: u64 = self.conn.query_row(
                &format!("SELECT COUNT(*) FROM {}", entity.table_name()),
                [],
                |r| r.get(0),
            )?;
            self.conn.execute_batch(&sql).with_context(|| {
                format!("insert {} partition {}", entity.table_name(), part_name)
            })?;
            let count_after: u64 = self.conn.query_row(
                &format!("SELECT COUNT(*) FROM {}", entity.table_name()),
                [],
                |r| r.get(0),
            )?;
            let inserted = count_after - count_before;
            self.log_partition(entity.table_name(), &part_name, "ok", inserted, 0)?;
            stats.partitions_loaded += 1;
            stats.rows_inserted += inserted;
            tracing::info!(
                entity = entity.table_name(),
                partition = %part_name,
                rows = inserted,
                "partition loaded"
            );
        }
        Ok(stats)
    }

    /// Load one works partition through the streaming pre-dedupe pipeline.
    /// Per-file: Appender → PK-less staging tables, gated by a shared
    /// `SeenSet` so only first-sightings reach staging → plain bulk INSERT
    /// into the live (PK'd) tables. No `ON CONFLICT` is needed because the
    /// staging tables only contain first-sightings.
    ///
    /// **Walk order matters.** Callers must invoke `load_works` (which walks
    /// partitions newest-first) so the first sighting of any work-ID is the
    /// canonical (newest-snapshot) version. Calling this directly with an
    /// out-of-order partition list will silently keep stale records and
    /// drop newer ones — don't.
    pub fn load_works_partition(
        &self,
        partition_dir: &Path,
        seen: &mut SeenSet,
    ) -> Result<EntityStats> {
        let part_name = partition_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("partition name not utf-8: {}", partition_dir.display()))?
            .to_string();
        if self.partition_done("works", &part_name)? {
            return Ok(EntityStats {
                skipped_partitions: 1,
                ..Default::default()
            });
        }

        // Per-file processing keeps memory bounded on the giant 2025-11-06
        // partition (2.59 TB). Each JSONL file (~1 GB) gets its own staging
        // tables → bulk INSERT cycle. Errors are localized per file.
        let result = self.load_works_partition_inner(partition_dir, seen);

        match result {
            Ok((rows, parse_errors)) => {
                self.log_partition("works", &part_name, "ok", rows, parse_errors)?;
                Ok(EntityStats {
                    partitions_loaded: 1,
                    rows_inserted: rows,
                    ..Default::default()
                })
            }
            Err(e) => Err(e),
        }
    }

    fn load_works_partition_inner(
        &self,
        partition_dir: &Path,
        seen: &mut SeenSet,
    ) -> Result<(u64, u64)> {
        // Strict `.jsonl` extension filter: rclone leaves `.gz.<hex>` partial-
        // upload files in some snapshot directories (e.g. `part_0049.gz.19B6a7d8`),
        // and the loader has no business reading those. Without this filter,
        // millions of UTF-8 errors get logged per garbage file, blowing out
        // stdout/stderr and contributing to OOM-kills.
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(partition_dir)
            .with_context(|| format!("read_dir {}", partition_dir.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e == "jsonl")
                        .unwrap_or(false)
            })
            .collect();
        files.sort();

        let mut total_rows = 0u64;
        let mut total_errs = 0u64;
        for f in files {
            let (rows, errs) = self
                .load_works_file(&f, seen)
                .with_context(|| format!("load file {}", f.display()))?;
            total_rows += rows;
            total_errs += errs;
        }
        Ok((total_rows, total_errs))
    }

    /// Bulk-load one JSONL file. For each parsed `Work`, gate on the
    /// `SeenSet`: first sighting → append to staging tables, duplicate →
    /// skip. After parsing, drop the appenders (flushes) and run plain bulk
    /// INSERT … SELECT into the live PK'd tables.
    ///
    /// Returns `(inserted_rows, parse_errors)`. `inserted_rows` counts only
    /// works that survived the dedupe gate; per-file skip counts can be
    /// inferred from the JSONL line count vs `inserted_rows` if needed.
    fn load_works_file(&self, file: &std::path::Path, seen: &mut SeenSet) -> Result<(u64, u64)> {
        // Temp tables live for the connection lifetime; we DROP at the end
        // of each file so the next file starts clean. CREATE OR REPLACE so
        // a partial run doesn't leave them behind.
        self.conn.execute_batch(
            "CREATE OR REPLACE TEMP TABLE _stage_works (
               openalex_id VARCHAR, doi VARCHAR, title VARCHAR, abstract_text VARCHAR,
               publication_year USMALLINT, publication_date DATE, language VARCHAR,
               type VARCHAR, cited_by_count UBIGINT, is_retracted BOOLEAN, is_oa BOOLEAN,
               oa_url VARCHAR, primary_source_id VARCHAR
             );
             CREATE OR REPLACE TEMP TABLE _stage_auth (
               work_id VARCHAR, author_id VARCHAR, author_position VARCHAR,
               raw_affiliation_string VARCHAR, institution_id VARCHAR
             );
             CREATE OR REPLACE TEMP TABLE _stage_topic (
               work_id VARCHAR, topic_id VARCHAR, score REAL
             );
             CREATE OR REPLACE TEMP TABLE _stage_concept (
               work_id VARCHAR, concept_id VARCHAR, score REAL
             );
             CREATE OR REPLACE TEMP TABLE _stage_ref (
               work_id VARCHAR, referenced_work_id VARCHAR
             );",
        )?;

        let mut works_app = self.conn.appender("_stage_works")?;
        let mut auth_app = self.conn.appender("_stage_auth")?;
        let mut topic_app = self.conn.appender("_stage_topic")?;
        let mut concept_app = self.conn.appender("_stage_concept")?;
        let mut ref_app = self.conn.appender("_stage_ref")?;

        let mut inserted = 0u64;
        let stats = crate::reader::read_jsonl_file::<Work, _>(file, &mut |w| {
            // Streaming dedupe gate. `insert` returns Ok(true) on first
            // sighting, Ok(false) on duplicate, Err on a malformed ID.
            // Malformed IDs are logged and skipped — they shouldn't happen
            // in a well-formed snapshot, but if they do we don't want one
            // bad row to abort a partition.
            match seen.insert(&w.id) {
                Ok(true) => {
                    stage_work(
                        &mut works_app,
                        &mut auth_app,
                        &mut topic_app,
                        &mut concept_app,
                        &mut ref_app,
                        &w,
                    );
                    inserted += 1;
                }
                Ok(false) => {
                    // Duplicate — already saw a newer version in an earlier
                    // (newer) partition. Skip silently; this is the expected
                    // dedupe path.
                }
                Err(e) => {
                    tracing::warn!(work_id = %w.id, error = %e, "skipping work with malformed id");
                }
            }
        })?;

        // Drop appenders so they flush before the merge.
        drop(ref_app);
        drop(concept_app);
        drop(topic_app);
        drop(auth_app);
        drop(works_app);

        // Bulk INSERTs into the PK'd live tables. Each INSERT auto-commits
        // (no BEGIN/COMMIT wrapping) so DuckDB can release the WAL between
        // statements — important on huge JSONL files where wrapping all 5
        // INSERTs in one transaction held all dirty pages until COMMIT and
        // hit `TransactionContext Error: Failed to commit` at the
        // memory_limit cap.
        //
        // The SeenSet gate keeps the staging tables free of cross-work
        // duplicates, but OpenAlex can emit the same edge target twice
        // WITHIN a single work's `concepts[]` / `topics[]` /
        // `referenced_works[]` array. The PK on each edge table is the
        // (work_id, edge_id) pair, so we collapse intra-work duplicates
        // here at merge time via SELECT DISTINCT ON (...).
        //
        // ON CONFLICT DO NOTHING on works (and edge tables) is a safety net
        // for the rare partial-merge consistency-break case: if (say)
        // `works` INSERT succeeds but `work_topics` INSERT fails on memory
        // pressure, the partition is logged as failed and the loader moves
        // on. A subsequent older partition will re-emit those work_ids
        // (the SeenSet only adds IDs after a clean parse, but data may
        // have committed before the failure); ON CONFLICT lets that
        // re-emission no-op instead of cascading PK violations through
        // every following partition.
        //
        // Cost of ON CONFLICT under streaming pre-dedupe: minimal. The
        // SeenSet handles 99% of dedup; ON CONFLICT only fires on the rare
        // collision. Hash anti-join builds against the staging side
        // (small) and probes the live PK index (B-tree lookup, log n).
        //
        // work_authorships has no PK (the same author can legitimately
        // appear multiple times on a work via different positions /
        // institutions) so we don't dedupe it.
        // ON CONFLICT removed at this stage of the migration: at corpus
        // scale (200+ GB DB, ~36M PK rows) DuckDB's ON CONFLICT path pins
        // the entire PK index in the buffer pool to do anti-join lookups,
        // which exceeds the 14 GB memory_limit on big and triggers cascade
        // OOMs. The SeenSet (~290 MB on disk, all parsed work_ids) already
        // catches cross-partition duplicates with 100% accuracy in normal
        // operation; ON CONFLICT was a safety net for the rare partial-
        // merge case where a previous loader crashed mid-INSERT. With
        // 309/311 partitions clean and only the two giant partitions
        // (2025-11-06 / 2025-10-10) still being retried, the safety net
        // is no longer worth its memory cost. Plain INSERTs let the buffer
        // pool stay small enough to actually finish.
        //
        // Risk: if a work_id is in `works` but not in SeenSet (previous
        // partial-merge artifact), this hits a PK violation that aborts
        // the partition. The SeenSet rehydrates from seen_set.bin on
        // restart, so this can only happen for IDs whose disk-checkpoint
        // hasn't fired yet — narrow window.
        self.conn.execute_batch(
            "INSERT INTO works            SELECT * FROM _stage_works;
             INSERT INTO work_authorships SELECT * FROM _stage_auth;
             INSERT INTO work_topics
                 SELECT DISTINCT ON (work_id, topic_id) work_id, topic_id, score
                 FROM _stage_topic;
             INSERT INTO work_concepts
                 SELECT DISTINCT ON (work_id, concept_id) work_id, concept_id, score
                 FROM _stage_concept;
             INSERT INTO work_references
                 SELECT DISTINCT work_id, referenced_work_id
                 FROM _stage_ref;
             DROP TABLE _stage_works;
             DROP TABLE _stage_auth;
             DROP TABLE _stage_topic;
             DROP TABLE _stage_concept;
             DROP TABLE _stage_ref;",
        )?;

        Ok((inserted, stats.parse_errors))
    }

    /// Load every works partition under `snapshot_root/works/` using the
    /// streaming pre-dedupe pipeline. Walks partitions **newest-first**
    /// (descending by `updated_date`) so the first sighting of any work-ID
    /// is the canonical (newest) version. Maintains a single `SeenSet`
    /// across the whole walk; checkpoints it to a sidecar file every N
    /// partitions for crash recovery.
    ///
    /// `seen_set_path` is where the sidecar checkpoint lives — typically
    /// `~/home-still/data/openalex/seen_set.bin`. If the file exists, the
    /// set rehydrates from it on entry; otherwise we start with an empty
    /// set. The `_ingest_log` table independently tracks completed
    /// partitions, so `load_works` is safe to re-run from any state.
    pub fn load_works(
        &self,
        snapshot_root: &Path,
        seen_set_path: std::path::PathBuf,
    ) -> Result<EntityStats> {
        let works_dir = snapshot_root.join("works");
        let mut partitions = list_partitions(&works_dir)?;
        // Walk newest-first. `list_partitions` sorts ascending by name; the
        // OpenAlex partition naming convention is `updated_date=YYYY-MM-DD`,
        // so reverse alphabetical = reverse chronological.
        partitions.reverse();

        let mut seen = SeenSet::load_or_default(seen_set_path)?;
        let mut total = EntityStats::default();
        for part in partitions {
            let pname = part
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_string();
            let s = match self.load_works_partition(&part, &mut seen) {
                Ok(s) => s,
                Err(e) => {
                    // Chained Display ({e:#}) walks the anyhow source chain
                    // so the root cause is visible — without :# we only see
                    // the outer "load file <path>" context wrapper, which
                    // tells you which file but not why it failed.
                    tracing::error!(partition = %pname, error = %format!("{:#}", e), "partition failed; skipping");
                    eprintln!("  ✗ partition {pname} failed: {e:#}");
                    // Best-effort rollback in case the inner path didn't.
                    let _ = self.conn.execute_batch("ROLLBACK");
                    continue;
                }
            };
            total.partitions_loaded += s.partitions_loaded;
            total.skipped_partitions += s.skipped_partitions;
            total.rows_inserted += s.rows_inserted;
            // Maybe-checkpoint counts only successful partitions.
            if s.partitions_loaded > 0 {
                seen.maybe_checkpoint()?;
                tracing::info!(
                    partition = %pname,
                    inserted = s.rows_inserted,
                    seen_total = seen.len(),
                    "works partition done"
                );
                // Force DuckDB to flush its WAL to disk between partitions
                // so the buffer pool doesn't grow unbounded across hours of
                // ingest. Without this, accumulated dirty pages from
                // already-committed partitions stay resident and squeeze
                // out the working set for the current partition's commit
                // — which manifested as silent kernel OOM-kills around
                // partition 2026-01-13 (50 files, biggest in the corpus).
                if let Err(e) = self.conn.execute_batch("CHECKPOINT;") {
                    tracing::warn!(error = %format!("{:#}", e), "CHECKPOINT after partition failed (continuing)");
                }
            }
        }
        // Final checkpoint so the sidecar reflects the end-of-load state.
        seen.force_checkpoint()?;
        Ok(total)
    }

    /// Load one authors partition via Rust streaming + Appender. The earlier
    /// SQL path (`SimpleEntity::Authors`) OOMed DuckDB on the 176 GB partition
    /// because `read_json` with auto-detected STRUCT lists has to buffer per
    /// row group; this path streams one record at a time and is bounded.
    pub fn load_authors_partition(&self, partition_dir: &Path) -> Result<EntityStats> {
        let part_name = partition_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("partition name not utf-8: {}", partition_dir.display()))?
            .to_string();
        if self.partition_done("authors", &part_name)? {
            return Ok(EntityStats {
                skipped_partitions: 1,
                ..Default::default()
            });
        }

        let mut app = self.conn.appender("authors")?;
        let mut rows = 0u64;
        let parse_stats = read_partition::<Author, _>(partition_dir, |a| {
            if let Err(e) = append_author(&mut app, &a) {
                tracing::warn!(author_id = %a.id, error = %e, "append_author failed");
            } else {
                rows += 1;
            }
        })?;
        drop(app);

        self.log_partition("authors", &part_name, "ok", rows, parse_stats.parse_errors)?;
        Ok(EntityStats {
            partitions_loaded: 1,
            rows_inserted: rows,
            ..Default::default()
        })
    }

    pub fn load_authors(&self, snapshot_root: &Path) -> Result<EntityStats> {
        let authors_dir = snapshot_root.join("authors");
        let partitions = list_partitions(&authors_dir)?;
        let mut total = EntityStats::default();
        for part in partitions {
            let s = self.load_authors_partition(&part)?;
            total.partitions_loaded += s.partitions_loaded;
            total.skipped_partitions += s.skipped_partitions;
            total.rows_inserted += s.rows_inserted;
            tracing::info!(
                partition = %part.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                rows = s.rows_inserted,
                cumulative = total.rows_inserted,
                "authors partition done"
            );
        }
        Ok(total)
    }

    pub fn row_counts(&self) -> Result<Vec<(String, u64)>> {
        let tables = [
            "concepts",
            "topics",
            "domains",
            "fields",
            "subfields",
            "sources",
            "institutions",
            "funders",
            "publishers",
            "authors",
            "works",
            "work_authorships",
            "work_topics",
            "work_concepts",
            "work_references",
        ];
        let mut out = Vec::with_capacity(tables.len());
        for t in tables {
            let n: u64 = self
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {}", t), [], |r| r.get(0))?;
            out.push((t.to_string(), n));
        }
        Ok(out)
    }
}

fn append_author(app: &mut duckdb::Appender, a: &Author) -> Result<()> {
    let id = strip_openalex_id(&a.id).to_string();
    let last_known_inst_id = a
        .last_known_institutions
        .first()
        .and_then(|i| i.id.as_deref())
        .map(|id| strip_openalex_id(id).to_string());
    let affiliations_json = serde_json::to_string(&a.affiliations).unwrap_or_else(|_| "[]".into());
    let ids_json = serde_json::to_string(&a.ids).unwrap_or_else(|_| "{}".into());
    app.append_row(params![
        id,
        a.display_name,
        a.orcid,
        a.works_count,
        a.cited_by_count,
        last_known_inst_id,
        affiliations_json,
        ids_json,
    ])?;
    Ok(())
}

/// Append one Work and its edges to the per-file staging tables. Uses
/// Appender (fastest path), and the staging tables have no PK / NOT NULL so
/// nothing fails per row. Conflict handling happens during the bulk merge.
fn stage_work(
    works_app: &mut duckdb::Appender,
    auth_app: &mut duckdb::Appender,
    topic_app: &mut duckdb::Appender,
    concept_app: &mut duckdb::Appender,
    ref_app: &mut duckdb::Appender,
    w: &Work,
) {
    let work_id = strip_openalex_id(&w.id).to_string();
    let abstract_text = w
        .abstract_inverted_index
        .as_ref()
        .and_then(reconstruct_abstract);
    let doi = w.doi.as_deref().map(|d| strip_doi(d).to_string());
    let title = w.title.clone().or_else(|| w.display_name.clone());
    let primary_source_id = w
        .primary_location
        .as_ref()
        .and_then(|l| l.source.as_ref())
        .and_then(|s| s.id.as_deref())
        .map(|id| strip_openalex_id(id).to_string());
    let oa_url = w.open_access.as_ref().and_then(|o| o.oa_url.clone());
    let is_oa = w.open_access.as_ref().and_then(|o| o.is_oa);

    let _ = works_app.append_row(params![
        work_id,
        doi,
        title,
        abstract_text,
        w.publication_year,
        w.publication_date,
        w.language,
        w.work_type,
        w.cited_by_count,
        w.is_retracted,
        is_oa,
        oa_url,
        primary_source_id,
    ]);

    for (idx, a) in w.authorships.iter().enumerate() {
        let author_id = match a.author.id.as_deref() {
            Some(id) => strip_openalex_id(id).to_string(),
            None => continue,
        };
        let pos = a
            .author_position
            .clone()
            .unwrap_or_else(|| format!("p{}", idx));
        let raw_aff = a.raw_affiliation_strings.first().cloned();
        if a.institutions.is_empty() {
            let _ = auth_app.append_row(params![
                work_id,
                author_id,
                pos,
                raw_aff,
                Option::<String>::None,
            ]);
        } else {
            for inst in &a.institutions {
                let inst_id = inst
                    .id
                    .as_deref()
                    .map(|id| strip_openalex_id(id).to_string());
                let _ = auth_app.append_row(params![work_id, author_id, pos, raw_aff, inst_id]);
            }
        }
    }

    for t in &w.topics {
        let tid = strip_openalex_id(&t.id).to_string();
        let _ = topic_app.append_row(params![work_id, tid, t.score]);
    }
    for c in &w.concepts {
        let cid = strip_openalex_id(&c.id).to_string();
        let _ = concept_app.append_row(params![work_id, cid, c.score]);
    }
    for r in &w.referenced_works {
        let rid = strip_openalex_id(r).to_string();
        let _ = ref_app.append_row(params![work_id, rid]);
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct EntityStats {
    pub partitions_loaded: u64,
    pub skipped_partitions: u64,
    pub rows_inserted: u64,
}

/// Per-entity ingest spec for the SQL-driven path.
#[derive(Debug, Clone, Copy)]
pub enum SimpleEntity {
    Concepts,
    Topics,
    Domains,
    Fields,
    Subfields,
    Sources,
    Institutions,
    Funders,
    Publishers,
}

impl SimpleEntity {
    pub fn snapshot_dir(self) -> &'static str {
        match self {
            SimpleEntity::Concepts => "concepts",
            SimpleEntity::Topics => "topics",
            SimpleEntity::Domains => "domains",
            SimpleEntity::Fields => "fields",
            SimpleEntity::Subfields => "subfields",
            SimpleEntity::Sources => "sources",
            SimpleEntity::Institutions => "institutions",
            SimpleEntity::Funders => "funders",
            SimpleEntity::Publishers => "publishers",
        }
    }

    pub fn table_name(self) -> &'static str {
        match self {
            SimpleEntity::Concepts => "concepts",
            SimpleEntity::Topics => "topics",
            SimpleEntity::Domains => "domains",
            SimpleEntity::Fields => "fields",
            SimpleEntity::Subfields => "subfields",
            SimpleEntity::Sources => "sources",
            SimpleEntity::Institutions => "institutions",
            SimpleEntity::Funders => "funders",
            SimpleEntity::Publishers => "publishers",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "concepts" => Ok(Self::Concepts),
            "topics" => Ok(Self::Topics),
            "domains" => Ok(Self::Domains),
            "fields" => Ok(Self::Fields),
            "subfields" => Ok(Self::Subfields),
            "sources" => Ok(Self::Sources),
            "institutions" => Ok(Self::Institutions),
            "funders" => Ok(Self::Funders),
            "publishers" => Ok(Self::Publishers),
            "authors" => Err(anyhow!(
                "authors goes through the Rust Appender path — use `hs openalex load authors` (which routes to OpenAlexDb::load_authors), not SimpleEntity"
            )),
            "works" => Err(anyhow!(
                "works goes through the Rust Appender path — use `hs openalex load-works`"
            )),
            other => Err(anyhow!("unknown entity '{}'", other)),
        }
    }

    /// SQL that ingests one partition's JSONL files into the target table.
    /// Uses `INSERT OR IGNORE` so the partition loader can safely retry the
    /// same partition without duplicating PK violations.
    fn insert_sql(self, glob: &str) -> String {
        let strip = "regexp_replace";
        match self {
            SimpleEntity::Concepts => format!(
                r#"
                INSERT OR IGNORE INTO concepts
                SELECT
                    {strip}(id, '^https://openalex.org/', '') AS openalex_id,
                    display_name,
                    level,
                    description,
                    wikidata,
                    works_count,
                    cited_by_count,
                    to_json(ancestors) AS ancestors
                FROM read_json('{glob}', format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Topics => format!(
                r#"
                INSERT OR IGNORE INTO topics
                SELECT
                    {strip}(id, '^https://openalex.org/', '') AS openalex_id,
                    display_name,
                    description,
                    keywords,
                    {strip}(subfield.id, '^https://openalex.org/subfields/', '') AS subfield_id,
                    {strip}(field.id, '^https://openalex.org/fields/', '') AS field_id,
                    {strip}(domain.id, '^https://openalex.org/domains/', '') AS domain_id
                FROM read_json('{glob}', format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Domains => format!(
                r#"
                INSERT OR IGNORE INTO domains
                SELECT
                    {strip}(id, '^https://openalex.org/domains/', '') AS openalex_id,
                    display_name
                FROM read_json('{glob}', format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Fields => format!(
                r#"
                INSERT OR IGNORE INTO fields
                SELECT
                    {strip}(id, '^https://openalex.org/fields/', '') AS openalex_id,
                    display_name,
                    {strip}(domain.id, '^https://openalex.org/domains/', '') AS domain_id
                FROM read_json('{glob}', format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Subfields => format!(
                r#"
                INSERT OR IGNORE INTO subfields
                SELECT
                    {strip}(id, '^https://openalex.org/subfields/', '') AS openalex_id,
                    display_name,
                    {strip}(field.id, '^https://openalex.org/fields/', '') AS field_id,
                    {strip}(domain.id, '^https://openalex.org/domains/', '') AS domain_id
                FROM read_json('{glob}', format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Sources => format!(
                r#"
                INSERT OR IGNORE INTO sources
                SELECT
                    {strip}(id, '^https://openalex.org/', '') AS openalex_id,
                    display_name,
                    issn_l,
                    issn,
                    -- host_organization is an int in the snapshot; the lineage
                    -- list carries the typed (P… or I…) URL we actually want
                    -- to join against publishers/institutions.
                    {strip}(host_organization_lineage[1], '^https://openalex.org/', '')
                        AS host_organization_id,
                    type,
                    is_oa,
                    is_in_doaj,
                    works_count,
                    cited_by_count
                FROM read_json('{glob}', format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Institutions => format!(
                r#"
                INSERT OR IGNORE INTO institutions
                SELECT
                    {strip}(id, '^https://openalex.org/', '') AS openalex_id,
                    display_name,
                    country_code,
                    type,
                    ror,
                    works_count,
                    cited_by_count
                FROM read_json('{glob}', format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Funders => format!(
                r#"
                INSERT OR IGNORE INTO funders
                SELECT
                    {strip}(id, '^https://openalex.org/', '') AS openalex_id,
                    display_name,
                    country_code,
                    works_count,
                    cited_by_count
                FROM read_json('{glob}', format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Publishers => format!(
                r#"
                INSERT OR IGNORE INTO publishers
                SELECT
                    {strip}(id, '^https://openalex.org/', '') AS openalex_id,
                    display_name,
                    works_count,
                    cited_by_count
                FROM read_json('{glob}', format='newline_delimited', auto_detect=true);
                "#
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_schema() {
        let dir = std::env::temp_dir().join(format!("oa-loader-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test.duckdb");
        let db = OpenAlexDb::open(&db_path).unwrap();
        let counts = db.row_counts().unwrap();
        // Every table exists with zero rows.
        assert_eq!(counts.len(), 15);
        for (name, n) in counts {
            assert_eq!(n, 0, "{} should be empty in fresh db", name);
        }
    }

    #[test]
    fn simple_entity_parse_roundtrip() {
        for s in [
            "concepts",
            "topics",
            "domains",
            "fields",
            "subfields",
            "sources",
            "institutions",
            "funders",
            "publishers",
        ] {
            let e = SimpleEntity::parse(s).unwrap();
            assert_eq!(e.snapshot_dir(), s);
        }
        // authors and works route through the Rust Appender path, not SimpleEntity.
        assert!(SimpleEntity::parse("authors").is_err());
        assert!(SimpleEntity::parse("works").is_err());
        assert!(SimpleEntity::parse("nope").is_err());
    }
}
