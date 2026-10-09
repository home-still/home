//! DuckDB connection management + entity loaders.
//!
//! Every entity follows ONE rule, the streaming pre-dedupe: walk the
//! `updated_date=*` partitions **newest-first**, gate each record on an
//! in-RAM [`SeenSet`] of the entity's integer IDs, and let only first
//! sightings (the newest version) reach the live table through a plain bulk
//! INSERT. A later sighting of an ID is a stale duplicate and is skipped
//! silently. The live tables keep their PRIMARY KEYs; nothing here uses
//! `ON CONFLICT` or `INSERT OR IGNORE`, which would let the oldest copy win
//! (the walk order is the only thing that decides which copy is newest).
//!
//! Three ingest shapes share that rule:
//!
//! 1. **Simple entities** (concepts, topics, sources, institutions,
//!    domains, fields, subfields, funders, publishers): DuckDB reads the
//!    partition's JSONL natively via `read_json` and projects / transforms
//!    columns inline into a temp stage table (no Rust round-trip for the
//!    payload; DuckDB handles arrays/structs directly). Rust then reads the
//!    stage's IDs, gates them on the entity's `SeenSet`, and ONE transaction
//!    inserts the first sightings into the live table. (`authors` goes
//!    through the Rust Appender path, see `load_authors`.)
//!
//! 2. **Authors**: Rust streaming + Appender into a temp stage table, gated
//!    the same way, merged in one transaction per partition.
//!
//! 3. **Works**: Rust streaming; for each row, gate on a shared `SeenSet`
//!    keyed on integer work-ID. First sighting → append to staging tables (5
//!    Appenders); duplicate → skip. At end of file, ONE transaction
//!    bulk-INSERTs the staging tables into the live tables. PRIMARY KEY
//!    constraints on works/work_topics/work_concepts/work_references are safe
//!    because the staging tables only ever contain first-sightings that
//!    don't already exist in the live tables.
//!
//! The dimension tables' sets ([`SeenSet::from_table`]) are rebuilt from the
//! table at the start of each `hs openalex load <entity>` and have no
//! checkpoint file; only the works set has one (see `seen_set.rs` for why a
//! checkpoint would add nothing at dimension scale).
//!
//! # Failure and commit ordering (the part that keeps the dedupe honest)
//!
//! * A partition's (works: a file's) IDs are *staged* (a local set) while it
//!   is read. They enter the `SeenSet` only after the merge transaction has
//!   committed ([`SeenSet::commit`]); a failed partition therefore leaves the
//!   set, the database and every later checkpoint exactly as they were.
//! * The merge is one transaction: `works` and its four edge tables land
//!   together or not at all, so there are never works without edges, edges
//!   without works, or duplicated `work_authorships` rows (that table has no
//!   PK to catch a retry). A dimension partition lands whole or not at all.
//! * Any failure aborts the whole load. Partitions are walked newest-first
//!   and the newest version of a record must win, so continuing past a failed
//!   partition would let older partitions load stale versions of its records.
//!   The failed partition is logged `error` in `_ingest_log` and the error is
//!   returned (`hs openalex load-works` / `load <entity>` exit non-zero).
//! * The works seen-set checkpoint is written only after the partition it
//!   covers has committed and been logged. A crash can leave the database
//!   ahead of the checkpoint but never behind it, and `SeenSet::open`
//!   reconciles the two on the next start (see `seen_set.rs`).
//!
//! Resumability: every partition load consults `_ingest_log` first and skips
//! if status='ok'. Progress inside a works partition is tracked by the data
//! itself: a re-run of a partly loaded partition finds the already-committed
//! works in the `SeenSet` (rebuilt from `works` on open) and skips exactly
//! those. A dimension partition is atomic, so it is either logged `ok` or
//! redone whole; its IDs are in the set only if its rows are in the table.

use anyhow::{anyhow, bail, Context, Result};
use duckdb::{params, Connection};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::model::{Author, Work};
use crate::parser::{
    parse_entity_id_u64, parse_work_id_u64, reconstruct_abstract, strip_doi, strip_openalex_id,
};
use crate::reader::{list_partitions, partition_files, read_jsonl_file, read_partition};
use crate::schema::{POST_LOAD_INDEXES, SCHEMA_DDL};
use crate::seen_set::SeenSet;

/// DuckDB `memory_limit` for the read-write (loading) connection. At corpus
/// scale (50M+ edge rows with PK indexes) the bulk INSERTs pin PK index pages
/// in the buffer pool and lower limits failed with `failed to pin block`.
const LOAD_MEMORY_LIMIT: &str = "24GB";

/// DuckDB `memory_limit` for read-only connections (`hs openalex status` /
/// `query`): enough for ad-hoc aggregations, small enough not to compete with
/// the loader or the services on the same host.
const READ_ONLY_MEMORY_LIMIT: &str = "4GB";

/// Type letter of an author ID (`A5023888391`); see [`parse_entity_id_u64`].
const AUTHOR_ID_PREFIX: char = 'A';

/// Default for [`LoadOptions::max_parse_errors_per_partition`]. The real
/// snapshot has the occasional malformed line (see `tests/snapshot_live.rs`),
/// so zero would stop every load; a schema drift produces thousands.
pub const DEFAULT_MAX_PARSE_ERRORS_PER_PARTITION: u64 = 100;

/// Tolerance policy for the Rust-side loaders (works, authors).
#[derive(Debug, Clone, Copy)]
pub struct LoadOptions {
    /// A partition fails when more than this many of its records are
    /// unusable (do not parse, or carry a malformed work/author ID). The records
    /// within the budget are skipped, logged, and counted in
    /// `_ingest_log.parse_errors`.
    pub max_parse_errors_per_partition: u64,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            max_parse_errors_per_partition: DEFAULT_MAX_PARSE_ERRORS_PER_PARTITION,
        }
    }
}

pub struct OpenAlexDb {
    conn: Connection,
}

impl OpenAlexDb {
    /// Open (creating if needed) the DuckDB file for loading and ensure the
    /// schema is applied. Idempotent. Read-write: use [`Self::open_read_only`]
    /// for anything that only reads.
    ///
    /// Sets `memory_limit` to [`LOAD_MEMORY_LIMIT`] and a co-located
    /// `temp_directory` so that loading huge JSONL partitions spills to disk
    /// rather than OOM-killing the process. `big` runs llama-server +
    /// hs-scribe-server simultaneously, so headroom is tight.
    pub fn open(db_path: &Path) -> Result<Self> {
        let parent = db_path.parent().ok_or_else(|| {
            anyhow!(
                "openalex db path {} has no parent directory",
                db_path.display()
            )
        })?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
        let conn = Connection::open(db_path)
            .with_context(|| format!("open duckdb at {}", db_path.display()))?;
        let temp_dir = parent.join(".duckdb_tmp");
        std::fs::create_dir_all(&temp_dir)
            .with_context(|| format!("create_dir_all {}", temp_dir.display()))?;
        let temp_dir = temp_dir
            .to_str()
            .ok_or_else(|| anyhow!("temp dir {} is not valid UTF-8", temp_dir.display()))?;
        // `preserve_insertion_order=false` is the bulk-load silver bullet:
        // without it, DuckDB buffers entire result sets to keep input ordering
        // visible, which OOMs on huge JSONL partitions. We don't care about
        // physical row order for an OLAP catalog.
        //
        // PRAGMA arguments cannot be bound parameters, so the path goes in as
        // an escaped SQL string literal.
        let pragmas = format!(
            "PRAGMA memory_limit='{LOAD_MEMORY_LIMIT}';\n\
             PRAGMA temp_directory={};\n\
             PRAGMA threads=4;\n\
             PRAGMA preserve_insertion_order=false;",
            sql_string_literal(temp_dir)
        );
        conn.execute_batch(&pragmas).context("apply pragmas")?;
        conn.execute_batch(SCHEMA_DDL).context("apply schema")?;
        Ok(Self { conn })
    }

    /// Open an existing DuckDB file read-only: no file creation, no DDL, no
    /// loader pragmas, and DuckDB itself rejects any statement that would
    /// write. Several read-only handles (hs-mcp's among them) can be open on
    /// the file at once; a read-write handle excludes them all.
    pub fn open_read_only(db_path: &Path) -> Result<Self> {
        let cfg = duckdb::Config::default()
            .access_mode(duckdb::AccessMode::ReadOnly)
            .context("configure read-only access")?;
        let conn = Connection::open_with_flags(db_path, cfg)
            .with_context(|| format!("open duckdb read-only at {}", db_path.display()))?;
        conn.execute_batch(&format!("SET memory_limit='{READ_ONLY_MEMORY_LIMIT}';"))
            .context("set read-only memory limit")?;
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
            self.conn
                .execute_batch("CHECKPOINT;")
                .with_context(|| format!("CHECKPOINT after index: {stmt}"))?;
            tracing::info!(
                target: "openalex_ingest",
                elapsed_ms = started.elapsed().as_millis() as u64,
                "index built"
            );
        }
        Ok(())
    }

    /// Download the DuckDB `fts` extension into DuckDB's extension directory
    /// (`~/.duckdb/extensions/<version>/<platform>/`) and `LOAD` it to prove
    /// it is usable: `hs openalex install-fts`, the explicit once-per-host
    /// (and per DuckDB version) step in front of [`Self::build_fts`]. The
    /// `duckdb` crate's `bundled` build has no FTS feature to link it in
    /// statically, and `build_fts` never touches the network, so this is the
    /// only place the extension is fetched.
    ///
    /// Needs network access to DuckDB's extension repository unless the
    /// extension is already present (then `INSTALL` is a no-op); offline and
    /// not installed it fails with an error saying so. Needs no database file:
    /// it runs on an in-memory connection, so it neither creates the catalog
    /// nor takes its write lock (the extension directory is per user, not per
    /// database).
    pub fn install_fts() -> Result<()> {
        let conn = Connection::open_in_memory().context("open an in-memory duckdb")?;
        install_fts_on(&conn)
    }

    /// Build the BM25 full-text index over works.title + works.abstract_text,
    /// then write the `openalex_works` readiness sentinel into
    /// `_corpus_state`. The sentinel is the gate hs-mcp checks at startup to
    /// decide whether to expose the 5 `openalex_*` MCP tools — by writing it
    /// only AFTER the FTS index lands, we guarantee that if the tools are
    /// visible, every code path they exercise (search, get, references,
    /// citations, authors_by_topic) has the data it needs.
    ///
    /// Only `LOAD`s the `fts` extension; it never downloads it. When the
    /// extension is not installed this fails before touching the database,
    /// naming `hs openalex install-fts` (see [`Self::install_fts`]). Slow at
    /// full corpus (~hour+) — run once after works ingest.
    ///
    /// The `_corpus_state` upsert is the one remaining `ON CONFLICT` in the
    /// crate: it stamps the readiness sentinel (one row per component,
    /// re-stamped when the index is rebuilt), which is not entity data and
    /// has no stream to pre-dedupe.
    pub fn build_fts(&self) -> Result<()> {
        self.conn.execute_batch("LOAD fts;").context(
            "fts extension not installed — run `hs openalex install-fts` (needs network once)",
        )?;
        self.conn
            .execute_batch(
                r#"
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

    /// Stamp a failed partition `error` in `_ingest_log`. The caller returns
    /// the original error either way, so a failure to write the stamp (the
    /// database may be what failed) is logged rather than replacing it.
    fn log_partition_failure(&self, entity: &str, partition: &str, progress: &PartitionProgress) {
        if let Err(e) = self.log_partition(
            entity,
            partition,
            "error",
            progress.rows,
            progress.parse_errors,
        ) {
            tracing::error!(
                entity,
                partition,
                error = %format!("{e:#}"),
                "could not record the error status in _ingest_log"
            );
        }
    }

    /// `_ingest_log` rows that need an operator's attention: every partition
    /// whose status is not `ok`, and every `ok` one that skipped records.
    pub fn ingest_log_attention(&self) -> Result<Vec<IngestLogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT entity, partition, status, COALESCE(rows, 0), COALESCE(parse_errors, 0) \
             FROM _ingest_log \
             WHERE status <> 'ok' OR COALESCE(parse_errors, 0) > 0 \
             ORDER BY entity, partition",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(IngestLogEntry {
                    entity: r.get(0)?,
                    partition: r.get(1)?,
                    status: r.get(2)?,
                    rows: r.get(3)?,
                    parse_errors: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Bulk-load a "simple" entity: DuckDB reads each partition's JSONL
    /// natively into a stage table, the entity's [`SeenSet`] gates the rows,
    /// and one transaction inserts the first sightings into the live table.
    /// `entity` is the snapshot subdir name (e.g. "concepts").
    ///
    /// Partitions are walked **newest-first**, so the first sighting of an ID
    /// is its newest version and a later (older) copy is skipped silently.
    /// The first failure aborts the load: continuing would let older
    /// partitions store stale versions of the failed partition's records.
    /// Resumability is per partition (`_ingest_log`); the set is rebuilt from
    /// the table on every call, so a re-run skips exactly the committed rows.
    pub fn load_simple_entity(
        &self,
        entity: SimpleEntity,
        snapshot_root: &Path,
    ) -> Result<EntityStats> {
        let table = entity.table_name();
        let partitions = newest_first_partitions(&snapshot_root.join(entity.snapshot_dir()))?;
        let mut seen = SeenSet::from_table(&self.conn, table, entity.id_prefix())?;
        let mut stats = EntityStats::default();
        for part in partitions {
            let part_name = partition_name(&part)?;
            if self.partition_done(table, &part_name)? {
                stats.skipped_partitions += 1;
                continue;
            }
            let loaded = self
                .load_simple_partition(entity, &part, &mut seen)
                .with_context(|| format!("insert {table} partition {part_name}"));
            let inserted = match loaded {
                Ok(n) => n,
                Err(e) => {
                    self.log_partition_failure(table, &part_name, &PartitionProgress::default());
                    return Err(e);
                }
            };
            self.log_partition(table, &part_name, "ok", inserted, 0)?;
            stats.partitions_loaded += 1;
            stats.rows_inserted += inserted;
            tracing::info!(
                entity = table,
                partition = %part_name,
                rows = inserted,
                "partition loaded"
            );
        }
        Ok(stats)
    }

    /// One partition: stage → gate → merge; returns the rows added. The stage
    /// tables do not outlive the call.
    fn load_simple_partition(
        &self,
        entity: SimpleEntity,
        partition_dir: &Path,
        seen: &mut SeenSet,
    ) -> Result<u64> {
        let dir = partition_dir
            .to_str()
            .ok_or_else(|| anyhow!("path {} is not valid UTF-8", partition_dir.display()))?;
        // The path is bound as a parameter, so quotes in it are harmless, but
        // `read_json` treats it as a glob and has no escape for `*?[`: a
        // directory containing them would silently match other files or none.
        if dir.contains(['*', '?', '[']) {
            bail!("partition path {dir:?} contains glob metacharacters");
        }
        let glob = format!("{dir}/*.jsonl");
        let loaded = self.stage_gate_merge_simple(entity, &glob, seen);
        if loaded.is_err() {
            // The merge is one transaction, so nothing was committed; drop
            // the stage so it does not hold memory until the next partition.
            if let Err(e) = self.conn.execute_batch(
                "DROP TABLE IF EXISTS _stage_simple; DROP TABLE IF EXISTS _keep_simple;",
            ) {
                tracing::warn!(error = %format!("{e:#}"), "could not drop the simple-entity stage after a failed partition");
            }
        }
        loaded
    }

    fn stage_gate_merge_simple(
        &self,
        entity: SimpleEntity,
        glob: &str,
        seen: &mut SeenSet,
    ) -> Result<u64> {
        // CREATE OR REPLACE clears the leftovers of an aborted partition.
        self.conn.execute(&entity.stage_sql(), params![glob])?;
        let staged = self.gate_staged_simple(entity, seen)?;
        let merged = self.merge_staged_simple(entity, staged.len() as u64)?;
        // The rows are committed; only now may the IDs count as seen.
        seen.commit(staged);
        Ok(merged)
    }

    /// The streaming pre-dedupe gate for a staged SQL partition: walk the
    /// stage in `rowid` order and keep a record only if its ID is not in
    /// `seen` (a newer partition, or an earlier run, already committed it) and
    /// is the first sighting inside this partition. The kept `rowid`s go to
    /// `_keep_simple`; the returned IDs are NOT yet in `seen`.
    ///
    /// A record without an ID, or with one that is not `<prefix><digits>`,
    /// cannot be deduped or keyed: the partition fails and the error names it
    /// (the SQL-ingested entities take no parse-error budget). All copies of
    /// an ID inside one partition carry the same `updated_date`, so which of
    /// them is kept does not matter; the lowest `rowid` is, deterministically.
    fn gate_staged_simple(&self, entity: SimpleEntity, seen: &SeenSet) -> Result<HashSet<u64>> {
        let table = entity.table_name();
        let prefix = entity.id_prefix();
        let mut staged: HashSet<u64> = HashSet::new();
        let mut keep: Vec<i64> = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT rowid, openalex_id FROM _stage_simple ORDER BY rowid")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let rowid: i64 = row.get(0)?;
                let raw: Option<String> = row.get(1)?;
                let raw = raw.ok_or_else(|| anyhow!("a {table} record has no id"))?;
                let id = parse_entity_id_u64(&raw, prefix)
                    .ok_or_else(|| anyhow!("a {table} record has a malformed id {raw:?}"))?;
                if seen.contains(id) || !staged.insert(id) {
                    continue;
                }
                keep.push(rowid);
            }
        }
        self.conn
            .execute_batch("CREATE OR REPLACE TEMP TABLE _keep_simple (stage_row BIGINT);")?;
        let mut app = self.conn.appender("_keep_simple")?;
        for rowid in keep {
            app.append_row(params![rowid])?;
        }
        // Appender flush errors are swallowed by its Drop, so flush
        // explicitly: a row that never reached the keep list would be dropped
        // from the table without trace.
        app.flush().context("flush the keep-list appender")?;
        Ok(staged)
    }

    /// Insert the kept rows of `_stage_simple` into the live table and drop
    /// both stage tables, in ONE transaction. Plain INSERT, no `ON CONFLICT`:
    /// the gate guarantees every kept ID is new (the set is rebuilt from the
    /// table), so a PK violation means the invariant broke and fails the
    /// transaction loudly; so does a row count that differs from `expected`.
    fn merge_staged_simple(&self, entity: SimpleEntity, expected: u64) -> Result<u64> {
        let table = entity.table_name();
        let tx = self.conn.unchecked_transaction()?;
        let inserted = tx.execute(
            &format!(
                "INSERT INTO {table} SELECT s.* FROM _stage_simple s \
                 JOIN _keep_simple k ON s.rowid = k.stage_row"
            ),
            [],
        )? as u64;
        if inserted != expected {
            bail!(
                "merged {inserted} {table} rows but staged {expected}; rolling the partition back"
            );
        }
        tx.execute_batch("DROP TABLE _stage_simple; DROP TABLE _keep_simple;")?;
        tx.commit()
            .with_context(|| format!("commit {table} merge"))?;
        Ok(inserted)
    }

    /// Load one works partition through the streaming pre-dedupe pipeline.
    /// Per-file: Appender → PK-less staging tables, gated by a shared
    /// `SeenSet` so only first-sightings reach staging → one transaction of
    /// plain bulk INSERTs into the live (PK'd) tables. No `ON CONFLICT` is
    /// needed because the staging tables only contain first-sightings.
    ///
    /// Returns `Err` (after stamping the partition `error` in `_ingest_log`)
    /// on the first failure: a work that cannot be staged, a file that cannot
    /// be read, a merge that does not commit, or more than
    /// `opts.max_parse_errors_per_partition` unusable records. Files of the
    /// partition that committed before the failure stay committed (and in the
    /// `SeenSet`); a re-run skips exactly those works.
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
        opts: &LoadOptions,
    ) -> Result<EntityStats> {
        let part_name = partition_name(partition_dir)?;
        if self.partition_done("works", &part_name)? {
            return Ok(EntityStats {
                skipped_partitions: 1,
                ..Default::default()
            });
        }

        // Per-file processing keeps memory bounded on the giant 2025-11-06
        // partition (2.59 TB). Each JSONL file (~1 GB) gets its own staging
        // tables → merge transaction cycle.
        let mut progress = PartitionProgress::default();
        match self.load_works_partition_inner(partition_dir, seen, opts, &mut progress) {
            Ok(()) => {
                self.log_partition(
                    "works",
                    &part_name,
                    "ok",
                    progress.rows,
                    progress.parse_errors,
                )?;
                Ok(EntityStats {
                    partitions_loaded: 1,
                    rows_inserted: progress.rows,
                    parse_errors: progress.parse_errors,
                    ..Default::default()
                })
            }
            Err(e) => {
                self.log_partition_failure("works", &part_name, &progress);
                Err(e.context(format!("works partition {part_name}")))
            }
        }
    }

    fn load_works_partition_inner(
        &self,
        partition_dir: &Path,
        seen: &mut SeenSet,
        opts: &LoadOptions,
        progress: &mut PartitionProgress,
    ) -> Result<()> {
        for f in partition_files(partition_dir)? {
            self.load_works_file(&f, seen, opts, progress)
                .with_context(|| format!("load file {}", f.display()))?;
        }
        Ok(())
    }

    /// Bulk-load one JSONL file. For each parsed `Work`, gate on the
    /// `SeenSet` and on the file's own staged IDs: first sighting → append to
    /// the staging tables, duplicate → skip. After parsing, flush the
    /// appenders (errors included) and merge the staging tables into the live
    /// tables in one transaction; only then are the staged IDs committed to
    /// the `SeenSet`.
    fn load_works_file(
        &self,
        file: &Path,
        seen: &mut SeenSet,
        opts: &LoadOptions,
        progress: &mut PartitionProgress,
    ) -> Result<()> {
        // Temp tables live for the connection lifetime; the merge drops them
        // and CREATE OR REPLACE clears any leftovers of an aborted file.
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

        // IDs first seen in this file. They are NOT in `seen` until the merge
        // has committed.
        let mut staged: HashSet<u64> = HashSet::new();
        let mut malformed_ids = 0u64;
        let budget = opts
            .max_parse_errors_per_partition
            .saturating_sub(progress.parse_errors);
        let stats = read_jsonl_file::<Work, _>(file, budget, &mut |w| {
            let Some(id) = parse_work_id_u64(&w.id) else {
                // Not a `W<digits>` ID: cannot be deduped or keyed. Counted
                // against the same budget as unparseable lines.
                tracing::warn!(work_id = %w.id, "skipping work with malformed id");
                malformed_ids += 1;
                return Ok(());
            };
            // Streaming dedupe gate: a work already committed from a newer
            // partition (or earlier in this very file) is a stale duplicate.
            if seen.contains(id) || !staged.insert(id) {
                return Ok(());
            }
            stage_work(
                &mut works_app,
                &mut auth_app,
                &mut topic_app,
                &mut concept_app,
                &mut ref_app,
                &w,
            )
            .with_context(|| format!("stage work {}", w.id))
        })?;

        // Appender flush errors are swallowed by its Drop, so flush
        // explicitly: rows that never reached the staging tables must fail
        // the file, not vanish from it.
        for app in [
            &mut works_app,
            &mut auth_app,
            &mut topic_app,
            &mut concept_app,
            &mut ref_app,
        ] {
            app.flush().context("flush staging appender")?;
        }
        drop((works_app, auth_app, topic_app, concept_app, ref_app));

        progress.parse_errors += stats.parse_errors + malformed_ids;
        if progress.parse_errors > opts.max_parse_errors_per_partition {
            bail!(
                "{} unusable records ({} unparseable, {} with a malformed id) in this partition \
                 exceed the limit of {}; nothing from this file was merged",
                progress.parse_errors,
                stats.parse_errors,
                malformed_ids,
                opts.max_parse_errors_per_partition
            );
        }

        self.merge_staged_works(staged.len() as u64)?;
        let merged = staged.len() as u64;
        // The rows are committed; only now may the IDs count as seen.
        seen.commit(staged);
        progress.rows += merged;
        Ok(())
    }

    /// Merge the five staging tables into the live tables in ONE transaction
    /// and drop them. Any error (or a `works` row count that differs from the
    /// number of staged IDs) rolls everything back.
    ///
    /// The SeenSet gate keeps the staging tables free of cross-work
    /// duplicates, but OpenAlex can emit the same edge target twice WITHIN a
    /// single work's `concepts[]` / `topics[]` / `referenced_works[]` array.
    /// The PK on each edge table is the (work_id, edge_id) pair, so intra-work
    /// duplicates are collapsed here at merge time via SELECT DISTINCT.
    ///
    /// `work_authorships` has no PK (the same author can legitimately appear
    /// multiple times on a work via different positions / institutions) so it
    /// is not deduped; the transaction is what keeps a retry from duplicating
    /// it.
    ///
    /// Plain INSERTs, no `ON CONFLICT`: at corpus scale DuckDB's ON CONFLICT
    /// path pins the entire PK index in the buffer pool to do anti-join
    /// lookups and OOMs. A work already in `works` but not in the `SeenSet`
    /// cannot happen (`SeenSet::open` rebuilds the set from `works`); if it
    /// ever does, the PK violation fails the transaction loudly.
    ///
    /// One transaction per file means its dirty pages are held until COMMIT.
    /// An earlier version split the merge into auto-committed statements
    /// because a single transaction hit `Failed to commit` at a 14 GB
    /// `memory_limit`; the limit is now 24 GB.
    fn merge_staged_works(&self, expected_works: u64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let inserted = tx.execute("INSERT INTO works SELECT * FROM _stage_works", [])?;
        if inserted as u64 != expected_works {
            bail!(
                "merged {inserted} works rows but staged {expected_works}; rolling the file back"
            );
        }
        tx.execute_batch(
            "INSERT INTO work_authorships SELECT * FROM _stage_auth;
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
        tx.commit().context("commit works merge")?;
        Ok(())
    }

    /// Load every works partition under `snapshot_root/works/` using the
    /// streaming pre-dedupe pipeline. Walks partitions **newest-first**
    /// (descending by `updated_date`) so the first sighting of any work-ID
    /// is the canonical (newest) version. Maintains a single `SeenSet`
    /// across the whole walk; checkpoints it to a sidecar file every N
    /// partitions for crash recovery.
    ///
    /// `seen_set_path` is where the sidecar checkpoint lives — typically
    /// `~/home-still/data/openalex/seen_set.bin`. The set is opened with
    /// `SeenSet::open`, which reconciles the file against the `works` table.
    /// The `_ingest_log` table independently tracks completed partitions, so
    /// `load_works` is safe to re-run from any state.
    ///
    /// The first failing partition aborts the load and its error is
    /// returned; see the module docs for why continuing is not safe.
    pub fn load_works(
        &self,
        snapshot_root: &Path,
        seen_set_path: std::path::PathBuf,
        opts: &LoadOptions,
    ) -> Result<EntityStats> {
        let partitions = newest_first_partitions(&snapshot_root.join("works"))?;

        let mut seen = SeenSet::open(&self.conn, seen_set_path)?;
        let mut total = EntityStats::default();
        for part in partitions {
            let pname = partition_name(&part)?;
            // Chained Display ({e:#}) walks the anyhow source chain so the
            // root cause is visible.
            let s = self
                .load_works_partition(&part, &mut seen, opts)
                .inspect_err(|e| {
                    tracing::error!(partition = %pname, error = %format!("{e:#}"), "partition failed; aborting load");
                })
                .with_context(|| {
                    format!(
                        "works load aborted at {pname}: partitions are walked newest-first, so \
                         loading older ones past a failure would store stale versions of its works"
                    )
                })?;
            total.partitions_loaded += s.partitions_loaded;
            total.skipped_partitions += s.skipped_partitions;
            total.rows_inserted += s.rows_inserted;
            total.parse_errors += s.parse_errors;
            // Only a partition that committed counts toward a checkpoint, and
            // the checkpoint is written after that commit.
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
    ///
    /// Each record is gated on `seen` (the `authors` set, see
    /// [`Self::load_authors`]): an author already committed from a newer
    /// partition, or earlier in this one, is a stale duplicate and is skipped.
    /// A record whose ID is not `A<digits>` cannot be keyed: it counts against
    /// `opts.max_parse_errors_per_partition` like an unparseable line, as for
    /// works.
    ///
    /// The partition is staged in a temp table and merged into `authors` in
    /// ONE transaction (the works loader's stage → merge path), so a failure
    /// anywhere before the commit leaves the live table and `seen` untouched
    /// and the partition can simply be re-run. Unlike works, a partition is
    /// staged whole (not per file).
    ///
    /// **Walk order matters**, as for [`Self::load_works_partition`]: call
    /// this newest-first (use [`Self::load_authors`]) or older records win.
    pub fn load_authors_partition(
        &self,
        partition_dir: &Path,
        seen: &mut SeenSet,
        opts: &LoadOptions,
    ) -> Result<EntityStats> {
        let part_name = partition_name(partition_dir)?;
        if self.partition_done("authors", &part_name)? {
            return Ok(EntityStats {
                skipped_partitions: 1,
                ..Default::default()
            });
        }

        let mut progress = PartitionProgress::default();
        let loaded = self
            .append_authors(partition_dir, seen, opts, &mut progress)
            .with_context(|| format!("authors partition {part_name}"));
        if let Err(e) = loaded {
            self.log_partition_failure("authors", &part_name, &progress);
            return Err(e);
        }
        self.log_partition(
            "authors",
            &part_name,
            "ok",
            progress.rows,
            progress.parse_errors,
        )?;
        Ok(EntityStats {
            partitions_loaded: 1,
            rows_inserted: progress.rows,
            parse_errors: progress.parse_errors,
            ..Default::default()
        })
    }

    fn append_authors(
        &self,
        partition_dir: &Path,
        seen: &mut SeenSet,
        opts: &LoadOptions,
        progress: &mut PartitionProgress,
    ) -> Result<()> {
        // The temp table lives for the connection lifetime; CREATE OR REPLACE
        // clears the leftovers of an aborted partition. Same columns as the
        // live table, no PK: the gate, not a constraint, keeps it duplicate-free.
        self.conn.execute_batch(
            "CREATE OR REPLACE TEMP TABLE _stage_authors AS SELECT * FROM authors LIMIT 0;",
        )?;
        let merged = self
            .stage_authors(partition_dir, seen, opts, progress)
            .and_then(|staged| {
                self.merge_staged_authors(staged.len() as u64)?;
                Ok(staged)
            });
        match merged {
            // The rows are committed; only now may the IDs count as seen.
            Ok(staged) => {
                seen.commit(staged);
                Ok(())
            }
            Err(e) => {
                // Nothing was committed: the live table was never touched and
                // the error stamp must not claim the staged rows.
                progress.rows = 0;
                if let Err(drop_err) = self
                    .conn
                    .execute_batch("DROP TABLE IF EXISTS _stage_authors;")
                {
                    tracing::warn!(error = %format!("{drop_err:#}"), "could not drop _stage_authors after a failed partition");
                }
                Err(e)
            }
        }
    }

    /// Stream the partition through the first-sighting gate into
    /// `_stage_authors`; returns the staged IDs, which are NOT yet in `seen`.
    fn stage_authors(
        &self,
        partition_dir: &Path,
        seen: &SeenSet,
        opts: &LoadOptions,
        progress: &mut PartitionProgress,
    ) -> Result<HashSet<u64>> {
        let mut app = self.conn.appender("_stage_authors")?;
        let mut staged: HashSet<u64> = HashSet::new();
        let mut malformed_ids = 0u64;
        let parsed =
            read_partition::<Author, _>(partition_dir, opts.max_parse_errors_per_partition, |a| {
                let Some(id) = parse_entity_id_u64(&a.id, Some(AUTHOR_ID_PREFIX)) else {
                    // Not an `A<digits>` ID: cannot be deduped or keyed.
                    tracing::warn!(author_id = %a.id, "skipping author with malformed id");
                    malformed_ids += 1;
                    return Ok(());
                };
                // Streaming dedupe gate: an author already committed from a
                // newer partition (or earlier in this one) is a stale duplicate.
                if seen.contains(id) || !staged.insert(id) {
                    return Ok(());
                }
                append_author(&mut app, &a).with_context(|| format!("append author {}", a.id))
            });
        // Report what was staged even when the read failed part-way.
        progress.rows = staged.len() as u64;
        let stats = parsed?;
        progress.parse_errors = stats.parse_errors + malformed_ids;
        if progress.parse_errors > opts.max_parse_errors_per_partition {
            bail!(
                "{} unusable records ({} unparseable, {} with a malformed id) in this partition \
                 exceed the limit of {}; nothing from this partition was merged",
                progress.parse_errors,
                stats.parse_errors,
                malformed_ids,
                opts.max_parse_errors_per_partition
            );
        }
        // Appender flush errors are swallowed by its Drop, so flush
        // explicitly: rows that never reached the staging table must fail
        // the partition, not vanish from it.
        app.flush().context("flush authors staging appender")?;
        Ok(staged)
    }

    /// Merge `_stage_authors` into `authors` and drop it, in one transaction.
    /// Plain INSERT, no `ON CONFLICT`: the gate keeps the stage free of
    /// duplicates and of IDs already in the table (the set is rebuilt from
    /// it), so a PK violation means the invariant broke and fails the
    /// transaction, rolling it back.
    fn merge_staged_authors(&self, expected: u64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let inserted = tx.execute("INSERT INTO authors SELECT * FROM _stage_authors", [])?;
        if inserted as u64 != expected {
            bail!(
                "merged {inserted} authors rows but staged {expected}; rolling the partition back"
            );
        }
        tx.execute_batch("DROP TABLE _stage_authors;")?;
        tx.commit().context("commit authors merge")?;
        Ok(())
    }

    /// Load every authors partition under `snapshot_root/authors/`, walking
    /// **newest-first** with one `authors` [`SeenSet`] (rebuilt from the table
    /// on entry, no checkpoint file; see `seen_set.rs`) so the newest version
    /// of each author wins. The first failing partition aborts the load; see
    /// the module docs for why continuing is not safe.
    pub fn load_authors(&self, snapshot_root: &Path, opts: &LoadOptions) -> Result<EntityStats> {
        let partitions = newest_first_partitions(&snapshot_root.join("authors"))?;
        let mut seen = SeenSet::from_table(&self.conn, "authors", Some(AUTHOR_ID_PREFIX))?;
        let mut total = EntityStats::default();
        for part in partitions {
            let s = self.load_authors_partition(&part, &mut seen, opts)?;
            total.partitions_loaded += s.partitions_loaded;
            total.skipped_partitions += s.skipped_partitions;
            total.rows_inserted += s.rows_inserted;
            total.parse_errors += s.parse_errors;
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

/// One `_ingest_log` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestLogEntry {
    pub entity: String,
    pub partition: String,
    /// `ok` or `error`.
    pub status: String,
    pub rows: u64,
    /// Records skipped in the partition (see `LoadOptions`).
    pub parse_errors: u64,
}

/// Rows committed / records skipped so far in the partition being loaded;
/// kept outside the load's `Result` so the `error` stamp can report them.
#[derive(Debug, Default)]
struct PartitionProgress {
    rows: u64,
    parse_errors: u64,
}

/// The partition directories of an entity, **newest first**.
/// `list_partitions` sorts ascending by name; the OpenAlex partition naming
/// convention is `updated_date=YYYY-MM-DD`, so reverse alphabetical = reverse
/// chronological. Every loader walks in this order: the first sighting of an
/// ID must be its newest version.
fn newest_first_partitions(entity_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut partitions = list_partitions(entity_dir)?;
    partitions.reverse();
    Ok(partitions)
}

fn partition_name(partition_dir: &Path) -> Result<String> {
    Ok(partition_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("partition name not utf-8: {}", partition_dir.display()))?
        .to_string())
}

/// `s` as a single-quoted SQL string literal (quotes doubled; DuckDB string
/// literals have no backslash escapes).
fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn append_author(app: &mut duckdb::Appender, a: &Author) -> Result<()> {
    let id = strip_openalex_id(&a.id).to_string();
    let last_known_inst_id = a
        .last_known_institutions
        .first()
        .and_then(|i| i.id.as_deref())
        .map(|id| strip_openalex_id(id).to_string());
    let affiliations_json =
        serde_json::to_string(&a.affiliations).context("serialize author affiliations")?;
    let ids_json = serde_json::to_string(&a.ids).context("serialize author ids")?;
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
/// Appender (fastest path). The staging tables have no PK / NOT NULL, but a
/// value can still be refused (a year outside `USMALLINT`, an unparseable
/// date): that error is returned and fails the file, because a work row that
/// was dropped while its edges were kept would be an orphan. Conflict
/// handling happens during the bulk merge.
fn stage_work(
    works_app: &mut duckdb::Appender,
    auth_app: &mut duckdb::Appender,
    topic_app: &mut duckdb::Appender,
    concept_app: &mut duckdb::Appender,
    ref_app: &mut duckdb::Appender,
    w: &Work,
) -> Result<()> {
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

    works_app.append_row(params![
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
    ])?;

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
            auth_app.append_row(params![
                work_id,
                author_id,
                pos,
                raw_aff,
                Option::<String>::None,
            ])?;
        } else {
            for inst in &a.institutions {
                let inst_id = inst
                    .id
                    .as_deref()
                    .map(|id| strip_openalex_id(id).to_string());
                auth_app.append_row(params![work_id, author_id, pos, raw_aff, inst_id])?;
            }
        }
    }

    for t in &w.topics {
        let tid = strip_openalex_id(&t.id).to_string();
        topic_app.append_row(params![work_id, tid, t.score])?;
    }
    for c in &w.concepts {
        let cid = strip_openalex_id(&c.id).to_string();
        concept_app.append_row(params![work_id, cid, c.score])?;
    }
    for r in &w.referenced_works {
        let rid = strip_openalex_id(r).to_string();
        ref_app.append_row(params![work_id, rid])?;
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy)]
pub struct EntityStats {
    pub partitions_loaded: u64,
    pub skipped_partitions: u64,
    pub rows_inserted: u64,
    /// Records skipped (unparseable or malformed id) in loaded partitions,
    /// within the `LoadOptions` budget. Zero for the SQL-driven entities.
    pub parse_errors: u64,
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

    /// Type letter of this entity's IDs, for the integer seen-set key
    /// ([`parse_entity_id_u64`]); `None` for domains, fields and subfields,
    /// whose stored ID is digits only (`3`, `17`, `1702`).
    fn id_prefix(self) -> Option<char> {
        match self {
            SimpleEntity::Concepts => Some('C'),
            SimpleEntity::Topics => Some('T'),
            SimpleEntity::Domains | SimpleEntity::Fields | SimpleEntity::Subfields => None,
            SimpleEntity::Sources => Some('S'),
            SimpleEntity::Institutions => Some('I'),
            SimpleEntity::Funders => Some('F'),
            SimpleEntity::Publishers => Some('P'),
        }
    }

    /// SQL that reads one partition's JSONL files into the temp table
    /// `_stage_simple`, projected to the live table's columns in order (the
    /// merge is `INSERT INTO <table> SELECT s.* …`, so the column order is
    /// the contract). The file glob is the statement's single `?` parameter.
    ///
    /// It only stages: which rows reach the live table is decided by the
    /// `SeenSet` gate (newest partition wins), not by an `INSERT OR IGNORE`
    /// that would keep the first-loaded, i.e. the oldest, copy.
    fn stage_sql(self) -> String {
        format!(
            "CREATE OR REPLACE TEMP TABLE _stage_simple AS {}",
            self.select_sql()
        )
    }

    /// The projection behind [`Self::stage_sql`].
    fn select_sql(self) -> String {
        let strip = "regexp_replace";
        match self {
            SimpleEntity::Concepts => format!(
                r#"
                SELECT
                    {strip}(id, '^https://openalex.org/', '') AS openalex_id,
                    display_name,
                    level,
                    description,
                    wikidata,
                    works_count,
                    cited_by_count,
                    to_json(ancestors) AS ancestors
                FROM read_json(?, format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Topics => format!(
                r#"
                SELECT
                    {strip}(id, '^https://openalex.org/', '') AS openalex_id,
                    display_name,
                    description,
                    keywords,
                    {strip}(subfield.id, '^https://openalex.org/subfields/', '') AS subfield_id,
                    {strip}(field.id, '^https://openalex.org/fields/', '') AS field_id,
                    {strip}(domain.id, '^https://openalex.org/domains/', '') AS domain_id
                FROM read_json(?, format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Domains => format!(
                r#"
                SELECT
                    {strip}(id, '^https://openalex.org/domains/', '') AS openalex_id,
                    display_name
                FROM read_json(?, format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Fields => format!(
                r#"
                SELECT
                    {strip}(id, '^https://openalex.org/fields/', '') AS openalex_id,
                    display_name,
                    {strip}(domain.id, '^https://openalex.org/domains/', '') AS domain_id
                FROM read_json(?, format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Subfields => format!(
                r#"
                SELECT
                    {strip}(id, '^https://openalex.org/subfields/', '') AS openalex_id,
                    display_name,
                    {strip}(field.id, '^https://openalex.org/fields/', '') AS field_id,
                    {strip}(domain.id, '^https://openalex.org/domains/', '') AS domain_id
                FROM read_json(?, format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Sources => format!(
                r#"
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
                FROM read_json(?, format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Institutions => format!(
                r#"
                SELECT
                    {strip}(id, '^https://openalex.org/', '') AS openalex_id,
                    display_name,
                    country_code,
                    type,
                    ror,
                    works_count,
                    cited_by_count
                FROM read_json(?, format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Funders => format!(
                r#"
                SELECT
                    {strip}(id, '^https://openalex.org/', '') AS openalex_id,
                    display_name,
                    country_code,
                    works_count,
                    cited_by_count
                FROM read_json(?, format='newline_delimited', auto_detect=true);
                "#
            ),
            SimpleEntity::Publishers => format!(
                r#"
                SELECT
                    {strip}(id, '^https://openalex.org/', '') AS openalex_id,
                    display_name,
                    works_count,
                    cited_by_count
                FROM read_json(?, format='newline_delimited', auto_detect=true);
                "#
            ),
        }
    }
}

/// `INSTALL fts` then `LOAD fts` on `conn`; see [`OpenAlexDb::install_fts`].
fn install_fts_on(conn: &Connection) -> Result<()> {
    conn.execute_batch("INSTALL fts;").context(
        "cannot install the DuckDB fts extension: `hs openalex install-fts` downloads it from \
         DuckDB's extension repository, so it needs network access (once per host and DuckDB \
         version)",
    )?;
    conn.execute_batch("LOAD fts;")
        .context("the fts extension installed but will not LOAD")
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

    /// Point `conn` at an empty extension directory and an unreachable
    /// extension repository, so neither an `fts` copy cached in the host's
    /// `~/.duckdb` nor network access on the machine running the tests can
    /// change the outcome.
    #[cfg(not(windows))]
    fn without_the_fts_extension(conn: &Connection, extension_dir: &Path) {
        conn.execute_batch(&format!(
            "SET extension_directory={}; \
             SET custom_extension_repository='http://127.0.0.1:1';",
            sql_string_literal(extension_dir.to_str().unwrap())
        ))
        .unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn build_fts_without_the_extension_names_install_fts_and_never_downloads() {
        let tmp = tempfile::tempdir().unwrap();
        let db = OpenAlexDb::open(&tmp.path().join("oa.duckdb")).unwrap();
        without_the_fts_extension(db.raw(), &tmp.path().join("ext"));

        let msg = format!("{:#}", db.build_fts().unwrap_err());
        assert!(
            msg.contains(
                "fts extension not installed — run `hs openalex install-fts` (needs network once)"
            ),
            "{msg}"
        );
        // `LOAD` found nothing and stopped: it did not go to the (unreachable)
        // repository, and the readiness sentinel was not written.
        assert!(!msg.contains("Failed to download"), "{msg}");
        let stamped: u64 = db
            .raw()
            .query_row("SELECT COUNT(*) FROM _corpus_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stamped, 0);
    }

    #[cfg(not(windows))]
    #[test]
    fn install_fts_offline_fails_loudly_and_says_what_it_needs() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        without_the_fts_extension(&conn, &tmp.path().join("ext"));

        let msg = format!("{:#}", install_fts_on(&conn).unwrap_err());
        assert!(
            msg.contains(
                "`hs openalex install-fts` downloads it from DuckDB's extension repository"
            ) && msg.contains("needs network access"),
            "{msg}"
        );
        // The DuckDB cause stays in the chain.
        assert!(msg.contains("Failed to download"), "{msg}");
    }
}
