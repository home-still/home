//! Read-side helpers for querying the OpenAlex DuckDB catalog by external
//! identifiers (DOI, OpenAlex ID). These are pure read functions — they take
//! a `&Connection` borrowed from either an `OpenAlexDb::raw()` or a fresh
//! read-only handle, and return small POD structs that callers can serialize
//! or re-shape as needed.
//!
//! `lookup_work_abstract_by_doi` exists because the `paper_abstracts` distill
//! pipeline (and any future per-DOI enrichment path) needs the same row
//! shape that `openalex_get` returns in hs-mcp. Keeping the SQL in one
//! place avoids the schema drifting between callers.
//!
//! `abstract_text` is plain text by the time it reaches this layer —
//! `parser::reconstruct_abstract` already ran at ingest, so the DuckDB
//! column holds the rebuilt sentence form, not the inverted index.
//!
//! DOIs are stored as OpenAlex provides them with only the `https://doi.org/`
//! prefix stripped at ingest (no `lower()`; changing that would need a full
//! re-ingest). OpenAlex DOIs are lowercase, and so is every DOI the rest of
//! the workspace produces, so lookups normalize the *query* the same way
//! (prefix stripped, lowercased): the stored column stays untouched and the
//! `idx_works_doi` index stays usable. Check the assumption on a live
//! database with `SELECT count(*) FROM works WHERE doi <> lower(doi)`; a
//! non-zero count means those rows are unreachable by DOI lookup.

use anyhow::Result;
use duckdb::Connection;

use crate::parser::{parse_work_id_u64, strip_openalex_id};

/// Minimal work-row projection used by the abstracts pipeline and other
/// per-DOI lookups. Field names match the underlying `works` columns so
/// downstream code can stuff this straight into a Qdrant payload.
#[derive(Debug, Clone)]
pub struct WorkAbstract {
    pub openalex_id: String,
    pub doi: Option<String>,
    pub title: Option<String>,
    pub abstract_text: Option<String>,
    pub publication_year: Option<u16>,
    pub cited_by_count: Option<u64>,
}

/// Primary-key lookup.
const BY_OPENALEX_ID_SQL: &str =
    "SELECT openalex_id, doi, title, abstract_text, publication_year, \
     cited_by_count FROM works WHERE openalex_id = ? LIMIT 1";

/// Point lookup on `idx_works_doi` (built by `hs openalex build-indexes`;
/// without it this is a scan).
const BY_DOI_SQL: &str = "SELECT openalex_id, doi, title, abstract_text, publication_year, \
     cited_by_count FROM works WHERE doi = ? LIMIT 1";

/// How a lookup string is resolved. An OpenAlex work ID (`W123…`, bare or as
/// a URL) and a DOI (`10.…`) can never be confused, so each goes straight to
/// the one column that holds it. A single `openalex_id = ? OR doi = ?`
/// predicate cannot use either index and scans all of `works`.
#[derive(Debug, PartialEq, Eq)]
enum Key {
    OpenAlexId(String),
    Doi(String),
}

fn classify(id_or_doi: &str) -> Key {
    let trimmed = id_or_doi.trim();
    let bare = strip_openalex_id(trimmed);
    if parse_work_id_u64(bare).is_some() {
        return Key::OpenAlexId(bare.to_string());
    }
    let lower = trimmed.to_lowercase();
    let doi = [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
        "doi:",
    ]
    .iter()
    .find_map(|p| lower.strip_prefix(p))
    .unwrap_or(&lower);
    Key::Doi(doi.trim().to_string())
}

/// Look up one work row by OpenAlex ID **or** DOI. Returns `Ok(None)` when
/// the identifier doesn't resolve. Same row shape as the `openalex_get` MCP
/// tool.
pub fn lookup_work_abstract_by_doi(
    conn: &Connection,
    id_or_doi: &str,
) -> Result<Option<WorkAbstract>> {
    let (sql, key) = match classify(id_or_doi) {
        Key::OpenAlexId(id) => (BY_OPENALEX_ID_SQL, id),
        Key::Doi(doi) => (BY_DOI_SQL, doi),
    };
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(duckdb::params![key])?;
    match rows.next()? {
        Some(row) => Ok(Some(WorkAbstract {
            openalex_id: row.get::<_, String>(0)?,
            doi: row.get::<_, Option<String>>(1)?,
            title: row.get::<_, Option<String>>(2)?,
            abstract_text: row.get::<_, Option<String>>(3)?,
            publication_year: row.get::<_, Option<u16>>(4)?,
            cited_by_count: row.get::<_, Option<u64>>(5)?,
        })),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::duckdb_loader::OpenAlexDb;

    /// A synthetic `works` table built like production: DDL (PK on
    /// `openalex_id`) from `OpenAlexDb::open`, bulk rows, then the
    /// post-load indexes.
    fn indexed_db(dir: &std::path::Path, rows: u64) -> OpenAlexDb {
        let db = OpenAlexDb::open(&dir.join("oa.duckdb")).unwrap();
        db.raw()
            .execute_batch(&format!(
                "INSERT INTO works(openalex_id, doi, title, abstract_text, publication_year, cited_by_count)
                 SELECT 'W' || i, '10.1000/w' || i, 'title ' || i, 'abstract ' || i, 2000 + (i % 25), i
                 FROM range(1, {}) t(i) ORDER BY hash(i);",
                rows + 1
            ))
            .unwrap();
        db.build_post_load_indexes().unwrap();
        db
    }

    #[test]
    fn classify_separates_ids_from_dois_and_normalizes_the_doi() {
        let id = |s: &str| Key::OpenAlexId(s.to_string());
        let doi = |s: &str| Key::Doi(s.to_string());
        assert_eq!(classify("W2741809807"), id("W2741809807"));
        assert_eq!(
            classify("https://openalex.org/W2741809807"),
            id("W2741809807")
        );
        assert_eq!(classify("  W5 "), id("W5"));
        assert_eq!(classify("10.1000/ABC.def"), doi("10.1000/abc.def"));
        assert_eq!(classify("https://doi.org/10.1000/ABC"), doi("10.1000/abc"));
        assert_eq!(
            classify("HTTPS://DX.DOI.ORG/10.1000/ABC"),
            doi("10.1000/abc")
        );
        assert_eq!(classify("doi:10.1000/ABC"), doi("10.1000/abc"));
        // Not a work ID, so it is looked up as a DOI and simply misses.
        assert_eq!(classify("A123"), doi("a123"));
        assert_eq!(classify("W12x"), doi("w12x"));
    }

    #[test]
    fn resolves_by_openalex_id_and_by_doi_in_any_case_or_form() {
        let tmp = tempfile::tempdir().unwrap();
        let db = indexed_db(tmp.path(), 50);
        for query in [
            "W7",
            "https://openalex.org/W7",
            "10.1000/w7",
            "10.1000/W7",
            "https://doi.org/10.1000/W7",
        ] {
            let hit = lookup_work_abstract_by_doi(db.raw(), query)
                .unwrap()
                .unwrap_or_else(|| panic!("{query} must resolve"));
            assert_eq!(hit.openalex_id, "W7", "{query}");
            assert_eq!(hit.doi.as_deref(), Some("10.1000/w7"));
            assert_eq!(hit.title.as_deref(), Some("title 7"));
            assert_eq!(hit.abstract_text.as_deref(), Some("abstract 7"));
            assert_eq!(hit.publication_year, Some(2007));
            assert_eq!(hit.cited_by_count, Some(7));
        }
    }

    #[test]
    fn unknown_identifiers_resolve_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        let db = indexed_db(tmp.path(), 5);
        for query in ["W999", "10.1000/w999", "not an id", ""] {
            assert!(
                lookup_work_abstract_by_doi(db.raw(), query)
                    .unwrap()
                    .is_none(),
                "{query:?}"
            );
        }
    }

    #[test]
    fn an_openalex_id_never_matches_through_the_doi_column() {
        // A row whose DOI text equals another row's ID must not shadow it (the
        // old `openalex_id = ? OR doi = ?` matched both rows).
        let tmp = tempfile::tempdir().unwrap();
        let db = indexed_db(tmp.path(), 3);
        db.raw()
            .execute_batch(
                "INSERT INTO works(openalex_id, doi, title) VALUES ('W500', 'W2', 'impostor')",
            )
            .unwrap();
        let hit = lookup_work_abstract_by_doi(db.raw(), "W2")
            .unwrap()
            .unwrap();
        assert_eq!(hit.title.as_deref(), Some("title 2"));
    }

    fn explain(db: &OpenAlexDb, sql: &str, key: &str) -> String {
        let mut stmt = db.raw().prepare(&format!("EXPLAIN {sql}")).unwrap();
        stmt.query_row(duckdb::params![key], |r| r.get::<_, String>(1))
            .unwrap()
    }

    /// RA-114: `openalex_id = ? OR doi = ?` cannot be pushed into the scan
    /// (DuckDB evaluates it in a FILTER over every row: ~700 ms per call at
    /// 3M rows against ~2.5 ms for a single-column equality, i.e. seconds per
    /// call at the 56M-row production size). Each lookup is a lone equality
    /// on the key column, pushed into the table scan, on a table built like
    /// production (PK + `idx_works_doi`, shuffled row order).
    #[test]
    fn lookups_are_pushed_into_the_scan_not_a_filter_over_every_row() {
        let tmp = tempfile::tempdir().unwrap();
        let db = indexed_db(tmp.path(), 200_000);

        // Detector sanity: the old predicate does produce a FILTER operator.
        let old = "SELECT openalex_id FROM works WHERE openalex_id = ? OR doi = ? LIMIT 1";
        let mut stmt = db.raw().prepare(&format!("EXPLAIN {old}")).unwrap();
        let old_plan: String = stmt
            .query_row(duckdb::params!["W777", "10.1000/w777"], |r| r.get(1))
            .unwrap();
        assert!(
            old_plan.contains("FILTER"),
            "detector is blind:\n{old_plan}"
        );

        for (sql, key) in [(BY_OPENALEX_ID_SQL, "W777"), (BY_DOI_SQL, "10.1000/w777")] {
            let plan = explain(&db, sql, key);
            assert!(
                plan.contains("Filters:") && !plan.contains("FILTER"),
                "`{sql}` must push its equality into the scan:\n{plan}"
            );
        }
    }
}
