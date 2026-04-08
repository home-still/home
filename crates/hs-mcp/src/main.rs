use std::path::PathBuf;

use clap::Parser;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ServerHandler,
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
        description = "Provider: all, arxiv, openalex, semantic_scholar, europmc, crossref, core (default: all)"
    )]
    provider: Option<String>,
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
struct ListParams {
    #[schemars(description = "Maximum items to return (default: all)")]
    limit: Option<usize>,
    #[schemars(description = "Number of items to skip (default: 0)")]
    offset: Option<usize>,
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

// ── MCP Server ──────────────────────────────────────────────────

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

        let provider_arg = match p.provider.as_deref() {
            Some("arxiv") => paper::cli::ProviderArg::Arxiv,
            Some("openalex") => paper::cli::ProviderArg::OpenAlex,
            Some("semantic_scholar") => paper::cli::ProviderArg::SemanticScholar,
            Some("europmc") => paper::cli::ProviderArg::EuropePmc,
            Some("crossref") => paper::cli::ProviderArg::CrossRef,
            Some("core") => paper::cli::ProviderArg::Core,
            _ => paper::cli::ProviderArg::All,
        };

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

        let query = paper::models::SearchQuery {
            query: p.query,
            search_type,
            max_results: p.max_results.unwrap_or(10) as usize,
            offset: p.offset.unwrap_or(0),
            date_filter,
            sort_by: paper::models::SortBy::Relevance,
            min_citations: None,
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

    // ── Catalog Tools ──────────────────────────────────────────

    #[tool(
        description = "List all papers in the catalog with titles and conversion status. Returns JSON array. Supports pagination via limit/offset.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn catalog_list(&self, Parameters(p): Parameters<ListParams>) -> Result<String, String> {
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

        let offset = p.offset.unwrap_or(0);
        if let Some(limit) = p.limit {
            entries = entries.into_iter().skip(offset).take(limit).collect();
        } else if offset > 0 {
            entries = entries.into_iter().skip(offset).collect();
        }

        Ok(serde_json::to_string_pretty(&entries).unwrap_or_default())
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
    fn catalog_read(&self, Parameters(p): Parameters<CatalogReadParams>) -> Result<String, String> {
        match hs_common::catalog::read_catalog_entry(&self.catalog_dir, &p.stem) {
            Some(entry) => Ok(serde_json::to_string_pretty(&entry).unwrap_or_default()),
            None => Err(format!("No catalog entry found for '{}'", p.stem)),
        }
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
    fn markdown_list(&self, Parameters(p): Parameters<ListParams>) -> Result<String, String> {
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

        let offset = p.offset.unwrap_or(0);
        if let Some(limit) = p.limit {
            entries = entries.into_iter().skip(offset).take(limit).collect();
        } else if offset > 0 {
            entries = entries.into_iter().skip(offset).collect();
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
    fn markdown_read(
        &self,
        Parameters(p): Parameters<MarkdownReadParams>,
    ) -> Result<String, String> {
        let path = self.markdown_dir.join(format!("{}.md", p.stem));
        match std::fs::read_to_string(&path) {
            Ok(content) => {
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
            Err(_) => Err(format!(
                "Markdown not found for '{}'. Check if it has been converted.",
                p.stem
            )),
        }
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
        let client = self.scribe_client().ok_or("No scribe server configured")?;

        let health = client.health().await.ok();
        let readiness = client.readiness().await.ok();

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "health": health,
            "readiness": readiness,
        }))
        .unwrap_or_default())
    }

    #[tool(
        description = "Convert a PDF from the papers directory to markdown using the scribe server. Takes a stem name (filename without extension). Returns the converted markdown text.",
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
    ) -> Result<String, String> {
        let client = self.scribe_client().ok_or("No scribe server configured")?;

        let pdf_path = self.papers_dir.join(format!("{}.pdf", p.stem));
        let pdf_bytes =
            std::fs::read(&pdf_path).map_err(|e| format!("Cannot read PDF '{}': {e}", p.stem))?;

        client
            .convert(pdf_bytes)
            .await
            .map_err(|e| format!("Conversion failed: {e}"))
    }

    // ── Distill Tools ──────────────────────────────────────────

    #[tool(
        description = "Semantic search across indexed academic documents. Returns ranked results with text snippets, metadata, and relevance scores.",
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
            .ok_or("No distill server configured")?;

        let filters = hs_distill::client::SearchFilters {
            year: p.year,
            topic: p.topic,
        };

        match client
            .search(&p.query, p.limit.unwrap_or(10), filters)
            .await
        {
            Ok(hits) => Ok(serde_json::to_string_pretty(&hits).unwrap_or_default()),
            Err(e) => Err(format!("Search failed: {e}")),
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
            .ok_or("No distill server configured")?;

        let md_path = self.markdown_dir.join(format!("{}.md", p.stem));
        let path_str = md_path.to_string_lossy().to_string();

        if !md_path.exists() {
            return Err(format!(
                "Markdown not found for '{}'. Convert the PDF first.",
                p.stem
            ));
        }

        match client.index_file(&path_str).await {
            Ok(result) => {
                // Write embedding metadata to catalog
                hs_common::catalog::update_embedding_catalog(
                    &self.catalog_dir,
                    &p.stem,
                    self.distill_servers
                        .first()
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                    result.chunks_indexed,
                    &result.embedding_device,
                );
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

    // ── System Tools ───────────────────────────────────────────

    #[tool(
        description = "Full pipeline status: PDF count, markdown count, catalog count, embedded document count, server health for all services.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn system_status(&self) -> Result<String, String> {
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

        Ok(serde_json::to_string_pretty(&serde_json::json!({
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
        .unwrap_or_default())
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

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HomeStillMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "home-still: Academic research pipeline server.\n\n\
             Workflows:\n\
             1. DISCOVER: paper_search → paper_get → paper_download\n\
             2. CONVERT: scribe_convert (PDF stem → markdown)\n\
             3. READ: catalog_read, markdown_read\n\
             4. INDEX: distill_index (markdown → vector DB)\n\
             5. SEARCH: distill_search (semantic search)\n\
             6. MONITOR: system_status, scribe_health, distill_status\n\n\
             Start with system_status to verify pipeline health.",
        )
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
