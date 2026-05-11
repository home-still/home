use std::sync::Arc;

use clap::Parser;
use hs_common::event_bus::{EventBus, NoOpBus};
use hs_common::storage::Storage;
use rmcp::{
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter},
        wrapper::Parameters,
    },
    model::{
        ErrorData, GetPromptRequestParams, GetPromptResult, ListPromptsResult,
        ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
        ProgressNotificationParam, PromptMessage, PromptMessageRole, RawResource,
        RawResourceTemplate, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
        ServerCapabilities, ServerInfo,
    },
    prompt, prompt_handler, prompt_router, schemars,
    service::RequestContext,
    tool, tool_handler, tool_router, RoleServer, ServerHandler,
};

// ── Tool parameter types ────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaperSearchParams {
    #[schemars(description = "Search query for academic papers")]
    query: String,
    #[schemars(description = "Maximum results to return (default 10)")]
    max_results: Option<u16>,
    #[schemars(
        description = "Search type: keywords, title, author, doi, subject (default: keywords)"
    )]
    search_type: Option<String>,
    #[schemars(description = "Date filter, e.g. '>=2023' or '2020-2024'")]
    date: Option<String>,
    #[schemars(description = "Result offset for pagination (default 0)")]
    offset: Option<usize>,
    #[schemars(
        description = "Provider: all, arxiv, openalex, semantic_scholar (s2), europmc (pmc), crossref, core (default: all). Unknown values return an error."
    )]
    provider: Option<String>,
    #[schemars(
        description = "Minimum citation count filter. Papers with fewer citations are excluded. Provider support varies (OpenAlex, Semantic Scholar, CORE honor this; arXiv does not)."
    )]
    min_citations: Option<u32>,
    #[schemars(
        description = "Sort order: relevance, citations, date (default: relevance). Unknown values fall back to relevance."
    )]
    sort: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaperGetParams {
    #[schemars(description = "DOI to look up")]
    doi: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaperReferencesParams {
    #[schemars(description = "DOI of the paper whose reference list to return")]
    doi: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaperCitationsParams {
    #[schemars(description = "DOI of the paper to find citing works for")]
    doi: String,
    #[schemars(description = "Maximum citations to return (default 100, max 1000)")]
    limit: Option<u32>,
    #[schemars(description = "Optional minimum publication year filter (post-fetch)")]
    year_from: Option<u16>,
    #[schemars(description = "Sort order: 'year' (default) or 'citations'")]
    sort: Option<String>,
}

// ── OpenAlex (local DuckDB) parameter types ─────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct OpenAlexSearchParams {
    #[schemars(description = "Free-text search over works title + abstract (BM25)")]
    query: String,
    #[schemars(description = "Maximum results (default 10, max 200)")]
    max_results: Option<u16>,
    #[schemars(description = "Minimum publication year filter")]
    year_from: Option<u16>,
    #[schemars(description = "Maximum publication year filter")]
    year_to: Option<u16>,
    #[schemars(description = "Minimum cited_by_count filter")]
    min_citations: Option<u32>,
    #[schemars(description = "Sort: 'relevance' (default), 'citations', 'year'")]
    sort: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct OpenAlexGetParams {
    #[schemars(description = "OpenAlex work ID (e.g. W2741809807) or DOI (e.g. 10.1234/x)")]
    id_or_doi: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct OpenAlexReferencesParams {
    #[schemars(description = "OpenAlex work ID whose outbound references to list")]
    openalex_id: String,
    #[schemars(description = "Maximum results (default 100, max 1000)")]
    limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct OpenAlexCitationsParams {
    #[schemars(description = "OpenAlex work ID to find citing works for")]
    openalex_id: String,
    #[schemars(description = "Maximum results (default 100, max 1000)")]
    limit: Option<u32>,
    #[schemars(description = "Optional minimum publication year filter")]
    year_from: Option<u16>,
    #[schemars(description = "Sort: 'citations' (default) or 'year'")]
    sort: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct OpenAlexAuthorsByTopicParams {
    #[schemars(
        description = "OpenAlex topic ID (e.g. T13975) — get from `openalex_search` or `topics` table"
    )]
    topic_id: String,
    #[schemars(description = "Maximum authors (default 25, max 200)")]
    limit: Option<u16>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CatalogReadParams {
    #[schemars(description = "Paper stem name (filename without extension)")]
    stem: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListParams {
    #[schemars(description = "Maximum items to return (default: all)")]
    limit: Option<usize>,
    #[schemars(description = "Number of items to skip (default: 0)")]
    offset: Option<usize>,
    #[schemars(
        description = "catalog_list only: filter by embedded-in-Qdrant state. true=only embedded, false=only not-yet-embedded, omit=all."
    )]
    embedded: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CatalogRecentParams {
    #[schemars(description = "Maximum items to return (default: 30)")]
    limit: Option<usize>,
    #[schemars(
        description = "When false (default), suppress rows whose timestamp came from a catalog_repair backfill (`downloaded_at == repair.repaired_at`, or synthetic Convert whose `converted_at == repair.repaired_at`). Set true for forensic mode — repair rows reappear with `\"repair\": true`."
    )]
    include_repaired: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
struct SystemStatusParams {
    #[schemars(
        description = "When false (default), the `history` pane mirrors `catalog_recent` and suppresses `catalog_repair` backfill rows. Set true for forensic mode — repair rows reappear with `\"repair\": true`."
    )]
    include_repaired: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct MarkdownReadParams {
    #[schemars(description = "Paper stem name (filename without extension)")]
    stem: String,
    #[schemars(description = "Specific page number (1-based). Omit for full document.")]
    page: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DistillSearchParams {
    #[schemars(description = "Search query for semantic search across indexed documents")]
    query: String,
    #[schemars(description = "Maximum results (default 10)")]
    limit: Option<u64>,
    #[schemars(description = "Year filter, e.g. '>2020', '2023', '>=2021'")]
    year: Option<String>,
    #[schemars(
        description = "When true (default), include the matched chunk_text in each hit. Set false for a metadata-only response — useful when an agent is ranking/deduping large result sets (e.g. building a DOI catalog) and the passages would overflow its context window. Score and ranking are unaffected."
    )]
    include_text: Option<bool>,
}

#[derive(serde::Serialize)]
struct DistillSearchHitOut {
    doc_id: String,
    title: Option<String>,
    authors: Vec<String>,
    year: Option<u64>,
    doi: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk_text: Option<String>,
    score: f32,
    pdf_path: Option<String>,
    line_start: usize,
    line_end: usize,
    page: Option<usize>,
    category: Option<String>,
}

fn map_distill_search_hits(
    hits: Vec<hs_distill::client::SearchHit>,
    include_text: bool,
) -> Vec<DistillSearchHitOut> {
    hits.into_iter()
        .map(|h| DistillSearchHitOut {
            doc_id: h.doc_id,
            title: h.title,
            authors: h.authors,
            year: h.year,
            doi: h.doi,
            chunk_text: include_text.then_some(h.chunk_text),
            score: h.score,
            pdf_path: h.pdf_path,
            line_start: h.line_start,
            line_end: h.line_end,
            page: h.page,
            category: h.category,
        })
        .collect()
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DistillExistsParams {
    #[schemars(description = "Document ID to check")]
    doc_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaperDownloadParams {
    #[schemars(description = "DOI of the paper to download")]
    doi: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ScribeConvertParams {
    #[schemars(
        description = "Paper stem name (filename without extension) of a PDF in the papers directory"
    )]
    stem: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DistillIndexParams {
    #[schemars(
        description = "Paper stem name (filename without extension) of a markdown document to index"
    )]
    stem: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DistillReconcileParams {
    #[schemars(description = "If true, report orphans without deleting. Default: true.")]
    #[serde(default = "default_true")]
    dry_run: bool,
    #[schemars(description = "Maximum number of doc_ids to scan. Default: 100000.")]
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DistillReindexParams {
    #[schemars(description = "Paper stem name (filename without extension) to purge and re-index")]
    stem: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DistillScanRepetitionsParams {
    #[schemars(
        description = "Maximum number of markdown objects to scan. Default: 100000 (effectively unbounded)."
    )]
    #[serde(default)]
    limit: Option<u64>,
    #[schemars(
        description = "Flag documents whose repetition truncation count exceeds this value. Default: 20. Lower is more aggressive; tune against a hand-labeled sample."
    )]
    #[serde(default)]
    threshold: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CatalogRepairParams {
    #[schemars(
        description = "If true, report what would be repaired without writing. Default: true."
    )]
    #[serde(default = "default_true")]
    dry_run: bool,
    #[schemars(description = "Maximum number of orphans to repair in this call.")]
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DedupeUrlEncodedParams {
    // rc.306 P0-6: field accepted for schema compatibility but ignored
    // server-side; MCP always runs in dry-run. The apply path is CLI-only.
    #[schemars(
        description = "Ignored by MCP (always dry-run). Use `hs catalog dedupe-url-encoded --apply` for the write path."
    )]
    #[serde(default = "default_true")]
    #[allow(dead_code)]
    dry_run: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CatalogBackfillTitleParams {
    #[schemars(
        description = "If true, report what would be backfilled without writing. Default: true."
    )]
    #[serde(default = "default_true")]
    dry_run: bool,
    #[schemars(
        description = "Maximum number of catalog rows to process in this call. Each row triggers one provider aggregate lookup — keep this bounded."
    )]
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DistillBackfillParams {
    #[schemars(
        description = "If true, report what would be re-indexed without writing. Default: true."
    )]
    #[serde(default = "default_true")]
    dry_run: bool,
    #[schemars(description = "Maximum number of documents to attempt this call.")]
    #[serde(default)]
    limit: Option<usize>,
    #[schemars(
        description = "If true, also retry documents previously stamped with an embedding_skip reason. Default: false."
    )]
    #[serde(default)]
    retry_skipped: bool,
}

fn default_true() -> bool {
    true
}

// ── Personal Tools params ─────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PersonalSearchParams {
    #[schemars(description = "Search query for the personal-documents collection")]
    query: String,
    #[schemars(description = "Maximum results (default 10)")]
    limit: Option<usize>,
    #[schemars(
        description = "Restrict to a single category (medical, financial, education, legal, employment, tax, insurance, correspondence, other). Omit to search across all categories."
    )]
    category: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PersonalListParams {
    #[schemars(description = "Maximum entries to return (default 50)")]
    limit: Option<usize>,
    #[schemars(description = "Restrict to a single category. Omit to list all categories.")]
    category: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PersonalReadParams {
    #[schemars(description = "Document stem (filename without extension)")]
    stem: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PersonalAddParams {
    #[schemars(
        description = "Filename inside the personal-store inbox (no path separators, no leading dot, no absolute paths). Drop the file under cfg.inbox_dir() yourself before calling."
    )]
    filename: String,
    #[schemars(
        description = "Override the LLM-picked category. Must be one of the configured categories."
    )]
    category: Option<String>,
    #[schemars(description = "Override the LLM-picked title.")]
    title: Option<String>,
    #[schemars(description = "If true, replace an existing document with the same stem.")]
    #[serde(default)]
    force: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PersonalReindexParams {
    #[schemars(description = "Document stem to re-chunk and re-embed.")]
    stem: String,
}

/// Format a `SystemTime` as an RFC3339 UTC string. Used by `catalog_repair`
/// to stamp per-object timestamps drawn from storage `last_modified` instead
/// of a shared batch `now()` — which was the root cause of the `catalog_recent`
/// "all rows on one nanosecond" anomaly.
fn system_time_to_rfc3339(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339()
}

// ── OpenAlex DuckDB helpers ─────────────────────────────────────

fn openalex_unavailable_error() -> String {
    "OpenAlex DB not configured. Add an `openalex:` section to ~/.home-still/config.yaml \
     with `db_path` and `snapshot_dir`, then run `hs openalex load <entity>`."
        .to_string()
}

/// Open the local OpenAlex DuckDB read-only. Reads `~/.home-still/config.yaml`
/// for the `openalex.db_path` setting. Errors if the section or file is
/// missing — caller decides whether to log+continue.
fn open_openalex_readonly() -> anyhow::Result<duckdb::Connection> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no $HOME"))?;
    let cfg_path = home.join(".home-still").join("config.yaml");
    let raw = std::fs::read_to_string(&cfg_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", cfg_path.display()))?;
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&raw)?;
    let oa = v
        .get("openalex")
        .ok_or_else(|| anyhow::anyhow!("missing 'openalex:' section in {}", cfg_path.display()))?;
    let db_path_str = oa
        .get("db_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("openalex.db_path missing"))?;
    let db_path = if let Some(rest) = db_path_str.strip_prefix("~/") {
        home.join(rest)
    } else {
        std::path::PathBuf::from(db_path_str)
    };
    if !db_path.exists() {
        anyhow::bail!(
            "openalex db not found at {} — run `hs openalex load <entity>` first",
            db_path.display()
        );
    }
    let cfg = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;
    let conn = duckdb::Connection::open_with_flags(&db_path, cfg)?;
    // Cap query memory so a single bad FTS call can't OOM-kill the entire
    // mcp process and trigger a systemd restart loop. `match_bm25` on
    // multi-term queries with common terms builds an ~11M-row intermediate
    // CTE; under the default unlimited budget DuckDB tries to materialize
    // it and the kernel OOM-killer fires. With a cap, DuckDB plans
    // conservatively and either completes or raises a recoverable
    // "Out of Memory" that surfaces cleanly to the MCP client.
    conn.execute_batch("SET memory_limit='4GB';")?;
    Ok(conn)
}

/// Probe whether the OpenAlex corpus has finished its end-to-end build
/// (load, indexes, FTS). The signal is a single row in `_corpus_state`
/// keyed `openalex_works`, written by `OpenAlexDb::build_fts` only after
/// the FTS index is persisted. Returns `false` if the table doesn't exist
/// (older DB from before the gate landed) or has no row — both mean
/// "not ready, don't expose the openalex_* tools yet."
fn is_openalex_corpus_ready(conn: &duckdb::Connection) -> bool {
    let result: Result<i64, _> = conn.query_row(
        "SELECT COUNT(*) FROM _corpus_state WHERE component = 'openalex_works'",
        [],
        |row| row.get(0),
    );
    matches!(result, Ok(n) if n > 0)
}

/// Run an OpenAlex SQL query inside spawn_blocking, return rows serialized as
/// a JSON array (pretty-printed). Single entry point for all 4 simple
/// SELECT-driven openalex_* tools.
async fn run_openalex_query_json(
    server: &HomeStillMcp,
    sql: &str,
    params: Vec<duckdb::types::Value>,
) -> Result<String, String> {
    let conn_arc = server
        .openalex_db
        .as_ref()
        .ok_or_else(openalex_unavailable_error)?
        .clone();
    let sql = sql.to_string();
    tokio::task::spawn_blocking(move || -> Result<String, String> {
        let conn = conn_arc
            .lock()
            .map_err(|_| "openalex DB mutex poisoned".to_string())?;
        let rows_json = collect_rows_json(&conn, &sql, duckdb::params_from_iter(params.iter()))?;
        Ok(serde_json::to_string_pretty(&rows_json).unwrap_or_default())
    })
    .await
    .map_err(|e| format!("join: {e}"))?
}

/// Build a `duckdb::types::Value` from a typed Option, mapping None → Null.
/// Used by tool handlers to construct the params Vec inline.
fn opt_value<T: Into<duckdb::types::Value>>(v: Option<T>) -> duckdb::types::Value {
    v.map(Into::into).unwrap_or(duckdb::types::Value::Null)
}

/// Run a query and return the result rows as a Vec<serde_json::Value>.
fn collect_rows_json(
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

fn row_to_json(row: &duckdb::Row, cols: &[String]) -> Result<serde_json::Value, String> {
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

// ── MCP Server ──────────────────────────────────────────────────

#[derive(Clone)]
#[allow(dead_code)]
struct HomeStillMcp {
    // Primary read-path handle: Storage trait (local fs or Garage/S3).
    storage: Arc<dyn Storage>,
    // Event bus for cross-service notifications (scribe.completed, …). Falls
    // back to NoOpBus if the NATS config is absent or the broker is down at
    // init; publishes become silent no-ops in that case.
    events: Arc<dyn EventBus>,
    catalog_prefix: String,
    markdown_prefix: String,
    papers_prefix: String,
    // TODO: We should remove legacy code, this is a grienfield project.
    // Legacy filesystem paths used only by mutation handlers that bridge to
    // external server binaries expecting local paths (scribe_convert,
    // distill_index). Derived from `home.project_dir` in config.
    legacy_papers_dir: std::path::PathBuf,
    legacy_markdown_dir: std::path::PathBuf,
    legacy_catalog_dir: std::path::PathBuf,
    scribe_servers: Vec<String>,
    scribe_convert_timeout: std::time::Duration,
    distill_servers: Vec<String>,
    /// Read-only handle to the local OpenAlex DuckDB (when `openalex:` section
    /// is present in config and the file exists). `None` when the section is
    /// absent — `openalex_*` tools then return a configuration error.
    openalex_db: Option<Arc<std::sync::Mutex<duckdb::Connection>>>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

impl HomeStillMcp {
    async fn new() -> anyhow::Result<Self> {
        let distill_cfg = hs_distill::config::DistillClientConfig::load().unwrap_or_default();
        let scribe_cfg = hs_scribe::config::ScribeConfig::load().unwrap_or_default();

        // Storage backend: honor the `storage:` section in
        // ~/.home-still/config.yaml. Missing or malformed config is fatal —
        // we do not fall back to LocalFsStorage at project_dir because that
        // masks typos and serves unrelated data silently (ONE PATH).
        let storage_cfg = hs_common::logging::load_config_sections()
            .0
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "hs-mcp requires a `storage:` section in ~/.home-still/config.yaml \
                     (backend: local|s3)"
                )
            })?;
        let storage: Arc<dyn Storage> = storage_cfg
            .build()
            .map_err(|e| anyhow::anyhow!("storage config invalid: {e:#}"))?;
        storage
            .ensure_ready()
            .await
            .map_err(|e| anyhow::anyhow!("storage ensure_ready failed: {e:#}"))?;

        // Event bus: reuse the events section already parsed into ScribeConfig.
        // Any failure (missing config, broker down, feature not compiled)
        // degrades to NoOpBus so the MCP server still starts.
        let events: Arc<dyn EventBus> = match scribe_cfg.build_event_bus().await {
            Ok(bus) => bus,
            Err(e) => {
                tracing::warn!("event bus init failed ({e:#}); using NoOpBus");
                Arc::new(NoOpBus)
            }
        };

        // Config is the sole source of server URLs. To route through the
        // gateway, set the gateway URL explicitly in config (e.g.
        // `servers: [https://gateway.example/gateway/scribe]`).
        let scribe_servers = scribe_cfg.servers.clone();
        let distill_servers = distill_cfg.servers.clone();

        // Best-effort open of the local OpenAlex DuckDB. Missing config section
        // = `None`, tools return a clear error. Missing file = `None` (don't
        // create here — let `hs openalex` own DB lifecycle).
        let openalex_db = open_openalex_readonly()
            .map_err(|e| {
                tracing::warn!("openalex DB unavailable: {e:#}");
                e
            })
            .ok()
            .map(|conn| Arc::new(std::sync::Mutex::new(conn)));

        // Gate: check the readiness sentinel BEFORE building the tool
        // router. If the openalex corpus isn't fully loaded + indexed + FTS'd,
        // strip the 5 `openalex_*` tools from the router so they don't show
        // up in tools/list — preventing downstream agents from calling them
        // and getting empty/partial results during a migration window. The
        // gate evaluates once at server startup; Phase 6 of the migration
        // restarts hs-serve-mcp, which re-evaluates against the now-set
        // sentinel and re-exposes the tools.
        let openalex_corpus_ready = openalex_db
            .as_ref()
            .map(|arc| {
                let conn = arc.lock().unwrap();
                is_openalex_corpus_ready(&conn)
            })
            .unwrap_or(false);

        let mut tool_router = Self::tool_router();
        if !openalex_corpus_ready {
            for name in [
                "openalex_search",
                "openalex_get",
                "openalex_references",
                "openalex_citations",
                "openalex_authors_by_topic",
            ] {
                tool_router.remove_route(name);
            }
            tracing::info!(
                "openalex corpus not ready (no _corpus_state sentinel); 5 openalex_* tools hidden \
                 from tools/list — restart hs-serve-mcp after `hs openalex build-fts` to expose them"
            );
        } else {
            tracing::info!("openalex corpus ready; openalex_* tools enabled");
        }

        Ok(Self {
            storage,
            events,
            catalog_prefix: "catalog".to_string(),
            markdown_prefix: hs_common::markdown::MARKDOWN_PREFIX.to_string(),
            papers_prefix: "papers".to_string(),
            legacy_papers_dir: scribe_cfg.watch_dir.clone(),
            legacy_markdown_dir: scribe_cfg.output_dir.clone(),
            legacy_catalog_dir: scribe_cfg.catalog_dir.clone(),
            scribe_servers,
            scribe_convert_timeout: std::time::Duration::from_secs(scribe_cfg.convert_timeout_secs),
            distill_servers,
            openalex_db,
            tool_router,
            prompt_router: Self::prompt_router(),
        })
    }

    fn scribe_client(&self) -> anyhow::Result<Option<hs_scribe::client::ScribeClient>> {
        match self.scribe_servers.first() {
            Some(url) => Ok(Some(hs_scribe::client::ScribeClient::new_with_timeout(
                url,
                self.scribe_convert_timeout,
            )?)),
            None => Ok(None),
        }
    }

    fn distill_client(&self) -> anyhow::Result<Option<hs_distill::client::DistillClient>> {
        match self.distill_servers.first() {
            Some(url) => Ok(Some(hs_distill::client::DistillClient::new(url)?)),
            None => Ok(None),
        }
    }
}

#[tool_router]
impl HomeStillMcp {
    // ── Paper Tools ────────────────────────────────────────────

    #[tool(
        description = "Search academic papers across 6 providers (arXiv, OpenAlex, Semantic Scholar, Europe PMC, CrossRef, CORE). Returns JSON array of papers with title, authors, abstract, DOI, citations.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn paper_search(
        &self,
        Parameters(p): Parameters<PaperSearchParams>,
    ) -> Result<String, String> {
        let config = paper::config::Config::load().map_err(|e| format!("Config error: {e}"))?;

        let provider_arg = resolve_provider_arg(p.provider.as_deref())?;

        let provider = paper::commands::paper::make_provider(&provider_arg, &config)
            .map_err(|e| format!("Provider error: {e}"))?;

        let search_type = match p.search_type.as_deref() {
            Some("title") => paper::models::SearchType::Title,
            Some("author") => paper::models::SearchType::Author,
            Some("doi") => paper::models::SearchType::DOI,
            Some("subject") => paper::models::SearchType::Subject,
            _ => paper::models::SearchType::Keywords,
        };

        let date_filter = p
            .date
            .as_deref()
            .and_then(|d| paper::models::DateFilter::parse(d).ok());

        let sort_by = match p.sort.as_deref() {
            Some("citations") => paper::models::SortBy::Citations,
            Some("date") => paper::models::SortBy::Date,
            _ => paper::models::SortBy::Relevance,
        };

        let query = paper::models::SearchQuery {
            query: p.query,
            search_type,
            max_results: p.max_results.unwrap_or(10) as usize,
            offset: p.offset.unwrap_or(0),
            date_filter,
            sort_by,
            min_citations: p.min_citations.map(u64::from),
        };

        match provider.search_by_query(&query).await {
            Ok(result) => Ok(serde_json::to_string_pretty(&result.papers).unwrap_or_default()),
            Err(e) => Err(format!("Search failed: {e}")),
        }
    }

    #[tool(
        description = "Look up a single paper by DOI. Returns JSON with full metadata.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn paper_get(&self, Parameters(p): Parameters<PaperGetParams>) -> Result<String, String> {
        let config = paper::config::Config::load().map_err(|e| format!("Config error: {e}"))?;

        let provider_arg = paper::cli::ProviderArg::All;
        let provider = paper::commands::paper::make_provider(&provider_arg, &config)
            .map_err(|e| format!("Provider error: {e}"))?;

        match provider.get_by_doi(&p.doi).await {
            Ok(Some(paper)) => Ok(serde_json::to_string_pretty(&paper).unwrap_or_default()),
            Ok(None) => Err(format!("No paper found for DOI: {}", p.doi)),
            Err(e) => Err(format!("Lookup failed: {e}")),
        }
    }

    #[tool(
        description = "Return the structured reference list of a paper by DOI. Returns JSON with each reference's DOI, title, year, authors, venue, and citation count. Source: Semantic Scholar Graph API.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn paper_references(
        &self,
        Parameters(p): Parameters<PaperReferencesParams>,
    ) -> Result<String, String> {
        let config = paper::config::Config::load().map_err(|e| format!("Config error: {e}"))?;
        let provider = paper::providers::semantic_scholar::SemanticScholarProvider::new(
            &config.providers.semantic_scholar,
        )
        .map_err(|e| format!("Provider error: {e}"))?;

        match provider.references(&p.doi).await {
            Ok(resp) => Ok(serde_json::to_string_pretty(&resp).unwrap_or_default()),
            Err(e) => Err(format!("References lookup failed: {e}")),
        }
    }

    #[tool(
        description = "Return the list of papers that cite a given paper by DOI (forward citation chaining). Supports limit (default 100, max 1000) and optional year_from / sort filters. Returns JSON with each citing paper's DOI, title, year, authors, venue, and citation count. Source: Semantic Scholar Graph API.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn paper_citations(
        &self,
        Parameters(p): Parameters<PaperCitationsParams>,
    ) -> Result<String, String> {
        let config = paper::config::Config::load().map_err(|e| format!("Config error: {e}"))?;
        let provider = paper::providers::semantic_scholar::SemanticScholarProvider::new(
            &config.providers.semantic_scholar,
        )
        .map_err(|e| format!("Provider error: {e}"))?;

        let opts = paper::providers::semantic_scholar::CitationsOpts {
            limit: p.limit,
            year_from: p.year_from,
            sort: p.sort,
        };

        match provider.citations(&p.doi, opts).await {
            Ok(resp) => Ok(serde_json::to_string_pretty(&resp).unwrap_or_default()),
            Err(e) => Err(format!("Citations lookup failed: {e}")),
        }
    }

    #[tool(
        description = "Download a paper PDF by DOI into the papers directory. Tries arXiv, Unpaywall, and provider resolvers. Creates a catalog entry with metadata. Returns JSON with file path, size, and sha256.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn paper_download(
        &self,
        Parameters(p): Parameters<PaperDownloadParams>,
    ) -> Result<String, String> {
        let config = paper::config::Config::load().map_err(|e| format!("Config error: {e}"))?;

        // Build provider resolvers for PDF URL resolution (Semantic Scholar, Europe PMC, CORE)
        let mut resolvers: Vec<Box<dyn paper::ports::provider::PaperProvider>> = Vec::new();
        if let Ok(s2) = paper::providers::semantic_scholar::SemanticScholarProvider::new(
            &config.providers.semantic_scholar,
        ) {
            resolvers.push(Box::new(s2));
        }
        if let Ok(epmc) =
            paper::providers::europe_pmc::EuropePmcProvider::new(&config.providers.europe_pmc)
        {
            resolvers.push(Box::new(epmc));
        }
        if config.providers.core.api_key.is_some() {
            if let Ok(core) = paper::providers::core::CoreProvider::new(&config.providers.core) {
                resolvers.push(Box::new(core));
            }
        }

        let storage = config
            .build_storage()
            .map_err(|e| format!("Storage init failed: {e}"))?;
        let events = config
            .build_event_bus()
            .await
            .map_err(|e| format!("Event bus init failed: {e}"))?;
        let downloader = paper::providers::downloader::PaperDownloader::with_event_bus(
            storage,
            events,
            &config.download,
            resolvers,
        )
        .map_err(|e| format!("Downloader init failed: {e}"))?;

        // Download by DOI
        use paper::ports::download_service::DownloadService;
        let result = downloader
            .download_by_doi(&p.doi)
            .await
            .map_err(|e| format!("Download failed: {e}"))?;

        if result.skipped {
            return Ok(serde_json::to_string_pretty(&serde_json::json!({
                "doi": p.doi,
                "skipped": true,
                "path": result.file_path.display().to_string(),
                "message": "File already exists",
            }))
            .unwrap_or_default());
        }

        // Look up paper metadata to populate catalog entry
        let provider_arg = paper::cli::ProviderArg::All;
        let paper_meta =
            if let Ok(provider) = paper::commands::paper::make_provider(&provider_arg, &config) {
                provider.get_by_doi(&p.doi).await.ok().flatten()
            } else {
                None
            };

        // Write catalog entry
        let stem = result
            .file_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let entry = hs_common::catalog::CatalogEntry {
            title: paper_meta.as_ref().map(|p| p.title.clone()),
            authors: paper_meta
                .as_ref()
                .map(|p| {
                    p.authors
                        .iter()
                        .map(|a| hs_common::catalog::AuthorEntry {
                            name: a.name.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            doi: Some(p.doi.clone()),
            publication_date: paper_meta
                .as_ref()
                .and_then(|p| p.publication_date.map(|d| d.to_string())),
            abstract_text: paper_meta.as_ref().and_then(|p| p.abstract_text.clone()),
            cited_by_count: paper_meta.as_ref().and_then(|p| p.cited_by_count),
            source: paper_meta.as_ref().map(|p| p.source.clone()),
            download_urls: paper_meta
                .as_ref()
                .map(|p| p.download_urls.clone())
                .unwrap_or_default(),
            pdf_path: Some(result.file_path.display().to_string()),
            markdown_path: None,
            downloaded_at: Some(chrono::Utc::now().to_rfc3339()),
            file_size_bytes: Some(result.size_bytes),
            sha256: Some(result.sha256.clone()),
            conversion: None,
            conversion_failed: None,
            embedding: None,
            embedding_skip: None,
            abstract_embed: None,
            repair: None,
            category: None,
            original_format: None,
        };
        if let Err(e) = hs_common::catalog::write_catalog_entry_via(
            &*self.storage,
            &self.catalog_prefix,
            &stem,
            &entry,
        )
        .await
        {
            tracing::warn!("catalog write failed for {stem}: {e}");
        }

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "doi": p.doi,
            "path": result.file_path.display().to_string(),
            "size_bytes": result.size_bytes,
            "sha256": result.sha256,
        }))
        .unwrap_or_default())
    }

    // ── Catalog Tools ──────────────────────────────────────────

    #[tool(
        description = "List all papers in the catalog with titles, conversion status, and embedded-in-Qdrant status. Returns JSON array. Supports pagination via limit/offset and filtering via `embedded=true|false` (omit for all). Use `embedded=false` to find papers stuck between convert and embed. Flag semantics: `converted` is true iff markdown exists (converters don't write failure rows — errors propagate and leave no catalog record). `embedded` is true only when Qdrant actually received chunks (`chunks_indexed > 0`).",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_list(&self, Parameters(p): Parameters<ListParams>) -> Result<String, String> {
        let mut triples =
            hs_common::catalog::list_catalog_entries_via(&*self.storage, &self.catalog_prefix)
                .await
                .map_err(|e| format!("catalog list failed: {e}"))?;
        // Stable ordering by stem for deterministic pagination.
        triples.sort_by(|a, b| a.0.cmp(&b.0));

        // Apply embedded filter before pagination so counts make sense. An entry
        // only counts as "embedded" if Qdrant actually got chunks — mirrors the
        // chunks_indexed > 0 predicate used by catalog_recent.
        if let Some(want) = p.embedded {
            triples.retain(|(_, _, cat)| {
                let is_embedded = cat.embedding.as_ref().is_some_and(|e| e.chunks_indexed > 0);
                is_embedded == want
            });
        }

        let offset = p.offset.unwrap_or(0);
        let slice: Vec<_> = match p.limit {
            Some(limit) => triples.into_iter().skip(offset).take(limit).collect(),
            None => triples.into_iter().skip(offset).collect(),
        };

        let entries: Vec<_> = slice
            .into_iter()
            .map(|(stem, _meta, cat)| {
                let title = cat.title.unwrap_or_default();
                let downloaded = cat.downloaded_at.is_some();
                // `converted` means markdown exists. Converters don't stamp
                // failure rows — errors propagate and no catalog record is
                // written, so `conversion.is_some()` is always truthful.
                let converted = cat.conversion.is_some();
                let embedded = cat.embedding.as_ref().is_some_and(|e| e.chunks_indexed > 0);
                let embedding_skipped = cat.embedding_skip.is_some();
                let repaired = cat.repair.is_some();
                // `corrupted` mirrors the `corrupted_pdfs` counter in
                // system_status — the catalog row was stamped
                // `conversion_failed` because the source bytes are not a
                // valid PDF (paywall HTML stub, truncated download, etc.).
                let corrupted = cat.conversion_failed.is_some();
                let doi = cat.doi.clone().unwrap_or_default();
                serde_json::json!({
                    "stem": stem,
                    "title": title,
                    "doi": doi,
                    "downloaded": downloaded,
                    "converted": converted,
                    "embedded": embedded,
                    "embedding_skipped": embedding_skipped,
                    "repaired": repaired,
                    "corrupted": corrupted,
                })
            })
            .collect();

        Ok(serde_json::to_string_pretty(&entries).unwrap_or_default())
    }

    #[tool(
        description = "Most recent catalog activity (download/convert/embed/embed-skip events). One row per event, sorted newest-first by event timestamp; ties broken by stem so output is deterministic. By default excludes rows whose timestamp is a `catalog_repair` backfill stamp (fingerprint: `downloaded_at == repair.repaired_at`, or synthetic Convert whose `converted_at == repair.repaired_at`) so the feed reflects organic activity. Pass `include_repaired=true` for forensic inspection — repair rows reappear annotated `\"repair\": true`. Used by the status dashboard's History pane.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_recent(
        &self,
        Parameters(p): Parameters<CatalogRecentParams>,
    ) -> Result<String, String> {
        let triples =
            hs_common::catalog::list_catalog_entries_via(&*self.storage, &self.catalog_prefix)
                .await
                .map_err(|e| format!("catalog list failed: {e}"))?;

        let include_repaired = p.include_repaired.unwrap_or(false);

        let mut events: Vec<serde_json::Value> = Vec::new();
        for (stem, _meta, entry) in triples {
            let name = entry.title.clone().unwrap_or_else(|| stem.clone());
            let repair_at: Option<&str> = entry.repair.as_ref().map(|r| r.repaired_at.as_str());

            if let Some(ref dl_at) = entry.downloaded_at {
                // Fingerprint: this `downloaded_at` was stamped by a prior
                // catalog_repair flag_drift pass. Skip by default so the feed
                // shows organic activity instead of hundreds of batch-stamped
                // rows collapsed onto one nanosecond.
                let is_repair = repair_at == Some(dl_at.as_str());
                if is_repair && !include_repaired {
                    // skip
                } else {
                    let size = entry
                        .file_size_bytes
                        .map(|b| b.to_string())
                        .unwrap_or_default();
                    let mut row = serde_json::json!({
                        "activity": "Download",
                        "stem": stem,
                        "name": name,
                        "detail_bytes": entry.file_size_bytes,
                        "detail": size,
                        "at": dl_at,
                    });
                    if is_repair {
                        row["repair"] = serde_json::Value::Bool(true);
                    }
                    events.push(row);
                }
            }
            if let Some(ref conv) = entry.conversion {
                // Synthetic Convert fingerprint: the flag_drift backfill
                // stamps `server = "catalog_repair:flag_drift"` and shares the
                // batch `now()` with `repair.repaired_at`. Matching on both
                // guards against a real conversion that happened to finish at
                // the same moment as an unrelated repair on another row.
                let is_repair = conv.server == "catalog_repair:flag_drift"
                    && repair_at == Some(conv.converted_at.as_str());
                if is_repair && !include_repaired {
                    // skip
                } else {
                    let mut row = serde_json::json!({
                        "activity": "Convert",
                        "stem": stem,
                        "name": name,
                        "pages": conv.total_pages,
                        "duration_secs": conv.duration_secs,
                        "server": conv.server,
                        "at": conv.converted_at,
                    });
                    if is_repair {
                        row["repair"] = serde_json::Value::Bool(true);
                    }
                    events.push(row);
                }
            }
            if let Some(ref emb) = entry.embedding {
                // TODO: Legacy, deal with it.
                // Skip zero-chunk "embeddings" — they were stamped by an older
                // pipeline when nothing actually made it into Qdrant, and only
                // pollute the history pane.
                if emb.chunks_indexed > 0 {
                    events.push(serde_json::json!({
                        "activity": "Embed",
                        "stem": stem,
                        "name": name,
                        "chunks": emb.chunks_indexed,
                        "at": emb.embedded_at,
                    }));
                }
            }
            if let Some(ref skip) = entry.embedding_skip {
                events.push(serde_json::json!({
                    "activity": "EmbedSkip",
                    "stem": stem,
                    "name": name,
                    "reason": skip.reason,
                    "at": skip.at,
                }));
            }
        }

        // Primary sort: RFC3339 lexicographic DESC. Secondary: stem ASC so
        // ties (common after batch backfills) produce a stable, readable
        // order instead of whatever the storage `list` happened to return.
        events.sort_by(|a, b| {
            let a_at = a.get("at").and_then(|v| v.as_str()).unwrap_or("");
            let b_at = b.get("at").and_then(|v| v.as_str()).unwrap_or("");
            let a_stem = a.get("stem").and_then(|v| v.as_str()).unwrap_or("");
            let b_stem = b.get("stem").and_then(|v| v.as_str()).unwrap_or("");
            b_at.cmp(a_at).then_with(|| a_stem.cmp(b_stem))
        });
        let limit = p.limit.unwrap_or(30);
        events.truncate(limit);

        Ok(serde_json::to_string_pretty(&events).unwrap_or_default())
    }

    #[tool(
        description = "Read full catalog entry for a paper. Returns JSON with metadata, conversion info, page offsets, download URLs.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_read(
        &self,
        Parameters(p): Parameters<CatalogReadParams>,
    ) -> Result<String, String> {
        match hs_common::catalog::read_catalog_entry_via(
            &*self.storage,
            &self.catalog_prefix,
            &p.stem,
        )
        .await
        .map_err(|e| e.to_string())?
        {
            Some(entry) => Ok(serde_json::to_string_pretty(&entry).unwrap_or_default()),
            None => Err(format!("No catalog entry found for '{}'", p.stem)),
        }
    }

    #[tool(
        description = "Reports catalog ↔ storage reconciliation in seven directions (forward disk orphans, catalog_no_markdown, catalog_no_source phantoms, flag-drift, flag-drift-resync, md-path-drift, stuck-convert). DRY-RUN ONLY from MCP: the apply path was removed in rc.306 — use `hs distill reconcile --fix-stamps --reembed` or the planned `hs catalog repair --apply` CLI for the write path. Any `dry_run=false` argument is ignored with a notice in the response.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_repair(
        &self,
        Parameters(mut p): Parameters<CatalogRepairParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        // rc.306 P0-6: MCP is a read-only surface. Any client-supplied
        // `dry_run=false` is ignored; the apply path lives in the CLI.
        let client_wanted_apply = !p.dry_run;
        p.dry_run = true;
        let _ = client_wanted_apply; // surfaced in the response via hint
                                     // One shared pass over all three prefixes — 3 concurrent LISTs and
                                     // bounded-concurrency parallel GETs of every catalog YAML. Replaces
                                     // the pre-fix "7 serial scans × (1 LIST + N serial GETs)" shape
                                     // whose cost at 3k+ rows exceeded the MCP 4-minute client budget
                                     // even under dry_run. Every scan below reads the same snapshot, so
                                     // dry-run and live-repair walk identical scan code.
        let progress_token = context.meta.get_progress_token();
        let peer = context.peer.clone();

        let progress_token_for_fetch = progress_token.clone();
        let peer_for_fetch = peer.clone();
        let on_fetch_progress = move |done: usize, total: usize| {
            let Some(token) = progress_token_for_fetch.clone() else {
                return;
            };
            let peer = peer_for_fetch.clone();
            tokio::spawn(async move {
                let params = ProgressNotificationParam::new(token, done as f64)
                    .with_message(format!("catalog snapshot: {done}/{total}"))
                    .with_total(total as f64);
                if let Err(e) = peer.notify_progress(params).await {
                    tracing::warn!(error = %e, "catalog_repair snapshot notify_progress failed");
                }
            });
        };

        let snapshot = hs_common::status::build_repair_snapshot(
            &*self.storage,
            &self.papers_prefix,
            &self.catalog_prefix,
            &self.markdown_prefix,
            hs_common::catalog::CATALOG_FETCH_CONCURRENCY,
            on_fetch_progress,
        )
        .await
        .map_err(|e| format!("snapshot build failed: {e}"))?;

        // Per-phase progress ping so the client timer resets between scans
        // and the operator can see which phase is running. The scans
        // themselves are pure in-memory filters over the snapshot — this
        // is heartbeat + visibility, not pacing.
        let emit_scan_phase = |phase: u64, name: &str| {
            let Some(token) = progress_token.clone() else {
                return;
            };
            let peer = peer.clone();
            let name = name.to_string();
            tokio::spawn(async move {
                let params = ProgressNotificationParam::new(token, phase as f64)
                    .with_message(format!("scan {phase}/7: {name}"))
                    .with_total(7.0);
                if let Err(e) = peer.notify_progress(params).await {
                    tracing::warn!(error = %e, "catalog_repair scan-phase notify_progress failed");
                }
            });
        };

        // Forward direction: files on disk with no catalog row.
        emit_scan_phase(1, "disk orphans");
        let disk_orphans =
            hs_common::status::list_orphan_document_stems(&snapshot.papers, &snapshot.catalog);

        // Reverse direction: catalog claims converted but markdown is gone.
        emit_scan_phase(2, "catalog rows without markdown");
        let md_orphans = hs_common::status::list_catalog_rows_without_markdown(
            &snapshot.catalog,
            &snapshot.markdown,
        );

        // Phantom direction: catalog row with neither a paper file nor a
        // markdown file. These inflate `catalog_entries` above `documents`
        // in the pipeline rollup and have no reachable payload anywhere —
        // safe to delete once confirmed via dry-run.
        emit_scan_phase(3, "phantom orphans");
        let phantom_orphans = hs_common::status::list_catalog_rows_without_source(
            &snapshot.papers,
            &snapshot.catalog,
            &snapshot.markdown,
        );

        // Flag-drift direction: catalog row where a stage flag is missing but
        // the storage evidence for that stage exists. Backfills without
        // deleting — unlike the three directions above, which clear or synth.
        emit_scan_phase(4, "flag drift");
        let drift_rows = hs_common::status::list_catalog_flag_drift(
            &snapshot.papers,
            &snapshot.catalog,
            &snapshot.markdown,
        );

        // Flag-drift-resync direction: rows still wearing the fingerprint of a
        // prior flag_drift batch stamp (`downloaded_at == repair.repaired_at` or
        // synthetic Convert whose `converted_at == repair.repaired_at`). Rewrite
        // to the storage object's `last_modified` so the activity feed stops
        // showing hundreds of rows collapsed onto one nanosecond.
        emit_scan_phase(5, "flag drift resync");
        let resync_rows = hs_common::status::list_catalog_flag_drift_resync_candidates(
            &snapshot.papers,
            &snapshot.catalog,
            &snapshot.markdown,
        );

        // Md-path-drift direction: catalog `markdown_path` disagrees with
        // where the markdown actually lives. Post-`bc2b6fb` the physical
        // layout moved from `markdown/<stem>.md` to `markdown/{XX}/<stem>.md`,
        // but pre-existing catalog rows weren't rewritten — so every
        // resolver that trusts `markdown_path` probes a stale key and
        // reports a ghost orphan. Rewrites the catalog field to the real
        // storage key (non-destructive; does not touch markdown or Qdrant).
        emit_scan_phase(6, "md_path drift");
        let md_path_drift_rows = hs_common::status::list_catalog_rows_with_md_path_drift(
            &snapshot.catalog,
            &snapshot.markdown,
        );

        // Stuck-convert direction: catalog has a PDF/HTML source on disk but
        // no `conversion` stamp. Re-queues via the event bus rather than an
        // inline scribe call — publishes `papers.ingested`, which the
        // `hs scribe watch-events` daemon converts and `hs distill
        // watch-events` then embeds. Type A rows (ghost Qdrant chunks from a
        // prior cycle whose markdown was later deleted) also get purged
        // here so the re-index writes fresh points instead of mixing with
        // stale ones.
        emit_scan_phase(7, "stuck convert");
        let stuck_rows =
            hs_common::status::list_catalog_stuck_convert(&snapshot.papers, &snapshot.catalog);

        let disk_total = disk_orphans.len();
        let md_total = md_orphans.len();
        let phantom_total = phantom_orphans.len();
        let drift_total = drift_rows.len();
        let md_path_drift_total = md_path_drift_rows.len();
        let stuck_total = stuck_rows.len();
        let stuck_pdf = stuck_rows.iter().filter(|r| r.source_ext == "pdf").count();
        let stuck_html = stuck_rows.iter().filter(|r| r.source_ext == "html").count();
        let drift_conversion_total = drift_rows
            .iter()
            .filter(|r| r.conversion_missing_with_markdown)
            .count();
        let drift_download_total = drift_rows
            .iter()
            .filter(|r| r.download_stamp_missing_with_source)
            .count();
        let resync_total = resync_rows.len();
        let resync_download_total = resync_rows
            .iter()
            .filter(|r| r.resync_download.is_some())
            .count();
        let resync_conversion_total = resync_rows
            .iter()
            .filter(|r| r.resync_conversion.is_some())
            .count();
        let limit = p.limit.unwrap_or(usize::MAX);
        let disk_samples: Vec<String> = disk_orphans
            .iter()
            .take(10)
            .map(|(s, _)| s.clone())
            .collect();
        let md_samples: Vec<String> = md_orphans.iter().take(10).cloned().collect();
        let phantom_samples: Vec<String> = phantom_orphans.iter().take(10).cloned().collect();
        let drift_samples: Vec<String> =
            drift_rows.iter().take(10).map(|r| r.stem.clone()).collect();
        let resync_samples: Vec<String> = resync_rows
            .iter()
            .take(10)
            .map(|r| r.stem.clone())
            .collect();
        let md_path_drift_samples: Vec<serde_json::Value> = md_path_drift_rows
            .iter()
            .take(10)
            .map(|r| {
                serde_json::json!({
                    "stem": r.stem,
                    "stale_path": r.stale_path,
                    "resolved_path": r.resolved_path,
                })
            })
            .collect();
        let stuck_samples: Vec<serde_json::Value> = stuck_rows
            .iter()
            .take(10)
            .map(|r| serde_json::json!({ "stem": r.stem, "source_ext": r.source_ext }))
            .collect();

        if p.dry_run {
            return Ok(serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": true,
                "disk_no_catalog": {
                    "orphans_found": disk_total,
                    "would_repair": disk_orphans.iter().take(limit).count(),
                    "samples": disk_samples,
                },
                "catalog_no_markdown": {
                    "orphans_found": md_total,
                    "would_clear_conversion": md_orphans.iter().take(limit).count(),
                    "samples": md_samples,
                },
                "catalog_no_source": {
                    "orphans_found": phantom_total,
                    "would_delete": phantom_orphans.iter().take(limit).count(),
                    "samples": phantom_samples,
                },
                "flag_drift": {
                    "drift_found": drift_total,
                    "would_backfill_conversion": drift_conversion_total.min(limit),
                    "would_backfill_downloaded_at": drift_download_total.min(limit),
                    "samples": drift_samples,
                },
                "flag_drift_resync": {
                    "candidates_found": resync_total,
                    "would_resync_downloaded_at": resync_download_total.min(limit),
                    "would_resync_conversion": resync_conversion_total.min(limit),
                    "samples": resync_samples,
                },
                "md_path_drift": {
                    "drift_found": md_path_drift_total,
                    "would_rewrite": md_path_drift_rows.iter().take(limit).count(),
                    "samples": md_path_drift_samples,
                },
                "stuck_convert": {
                    "stuck_found": stuck_total,
                    "would_emit": stuck_rows.iter().take(limit).count(),
                    "pdf_candidates": stuck_pdf,
                    "html_candidates": stuck_html,
                    "samples": stuck_samples,
                },
            }))
            .unwrap_or_default());
        }

        let now = chrono::Utc::now().to_rfc3339();
        let mut disk_repaired = 0u64;
        let mut md_cleared = 0u64;
        let mut phantom_deleted = 0u64;
        let mut errors: Vec<String> = Vec::new();

        // Forward repair: synthesize catalog rows for disk orphans.
        for (stem, ext) in disk_orphans.iter().take(limit) {
            let mut entry = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                stem,
            )
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
            if entry.pdf_path.is_none() {
                entry.pdf_path = Some(format!(
                    "{}/{}",
                    self.papers_prefix,
                    hs_common::sharded_key(stem, ext)
                ));
            }
            entry.repair = Some(hs_common::catalog::RepairMeta {
                repaired_at: now.clone(),
                reason: format!("orphan {ext} on disk with no catalog row"),
            });
            match hs_common::catalog::write_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                stem,
                &entry,
            )
            .await
            {
                Ok(()) => disk_repaired += 1,
                Err(e) => errors.push(format!("disk/{stem}: {e}")),
            }
        }

        // Reverse repair: clear stale conversion/embedding blocks AND purge
        // any Qdrant vectors for the doc_id. Clearing the catalog flag
        // without purging Qdrant is the 2026-04-18 ghost-chunk class of
        // bug — the catalog says "not converted" while Qdrant still serves
        // stale chunks from the deleted markdown. We keep downloaded_at /
        // sha256 / file_size_bytes untouched — that data is still
        // authoritative and lets the convert queue re-pick the row without
        // re-downloading.
        let distill_client_for_purge = self.distill_client().map_err(|e| e.to_string())?;
        let mut md_qdrant_purged = 0u64;
        for stem in md_orphans.iter().take(limit) {
            let Some(mut entry) = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                stem,
            )
            .await
            .map_err(|e| e.to_string())?
            else {
                errors.push(format!("md/{stem}: catalog entry vanished mid-repair"));
                continue;
            };
            entry.conversion = None;
            entry.embedding = None;
            entry.embedding_skip = None;
            entry.repair = Some(hs_common::catalog::RepairMeta {
                repaired_at: now.clone(),
                reason: "catalog claimed converted but markdown missing — cleared + Qdrant purged"
                    .into(),
            });
            match hs_common::catalog::write_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                stem,
                &entry,
            )
            .await
            {
                Ok(()) => md_cleared += 1,
                Err(e) => {
                    errors.push(format!("md/{stem}: {e}"));
                    continue;
                }
            }
            // Best-effort Qdrant purge. A failure here doesn't roll back the
            // catalog clear — the next distill_reconcile will surface any
            // ghost chunks that slipped through, and the catalog change is
            // the load-bearing invariant.
            if let Some(ref client) = distill_client_for_purge {
                match client.delete_doc(stem).await {
                    Ok(n) if n > 0 => {
                        md_qdrant_purged += n;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("md-orphan qdrant purge {stem} failed: {e}");
                    }
                }
            }
        }

        // Phantom purge: catalog YAMLs with no backing paper file AND no
        // markdown. These have nowhere to be reconstructed from, so the
        // row itself is the orphan — delete it outright.
        for stem in phantom_orphans.iter().take(limit) {
            match hs_common::catalog::delete_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                stem,
            )
            .await
            {
                Ok(()) => phantom_deleted += 1,
                Err(e) => errors.push(format!("phantom/{stem}: {e}")),
            }
        }

        // Flag-drift repair: backfill the missing stage flag using the
        // storage evidence. We can't recover the real conversion duration /
        // page count that the original convert would have stamped — we
        // record that the metadata came from repair so operators can tell
        // organic stamps from repair stamps, and re-run `distill_reindex`
        // if they need accurate page offsets.
        //
        // Stamp values come from the source object's `last_modified` (S3
        // LastModified / fs mtime) — NOT a shared batch `now()`. The prior
        // behavior collapsed every repaired row onto one nanosecond, which
        // poisoned `catalog_recent` for gap detection. `now` is used only
        // for `RepairMeta.repaired_at`, which legitimately reflects when
        // the repair itself ran.
        let mut drift_conversion_repaired = 0u64;
        let mut drift_download_repaired = 0u64;
        for row in drift_rows.iter().take(limit) {
            let Some(mut entry) = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                &row.stem,
            )
            .await
            .map_err(|e| e.to_string())?
            else {
                errors.push(format!(
                    "drift/{}: catalog entry vanished mid-repair",
                    row.stem
                ));
                continue;
            };
            let mut changed = false;
            if row.conversion_missing_with_markdown && entry.conversion.is_none() {
                let converted_at = row
                    .markdown_last_modified
                    .map(system_time_to_rfc3339)
                    .unwrap_or_else(|| now.clone());
                entry.conversion = Some(hs_common::catalog::ConversionMeta {
                    server: "catalog_repair:flag_drift".to_string(),
                    duration_secs: 0.0,
                    total_pages: 0,
                    converted_at,
                    pages: Vec::new(),
                });
                drift_conversion_repaired += 1;
                changed = true;
            }
            if row.download_stamp_missing_with_source && entry.downloaded_at.is_none() {
                let downloaded_at = row
                    .source_last_modified
                    .map(system_time_to_rfc3339)
                    .unwrap_or_else(|| now.clone());
                entry.downloaded_at = Some(downloaded_at);
                drift_download_repaired += 1;
                changed = true;
            }
            if changed {
                entry.repair = Some(hs_common::catalog::RepairMeta {
                    repaired_at: now.clone(),
                    reason: "flag_drift backfill — storage had evidence the catalog flags didn't"
                        .to_string(),
                });
                if let Err(e) = hs_common::catalog::write_catalog_entry_via(
                    &*self.storage,
                    &self.catalog_prefix,
                    &row.stem,
                    &entry,
                )
                .await
                {
                    errors.push(format!("drift/{}: {e}", row.stem));
                }
            }
        }

        // Flag-drift-resync: for rows whose timestamps still carry a prior
        // batch `now()` stamp, rewrite `downloaded_at` / synthetic Convert
        // `converted_at` to the storage `last_modified`. Leaves `entry.repair`
        // intact so the audit trail of "this came from a backfill" survives.
        let mut resync_download_repaired = 0u64;
        let mut resync_conversion_repaired = 0u64;
        for row in resync_rows.iter().take(limit) {
            let Some(mut entry) = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                &row.stem,
            )
            .await
            .map_err(|e| e.to_string())?
            else {
                errors.push(format!(
                    "resync/{}: catalog entry vanished mid-repair",
                    row.stem
                ));
                continue;
            };
            // Re-verify the fingerprint on the freshly-read entry — guards
            // against the row being rewritten by another process between scan
            // and repair.
            let Some(repair_at) = entry.repair.as_ref().map(|r| r.repaired_at.clone()) else {
                continue;
            };
            let mut changed = false;
            if let Some(mtime) = row.resync_download {
                if entry.downloaded_at.as_deref() == Some(repair_at.as_str()) {
                    entry.downloaded_at = Some(system_time_to_rfc3339(mtime));
                    resync_download_repaired += 1;
                    changed = true;
                }
            }
            if let Some(mtime) = row.resync_conversion {
                if let Some(conv) = entry.conversion.as_mut() {
                    if conv.server == "catalog_repair:flag_drift" && conv.converted_at == repair_at
                    {
                        conv.converted_at = system_time_to_rfc3339(mtime);
                        resync_conversion_repaired += 1;
                        changed = true;
                    }
                }
            }
            if changed {
                if let Err(e) = hs_common::catalog::write_catalog_entry_via(
                    &*self.storage,
                    &self.catalog_prefix,
                    &row.stem,
                    &entry,
                )
                .await
                {
                    errors.push(format!("resync/{}: {e}", row.stem));
                }
            }
        }

        // Md-path-drift repair: rewrite `markdown_path` to the real storage
        // key. Non-destructive — does not touch markdown objects or Qdrant.
        // Eliminates the 2026-04 ghost-orphan class where every reconcile
        // was re-probing a stale path and burning two HEADs per doc_id.
        let mut md_path_drift_repaired = 0u64;
        for d in md_path_drift_rows.iter().take(limit) {
            let Some(mut entry) = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                &d.stem,
            )
            .await
            .map_err(|e| e.to_string())?
            else {
                errors.push(format!(
                    "md_path_drift/{}: catalog entry vanished mid-repair",
                    d.stem
                ));
                continue;
            };
            entry.markdown_path = Some(d.resolved_path.clone());
            entry.repair = Some(hs_common::catalog::RepairMeta {
                repaired_at: now.clone(),
                reason: format!(
                    "md_path_drift: rewrote markdown_path '{}' → '{}'",
                    d.stale_path, d.resolved_path
                ),
            });
            match hs_common::catalog::write_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                &d.stem,
                &entry,
            )
            .await
            {
                Ok(()) => md_path_drift_repaired += 1,
                Err(e) => errors.push(format!("md_path_drift/{}: {e}", d.stem)),
            }
        }

        // Stuck-convert repair: purge any residual Qdrant vectors for the
        // doc_id (Type A ghost chunks from a prior cycle whose markdown was
        // deleted), then publish `papers.ingested`. `hs scribe watch-events`
        // picks it up, converts, emits `scribe.completed`; `hs distill
        // watch-events` then indexes. Both daemons must be running on the
        // GPU host for this to drain — document in deployment.md.
        let stuck_limit = limit;
        let mut stuck_emitted = 0u64;
        let mut stuck_qdrant_purged: u64 = 0;
        for row in stuck_rows.iter().take(stuck_limit) {
            if let Some(ref client) = distill_client_for_purge {
                match client.delete_doc(&row.stem).await {
                    Ok(n) => stuck_qdrant_purged += n,
                    Err(e) => {
                        errors.push(format!("stuck-purge/{}: {e}", row.stem));
                    }
                }
            }
            let source_key = format!(
                "{}/{}",
                self.papers_prefix.trim_end_matches('/'),
                hs_common::sharded_key(&row.stem, &row.source_ext)
            );
            let payload = serde_json::json!({
                "key": source_key,
                "source": "catalog_repair:stuck_convert",
            });
            match self
                .events
                .publish(
                    "papers.ingested",
                    serde_json::to_vec(&payload).unwrap_or_default().as_slice(),
                )
                .await
            {
                Ok(()) => stuck_emitted += 1,
                Err(e) => errors.push(format!("stuck-emit/{}: {e}", row.stem)),
            }
        }

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "dry_run": false,
            "disk_no_catalog": {
                "orphans_found": disk_total,
                "repaired": disk_repaired,
                "samples": disk_samples,
            },
            "catalog_no_markdown": {
                "orphans_found": md_total,
                "cleared": md_cleared,
                "qdrant_points_purged": md_qdrant_purged,
                "samples": md_samples,
            },
            "catalog_no_source": {
                "orphans_found": phantom_total,
                "deleted": phantom_deleted,
                "samples": phantom_samples,
            },
            "flag_drift": {
                "drift_found": drift_total,
                "conversion_backfilled": drift_conversion_repaired,
                "downloaded_at_backfilled": drift_download_repaired,
                "samples": drift_samples,
            },
            "flag_drift_resync": {
                "candidates_found": resync_total,
                "downloaded_at_resynced": resync_download_repaired,
                "conversion_resynced": resync_conversion_repaired,
                "samples": resync_samples,
            },
            "md_path_drift": {
                "drift_found": md_path_drift_total,
                "rewritten": md_path_drift_repaired,
                "samples": md_path_drift_samples,
            },
            "stuck_convert": {
                "stuck_found": stuck_total,
                "emitted": stuck_emitted,
                "qdrant_points_purged": stuck_qdrant_purged,
                "pdf_candidates": stuck_pdf,
                "html_candidates": stuck_html,
                "samples": stuck_samples,
            },
            "errors": errors,
        }))
        .unwrap_or_default())
    }

    #[tool(
        description = "Reports URL-encoded duplicate stems (encoded-form + decoded-form pairs where the decoded twin is the one actually indexed). DRY-RUN ONLY from MCP: the apply path was removed in rc.306 — use the planned `hs catalog dedupe-url-encoded --apply` CLI for the write path. Any `dry_run=false` argument is ignored with a notice in the response.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn dedupe_url_encoded(
        &self,
        Parameters(_p): Parameters<DedupeUrlEncodedParams>,
    ) -> Result<String, String> {
        // rc.306 P0-6: MCP is a read-only surface. The caller's dry_run
        // argument is intentionally ignored — this handler always reports
        // without writing; the apply path lives in the (CLI-only) future
        // `hs catalog dedupe-url-encoded --apply`.
        let dry_run_forced = true;
        // Enumerate all markdown stems.
        let markdown = self
            .storage
            .list(&self.markdown_prefix)
            .await
            .map_err(|e| format!("markdown list failed: {e}"))?;

        use std::collections::HashSet;
        let stems: HashSet<String> = markdown
            .iter()
            .filter_map(|o| {
                if !o.key.ends_with(".md") {
                    return None;
                }
                let filename = o.key.rsplit('/').next()?;
                if filename.starts_with("._") {
                    return None;
                }
                Some(filename.trim_end_matches(".md").to_string())
            })
            .collect();

        // Find encoded-form stems whose decoded twin also exists.
        let mut pairs: Vec<(String, String)> = Vec::new();
        for s in &stems {
            if !s.contains('%') {
                continue;
            }
            let decoded = percent_encoding::percent_decode_str(s)
                .decode_utf8_lossy()
                .into_owned();
            if decoded != *s && stems.contains(&decoded) {
                pairs.push((s.clone(), decoded));
            }
        }
        pairs.sort();

        let total = pairs.len();
        let samples: Vec<serde_json::Value> = pairs
            .iter()
            .take(10)
            .map(|(e, d)| serde_json::json!({"encoded": e, "decoded": d}))
            .collect();

        // rc.306 P0-6: apply path is CLI-only. MCP emits the report and stops.
        let _ = dry_run_forced;
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "dry_run": true,
            "mcp_forced_dry_run": true,
            "apply_hint": "use `hs catalog dedupe-url-encoded --apply` (CLI-only) for the write path",
            "pairs_found": total,
            "would_delete_encoded_rows": total,
            "samples": samples,
        }))
        .unwrap_or_default())
    }

    // ── Markdown Tools ─────────────────────────────────────────

    #[tool(
        description = "List all converted markdown documents with file sizes and page counts. Supports pagination via limit/offset.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn markdown_list(&self, Parameters(p): Parameters<ListParams>) -> Result<String, String> {
        let mut metas =
            hs_common::markdown::list_markdown_meta_via(&*self.storage, &self.markdown_prefix)
                .await
                .map_err(|e| format!("markdown list failed: {e}"))?;
        metas.sort_by(|a, b| a.0.cmp(&b.0));

        let offset = p.offset.unwrap_or(0);
        let slice: Vec<_> = match p.limit {
            Some(limit) => metas.into_iter().skip(offset).take(limit).collect(),
            None => metas.into_iter().skip(offset).collect(),
        };

        let mut entries = Vec::with_capacity(slice.len());
        for (stem, obj) in slice {
            let pages = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                &stem,
            )
            .await
            .map_err(|e| e.to_string())?
            .and_then(|c| c.conversion)
            .map(|cv| cv.total_pages)
            .unwrap_or(0);
            entries.push(serde_json::json!({
                "stem": stem,
                "size_bytes": obj.size,
                "pages": pages,
            }));
        }

        Ok(serde_json::to_string_pretty(&entries).unwrap_or_default())
    }

    #[tool(
        description = "Read a converted markdown document. Optionally specify a page number (1-based) to read just one page.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn markdown_read(
        &self,
        Parameters(p): Parameters<MarkdownReadParams>,
    ) -> Result<String, String> {
        match hs_common::markdown::read_markdown_via(&*self.storage, &self.markdown_prefix, &p.stem)
            .await
        {
            Some(content) => {
                if let Some(page) = p.page {
                    let pages: Vec<&str> = content.split("\n\n---\n\n").collect();
                    if page == 0 || page > pages.len() {
                        Err(format!(
                            "Page {} not found. Document has {} pages.",
                            page,
                            pages.len()
                        ))
                    } else {
                        Ok(pages[page - 1].to_string())
                    }
                } else {
                    Ok(content)
                }
            }
            None => Err(format!(
                "Markdown not found for '{}'. Check if it has been converted.",
                p.stem
            )),
        }
    }

    #[tool(
        description = "Backfill empty/missing `title` fields on catalog rows that have a DOI. Rows synthesized by `catalog_repair`'s `disk_no_catalog` direction, by the inbox watcher, or by server-event conversions have no title until a metadata fan-in happens. This tool calls the aggregate paper provider (`paper::get_by_doi`) for each eligible row and stamps `title` (plus `authors`, `publication_date`, `abstract_text`, `cited_by_count` when we're already on the wire). Defaults to dry-run.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn catalog_backfill_title(
        &self,
        Parameters(p): Parameters<CatalogBackfillTitleParams>,
    ) -> Result<String, String> {
        let triples =
            hs_common::catalog::list_catalog_entries_via(&*self.storage, &self.catalog_prefix)
                .await
                .map_err(|e| format!("catalog list failed: {e}"))?;

        // Candidates: row has a DOI and title is missing/empty.
        let candidates: Vec<(String, hs_common::catalog::CatalogEntry)> = triples
            .into_iter()
            .filter_map(|(stem, _meta, entry)| {
                let has_doi = entry.doi.as_ref().is_some_and(|d| !d.trim().is_empty());
                let missing_title = entry
                    .title
                    .as_ref()
                    .map(|t| t.trim().is_empty())
                    .unwrap_or(true);
                if has_doi && missing_title {
                    Some((stem, entry))
                } else {
                    None
                }
            })
            .collect();

        let total = candidates.len();
        let limit = p.limit.unwrap_or(usize::MAX);
        let take: Vec<(String, hs_common::catalog::CatalogEntry)> =
            candidates.into_iter().take(limit).collect();
        let samples: Vec<String> = take.iter().take(10).map(|(s, _)| s.clone()).collect();

        if p.dry_run {
            return Ok(serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": true,
                "candidates": total,
                "would_backfill": take.len(),
                "samples": samples,
            }))
            .unwrap_or_default());
        }

        // Provider aggregate for metadata fan-in. Same factory `paper_download`
        // uses, so behavior is consistent.
        let config = paper::config::Config::load().map_err(|e| format!("Config error: {e}"))?;
        let provider_arg = paper::cli::ProviderArg::All;
        let provider = paper::commands::paper::make_provider(&provider_arg, &config)
            .map_err(|e| format!("Provider init failed: {e}"))?;

        let now = chrono::Utc::now().to_rfc3339();
        let mut backfilled = 0u64;
        let mut no_metadata = 0u64;
        let mut errors: Vec<String> = Vec::new();

        for (stem, mut entry) in take {
            let doi = match entry.doi.as_deref() {
                Some(d) => d,
                None => continue,
            };
            let meta = match provider.get_by_doi(doi).await {
                Ok(Some(p)) => p,
                Ok(None) => {
                    no_metadata += 1;
                    continue;
                }
                Err(e) => {
                    errors.push(format!("{stem}: provider error: {e}"));
                    continue;
                }
            };

            if meta.title.trim().is_empty() {
                no_metadata += 1;
                continue;
            }

            entry.title = Some(meta.title.clone());
            if entry.authors.is_empty() && !meta.authors.is_empty() {
                entry.authors = meta
                    .authors
                    .iter()
                    .map(|a| hs_common::catalog::AuthorEntry {
                        name: a.name.clone(),
                    })
                    .collect();
            }
            if entry.publication_date.is_none() {
                entry.publication_date = meta.publication_date.map(|d| d.to_string());
            }
            if entry.abstract_text.is_none() {
                entry.abstract_text = meta.abstract_text.clone();
            }
            if entry.cited_by_count.is_none() {
                entry.cited_by_count = meta.cited_by_count;
            }
            entry.repair = Some(hs_common::catalog::RepairMeta {
                repaired_at: now.clone(),
                reason: "title backfilled via paper aggregate".to_string(),
            });

            match hs_common::catalog::write_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                &stem,
                &entry,
            )
            .await
            {
                Ok(()) => backfilled += 1,
                Err(e) => errors.push(format!("{stem}: write failed: {e}")),
            }
        }

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "dry_run": false,
            "candidates": total,
            "backfilled": backfilled,
            "no_metadata": no_metadata,
            "samples": samples,
            "errors": errors,
        }))
        .unwrap_or_default())
    }

    // ── Scribe Tools ───────────────────────────────────────────

    #[tool(
        description = "Check scribe server health: model status, version, in-flight conversions, available VLM slots.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn scribe_health(&self) -> Result<String, String> {
        let client = self
            .scribe_client()
            .map_err(|e| e.to_string())?
            .ok_or("No scribe server configured")?;

        let health = client.health().await.ok();
        let readiness = client.readiness().await.ok();

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "health": health,
            "readiness": readiness,
        }))
        .unwrap_or_default())
    }

    #[tool(
        description = "Convert a paper to markdown. For PDFs, uses the scribe VLM server. For HTML papers (PMC/PubMed fallbacks), converts locally. Takes a stem name (filename without extension). Writes markdown to storage, updates the catalog, and returns a summary. Use `markdown_read` to fetch content.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn scribe_convert(
        &self,
        Parameters(p): Parameters<ScribeConvertParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let pdf_key = format!(
            "{}/{}",
            self.papers_prefix.trim_end_matches('/'),
            hs_common::sharded_key(&p.stem, "pdf"),
        );
        let html_key = format!(
            "{}/{}",
            self.papers_prefix.trim_end_matches('/'),
            hs_common::sharded_key(&p.stem, "html"),
        );
        let epub_key = format!(
            "{}/{}",
            self.papers_prefix.trim_end_matches('/'),
            hs_common::sharded_key(&p.stem, "epub"),
        );

        let start = std::time::Instant::now();
        // Dispatch by source type — one path per file extension. No
        // fallback between types; if the named source isn't present, we
        // error loudly instead of silently converting something else.
        let (md, per_page_region_classes, source_key, server_label) =
            if let Ok(pdf_bytes) = self.storage.get(&pdf_key).await {
                let client = self
                    .scribe_client()
                    .map_err(|e| e.to_string())?
                    .ok_or("No scribe server configured")?;
                // Stream scribe's per-page progress through the MCP peer as
                // notifications/progress events. Each event resets Claude
                // Desktop's 4-min tool-call timeout, so multi-page PDFs that
                // take longer than 240s end-to-end can complete.
                let progress_token = context.meta.get_progress_token();
                let peer = context.peer.clone();
                let stem_for_progress = p.stem.clone();
                let on_progress = move |event: hs_scribe::client::ProgressEvent| {
                    let Some(token) = progress_token.clone() else {
                        return;
                    };
                    let peer = peer.clone();
                    let stem = stem_for_progress.clone();
                    tokio::spawn(async move {
                        let mut params = ProgressNotificationParam::new(token, event.page as f64)
                            .with_message(format!(
                                "{stem}: {} {}/{}",
                                event.stage, event.page, event.total_pages
                            ));
                        if event.total_pages > 0 {
                            params = params.with_total(event.total_pages as f64);
                        }
                        if let Err(e) = peer.notify_progress(params).await {
                            tracing::warn!(stem = %stem, error = %e, "notify_progress failed");
                        }
                    });
                };
                let conversion = client
                    .convert_with_progress(pdf_bytes, None, Some(p.stem.as_str()), on_progress)
                    .await
                    .map_err(|e| format!("Conversion failed: {e}"))?;
                (
                    conversion.markdown,
                    conversion.per_page_region_classes,
                    pdf_key,
                    "scribe-vlm".to_string(),
                )
            } else if let Ok(html_bytes) = self.storage.get(&html_key).await {
                let html = String::from_utf8(html_bytes)
                    .map_err(|e| format!("HTML at {html_key} is not valid UTF-8: {e}"))?;
                let md = hs_scribe::html::convert_html_to_markdown(&html);
                (md, Vec::new(), html_key, "html-parser".to_string())
            } else if let Ok(epub_bytes) = self.storage.get(&epub_key).await {
                let md = hs_scribe::epub::convert_epub_to_markdown(&epub_bytes)
                    .map_err(|e| format!("EPUB parse failed for {epub_key}: {e}"))?;
                (md, Vec::new(), epub_key, "epub-parser".to_string())
            } else {
                return Err(format!(
                "No PDF, HTML, or EPUB found for '{}' (tried {pdf_key}, {html_key}, {epub_key})",
                p.stem
            ));
            };
        let duration_secs = start.elapsed().as_secs_f64();

        let longest_run = hs_scribe::postprocess::longest_repeated_run_bytes(&md);
        let (md, per_page_truncations) = hs_scribe::postprocess::clean_repetitions_per_page(&md);
        let truncations: usize = per_page_truncations.iter().map(|t| t.total()).sum();
        if truncations > 0 {
            tracing::info!("{}: cleaned {} repetition site(s)", p.stem, truncations);
        }

        let page_offsets = hs_common::catalog::compute_page_offsets(&md);
        let total_pages = page_offsets.len() as u64;
        let per_page_is_bibliography: Vec<bool> = (0..per_page_truncations.len())
            .map(|i| {
                per_page_region_classes
                    .get(i)
                    .map(|classes| hs_scribe::postprocess::is_bibliography_page(classes))
                    .unwrap_or(false)
            })
            .collect();

        // VLM repetition-loop check: propagate as a hard error. No catalog
        // row is written — operator sees it in MCP error response + logs.
        if hs_scribe::postprocess::qc_verdict(
            &per_page_truncations,
            &per_page_is_bibliography,
            longest_run,
        ) == hs_scribe::postprocess::QcVerdict::RejectLoop
        {
            return Err(format!(
                "{}: VLM repetition loop ({} truncation site(s), longest_run={}B across {} page(s)) — not persisted",
                p.stem, truncations, longest_run, total_pages
            ));
        }

        let md_key = format!(
            "{}/{}",
            self.markdown_prefix.trim_end_matches('/'),
            hs_common::sharded_key(&p.stem, "md")
        );
        let md_bytes = md.into_bytes();
        let bytes_written = md_bytes.len();
        self.storage
            .put(&md_key, md_bytes)
            .await
            .map_err(|e| format!("Failed to write markdown to storage ({md_key}): {e}"))?;

        hs_common::catalog::update_conversion_catalog_via(
            &*self.storage,
            &self.catalog_prefix,
            &p.stem,
            &server_label,
            duration_secs,
            total_pages,
            page_offsets,
            &md_key,
        )
        .await
        .map_err(|e| format!("Failed to update catalog for '{}': {e}", p.stem))?;

        let event_payload = serde_json::json!({
            "key": md_key,
            "source_key": source_key,
        });
        if let Err(e) = self
            .events
            .publish(
                "scribe.completed",
                serde_json::to_vec(&event_payload)
                    .unwrap_or_default()
                    .as_slice(),
            )
            .await
        {
            tracing::warn!(
                stem = %p.stem,
                error = %e,
                "scribe.completed publish failed",
            );
        }

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "stem": p.stem,
            "markdown_key": md_key,
            "bytes_written": bytes_written,
            "total_pages": total_pages,
            "duration_secs": duration_secs,
            "server": server_label,
        }))
        .unwrap_or_default())
    }

    // ── Distill Tools ──────────────────────────────────────────

    #[tool(
        description = "Semantic search across indexed academic documents. Returns ranked results with text snippets, metadata, and relevance scores. Pass include_text=false to omit chunk_text from each hit — a metadata-only response that lets an agent rank/dedupe large result sets (e.g. build a DOI catalog) without the passages overflowing its context window. Score and ranking are unaffected.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn distill_search(
        &self,
        Parameters(p): Parameters<DistillSearchParams>,
    ) -> Result<String, String> {
        let client = self
            .distill_client()
            .map_err(|e| e.to_string())?
            .ok_or("No distill server configured")?;

        let filters = hs_distill::client::SearchFilters {
            year: p.year,
            topic: None,
            category: None,
        };

        match client
            .search(&p.query, p.limit.unwrap_or(10), filters)
            .await
        {
            Ok(hits) => {
                let include_text = p.include_text.unwrap_or(true);
                let out = map_distill_search_hits(hits, include_text);
                Ok(serde_json::to_string_pretty(&out).unwrap_or_default())
            }
            Err(e) => Err(format!("Search failed: {e}")),
        }
    }

    #[tool(
        description = "Semantic search over downloaded papers' abstracts. Targets the `paper_abstracts` Qdrant collection — one point per paper (not per chunk), embedded from `{title}\\n\\n{abstract}` where the abstract is sourced from the local OpenAlex catalog (preferred), the converted markdown's `## Abstract` section (fallback), or the title alone (last resort). Higher-precision than `distill_search` for 'find me the paper that argues X' queries because body-section noise (methods, references, citations) is excluded. Returns ranked hits with score, title, doi, year, and the embedded abstract text.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn abstract_search(
        &self,
        Parameters(p): Parameters<DistillSearchParams>,
    ) -> Result<String, String> {
        let client = self
            .distill_client()
            .map_err(|e| e.to_string())?
            .ok_or("No distill server configured")?;

        let filters = hs_distill::client::SearchFilters {
            year: p.year,
            topic: None,
            category: None,
        };

        match client
            .search_in(
                &p.query,
                p.limit.unwrap_or(10),
                filters,
                Some("paper_abstracts"),
            )
            .await
        {
            Ok(hits) => {
                let include_text = p.include_text.unwrap_or(true);
                let out = map_distill_search_hits(hits, include_text);
                Ok(serde_json::to_string_pretty(&out).unwrap_or_default())
            }
            Err(e) => Err(format!("abstract_search failed: {e}")),
        }
    }

    #[tool(
        description = "Get distill system status: Qdrant collection info, document/chunk counts, compute device, server version.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn distill_status(&self) -> Result<String, String> {
        let client = self
            .distill_client()
            .map_err(|e| e.to_string())?
            .ok_or("No distill server configured")?;

        let health = client.health().await.ok();
        let status = client.status().await.ok();

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "health": health,
            "status": status,
        }))
        .unwrap_or_default())
    }

    #[tool(
        description = "Check if a specific document has been indexed in the vector database.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn distill_exists(
        &self,
        Parameters(p): Parameters<DistillExistsParams>,
    ) -> Result<String, String> {
        let client = self
            .distill_client()
            .map_err(|e| e.to_string())?
            .ok_or("No distill server configured")?;

        match client.doc_exists(&p.doc_id).await {
            Ok(exists) => Ok(serde_json::to_string_pretty(&serde_json::json!({
                "doc_id": p.doc_id,
                "indexed": exists,
            }))
            .unwrap_or_default()),
            Err(e) => Err(format!("Check failed: {e}")),
        }
    }

    #[tool(
        description = "Index a converted markdown document into the vector database for semantic search. Takes a stem name.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn distill_index(
        &self,
        Parameters(p): Parameters<DistillIndexParams>,
    ) -> Result<String, String> {
        let client = self
            .distill_client()
            .map_err(|e| e.to_string())?
            .ok_or("No distill server configured")?;

        // Load the catalog entry first so we can prefer the exact key scribe
        // wrote (`markdown_path`). Re-deriving via `sharded_key` is unsafe
        // for stems with apostrophes or percent-encoded bytes — the derived
        // key can silently miss the stored object. Fall back to re-derivation
        // only for pre-rc.241 rows that predate the `markdown_path` field.
        let catalog_entry = hs_common::catalog::read_catalog_entry_via(
            &*self.storage,
            &self.catalog_prefix,
            &p.stem,
        )
        .await
        .map_err(|e| e.to_string())?;

        let key = hs_common::markdown::resolve_markdown_key_verified(
            &*self.storage,
            &self.markdown_prefix,
            &p.stem,
            catalog_entry
                .as_ref()
                .and_then(|e| e.markdown_path.as_deref()),
        )
        .await;
        if !self.storage.exists(&key).await.unwrap_or(false) {
            return Err(format!(
                "Markdown not found for '{}' at storage key '{key}'. Convert the PDF first.",
                p.stem
            ));
        }

        match client
            .index_from_storage_with_catalog(&*self.storage, &key, catalog_entry.as_ref())
            .await
        {
            Ok(result) => {
                // Stamp embedding on success or embedding_skip on a 0-chunk return,
                // so the catalog distinguishes "indexed" from "tried-and-skipped"
                // from "never tried."
                if let Err(e) = hs_common::catalog::record_embedding_outcome_via(
                    &*self.storage,
                    &self.catalog_prefix,
                    &p.stem,
                    self.distill_servers
                        .first()
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                    result.chunks_indexed,
                    &result.embedding_device,
                )
                .await
                {
                    tracing::warn!("embedding catalog update failed for {}: {e}", p.stem);
                }
                Ok(serde_json::to_string_pretty(&serde_json::json!({
                    "stem": p.stem,
                    "chunks_indexed": result.chunks_indexed,
                    "embedding_device": result.embedding_device,
                }))
                .unwrap_or_default())
            }
            Err(e) => Err(format!("Indexing failed: {e}")),
        }
    }

    // distill_purge: removed in rc.306. Bulk-delete is CLI-only via
    // `hs distill purge <doc_id>`. Agents can no longer reach the
    // write path through MCP.

    #[tool(
        description = "Reports Qdrant doc_ids whose markdown object is missing. DRY-RUN ONLY from MCP: the delete path was removed in rc.306 — use `hs distill reconcile --reembed` or `hs distill purge <doc_id>` for the write path. Any `dry_run=false` argument is ignored with a notice in the response.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn distill_reconcile(
        &self,
        Parameters(p): Parameters<DistillReconcileParams>,
    ) -> Result<String, String> {
        // rc.306 P0-6: MCP is read-only. The caller's dry_run argument
        // is ignored; the delete path lives in `hs distill reconcile`
        // (existing CLI) and `hs distill purge <doc_id>`.
        let _ = p.dry_run;
        let client = self
            .distill_client()
            .map_err(|e| e.to_string())?
            .ok_or("No distill server configured")?;
        let limit = p.limit.unwrap_or(100_000);
        let doc_ids = client
            .list_docs(limit)
            .await
            .map_err(|e| format!("list_docs failed: {e}"))?;

        let mut orphans: Vec<String> = Vec::new();
        for doc_id in &doc_ids {
            let catalog_entry = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                doc_id,
            )
            .await
            .map_err(|e| e.to_string())?;
            let key = hs_common::markdown::resolve_markdown_key_verified(
                &*self.storage,
                &self.markdown_prefix,
                doc_id,
                catalog_entry
                    .as_ref()
                    .and_then(|e| e.markdown_path.as_deref()),
            )
            .await;
            if !self.storage.exists(&key).await.unwrap_or(false) {
                orphans.push(doc_id.clone());
            }
        }

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "dry_run": true,
            "mcp_forced_dry_run": true,
            "apply_hint": "use `hs distill reconcile --reembed` or `hs distill purge <doc_id>` (CLI-only) for the delete path",
            "scanned_doc_ids": doc_ids.len(),
            "orphan_count": orphans.len(),
            "orphans": orphans,
            "points_deleted": 0,
        }))
        .unwrap_or_default())
    }

    #[tool(
        description = "Scan every markdown object for VLM repetition artifacts and report doc_ids whose cleanup truncation count exceeds `threshold` (default 20). Read-only — reports only, does not purge or delete. Pair with `distill_purge` to remediate poisoned docs.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn distill_scan_repetitions(
        &self,
        Parameters(p): Parameters<DistillScanRepetitionsParams>,
    ) -> Result<String, String> {
        let limit = p.limit.unwrap_or(100_000) as usize;
        let threshold = p.threshold.unwrap_or(20);

        let objects = self
            .storage
            .list(&self.markdown_prefix)
            .await
            .map_err(|e| format!("list({}) failed: {e}", self.markdown_prefix))?;

        let mut scanned: usize = 0;
        let mut flagged: Vec<serde_json::Value> = Vec::new();

        for obj in objects.iter().take(limit) {
            if !obj.key.ends_with(".md") {
                continue;
            }
            scanned += 1;

            let bytes = match self.storage.get(&obj.key).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("skip {}: get failed: {e}", obj.key);
                    continue;
                }
            };
            let original = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => {
                    tracing::warn!("skip {}: not UTF-8", obj.key);
                    continue;
                }
            };
            let (cleaned, breakdown) = hs_scribe::postprocess::clean_repetitions(&original);
            let truncations = breakdown.total();
            if truncations <= threshold {
                continue;
            }
            let stem = obj
                .key
                .rsplit('/')
                .next()
                .and_then(|f| f.strip_suffix(".md"))
                .unwrap_or(obj.key.as_str())
                .to_string();
            let snippet = hs_scribe::postprocess::divergence_snippet(&original, &cleaned, 120)
                .unwrap_or_default();
            flagged.push(serde_json::json!({
                "stem": stem,
                "key": obj.key,
                "truncations": truncations,
                "truncations_by_pass": breakdown,
                "offending_snippet": snippet,
            }));
        }

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "scanned": scanned,
            "threshold": threshold,
            "flagged_count": flagged.len(),
            "flagged": flagged,
        }))
        .unwrap_or_default())
    }

    #[tool(
        description = "Purge all vectors for a document and re-index it from storage with fresh catalog metadata. Use to fix documents with null/wrong metadata or stale embeddings.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn distill_reindex(
        &self,
        Parameters(p): Parameters<DistillReindexParams>,
    ) -> Result<String, String> {
        let client = self
            .distill_client()
            .map_err(|e| e.to_string())?
            .ok_or("No distill server configured")?;

        let deleted = client
            .delete_doc(&p.stem)
            .await
            .map_err(|e| format!("Purge failed for '{}': {e}", p.stem))?;

        // Read the catalog BEFORE resolving the key so we pick up the
        // canonical markdown_path for pre-rc.241 unsharded rows.
        let catalog_entry = hs_common::catalog::read_catalog_entry_via(
            &*self.storage,
            &self.catalog_prefix,
            &p.stem,
        )
        .await
        .map_err(|e| e.to_string())?;

        let key = hs_common::markdown::resolve_markdown_key_verified(
            &*self.storage,
            &self.markdown_prefix,
            &p.stem,
            catalog_entry
                .as_ref()
                .and_then(|e| e.markdown_path.as_deref()),
        )
        .await;
        if !self.storage.exists(&key).await.unwrap_or(false) {
            return Err(format!(
                "Purged {deleted} old vectors but markdown not found at '{key}'. Convert the paper first.",
            ));
        }

        let result = client
            .index_from_storage_with_catalog(&*self.storage, &key, catalog_entry.as_ref())
            .await
            .map_err(|e| format!("Re-index failed for '{}': {e}", p.stem))?;

        if let Err(e) = hs_common::catalog::record_embedding_outcome_via(
            &*self.storage,
            &self.catalog_prefix,
            &p.stem,
            self.distill_servers
                .first()
                .map(|s| s.as_str())
                .unwrap_or(""),
            result.chunks_indexed,
            &result.embedding_device,
        )
        .await
        {
            tracing::warn!("embedding catalog update failed for {}: {e}", p.stem);
        }

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "stem": p.stem,
            "old_vectors_purged": deleted,
            "chunks_indexed": result.chunks_indexed,
            "embedding_device": result.embedding_device,
            "has_catalog": catalog_entry.is_some(),
        }))
        .unwrap_or_default())
    }

    #[tool(
        description = "Find catalog entries that are converted but not embedded and re-attempt indexing for each. Use `dry_run=true` to preview the candidate count. By default, documents previously stamped with an embedding_skip reason are excluded; pass `retry_skipped=true` to retry those too. Pairs with the per-document `distill_reindex` tool for batch recovery.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn distill_backfill(
        &self,
        Parameters(p): Parameters<DistillBackfillParams>,
    ) -> Result<String, String> {
        let triples =
            hs_common::catalog::list_catalog_entries_via(&*self.storage, &self.catalog_prefix)
                .await
                .map_err(|e| format!("catalog list failed: {e}"))?;

        let candidates: Vec<String> = triples
            .into_iter()
            .filter_map(|(stem, _meta, cat)| {
                let converted = cat.conversion.is_some();
                let already_embedded = cat.embedding.as_ref().is_some_and(|e| e.chunks_indexed > 0);
                let was_skipped = cat.embedding_skip.is_some();
                if !converted || already_embedded {
                    return None;
                }
                if was_skipped && !p.retry_skipped {
                    return None;
                }
                Some(stem)
            })
            .collect();

        let total = candidates.len();
        let limit = p.limit.unwrap_or(usize::MAX);
        let take: Vec<&String> = candidates.iter().take(limit).collect();
        let sample: Vec<String> = take.iter().take(10).map(|s| (*s).clone()).collect();

        if p.dry_run {
            return Ok(serde_json::to_string_pretty(&serde_json::json!({
                "candidates": total,
                "would_index": take.len(),
                "samples": sample,
                "dry_run": true,
            }))
            .unwrap_or_default());
        }

        let client = self
            .distill_client()
            .map_err(|e| e.to_string())?
            .ok_or("No distill server configured")?;

        let mut indexed = 0u64;
        let mut still_skipped = 0u64;
        let mut errors: Vec<String> = Vec::new();

        for stem in take {
            let catalog_entry = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                stem,
            )
            .await
            .map_err(|e| e.to_string())?;
            let key = hs_common::markdown::resolve_markdown_key_verified(
                &*self.storage,
                &self.markdown_prefix,
                stem,
                catalog_entry
                    .as_ref()
                    .and_then(|e| e.markdown_path.as_deref()),
            )
            .await;
            if !self.storage.exists(&key).await.unwrap_or(false) {
                errors.push(format!("{stem}: markdown missing at {key}"));
                continue;
            }
            match client
                .index_from_storage_with_catalog(&*self.storage, &key, catalog_entry.as_ref())
                .await
            {
                Ok(result) => {
                    if let Err(e) = hs_common::catalog::record_embedding_outcome_via(
                        &*self.storage,
                        &self.catalog_prefix,
                        stem,
                        self.distill_servers
                            .first()
                            .map(|s| s.as_str())
                            .unwrap_or(""),
                        result.chunks_indexed,
                        &result.embedding_device,
                    )
                    .await
                    {
                        errors.push(format!("{stem}: catalog stamp failed: {e}"));
                    }
                    if result.chunks_indexed > 0 {
                        indexed += 1;
                    } else {
                        still_skipped += 1;
                    }
                }
                Err(e) => errors.push(format!("{stem}: {e}")),
            }
        }

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "candidates": total,
            "indexed": indexed,
            "still_skipped": still_skipped,
            "errors": errors,
            "samples": sample,
            "dry_run": false,
        }))
        .unwrap_or_default())
    }

    // ── Personal Tools ─────────────────────────────────────────
    //
    // Read tools (search/list/read) are unconstrained. Write tools
    // (add/reindex) are scoped: `personal_add` accepts a filename and
    // resolves it under the configured inbox (cfg.inbox_dir()), refusing
    // any path separator, leading dot, or symlink-escape — see
    // `personal::services::inbox::resolve_inbox_path` for the constraints.
    // Delete is intentionally still CLI-only (`hs personal delete <stem>`).

    #[tool(
        description = "Semantic search over the personal-documents collection. Use category to scope to medical, financial, etc. Read-only — for ingest, run `hs personal add` from the CLI.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn personal_search(
        &self,
        Parameters(p): Parameters<PersonalSearchParams>,
    ) -> Result<String, String> {
        let cfg = personal::config::Config::load().map_err(|e| e.to_string())?;
        let hits = personal::services::search::search(
            &cfg,
            &p.query,
            p.category.as_deref(),
            p.limit.unwrap_or(10),
        )
        .await
        .map_err(|e| e.to_string())?;
        let json: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|h| {
                serde_json::json!({
                    "stem": h.stem,
                    "title": h.title,
                    "category": h.category,
                    "snippet": h.snippet,
                    "score": h.score,
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&json).unwrap_or_default())
    }

    #[tool(
        description = "List ingested personal documents (most recent first), optionally filtered by category. Read-only — for ingest, run `hs personal add` from the CLI.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn personal_list(
        &self,
        Parameters(p): Parameters<PersonalListParams>,
    ) -> Result<String, String> {
        let cfg = personal::config::Config::load().map_err(|e| e.to_string())?;
        let entries = personal::services::catalog::list_entries(
            &cfg,
            p.category.as_deref(),
            p.limit.unwrap_or(50),
        )
        .map_err(|e| e.to_string())?;
        let json: Vec<serde_json::Value> = entries
            .into_iter()
            .map(|e| {
                serde_json::json!({
                    "stem": e.stem,
                    "title": e.title,
                    "category": e.category,
                    "original_format": e.original_format,
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&json).unwrap_or_default())
    }

    #[tool(
        description = "Read the converted markdown of a single personal document by stem. Read-only.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn personal_read(
        &self,
        Parameters(p): Parameters<PersonalReadParams>,
    ) -> Result<String, String> {
        let cfg = personal::config::Config::load().map_err(|e| e.to_string())?;
        personal::services::catalog::read_markdown(&cfg, &p.stem).map_err(|e| e.to_string())
    }

    #[tool(
        description = "Ingest a file already staged in the personal inbox. Provide just the FILENAME (e.g. 'medical-record.pdf') — absolute paths and path separators are rejected. The user drops the file under the configured inbox directory; this tool resolves the name, runs the same ingest pipeline as `hs personal add`, and reports the assigned stem/title/category. PDF conversion of long documents can take many minutes; the tool blocks until indexing finishes.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn personal_add(
        &self,
        Parameters(p): Parameters<PersonalAddParams>,
    ) -> Result<String, String> {
        let cfg = personal::config::Config::load().map_err(|e| e.to_string())?;
        let path = personal::services::inbox::resolve_inbox_path(&cfg, &p.filename)
            .map_err(|e| e.to_string())?;
        let opts = personal::services::ingest::IngestOptions {
            category_override: p.category,
            title_override: p.title,
            force: p.force,
        };
        let outcome = personal::services::ingest::ingest(&cfg, &path, opts)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "stem": outcome.stem,
            "title": outcome.title,
            "category": outcome.category,
            "chunks_indexed": outcome.chunk_count,
            "source_file": p.filename,
        }))
        .unwrap_or_default())
    }

    #[tool(
        description = "Re-chunk and re-embed an already-ingested personal document by stem. Useful after chunker/embedder changes. Deletes the existing Qdrant points for the stem and re-creates them from the stored markdown.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn personal_reindex(
        &self,
        Parameters(p): Parameters<PersonalReindexParams>,
    ) -> Result<String, String> {
        let cfg = personal::config::Config::load().map_err(|e| e.to_string())?;
        let chunks = personal::services::catalog::reindex(&cfg, &p.stem)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "stem": p.stem,
            "chunks_indexed": chunks,
        }))
        .unwrap_or_default())
    }

    // ── System Tools ───────────────────────────────────────────

    #[tool(
        description = "Full pipeline status: PDF count, markdown count, catalog count, embedded document count, server health for all services. The `history` pane matches `catalog_recent`'s defaults — rows stamped by a `catalog_repair` backfill (`downloaded_at == repair.repaired_at`, or synthetic Convert whose `converted_at == repair.repaired_at`) are suppressed. Pass `include_repaired=true` for forensic mode; repair rows reappear annotated `\"repair\": true`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn system_status(
        &self,
        Parameters(p): Parameters<SystemStatusParams>,
    ) -> Result<String, String> {
        let include_repaired = p.include_repaired.unwrap_or(false);
        let snap = self.build_status_snapshot(20, include_repaired).await;
        Ok(serde_json::to_string_pretty(&snap).unwrap_or_default())
    }

    // ── OpenAlex (local DuckDB) Tools ────────────────────────────

    #[tool(
        description = "Search the local OpenAlex catalog by free text (BM25 over title+abstract). Returns JSON array of works with openalex_id, doi, title, year, citations, and bm25 score. Local DB — no external API calls. Requires `hs openalex build-fts` to have been run.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn openalex_search(
        &self,
        Parameters(p): Parameters<OpenAlexSearchParams>,
    ) -> Result<String, String> {
        let limit = p.max_results.unwrap_or(10).min(200) as i64;
        let sort = p.sort.as_deref().unwrap_or("relevance");
        let order_by = match sort {
            "citations" => "ORDER BY cited_by_count DESC NULLS LAST, bm25 DESC",
            "year" => "ORDER BY publication_year DESC NULLS LAST, bm25 DESC",
            _ => "ORDER BY bm25 DESC",
        };
        // `conjunctive := 1` requires every query term to appear in the doc.
        // Without it (default disjunctive), a multi-term query against a
        // 56M-row corpus matches millions of docs and DuckDB's BM25
        // implementation OOMs trying to score them all. Conjunctive matches
        // are also the semantics users actually want for multi-word search.
        let sql = format!(
            r#"
            WITH ranked AS (
              SELECT
                openalex_id, doi, title, publication_year, cited_by_count,
                fts_main_works.match_bm25(openalex_id, ?, conjunctive := 1) AS bm25
              FROM works
            )
            SELECT openalex_id, doi, title, publication_year, cited_by_count, bm25
            FROM ranked
            WHERE bm25 IS NOT NULL
              AND publication_year >= COALESCE(?, 0)
              AND publication_year <= COALESCE(?, 9999)
              AND cited_by_count >= COALESCE(?, 0)
            {order_by}
            LIMIT ?;
            "#
        );
        run_openalex_query_json(
            self,
            &sql,
            vec![
                duckdb::types::Value::Text(p.query),
                opt_value(p.year_from.map(i64::from)),
                opt_value(p.year_to.map(i64::from)),
                opt_value(p.min_citations.map(i64::from)),
                duckdb::types::Value::BigInt(limit),
            ],
        )
        .await
    }

    #[tool(
        description = "Look up a single work in the local OpenAlex catalog by OpenAlex ID (e.g. W2741809807) or DOI (e.g. 10.1234/x). Returns JSON with the work + its denormalized authors and topics.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn openalex_get(
        &self,
        Parameters(p): Parameters<OpenAlexGetParams>,
    ) -> Result<String, String> {
        let conn_arc = self
            .openalex_db
            .as_ref()
            .ok_or_else(openalex_unavailable_error)?
            .clone();
        let id = p.id_or_doi.clone();
        let result: Result<serde_json::Value, String> = tokio::task::spawn_blocking(move || {
            let conn = conn_arc
                .lock()
                .map_err(|_| "openalex DB mutex poisoned".to_string())?;

            // Work row
            let work = {
                let mut stmt = conn
                    .prepare(
                        "SELECT openalex_id, doi, title, abstract_text, publication_year,
                                publication_date, language, type, cited_by_count, is_retracted,
                                is_oa, oa_url, primary_source_id
                         FROM works
                         WHERE openalex_id = ? OR doi = ?
                         LIMIT 1",
                    )
                    .map_err(|e| format!("prepare: {e}"))?;
                let mut rows = stmt
                    .query(duckdb::params![id, id])
                    .map_err(|e| format!("query: {e}"))?;
                let cols: Vec<String> = rows
                    .as_ref()
                    .map(|s| s.column_names().into_iter().collect())
                    .unwrap_or_default();
                if let Some(row) = rows.next().map_err(|e| format!("next: {e}"))? {
                    Some(row_to_json(row, &cols)?)
                } else {
                    None
                }
            };
            let work = match work {
                Some(w) => w,
                None => {
                    return Ok(serde_json::json!({"error": "not_found", "id_or_doi": id}));
                }
            };
            let work_id = work
                .get("openalex_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let authors = collect_rows_json(
                &conn,
                "SELECT a.openalex_id, a.display_name, wa.author_position,
                        wa.raw_affiliation_string, wa.institution_id
                 FROM work_authorships wa
                 LEFT JOIN authors a ON a.openalex_id = wa.author_id
                 WHERE wa.work_id = ?
                 ORDER BY wa.author_position",
                duckdb::params![work_id],
            )?;
            let topics = collect_rows_json(
                &conn,
                "SELECT t.openalex_id, t.display_name, wt.score
                 FROM work_topics wt
                 LEFT JOIN topics t ON t.openalex_id = wt.topic_id
                 WHERE wt.work_id = ?
                 ORDER BY wt.score DESC NULLS LAST",
                duckdb::params![work_id],
            )?;

            let mut out = work;
            if let serde_json::Value::Object(ref mut m) = out {
                m.insert("authors".into(), serde_json::Value::Array(authors));
                m.insert("topics".into(), serde_json::Value::Array(topics));
            }
            Ok::<serde_json::Value, String>(out)
        })
        .await
        .map_err(|e| format!("join: {e}"))?;

        let v = result?;
        Ok(serde_json::to_string_pretty(&v).unwrap_or_default())
    }

    #[tool(
        description = "List works that the given OpenAlex work cites (outbound references). Returns JSON array of cited works with openalex_id, doi, title, year, cited_by_count.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn openalex_references(
        &self,
        Parameters(p): Parameters<OpenAlexReferencesParams>,
    ) -> Result<String, String> {
        let limit = p.limit.unwrap_or(100).min(1000) as i64;
        let sql = "SELECT w.openalex_id, w.doi, w.title, w.publication_year, w.cited_by_count
                   FROM work_references wr
                   LEFT JOIN works w ON w.openalex_id = wr.referenced_work_id
                   WHERE wr.work_id = ?
                   ORDER BY w.cited_by_count DESC NULLS LAST
                   LIMIT ?";
        run_openalex_query_json(
            self,
            sql,
            vec![
                duckdb::types::Value::Text(p.openalex_id),
                duckdb::types::Value::BigInt(limit),
            ],
        )
        .await
    }

    #[tool(
        description = "List works that cite the given OpenAlex work (forward citation chaining). Returns JSON array of citing works.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn openalex_citations(
        &self,
        Parameters(p): Parameters<OpenAlexCitationsParams>,
    ) -> Result<String, String> {
        let limit = p.limit.unwrap_or(100).min(1000) as i64;
        let order_by = match p.sort.as_deref() {
            Some("year") => "ORDER BY w.publication_year DESC NULLS LAST",
            _ => "ORDER BY w.cited_by_count DESC NULLS LAST",
        };
        let sql = format!(
            "SELECT w.openalex_id, w.doi, w.title, w.publication_year, w.cited_by_count
             FROM work_references wr
             LEFT JOIN works w ON w.openalex_id = wr.work_id
             WHERE wr.referenced_work_id = ?
               AND w.publication_year >= COALESCE(?, 0)
             {order_by}
             LIMIT ?"
        );
        run_openalex_query_json(
            self,
            &sql,
            vec![
                duckdb::types::Value::Text(p.openalex_id),
                opt_value(p.year_from.map(i64::from)),
                duckdb::types::Value::BigInt(limit),
            ],
        )
        .await
    }

    #[tool(
        description = "Top authors for an OpenAlex topic, ranked by total cited_by_count. Joins work_topics → work_authorships → authors. Returns JSON array of (openalex_id, display_name, cited_by_count, papers_in_topic).",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn openalex_authors_by_topic(
        &self,
        Parameters(p): Parameters<OpenAlexAuthorsByTopicParams>,
    ) -> Result<String, String> {
        let limit = p.limit.unwrap_or(25).min(200) as i64;
        let sql = "SELECT a.openalex_id, a.display_name, a.cited_by_count,
                          COUNT(*) AS papers_in_topic
                   FROM work_topics wt
                   JOIN work_authorships wa ON wa.work_id = wt.work_id
                   JOIN authors a ON a.openalex_id = wa.author_id
                   WHERE wt.topic_id = ?
                   GROUP BY a.openalex_id, a.display_name, a.cited_by_count
                   ORDER BY a.cited_by_count DESC NULLS LAST
                   LIMIT ?";
        run_openalex_query_json(
            self,
            sql,
            vec![
                duckdb::types::Value::Text(p.topic_id),
                duckdb::types::Value::BigInt(limit),
            ],
        )
        .await
    }
}

impl HomeStillMcp {
    /// Build the unified StatusSnapshot used by both `system_status` (MCP) and
    /// `hs status` (CLI cloud-client mode). Truth source for pipeline state.
    pub(crate) async fn build_status_snapshot(
        &self,
        history_limit: usize,
        include_repaired: bool,
    ) -> hs_common::status::StatusSnapshot {
        use hs_common::status::{
            build_history, collect_pipeline_counts, QdrantInfo, ServiceInstance, StatusSnapshot,
        };

        // Per-distill health fanout (also yields embedded doc/chunk counts).
        let mut distill_instances: Vec<ServiceInstance> = Vec::new();
        let mut embedded_documents: Option<u64> = None;
        let mut embedded_chunks: Option<u64> = None;
        let mut qdrant: Option<QdrantInfo> = None;
        for url in &self.distill_servers {
            let client = match hs_distill::client::DistillClient::new(url) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("distill client build for {url} failed: {e}");
                    continue;
                }
            };
            let health = client.health().await.ok();
            let readiness = client.readiness().await.ok();
            let status = client.status().await.ok();

            let healthy = health.is_some();
            let version = health
                .as_ref()
                .map(|h| h.version.clone())
                .unwrap_or_default();
            let compute_device = status
                .as_ref()
                .map(|s| s.compute_device.clone())
                .or_else(|| health.as_ref().map(|h| h.compute_device.clone()))
                .unwrap_or_default();
            let embed_model = status
                .as_ref()
                .map(|s| s.embed_model.clone())
                .or_else(|| health.as_ref().map(|h| h.embed_model.clone()))
                .unwrap_or_default();
            let collection = status
                .as_ref()
                .map(|s| s.collection.clone())
                .or_else(|| health.as_ref().map(|h| h.collection.clone()))
                .unwrap_or_default();
            let in_flight = readiness.as_ref().map(|r| r.in_flight as u64).unwrap_or(0);
            let activity = if !healthy {
                "unhealthy".to_string()
            } else if in_flight > 0 {
                format!("{in_flight} embedding")
            } else {
                "idle".to_string()
            };

            if let Some(s) = status.as_ref() {
                if embedded_documents.is_none() {
                    embedded_documents = Some(s.documents_count);
                    embedded_chunks = Some(s.points_count);
                }
            }
            if qdrant.is_none() && !collection.is_empty() {
                qdrant = Some(QdrantInfo {
                    collection: collection.clone(),
                    compute_device: compute_device.clone(),
                    embed_model: embed_model.clone(),
                    qdrant_version: health
                        .as_ref()
                        .map(|h| h.qdrant_version.clone())
                        .unwrap_or_default(),
                    qdrant_url: health
                        .as_ref()
                        .map(|h| h.qdrant_url.clone())
                        .unwrap_or_default(),
                });
            }

            distill_instances.push(ServiceInstance {
                url: url.clone(),
                healthy,
                version,
                compute_device,
                embed_model,
                collection,
                activity,
                in_flight,
                slots_total: None,
                slots_available: None,
            });
        }

        // Per-scribe health fanout.
        let mut scribe_instances: Vec<ServiceInstance> = Vec::new();
        for url in &self.scribe_servers {
            let client = match hs_scribe::client::ScribeClient::new(url) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("scribe client build for {url} failed: {e}");
                    continue;
                }
            };
            let health = client.health().await.ok();
            let readiness = client.readiness().await.ok();

            let healthy = health.is_some();
            let version = health
                .as_ref()
                .map(|h| h.version.clone())
                .unwrap_or_default();
            let (in_flight, slots_total, slots_available) = match readiness.as_ref() {
                Some(r) => (
                    r.in_flight_conversions as u64,
                    Some(r.vlm_slots_total as u64),
                    Some(r.vlm_slots_available as u64),
                ),
                None => (0, None, None),
            };
            let activity = if !healthy {
                "unhealthy".to_string()
            } else if in_flight > 0 {
                format!("{in_flight} converting")
            } else {
                "idle".to_string()
            };

            scribe_instances.push(ServiceInstance {
                url: url.clone(),
                healthy,
                version,
                compute_device: String::new(),
                embed_model: String::new(),
                collection: String::new(),
                activity,
                in_flight,
                slots_total,
                slots_available,
            });
        }

        // Scan the catalog once; reuse for both the pipeline failed-count
        // and the history panel so we don't pay to deserialize every YAML twice.
        let catalog_triples =
            hs_common::catalog::list_catalog_entries_via(&*self.storage, &self.catalog_prefix)
                .await
                .ok();

        // Count entries stamped `embedding_skip` so progress percentages
        // can exclude intentionally-skipped docs from the denominator.
        let embedding_skipped: Option<u64> = catalog_triples.as_ref().map(|triples| {
            triples
                .iter()
                .filter(|(_, _, entry)| entry.embedding_skip.is_some())
                .count() as u64
        });

        // Same pass — catalog rows stamped `conversion_failed` surface as
        // the Corrupted PDFs count in `hs status`.
        let corrupted_pdfs: Option<u64> = catalog_triples.as_ref().map(|triples| {
            triples
                .iter()
                .filter(|(_, _, entry)| entry.conversion_failed.is_some())
                .count() as u64
        });

        // Inbox queue — files waiting in `papers/manually_downloaded/` for
        // the next sweep tick. Filtered by the same whitelist the sweeper
        // applies, so the dashboard number tracks what the daemon will
        // actually relocate. `.ok()` swallows storage blips — we'd rather
        // show `Inbox ···` than fail the whole `system_status`.
        let inbox_prefix = format!(
            "{}/manually_downloaded/",
            self.papers_prefix.trim_end_matches('/')
        );
        let inbox_pending: Option<u64> = self.storage.list(&inbox_prefix).await.ok().map(|objs| {
            objs.iter()
                .filter(|o| {
                    let fn_ = o.key.rsplit('/').next().unwrap_or("");
                    hs_common::inbox::is_inbox_candidate_filename(fn_)
                })
                .count() as u64
        });

        let mut pipeline = collect_pipeline_counts(
            &*self.storage,
            &self.papers_prefix,
            &self.markdown_prefix,
            &self.catalog_prefix,
            embedded_documents,
            embedded_chunks,
            embedding_skipped,
        )
        .await;

        // Pipeline drift: source documents that haven't produced markdown
        // yet. Saturating subtraction so stage lag never yields a negative.
        // Drift = `documents - markdown - in_flight`. By design, catalog
        // rows stamped `conversion_failed` (surfaced separately as
        // `corrupted_pdfs`) are NOT subtracted — drift is meant to surface
        // them too, since failed converts represent stuck pipeline state
        // the operator should see. Values above `pipeline_drift_threshold`
        // indicate either stamped failures or stems that errored without a
        // stamp; check scribe/event-watch logs for the latter.
        let total_in_flight: u64 = scribe_instances.iter().map(|s| s.in_flight).sum();
        pipeline.pipeline_drift = pipeline
            .documents
            .saturating_sub(pipeline.markdown)
            .saturating_sub(total_in_flight);
        pipeline.pipeline_drift_threshold = hs_common::status::PIPELINE_DRIFT_THRESHOLD;
        pipeline.corrupted_pdfs = corrupted_pdfs;
        pipeline.inbox_pending = inbox_pending;
        pipeline.in_flight_conversions = Some(total_in_flight);

        // History from the catalog — same source and same default filter as
        // `catalog_recent` so the two activity feeds can never disagree.
        let history = match catalog_triples {
            Some(triples) => {
                let pairs: Vec<(String, hs_common::catalog::CatalogEntry)> =
                    triples.into_iter().map(|(s, _m, e)| (s, e)).collect();
                build_history(&pairs, history_limit, include_repaired)
            }
            None => Vec::new(),
        };

        // Inbox-sweeper heartbeat. Populated server-side so every client
        // (mac_air, big, laptop) renders the same verdict. `None` = no
        // heartbeat key in storage; `Some(.running=false)` = stale.
        let inbox_heartbeat = hs_common::status::read_inbox_heartbeat(&*self.storage).await;

        StatusSnapshot {
            pipeline,
            scribe_instances,
            distill_instances,
            qdrant,
            history,
            inbox_heartbeat,
            generated_at: Some(chrono::Utc::now().to_rfc3339()),
        }
    }
}

// ── Prompt parameter types ───────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ResearchPromptParams {
    #[schemars(description = "Research topic to investigate")]
    topic: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SummarizePromptParams {
    #[schemars(description = "Paper stem name to summarize")]
    stem: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ComparePromptParams {
    #[schemars(description = "First paper stem name")]
    stem_a: String,
    #[schemars(description = "Second paper stem name")]
    stem_b: String,
}

#[prompt_router]
impl HomeStillMcp {
    #[prompt(description = "Research a topic: search papers, read documents, synthesize findings")]
    fn research_paper(
        &self,
        Parameters(p): Parameters<ResearchPromptParams>,
    ) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            format!(
                "Research the topic: \"{}\"\n\n\
                 Use the home-still tools in this order:\n\
                 1. paper_search to find relevant papers\n\
                 2. catalog_read to check which papers are in our collection\n\
                 3. markdown_read to read the full text of converted papers\n\
                 4. distill_search for semantic search across the indexed corpus\n\
                 5. Synthesize the findings into a comprehensive summary with citations",
                p.topic
            ),
        )]
    }

    #[prompt(description = "Summarize a specific document from the collection")]
    fn summarize_document(
        &self,
        Parameters(p): Parameters<SummarizePromptParams>,
    ) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            format!(
                "Read and summarize the paper with stem \"{}\".\n\n\
                 1. Use catalog_read to get the metadata\n\
                 2. Use markdown_read to get the full text\n\
                 3. Provide a structured summary: objective, methods, key findings, limitations, and relevance",
                p.stem
            ),
        )]
    }

    #[prompt(description = "Compare two papers from the collection")]
    fn compare_papers(&self, Parameters(p): Parameters<ComparePromptParams>) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            format!(
                "Compare these two papers:\n\
                 - Paper A: \"{}\"\n\
                 - Paper B: \"{}\"\n\n\
                 1. Use catalog_read for metadata on both\n\
                 2. Use markdown_read for full text of both\n\
                 3. Compare: research questions, methodology, findings, and conclusions\n\
                 4. Note agreements, contradictions, and complementary insights",
                p.stem_a, p.stem_b
            ),
        )]
    }
}

// ── ServerHandler ───────────────────────────────────────────────

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for HomeStillMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_instructions(
            "home-still: Academic research pipeline server.\n\n\
             Full pipeline workflow:\n\
             1. DISCOVER: paper_search (by query) or paper_get (by DOI) — metadata lookup\n\
             2. DOWNLOAD: paper_download (by DOI) — downloads the actual PDF/HTML into the papers directory. This is REQUIRED before conversion.\n\
             3. CONVERT: scribe_convert (PDF/HTML stem → markdown)\n\
             4. READ: catalog_read, markdown_read, or use resources (catalog:///{stem}, markdown:///{stem})\n\
             5. INDEX: distill_index (markdown → vector DB)\n\
             6. SEARCH: distill_search (semantic search across all indexed papers)\n\
             7. MONITOR: system_status, scribe_health, distill_status\n\n\
             To add a new paper to the pipeline: paper_search → paper_download → scribe_convert → distill_index\n\n\
             Personal documents: personal_search, personal_list, personal_read are read-only \
             over the user's private medical/financial/school records. Writes are scoped: \
             personal_add takes a FILENAME (not a path) and resolves it under the configured \
             inbox directory (default ~/home-still/personal/inbox/); the user drops the file \
             there, the tool ingests it. personal_reindex re-chunks an existing stem. Delete \
             is intentionally CLI-only — run `hs personal delete <stem>`.\n\n\
             Prompts: research_paper, summarize_document, compare_papers",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        use rmcp::model::AnnotateAble;

        let mut resources = Vec::new();

        // Catalog entries via Storage
        if let Ok(triples) =
            hs_common::catalog::list_catalog_entries_via(&*self.storage, &self.catalog_prefix).await
        {
            for (stem, _meta, cat) in triples {
                let title = cat.title.unwrap_or_else(|| stem.clone());
                resources.push(
                    RawResource {
                        uri: format!("catalog:///{stem}"),
                        name: title,
                        title: None,
                        description: Some("Catalog entry with paper metadata".into()),
                        mime_type: Some("application/yaml".into()),
                        size: None,
                        icons: None,
                        meta: None,
                    }
                    .no_annotation(),
                );
            }
        }

        // Markdown documents via Storage
        if let Ok(metas) =
            hs_common::markdown::list_markdown_meta_via(&*self.storage, &self.markdown_prefix).await
        {
            for (stem, obj) in metas {
                resources.push(
                    RawResource {
                        uri: format!("markdown:///{stem}"),
                        name: stem.clone(),
                        title: None,
                        description: Some("Converted markdown document".into()),
                        mime_type: Some("text/markdown".into()),
                        size: Some(obj.size as u32),
                        icons: None,
                        meta: None,
                    }
                    .no_annotation(),
                );
            }
        }

        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        use rmcp::model::AnnotateAble;

        Ok(ListResourceTemplatesResult::with_all_items(vec![
            RawResourceTemplate {
                uri_template: "catalog:///{stem}".into(),
                name: "Catalog Entry".into(),
                title: None,
                description: Some(
                    "Paper catalog entry with metadata, conversion info, and page offsets".into(),
                ),
                mime_type: Some("application/yaml".into()),
                icons: None,
            }
            .no_annotation(),
            RawResourceTemplate {
                uri_template: "markdown:///{stem}".into(),
                name: "Markdown Document".into(),
                title: None,
                description: Some("Full converted markdown of an academic paper".into()),
                mime_type: Some("text/markdown".into()),
                icons: None,
            }
            .no_annotation(),
            RawResourceTemplate {
                uri_template: "markdown:///{stem}/page/{page}".into(),
                name: "Markdown Page".into(),
                title: None,
                description: Some(
                    "Single page from a converted markdown document (1-based)".into(),
                ),
                mime_type: Some("text/markdown".into()),
                icons: None,
            }
            .no_annotation(),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        let uri = &request.uri;

        if let Some(stem) = uri.strip_prefix("catalog:///") {
            // Catalog resource via Storage
            let entry = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                stem,
            )
            .await
            .map_err(|e| ErrorData::internal_error(format!("catalog read: {e}"), None))?
            .ok_or_else(|| ErrorData::resource_not_found("catalog entry not found", None))?;
            let yaml = serde_json::to_string_pretty(&entry)
                .map_err(|e| ErrorData::internal_error(format!("serialize error: {e}"), None))?;
            Ok(ReadResourceResult::new(vec![
                ResourceContents::TextResourceContents {
                    uri: uri.clone(),
                    mime_type: Some("application/yaml".into()),
                    text: yaml,
                    meta: None,
                },
            ]))
        } else if let Some(rest) = uri.strip_prefix("markdown:///") {
            // Markdown resource — check for page number
            let (stem, page) = if let Some((s, p)) = rest.rsplit_once("/page/") {
                let page: usize = p.parse().map_err(|_| {
                    ErrorData::invalid_params(format!("invalid page number: {p}"), None)
                })?;
                (s, Some(page))
            } else {
                (rest, None)
            };

            let content =
                hs_common::markdown::read_markdown_via(&*self.storage, &self.markdown_prefix, stem)
                    .await
                    .ok_or_else(|| ErrorData::resource_not_found("markdown not found", None))?;

            let text = if let Some(page) = page {
                let pages: Vec<&str> = content.split("\n\n---\n\n").collect();
                if page == 0 || page > pages.len() {
                    return Err(ErrorData::invalid_params(
                        format!("page {page} not found, document has {} pages", pages.len()),
                        None,
                    ));
                }
                pages[page - 1].to_string()
            } else {
                content
            };

            Ok(ReadResourceResult::new(vec![
                ResourceContents::TextResourceContents {
                    uri: uri.clone(),
                    mime_type: Some("text/markdown".into()),
                    text,
                    meta: None,
                },
            ]))
        } else {
            Err(ErrorData::resource_not_found(
                "unknown resource URI scheme",
                None,
            ))
        }
    }
}

// ── Entrypoint ──────────────────────────────────────────────────

/// hs-mcp — MCP server for the home-still research pipeline
#[derive(Parser)]
#[command(name = "hs-mcp")]
struct Args {
    /// Run as HTTP/SSE server on this address (default: stdio mode)
    /// Example: --serve 127.0.0.1:7445
    #[arg(long)]
    serve: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = hs_common::secrets::load_default_secrets();
    let args = Args::parse();

    // In stdio mode, stdout is the MCP protocol — never write human-readable
    // lines to stderr either, so logs are spool-only and ship to the logs
    // bucket like every other service.
    let logging_handle = install_logging(args.serve.is_some()).await;

    let server = HomeStillMcp::new().await?;

    let result: anyhow::Result<()> = if let Some(addr) = args.serve {
        tracing::info!("Starting MCP SSE server on {addr}");

        use rmcp::transport::streamable_http_server::{
            session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
        };

        // Default rmcp session config times out idle sessions; the Mac dashboard
        // holds an SSE connection open between `hs status` refreshes, so we
        // disable the idle timeout.
        let mut session_manager = LocalSessionManager::default();
        session_manager.session_config.keep_alive = None;

        let service = StreamableHttpService::new(
            move || Ok(server.clone()),
            std::sync::Arc::new(session_manager),
            StreamableHttpServerConfig::default(),
        );

        let router = axum::Router::new().fallback_service(service);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!("MCP server listening on {addr}");

        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                tokio::signal::ctrl_c().await.ok();
            })
            .await?;
        Ok(())
    } else {
        // stdio mode: standard MCP transport
        let transport = rmcp::transport::io::stdio();
        let ct = rmcp::service::serve_server(server, transport).await?;
        let _ = ct.waiting().await;
        Ok(())
    };

    if let Some(h) = logging_handle {
        let _ = h.shutdown().await;
    }
    result
}

fn resolve_provider_arg(s: Option<&str>) -> Result<paper::cli::ProviderArg, String> {
    use paper::cli::ProviderArg;
    let Some(name) = s else {
        return Ok(ProviderArg::All);
    };
    match name {
        "all" => Ok(ProviderArg::All),
        "arxiv" => Ok(ProviderArg::Arxiv),
        "openalex" => Ok(ProviderArg::OpenAlex),
        "semantic_scholar" | "s2" => Ok(ProviderArg::SemanticScholar),
        "europmc" | "pmc" => Ok(ProviderArg::EuropePmc),
        "crossref" => Ok(ProviderArg::CrossRef),
        "core" => Ok(ProviderArg::Core),
        other => Err(format!(
            "Unknown provider {other:?}. Accepted: all, arxiv, openalex, semantic_scholar (s2), europmc (pmc), crossref, core."
        )),
    }
}

async fn install_logging(is_sse: bool) -> Option<hs_common::logging::LoggingHandle> {
    use hs_common::logging::{self, LoggingConfig, StderrOutput};
    let (primary_storage, logs_yaml) = logging::load_config_sections();
    let (service, stderr) = if is_sse {
        ("hs-mcp-sse", StderrOutput::EnvFilter("info".into()))
    } else {
        ("hs-mcp-stdio", StderrOutput::Disabled)
    };
    let mut cfg = LoggingConfig::for_service(service).with_stderr(stderr);
    logs_yaml.apply_to(&mut cfg);
    let mut handle = match logging::init(cfg) {
        Ok(h) => h,
        Err(e) => {
            if is_sse {
                eprintln!("hs-mcp: logging init failed: {e:#}");
            }
            return None;
        }
    };
    if let Some(storage_cfg) = primary_storage {
        if let Ok(storage) = logging::build_logs_storage(&storage_cfg, &logs_yaml.bucket).await {
            let _ = handle.spawn_shipper(storage);
        }
    }
    Some(handle)
}

#[cfg(test)]
mod provider_arg_tests {
    use super::resolve_provider_arg;
    use paper::cli::ProviderArg;

    #[test]
    fn none_routes_to_all() {
        assert!(matches!(resolve_provider_arg(None), Ok(ProviderArg::All)));
    }

    #[test]
    fn pmc_aliases_to_europe_pmc() {
        assert!(matches!(
            resolve_provider_arg(Some("pmc")),
            Ok(ProviderArg::EuropePmc)
        ));
    }

    #[test]
    fn s2_aliases_to_semantic_scholar() {
        assert!(matches!(
            resolve_provider_arg(Some("s2")),
            Ok(ProviderArg::SemanticScholar)
        ));
    }

    #[test]
    fn canonical_names_resolve() {
        assert!(matches!(
            resolve_provider_arg(Some("crossref")),
            Ok(ProviderArg::CrossRef)
        ));
        assert!(matches!(
            resolve_provider_arg(Some("europmc")),
            Ok(ProviderArg::EuropePmc)
        ));
        assert!(matches!(
            resolve_provider_arg(Some("semantic_scholar")),
            Ok(ProviderArg::SemanticScholar)
        ));
    }

    #[test]
    fn unknown_returns_error_naming_value_and_aliases() {
        let err = resolve_provider_arg(Some("bogus")).unwrap_err();
        assert!(
            err.contains("bogus"),
            "error should name the bad value: {err}"
        );
        assert!(err.contains("pmc"), "error should hint at pmc alias: {err}");
    }
}

#[cfg(test)]
mod startup_tests {
    use super::HomeStillMcp;

    /// Regression for P0-4: when `~/.home-still/config.yaml` has no
    /// `storage:` section, the MCP server must refuse to start instead
    /// of silently falling back to LocalFsStorage rooted at project_dir.
    #[tokio::test]
    async fn refuses_to_start_without_storage_config() {
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("HOME").ok();
        // SAFETY: #[tokio::test] default runtime is single-threaded; this
        // block brackets the HOME mutation to a single await region.
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let result = HomeStillMcp::new().await;
        unsafe {
            match prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        let err = result
            .err()
            .expect("new() must fail without storage config");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("storage"),
            "error should mention storage; got: {msg}"
        );
    }
}

#[cfg(test)]
mod distill_search_mapping_tests {
    use super::map_distill_search_hits;
    use hs_distill::client::SearchHit;

    fn fixture() -> Vec<SearchHit> {
        vec![SearchHit {
            doc_id: "10.1234_test".to_string(),
            title: Some("Test paper".to_string()),
            authors: vec!["Doe".to_string()],
            year: Some(2024),
            doi: Some("10.1234/test".to_string()),
            chunk_text: "lorem ipsum dolor sit amet".to_string(),
            score: 0.87,
            pdf_path: Some("papers/10/10.1234_test.pdf".to_string()),
            line_start: 12,
            line_end: 16,
            page: Some(3),
            category: None,
        }]
    }

    #[test]
    fn include_text_true_preserves_chunk_text() {
        let out = map_distill_search_hits(fixture(), true);
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"chunk_text\""), "json: {json}");
        assert!(json.contains("lorem ipsum"), "json: {json}");
    }

    #[test]
    fn include_text_false_omits_chunk_text_key() {
        let out = map_distill_search_hits(fixture(), false);
        let json = serde_json::to_string(&out).unwrap();
        assert!(
            !json.contains("\"chunk_text\""),
            "chunk_text key should be absent; got: {json}"
        );
        assert!(
            !json.contains("lorem ipsum"),
            "passage text should not leak; got: {json}"
        );
        assert!(json.contains("\"doc_id\":\"10.1234_test\""));
        assert!(json.contains("\"doi\":\"10.1234/test\""));
        assert!(json.contains("\"score\":0.87"));
    }

    #[test]
    fn empty_input_round_trips() {
        let full = serde_json::to_string(&map_distill_search_hits(vec![], true)).unwrap();
        let lite = serde_json::to_string(&map_distill_search_hits(vec![], false)).unwrap();
        assert_eq!(full, "[]");
        assert_eq!(lite, "[]");
    }
}
