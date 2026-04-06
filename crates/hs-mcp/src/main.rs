use std::path::PathBuf;

use clap::Parser;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_router, ServerHandler,
};

// ── Tool parameter types ─────────────��──────────────────────────

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
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PaperGetParams {
    #[schemars(description = "DOI to look up")]
    doi: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CatalogReadParams {
    #[schemars(description = "Paper stem name (filename without extension)")]
    stem: String,
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
    #[schemars(description = "Topic filter keyword")]
    topic: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DistillExistsParams {
    #[schemars(description = "Document ID to check")]
    doc_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ScribeConvertParams {
    #[schemars(description = "Absolute path to the PDF file to convert")]
    pdf_path: String,
}

// ── MCP Server ────────────────────���─────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct HomeStillMcp {
    catalog_dir: PathBuf,
    markdown_dir: PathBuf,
    papers_dir: PathBuf,
    scribe_servers: Vec<String>,
    distill_servers: Vec<String>,
    tool_router: ToolRouter<Self>,
}

impl HomeStillMcp {
    async fn new() -> Self {
        let scribe_cfg = hs_scribe::config::ScribeConfig::load().unwrap_or_default();
        let distill_cfg = hs_distill::config::DistillClientConfig::load().unwrap_or_default();

        // Discover servers from gateway registry, falling back to config
        let scribe_servers = hs_common::service::registry::discover_or_fallback(
            "scribe",
            scribe_cfg.servers.clone(),
        )
        .await;
        let distill_servers = hs_common::service::registry::discover_or_fallback(
            "distill",
            distill_cfg.servers.clone(),
        )
        .await;

        Self {
            catalog_dir: scribe_cfg.catalog_dir.clone(),
            markdown_dir: scribe_cfg.output_dir.clone(),
            papers_dir: scribe_cfg.watch_dir.clone(),
            scribe_servers,
            distill_servers,
            tool_router: Self::tool_router(),
        }
    }

    fn scribe_client(&self) -> Option<hs_scribe::client::ScribeClient> {
        self.scribe_servers
            .first()
            .map(|url| hs_scribe::client::ScribeClient::new(url))
    }

    fn distill_client(&self) -> Option<hs_distill::client::DistillClient> {
        self.distill_servers
            .first()
            .map(|url| hs_distill::client::DistillClient::new(url))
    }
}

#[tool_router]
impl HomeStillMcp {
    // ── Paper Tools ────────────────────────────────────────────

    #[tool(
        description = "Search academic papers across 6 providers (arXiv, OpenAlex, Semantic Scholar, Europe PMC, CrossRef, CORE). Returns JSON array of papers with title, authors, abstract, DOI, citations."
    )]
    async fn paper_search(&self, Parameters(p): Parameters<PaperSearchParams>) -> String {
        let config = match paper::config::Config::load() {
            Ok(c) => c,
            Err(e) => return format!("Config error: {e}"),
        };

        let provider_arg = paper::cli::ProviderArg::All;
        let provider = match paper::commands::paper::make_provider(&provider_arg, &config) {
            Ok(p) => p,
            Err(e) => return format!("Provider error: {e}"),
        };

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

        let query = paper::models::SearchQuery {
            query: p.query,
            search_type,
            max_results: p.max_results.unwrap_or(10) as usize,
            offset: 0,
            date_filter,
            sort_by: paper::models::SortBy::Relevance,
            min_citations: None,
        };

        match provider.search_by_query(&query).await {
            Ok(result) => serde_json::to_string_pretty(&result.papers).unwrap_or_default(),
            Err(e) => format!("Search failed: {e}"),
        }
    }

    #[tool(description = "Look up a single paper by DOI. Returns JSON with full metadata.")]
    async fn paper_get(&self, Parameters(p): Parameters<PaperGetParams>) -> String {
        let config = match paper::config::Config::load() {
            Ok(c) => c,
            Err(e) => return format!("Config error: {e}"),
        };

        let provider_arg = paper::cli::ProviderArg::All;
        let provider = match paper::commands::paper::make_provider(&provider_arg, &config) {
            Ok(p) => p,
            Err(e) => return format!("Provider error: {e}"),
        };

        match provider.get_by_doi(&p.doi).await {
            Ok(Some(paper)) => serde_json::to_string_pretty(&paper).unwrap_or_default(),
            Ok(None) => format!("No paper found for DOI: {}", p.doi),
            Err(e) => format!("Lookup failed: {e}"),
        }
    }

    // ── Catalog Tools ──────────────────────────────────────────

    #[tool(
        description = "List all papers in the catalog with titles and conversion status. Returns JSON array."
    )]
    fn catalog_list(&self) -> String {
        let mut entries = Vec::new();
        if let Ok(dir) = std::fs::read_dir(&self.catalog_dir) {
            for entry in dir.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "yaml") {
                    if let Some(stem) = path.file_stem() {
                        let stem = stem.to_string_lossy().to_string();
                        let cat = hs_common::catalog::read_catalog_entry(&self.catalog_dir, &stem);
                        let title = cat
                            .as_ref()
                            .and_then(|c| c.title.clone())
                            .unwrap_or_default();
                        let converted = cat.as_ref().and_then(|c| c.conversion.as_ref()).is_some();
                        entries.push(serde_json::json!({
                            "stem": stem,
                            "title": title,
                            "converted": converted,
                        }));
                    }
                }
            }
        }
        entries.sort_by(|a, b| {
            a["stem"]
                .as_str()
                .unwrap_or("")
                .cmp(b["stem"].as_str().unwrap_or(""))
        });
        serde_json::to_string_pretty(&entries).unwrap_or_default()
    }

    #[tool(
        description = "Read full catalog entry for a paper. Returns JSON with metadata, conversion info, page offsets, download URLs."
    )]
    fn catalog_read(&self, Parameters(p): Parameters<CatalogReadParams>) -> String {
        match hs_common::catalog::read_catalog_entry(&self.catalog_dir, &p.stem) {
            Some(entry) => serde_json::to_string_pretty(&entry).unwrap_or_default(),
            None => format!("No catalog entry found for '{}'", p.stem),
        }
    }

    // ── Markdown Tools ─────────────────────────────────────────

    #[tool(description = "List all converted markdown documents with file sizes and page counts.")]
    fn markdown_list(&self) -> String {
        let mut entries = Vec::new();
        if let Ok(dir) = std::fs::read_dir(&self.markdown_dir) {
            for entry in dir.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "md") {
                    if let Some(stem) = path.file_stem() {
                        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        let pages = std::fs::read_to_string(&path)
                            .map(|c| c.matches("\n\n---\n\n").count() + 1)
                            .unwrap_or(0);
                        entries.push(serde_json::json!({
                            "stem": stem.to_string_lossy(),
                            "size_bytes": size,
                            "pages": pages,
                        }));
                    }
                }
            }
        }
        entries.sort_by(|a, b| {
            a["stem"]
                .as_str()
                .unwrap_or("")
                .cmp(b["stem"].as_str().unwrap_or(""))
        });
        serde_json::to_string_pretty(&entries).unwrap_or_default()
    }

    #[tool(
        description = "Read a converted markdown document. Optionally specify a page number (1-based) to read just one page."
    )]
    fn markdown_read(&self, Parameters(p): Parameters<MarkdownReadParams>) -> String {
        let path = self.markdown_dir.join(format!("{}.md", p.stem));
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                if let Some(page) = p.page {
                    let pages: Vec<&str> = content.split("\n\n---\n\n").collect();
                    if page == 0 || page > pages.len() {
                        format!(
                            "Page {} not found. Document has {} pages.",
                            page,
                            pages.len()
                        )
                    } else {
                        pages[page - 1].to_string()
                    }
                } else {
                    content
                }
            }
            Err(_) => format!(
                "Markdown not found for '{}'. Check if it has been converted.",
                p.stem
            ),
        }
    }

    // ── Scribe Tools ───────────────────��───────────────────────

    #[tool(
        description = "Check scribe server health: model status, version, in-flight conversions, available VLM slots."
    )]
    async fn scribe_health(&self) -> String {
        let client = match self.scribe_client() {
            Some(c) => c,
            None => return "No scribe server configured".into(),
        };

        let health = client.health().await.ok();
        let readiness = client.readiness().await.ok();

        serde_json::to_string_pretty(&serde_json::json!({
            "health": health,
            "readiness": readiness,
        }))
        .unwrap_or_default()
    }

    #[tool(
        description = "Convert a PDF to markdown using the scribe server. Returns the converted markdown text."
    )]
    async fn scribe_convert(&self, Parameters(p): Parameters<ScribeConvertParams>) -> String {
        let client = match self.scribe_client() {
            Some(c) => c,
            None => return "No scribe server configured".into(),
        };

        let pdf_bytes = match std::fs::read(&p.pdf_path) {
            Ok(b) => b,
            Err(e) => return format!("Cannot read PDF: {e}"),
        };

        match client.convert(pdf_bytes).await {
            Ok(md) => md,
            Err(e) => format!("Conversion failed: {e}"),
        }
    }

    // ── Distill Tools ─────────────��────────────────────────────

    #[tool(
        description = "Semantic search across indexed academic documents. Returns ranked results with text snippets, metadata, and relevance scores."
    )]
    async fn distill_search(&self, Parameters(p): Parameters<DistillSearchParams>) -> String {
        let client = match self.distill_client() {
            Some(c) => c,
            None => return "No distill server configured".into(),
        };

        let filters = hs_distill::client::SearchFilters {
            year: p.year,
            topic: p.topic,
        };

        match client
            .search(&p.query, p.limit.unwrap_or(10), filters)
            .await
        {
            Ok(hits) => serde_json::to_string_pretty(&hits).unwrap_or_default(),
            Err(e) => format!("Search failed: {e}"),
        }
    }

    #[tool(
        description = "Get distill system status: Qdrant collection info, document/chunk counts, compute device, server version."
    )]
    async fn distill_status(&self) -> String {
        let client = match self.distill_client() {
            Some(c) => c,
            None => return "No distill server configured".into(),
        };

        let health = client.health().await.ok();
        let status = client.status().await.ok();

        serde_json::to_string_pretty(&serde_json::json!({
            "health": health,
            "status": status,
        }))
        .unwrap_or_default()
    }

    #[tool(description = "Check if a specific document has been indexed in the vector database.")]
    async fn distill_exists(&self, Parameters(p): Parameters<DistillExistsParams>) -> String {
        let client = match self.distill_client() {
            Some(c) => c,
            None => return "No distill server configured".into(),
        };

        match client.doc_exists(&p.doc_id).await {
            Ok(exists) => serde_json::to_string_pretty(&serde_json::json!({
                "doc_id": p.doc_id,
                "indexed": exists,
            }))
            .unwrap_or_default(),
            Err(e) => format!("Check failed: {e}"),
        }
    }

    // ── System Tools ──────────────��────────────────────────────

    #[tool(
        description = "Full pipeline status: PDF count, markdown count, catalog count, embedded document count, server health for all services."
    )]
    async fn system_status(&self) -> String {
        let pdf_count = count_files(&self.papers_dir, "pdf");
        let md_count = count_files(&self.markdown_dir, "md");
        let catalog_count = count_files(&self.catalog_dir, "yaml");

        let scribe_health = if let Some(c) = self.scribe_client() {
            c.health().await.ok()
        } else {
            None
        };

        let distill_health = if let Some(c) = self.distill_client() {
            c.health().await.ok()
        } else {
            None
        };

        let distill_stats = if let Some(c) = self.distill_client() {
            c.status().await.ok()
        } else {
            None
        };

        serde_json::to_string_pretty(&serde_json::json!({
            "pipeline": {
                "pdfs": pdf_count,
                "markdown": md_count,
                "catalog_entries": catalog_count,
                "embedded_documents": distill_stats.as_ref().map(|s| s.documents_count),
                "embedded_chunks": distill_stats.as_ref().map(|s| s.points_count),
            },
            "services": {
                "scribe": scribe_health,
                "distill": distill_health,
            },
        }))
        .unwrap_or_default()
    }
}

fn count_files(dir: &std::path::Path, ext: &str) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == ext))
                .count() as u64
        })
        .unwrap_or(0)
}

impl ServerHandler for HomeStillMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "home-still: Full academic research pipeline. Search papers across 6 providers, \
             read catalog metadata, retrieve converted markdown, perform semantic search \
             across indexed documents, and monitor pipeline health.",
        )
    }
}

// ── Entrypoint ────────────────���─────────────────────────────────

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
    let args = Args::parse();
    let server = HomeStillMcp::new().await;

    if let Some(addr) = args.serve {
        // SSE mode: Streamable HTTP transport
        tracing_subscriber::fmt()
            .with_target(false)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();

        tracing::info!("Starting MCP SSE server on {addr}");

        use rmcp::transport::streamable_http_server::{
            session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
        };

        let service = StreamableHttpService::new(
            move || Ok(server.clone()),
            std::sync::Arc::new(LocalSessionManager::default()),
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
    } else {
        // stdio mode: standard MCP transport
        let transport = rmcp::transport::io::stdio();
        let ct = rmcp::service::serve_server(server, transport).await?;
        let _ = ct.waiting().await;
    }

    Ok(())
}
