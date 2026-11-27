//! The local OpenAlex corpus: finding it at startup and querying it.
//!
//! `discover` and `graph` read it when asked for `source: "openalex"`.
//! The corpus is a DuckDB file that `hs openalex` builds; this server only
//! ever opens it read-only.

use std::sync::{Arc, Mutex};

/// What this host has of the local OpenAlex corpus. Decided once at startup:
/// the read-only DuckDB handle sees the file as it was when it was opened, so
/// a corpus that is built later is picked up by a restart.
#[derive(Clone)]
pub(crate) enum OpenAlexCorpus {
    /// The corpus is built and open.
    Ready(Arc<Mutex<duckdb::Connection>>),
    /// The corpus cannot be read. The text says why and what to do about it,
    /// and is what `discover` and `graph` answer for `source: "openalex"`.
    Unavailable(String),
}

/// The `openalex:` section of `~/.home-still/config.yaml` as hs-mcp reads it.
/// `snapshot_dir` belongs to `hs openalex load`; it is accepted here so the
/// shared section is not rejected, and not used. hs-mcp does not own this
/// section, and deployed configs carry stale keys in it, so an unknown key is
/// a warning (the convention for such sections), not a startup error.
#[derive(Debug, serde::Deserialize)]
struct OpenAlexSection {
    db_path: String,
    #[serde(default)]
    #[allow(dead_code)]
    snapshot_dir: Option<String>,
}

impl OpenAlexCorpus {
    /// Open the local OpenAlex DuckDB read-only, using `openalex.db_path` from
    /// the already-loaded config file. `Unavailable` covers the legitimate
    /// "OpenAlex is not usable on this host right now" states: no `openalex:`
    /// section, a database file that `hs openalex load-works` has not created
    /// yet, a file another process holds open for writing, and a corpus whose
    /// build has not finished. A section that is present but malformed (a
    /// typo, a missing `db_path`) is an error that stops the server: it must
    /// not read as "not configured".
    pub(crate) fn open(file: &hs_common::config_file::ConfigFile) -> anyhow::Result<Self> {
        if let Some(raw) = file.section_json("openalex")? {
            hs_common::config_file::warn_unknown_keys(
                file.path(),
                "openalex",
                &raw,
                &serde_json::json!({ "db_path": "", "snapshot_dir": "" }),
                &[],
            );
        }
        let Some(section) = file.section::<OpenAlexSection>("openalex")? else {
            let reason = format!(
            "the local OpenAlex corpus is not configured on this host: add an `openalex:` section \
             with `db_path` to {}, build the corpus with {BUILD_COMMANDS}, then restart hs-mcp",
            file.path().display()
        );
            tracing::info!("{reason}");
            return Ok(Self::Unavailable(reason));
        };
        let home = hs_common::config_file::home_dir()?;
        let db_path_str = section.db_path.trim();
        if db_path_str.is_empty() {
            anyhow::bail!(
                "{}: invalid `openalex` section: `db_path` is empty",
                file.path().display()
            );
        }
        let db_path = if let Some(rest) = db_path_str.strip_prefix("~/") {
            home.join(rest)
        } else {
            std::path::PathBuf::from(db_path_str)
        };
        if !db_path.exists() {
            let reason = format!(
            "the local OpenAlex corpus has not been built: there is no database at {}. Build it \
             with {BUILD_COMMANDS}, then restart hs-mcp",
            db_path.display()
        );
            tracing::warn!("{reason}");
            return Ok(Self::Unavailable(reason));
        }
        let cfg = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;
        let conn = match duckdb::Connection::open_with_flags(&db_path, cfg) {
            Ok(conn) => conn,
            // The database is configured and present but cannot be opened: most
            // often `hs openalex load-works` holds the write lock. Not a config
            // error, so the server still starts.
            Err(e) => {
                let reason = format!(
                "the local OpenAlex database at {} could not be opened when hs-mcp started ({e}); \
                 most often `hs openalex load-works` held it for writing. Restart hs-mcp once \
                 the load has finished",
                db_path.display()
            );
                tracing::warn!("{reason}");
                return Ok(Self::Unavailable(reason));
            }
        };
        // Cap query memory so a single bad FTS call can't OOM-kill the entire
        // mcp process and trigger a systemd restart loop. `match_bm25` on
        // multi-term queries with common terms builds an ~11M-row intermediate
        // CTE; under the default unlimited budget DuckDB tries to materialize
        // it and the kernel OOM-killer fires. With a cap, DuckDB plans
        // conservatively and either completes or raises a recoverable
        // "Out of Memory" that surfaces cleanly to the MCP client.
        conn.execute_batch("SET memory_limit='4GB';")?;
        if !corpus_is_built(&conn) {
            let reason = format!(
                "the local OpenAlex corpus at {} is not finished: run `hs openalex build-indexes` \
             and `hs openalex build-fts`, then restart hs-mcp",
                db_path.display()
            );
            tracing::warn!("{reason}");
            return Ok(Self::Unavailable(reason));
        }
        Ok(Self::Ready(Arc::new(Mutex::new(conn))))
    }

    /// A private read-only handle for one request. The shared connection is
    /// locked only long enough to clone it, so a slow query (a BM25 search)
    /// never blocks the others; a poisoned lock or a failed clone is an
    /// error that names its cause.
    pub(crate) fn connection(&self) -> Result<duckdb::Connection, String> {
        let shared = match self {
            Self::Ready(shared) => shared,
            Self::Unavailable(reason) => return Err(reason.clone()),
        };
        let guard = shared.lock().map_err(|_| {
            "the OpenAlex database handle is poisoned by an earlier panic".to_string()
        })?;
        guard
            .try_clone()
            .map_err(|e| format!("opening a request handle on the OpenAlex database failed: {e}"))
    }

    /// Run one query off the async runtime and return its rows, each a JSON
    /// object keyed by column name.
    pub(crate) async fn rows(
        &self,
        sql: &str,
        params: Vec<duckdb::types::Value>,
    ) -> Result<Vec<serde_json::Value>, String> {
        let conn = self.connection()?;
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            collect_rows_json(&conn, &sql, duckdb::params_from_iter(params.iter()))
        })
        .await
        .map_err(|e| format!("the OpenAlex query task failed: {e}"))?
    }
}

/// The three `hs openalex` commands that build the corpus, in order, as
/// every "not built" message names them.
const BUILD_COMMANDS: &str =
    "`hs openalex load-works`, `hs openalex build-indexes` and `hs openalex build-fts`";

/// Whether the OpenAlex corpus has finished its end-to-end build (load,
/// indexes, FTS). The signal is a single row in `_corpus_state` keyed
/// `openalex_works`, written by `OpenAlexDb::build_fts` only after the FTS
/// index is persisted. A missing table (a database from before that gate) or
/// a missing row both mean "not built".
fn corpus_is_built(conn: &duckdb::Connection) -> bool {
    let result: Result<i64, _> = conn.query_row(
        "SELECT COUNT(*) FROM _corpus_state WHERE component = 'openalex_works'",
        [],
        |row| row.get(0),
    );
    match result {
        Ok(n) => n > 0,
        // A missing table is the expected "older DB" answer; say so rather
        // than hiding whichever error this really was.
        Err(e) => {
            tracing::warn!("openalex readiness probe failed: {e}");
            false
        }
    }
}

/// Build a `duckdb::types::Value` from a typed Option, mapping None → Null.
/// Used by the tools to construct the params Vec inline.
pub(crate) fn opt_value<T: Into<duckdb::types::Value>>(v: Option<T>) -> duckdb::types::Value {
    v.map(Into::into).unwrap_or(duckdb::types::Value::Null)
}

/// Run a query and return the result rows as a Vec<serde_json::Value>.
pub(crate) fn collect_rows_json(
    conn: &duckdb::Connection,
    sql: &str,
    params: impl duckdb::Params,
) -> Result<Vec<serde_json::Value>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
    let mut rows = stmt.query(params).map_err(|e| format!("query: {e}"))?;
    let cols: Vec<String> = rows
        .as_ref()
        .map(|s| s.column_names().into_iter().collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| format!("next: {e}"))? {
        out.push(row_to_json(row, &cols)?);
    }
    Ok(out)
}

pub(crate) fn row_to_json(row: &duckdb::Row, cols: &[String]) -> Result<serde_json::Value, String> {
    let mut m = serde_json::Map::with_capacity(cols.len());
    for (i, name) in cols.iter().enumerate() {
        let v: duckdb::types::Value = row.get(i).map_err(|e| format!("col {i}: {e}"))?;
        m.insert(name.clone(), duckdb_value_to_json(v));
    }
    Ok(serde_json::Value::Object(m))
}

fn duckdb_value_to_json(v: duckdb::types::Value) -> serde_json::Value {
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
