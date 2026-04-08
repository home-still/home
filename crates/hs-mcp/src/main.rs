use std::path::PathBuf;

use clap::Parser;
use rmcp::{
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter},
        wrapper::Parameters,
    },
    model::{
        ErrorData, GetPromptRequestParams, GetPromptResult, ListPromptsResult,
        ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, PromptMessage,
        PromptMessageRole, RawResource, RawResourceTemplate, ReadResourceRequestParams,
        ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo,
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
    prompt_router: PromptRouter<Self>,
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
            prompt_router: Self::prompt_router(),
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
        if let Ok(s2) =
            paper::providers::semantic_scholar::SemanticScholarProvider::new(
                &config.providers.semantic_scholar,
            )
        {
            resolvers.push(Box::new(s2));
        }
        if let Ok(epmc) =
            paper::providers::europe_pmc::EuropePmcProvider::new(&config.providers.europe_pmc)
        {
            resolvers.push(Box::new(epmc));
        }
        if config.providers.core.api_key.is_some() {
            if let Ok(core) =
                paper::providers::core::CoreProvider::new(&config.providers.core)
            {
                resolvers.push(Box::new(core));
            }
        }

        let downloader = paper::providers::downloader::PaperDownloader::new(
            self.papers_dir.clone(),
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
            embedding: None,
        };
        hs_common::catalog::write_catalog_entry(&self.catalog_dir, &stem, &entry);

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

        // Catalog entries
        if let Ok(dir) = std::fs::read_dir(&self.catalog_dir) {
            for entry in dir.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "yaml") {
                    if let Some(stem) = path.file_stem() {
                        let stem = stem.to_string_lossy();
                        let cat = hs_common::catalog::read_catalog_entry(&self.catalog_dir, &stem);
                        let title = cat
                            .as_ref()
                            .and_then(|c| c.title.clone())
                            .unwrap_or_else(|| stem.to_string());
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
            }
        }

        // Markdown documents
        if let Ok(dir) = std::fs::read_dir(&self.markdown_dir) {
            for entry in dir.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "md") {
                    if let Some(stem) = path.file_stem() {
                        let stem = stem.to_string_lossy();
                        let size = std::fs::metadata(&path).map(|m| m.len() as u32).ok();
                        resources.push(
                            RawResource {
                                uri: format!("markdown:///{stem}"),
                                name: stem.to_string(),
                                title: None,
                                description: Some("Converted markdown document".into()),
                                mime_type: Some("text/markdown".into()),
                                size,
                                icons: None,
                                meta: None,
                            }
                            .no_annotation(),
                        );
                    }
                }
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
            // Catalog resource
            let entry = hs_common::catalog::read_catalog_entry(&self.catalog_dir, stem)
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

            let path = self.markdown_dir.join(format!("{stem}.md"));
            let content = std::fs::read_to_string(&path)
                .map_err(|_| ErrorData::resource_not_found("markdown not found", None))?;

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
