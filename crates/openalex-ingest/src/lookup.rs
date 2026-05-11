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
//! DOI strings are stored case-sensitive (no `lower()` at ingest), so
//! callers must pass DOIs in the same form OpenAlex provides — typically
//! lowercase with the `https://doi.org/` prefix already stripped.

use anyhow::Result;
use duckdb::Connection;

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

/// Look up one work row by DOI **or** OpenAlex ID. Returns `Ok(None)` when
/// the identifier doesn't resolve. The SQL matches the `openalex_get` MCP
/// tool's lookup so behavior is consistent across read paths.
pub fn lookup_work_abstract_by_doi(
    conn: &Connection,
    id_or_doi: &str,
) -> Result<Option<WorkAbstract>> {
    let mut stmt = conn.prepare(
        "SELECT openalex_id, doi, title, abstract_text, publication_year, cited_by_count
         FROM works
         WHERE openalex_id = ? OR doi = ?
         LIMIT 1",
    )?;
    let mut rows = stmt.query(duckdb::params![id_or_doi, id_or_doi])?;
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
