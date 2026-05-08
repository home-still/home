//! DuckDB connection management + entity loaders.
//!
//! Two ingest patterns:
//!
//! 1. **Simple entities** (concepts, topics, sources, institutions, authors,
//!    domains, fields, subfields, funders, publishers): DuckDB reads the
//!    JSONL files natively via `read_json` and projects / transforms columns
//!    inline. No Rust round-trip — DuckDB handles arrays/structs directly.
//!
//! 2. **Works**: Rust parses each record (abstract reconstruction is the
//!    irreducible reason Rust is in the loop here), then fans rows out to
//!    five Appenders: `works`, `work_authorships`, `work_topics`,
//!    `work_concepts`, `work_references`.
//!
//! Resumability: every partition load consults `_ingest_log` first and skips
//! if status='ok'. Partial progress within a partition is NOT tracked — a
//! crash mid-partition means re-doing that partition from scratch (the
//! Appender's pending rows are dropped on connection close, so the partial
//! state is invisible).

use anyhow::{anyhow, Context, Result};
use duckdb::{params, Connection};
use std::path::Path;

use crate::model::{Author, Work};
use crate::parser::{reconstruct_abstract, strip_doi, strip_openalex_id};
use crate::reader::{list_partitions, read_partition};
use crate::schema::{POST_LOAD_INDEXES, SCHEMA_DDL};

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
        let pragmas = format!(
            "PRAGMA memory_limit='8GB';\n\
             PRAGMA temp_directory='{}';\n\
             PRAGMA threads=6;\n\
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

    /// Apply the post-load secondary indexes. Slow on `works` (~minutes) and
    /// `work_references` (~hours at full corpus); call once after bulk load.
    pub fn build_post_load_indexes(&self) -> Result<()> {
        self.conn
            .execute_batch(POST_LOAD_INDEXES)
            .context("build post-load indexes")
    }

    /// Build the BM25 full-text index over works.title + works.abstract_text.
    /// Loads the FTS extension if not already present. Slow at full corpus
    /// (~hour+) — run once after works ingest.
    pub fn build_fts(&self) -> Result<()> {
        self.conn
            .execute_batch(
                r#"
                INSTALL fts;
                LOAD fts;
                PRAGMA create_fts_index('works', 'openalex_id', 'title', 'abstract_text', overwrite=1);
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

    /// Load one works partition. Uses prepared `INSERT ... ON CONFLICT DO
    /// NOTHING` statements (not Appender) because OpenAlex re-emits the same
    /// work in multiple `updated_date=*` partitions when it gets updated, and
    /// Appender has no PK-tolerance — it errors per row and rolled back whole
    /// batches in the previous implementation, leaving asymmetric data (works
    /// table populated but work_authorships near-empty).
    ///
    /// Cost: ~3x slower than Appender in steady state, but correct and
    /// idempotent across cross-partition duplicates.
    pub fn load_works_partition(&self, partition_dir: &Path) -> Result<EntityStats> {
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

        // Wrap the partition in one transaction so the per-row INSERTs don't
        // each pay an autocommit fsync. Commit at the end alongside the
        // _ingest_log row.
        self.conn.execute_batch("BEGIN TRANSACTION")?;

        let result = self.load_works_partition_inner(partition_dir);

        match result {
            Ok((rows, parse_errors)) => {
                self.log_partition("works", &part_name, "ok", rows, parse_errors)?;
                self.conn.execute_batch("COMMIT")?;
                Ok(EntityStats {
                    partitions_loaded: 1,
                    rows_inserted: rows,
                    ..Default::default()
                })
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn load_works_partition_inner(&self, partition_dir: &Path) -> Result<(u64, u64)> {
        let mut stmt_works = self.conn.prepare(
            "INSERT INTO works(openalex_id, doi, title, abstract_text, publication_year, \
             publication_date, language, type, cited_by_count, is_retracted, is_oa, oa_url, \
             primary_source_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (openalex_id) DO NOTHING",
        )?;
        let mut stmt_auth = self.conn.prepare(
            "INSERT INTO work_authorships(work_id, author_id, author_position, \
             raw_affiliation_string, institution_id) VALUES (?, ?, ?, ?, ?)",
        )?;
        let mut stmt_topic = self.conn.prepare(
            "INSERT INTO work_topics(work_id, topic_id, score) VALUES (?, ?, ?) \
             ON CONFLICT (work_id, topic_id) DO NOTHING",
        )?;
        let mut stmt_concept = self.conn.prepare(
            "INSERT INTO work_concepts(work_id, concept_id, score) VALUES (?, ?, ?) \
             ON CONFLICT (work_id, concept_id) DO NOTHING",
        )?;
        let mut stmt_ref = self.conn.prepare(
            "INSERT INTO work_references(work_id, referenced_work_id) VALUES (?, ?) \
             ON CONFLICT (work_id, referenced_work_id) DO NOTHING",
        )?;

        let mut rows = 0u64;
        let parse_stats = read_partition::<Work, _>(partition_dir, |w| {
            insert_work(
                &mut stmt_works,
                &mut stmt_auth,
                &mut stmt_topic,
                &mut stmt_concept,
                &mut stmt_ref,
                &w,
            );
            rows += 1;
        })?;

        Ok((rows, parse_stats.parse_errors))
    }

    pub fn load_works(&self, snapshot_root: &Path) -> Result<EntityStats> {
        let works_dir = snapshot_root.join("works");
        let partitions = list_partitions(&works_dir)?;
        let mut total = EntityStats::default();
        for part in partitions {
            let s = self.load_works_partition(&part)?;
            total.partitions_loaded += s.partitions_loaded;
            total.skipped_partitions += s.skipped_partitions;
            total.rows_inserted += s.rows_inserted;
        }
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

/// Insert one Work and its edges via prepared statements with ON CONFLICT
/// handling. Errors on individual rows are swallowed silently — the catalog
/// is read-only ground truth and most failures here are cross-partition
/// duplicates we deliberately want to skip.
fn insert_work(
    stmt_works: &mut duckdb::Statement,
    stmt_auth: &mut duckdb::Statement,
    stmt_topic: &mut duckdb::Statement,
    stmt_concept: &mut duckdb::Statement,
    stmt_ref: &mut duckdb::Statement,
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

    let _ = stmt_works.execute(params![
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
            let _ = stmt_auth.execute(params![
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
                let _ = stmt_auth.execute(params![work_id, author_id, pos, raw_aff, inst_id]);
            }
        }
    }

    for t in &w.topics {
        let tid = strip_openalex_id(&t.id).to_string();
        let _ = stmt_topic.execute(params![work_id, tid, t.score]);
    }
    for c in &w.concepts {
        let cid = strip_openalex_id(&c.id).to_string();
        let _ = stmt_concept.execute(params![work_id, cid, c.score]);
    }
    for r in &w.referenced_works {
        let rid = strip_openalex_id(r).to_string();
        let _ = stmt_ref.execute(params![work_id, rid]);
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
