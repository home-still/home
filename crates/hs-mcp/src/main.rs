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
struct DistillPurgeParams {
    #[schemars(description = "doc_id whose chunks should be deleted from Qdrant")]
    doc_id: String,
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
    #[schemars(
        description = "If true, report what would be deleted without writing. Default: true."
    )]
    #[serde(default = "default_true")]
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
    distill_servers: Vec<String>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

impl HomeStillMcp {
    async fn new() -> Self {
        let distill_cfg = hs_distill::config::DistillClientConfig::load().unwrap_or_default();
        let scribe_cfg = hs_scribe::config::ScribeConfig::load().unwrap_or_default();

        // TODO: We don't want fallbacks, this leads to magical behavior.
        // Storage backend: honor the `storage:` section in ~/.home-still/config.yaml
        // (`backend: local` or `backend: s3`). If no storage section is present,
        // fall back to LocalFsStorage rooted at the configured project_dir — that
        // matches the legacy behavior of reading {project_dir}/{catalog,markdown,papers}.
        let storage: Arc<dyn Storage> = match hs_common::logging::load_config_sections().0 {
            Some(cfg) => cfg.build().unwrap_or_else(|e| {
                tracing::warn!(
                    "storage config invalid ({e}); falling back to LocalFsStorage at project_dir"
                );
                Arc::new(hs_common::storage::LocalFsStorage::new(
                    hs_common::resolve_project_dir(),
                ))
            }),
            None => Arc::new(hs_common::storage::LocalFsStorage::new(
                hs_common::resolve_project_dir(),
            )),
        };
        if let Err(e) = storage.ensure_ready().await {
            tracing::warn!("storage ensure_ready failed: {e:#}");
        }

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
            storage,
            events,
            catalog_prefix: "catalog".to_string(),
            markdown_prefix: hs_common::markdown::MARKDOWN_PREFIX.to_string(),
            papers_prefix: "papers".to_string(),
            legacy_papers_dir: scribe_cfg.watch_dir.clone(),
            legacy_markdown_dir: scribe_cfg.output_dir.clone(),
            legacy_catalog_dir: scribe_cfg.catalog_dir.clone(),
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
            embedding: None,
            embedding_skip: None,
            repair: None,
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
        description = "List all papers in the catalog with titles, conversion status, and embedded-in-Qdrant status. Returns JSON array. Supports pagination via limit/offset and filtering via `embedded=true|false` (omit for all). Use `embedded=false` to find papers stuck between convert and embed.",
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
                let converted = cat.conversion.is_some();
                let conversion_failed = cat.conversion.as_ref().is_some_and(|c| c.failed);
                let embedded = cat.embedding.as_ref().is_some_and(|e| e.chunks_indexed > 0);
                let embedding_skipped = cat.embedding_skip.is_some();
                let repaired = cat.repair.is_some();
                serde_json::json!({
                    "stem": stem,
                    "title": title,
                    "downloaded": downloaded,
                    "converted": converted,
                    "conversion_failed": conversion_failed,
                    "embedded": embedded,
                    "embedding_skipped": embedding_skipped,
                    "repaired": repaired,
                })
            })
            .collect();

        Ok(serde_json::to_string_pretty(&entries).unwrap_or_default())
    }

    #[tool(
        description = "Most recent catalog activity across all papers (download/convert/embed events). One row per event, sorted newest first. Used by the status dashboard's History pane.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_recent(
        &self,
        Parameters(p): Parameters<ListParams>,
    ) -> Result<String, String> {
        let triples =
            hs_common::catalog::list_catalog_entries_via(&*self.storage, &self.catalog_prefix)
                .await
                .map_err(|e| format!("catalog list failed: {e}"))?;

        let mut events: Vec<serde_json::Value> = Vec::new();
        for (stem, _meta, entry) in triples {
            let name = entry.title.clone().unwrap_or_else(|| stem.clone());
            if let Some(ref dl_at) = entry.downloaded_at {
                let size = entry
                    .file_size_bytes
                    .map(|b| b.to_string())
                    .unwrap_or_default();
                events.push(serde_json::json!({
                    "activity": "Download",
                    "stem": stem,
                    "name": name,
                    "detail_bytes": entry.file_size_bytes,
                    "detail": size,
                    "at": dl_at,
                }));
            }
            if let Some(ref conv) = entry.conversion {
                events.push(serde_json::json!({
                    "activity": "Convert",
                    "stem": stem,
                    "name": name,
                    "pages": conv.total_pages,
                    "duration_secs": conv.duration_secs,
                    "failed": conv.failed,
                    "reason": conv.reason,
                    "at": conv.converted_at,
                }));
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

        events.sort_by(|a, b| {
            let a_at = a.get("at").and_then(|v| v.as_str()).unwrap_or("");
            let b_at = b.get("at").and_then(|v| v.as_str()).unwrap_or("");
            b_at.cmp(a_at)
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
        {
            Some(entry) => Ok(serde_json::to_string_pretty(&entry).unwrap_or_default()),
            None => Err(format!("No catalog entry found for '{}'", p.stem)),
        }
    }

    #[tool(
        description = "Reconcile catalog ↔ storage in three directions. Forward: document files (PDFs/HTML) on disk with no catalog row → synthesize minimal catalog entries (carry a `repair` block). Reverse: catalog rows that claim a successful conversion but whose markdown object is missing → clear the stale `conversion` / `embedding` / `embedding_skip` blocks so the row re-enters the convert queue (original `downloaded_at` / `sha256` / etc preserved). Phantom: catalog rows with neither a paper file nor a markdown in storage → delete the row outright (the YAML is the last trace; with no source to re-derive from, it's unreachable). Use `dry_run=true` first to see counts in all three directions.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn catalog_repair(
        &self,
        Parameters(p): Parameters<CatalogRepairParams>,
    ) -> Result<String, String> {
        // Forward direction: files on disk with no catalog row.
        let disk_orphans = hs_common::status::list_orphan_document_stems(
            &*self.storage,
            &self.papers_prefix,
            &self.catalog_prefix,
        )
        .await
        .map_err(|e| format!("disk-orphan scan failed: {e}"))?;

        // Reverse direction: catalog claims converted but markdown is gone.
        let md_orphans = hs_common::status::list_catalog_rows_without_markdown(
            &*self.storage,
            &self.catalog_prefix,
            &self.markdown_prefix,
        )
        .await
        .map_err(|e| format!("markdown-orphan scan failed: {e}"))?;

        // Phantom direction: catalog row with neither a paper file nor a
        // markdown file. These inflate `catalog_entries` above `documents`
        // in the pipeline rollup and have no reachable payload anywhere —
        // safe to delete once confirmed via dry-run.
        let phantom_orphans = hs_common::status::list_catalog_rows_without_source(
            &*self.storage,
            &self.papers_prefix,
            &self.catalog_prefix,
            &self.markdown_prefix,
        )
        .await
        .map_err(|e| format!("phantom-orphan scan failed: {e}"))?;

        // Flag-drift direction: catalog row where a stage flag is missing but
        // the storage evidence for that stage exists. Backfills without
        // deleting — unlike the three directions above, which clear or synth.
        let drift_rows = hs_common::status::list_catalog_flag_drift(
            &*self.storage,
            &self.papers_prefix,
            &self.catalog_prefix,
            &self.markdown_prefix,
        )
        .await
        .map_err(|e| format!("flag-drift scan failed: {e}"))?;

        // Stuck-convert direction: catalog has a PDF/HTML source on disk but
        // no `conversion` stamp. Re-queues via the event bus rather than an
        // inline scribe call — publishes `papers.ingested`, which the
        // `hs scribe watch-events` daemon converts and `hs distill
        // watch-events` then embeds. Type A rows (ghost Qdrant chunks from a
        // prior cycle whose markdown was later deleted) also get purged
        // here so the re-index writes fresh points instead of mixing with
        // stale ones.
        let stuck_rows = hs_common::status::list_catalog_stuck_convert(
            &*self.storage,
            &self.papers_prefix,
            &self.catalog_prefix,
        )
        .await
        .map_err(|e| format!("stuck-convert scan failed: {e}"))?;

        let disk_total = disk_orphans.len();
        let md_total = md_orphans.len();
        let phantom_total = phantom_orphans.len();
        let drift_total = drift_rows.len();
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
        let distill_client_for_purge = self.distill_client();
        let mut md_qdrant_purged = 0u64;
        for stem in md_orphans.iter().take(limit) {
            let Some(mut entry) = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                stem,
            )
            .await
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
        let mut drift_conversion_repaired = 0u64;
        let mut drift_download_repaired = 0u64;
        for row in drift_rows.iter().take(limit) {
            let Some(mut entry) = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                &row.stem,
            )
            .await
            else {
                errors.push(format!(
                    "drift/{}: catalog entry vanished mid-repair",
                    row.stem
                ));
                continue;
            };
            let mut changed = false;
            if row.conversion_missing_with_markdown && entry.conversion.is_none() {
                entry.conversion = Some(hs_common::catalog::ConversionMeta {
                    server: "catalog_repair:flag_drift".to_string(),
                    duration_secs: 0.0,
                    total_pages: 0,
                    converted_at: now.clone(),
                    pages: Vec::new(),
                    failed: false,
                    reason: Some(
                        "backfilled by flag_drift repair; markdown existed without conversion stamp"
                            .to_string(),
                    ),
                });
                drift_conversion_repaired += 1;
                changed = true;
            }
            if row.download_stamp_missing_with_source && entry.downloaded_at.is_none() {
                entry.downloaded_at = Some(now.clone());
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
        description = "Find and remove URL-encoded duplicate stems. When a PDF is ingested twice — once with the original filename containing unicode or unsafe chars, once with the URL-encoded form (e.g., `Anna%E2%80%99s Archive` + `Anna's Archive`) — both markdown objects get written and the decoded-form is usually the one that embeds. The encoded-form is a ghost: it may have a catalog row with only `embedding_skip`, a markdown in storage, and no Qdrant points. This tool finds pairs where both an encoded stem and its decoded twin exist as markdown, confirms the decoded twin is the one that was indexed, and deletes the encoded-form catalog row, markdown object, and any stray Qdrant points. Default is dry-run.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn dedupe_url_encoded(
        &self,
        Parameters(p): Parameters<DedupeUrlEncodedParams>,
    ) -> Result<String, String> {
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

        if p.dry_run {
            return Ok(serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": true,
                "pairs_found": total,
                "would_delete_encoded_rows": total,
                "samples": samples,
            }))
            .unwrap_or_default());
        }

        let mut md_deleted = 0u64;
        let mut md_missing = 0u64;
        let mut cat_deleted = 0u64;
        let mut errors: Vec<String> = Vec::new();

        for (encoded, _decoded) in &pairs {
            // 1. Delete the markdown storage object. Object-store's S3 bulk
            // DeleteObjects path re-URL-encodes keys that already contain
            // percent-sequences, producing a 404 — tolerate that, since the
            // objective is to break the Embed/EmbedSkip loop, which the
            // catalog-row delete alone accomplishes.
            let md_key = format!(
                "{}/{}",
                self.markdown_prefix.trim_end_matches('/'),
                hs_common::sharded_key(encoded, "md")
            );
            match self.storage.delete(&md_key).await {
                Ok(()) => md_deleted += 1,
                Err(e) => {
                    let msg = format!("{e}");
                    if msg.contains("NoSuchKey") || msg.contains("404") {
                        md_missing += 1;
                    } else {
                        errors.push(format!("md/{encoded}: {msg}"));
                    }
                }
            }

            // 2. Delete the catalog YAML row. This is the load-bearing
            // operation — with no row, the reconciler won't try to re-embed
            // the encoded stem, and the Embed/EmbedSkip loop breaks.
            match hs_common::catalog::delete_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                encoded,
            )
            .await
            {
                Ok(()) => cat_deleted += 1,
                Err(e) => errors.push(format!("cat/{encoded}: {e}")),
            }

            // Deliberately NOT calling distill.delete_doc here: the reqwest
            // client URL-encodes the path segment, axum URL-decodes it twice
            // in the server router, and the encoded stem's doc_id round-
            // trips to the decoded twin's doc_id — deleting the wrong (good)
            // embeddings. Since the reconciler has already confirmed the
            // encoded form is embed_missing, there's nothing to clean up in
            // Qdrant anyway.
        }

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "dry_run": false,
            "pairs_found": total,
            "markdown_deleted": md_deleted,
            "markdown_already_missing": md_missing,
            "catalog_rows_deleted": cat_deleted,
            "samples": samples,
            "errors": errors,
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
        let pdf_key = hs_common::sharded_key(&p.stem, "pdf");
        let html_key = hs_common::sharded_key(&p.stem, "html");

        let start = std::time::Instant::now();
        let (md, source_key, server_label) = if let Ok(pdf_bytes) = self.storage.get(&pdf_key).await
        {
            let client = self.scribe_client().ok_or("No scribe server configured")?;
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
            let md = client
                .convert_with_progress(pdf_bytes, on_progress)
                .await
                .map_err(|e| format!("Conversion failed: {e}"))?;
            let server_url = self
                .scribe_servers
                .first()
                .map(|s| s.as_str())
                .unwrap_or("");
            let short = server_url
                .strip_prefix("http://")
                .or_else(|| server_url.strip_prefix("https://"))
                .unwrap_or(server_url);
            (md, pdf_key, short.to_string())
        } else if let Ok(html_bytes) = self.storage.get(&html_key).await {
            let html = String::from_utf8(html_bytes)
                .map_err(|e| format!("HTML at {html_key} is not valid UTF-8: {e}"))?;
            let md = hs_scribe::html::convert_html_to_markdown(&html);
            (md, html_key, "local-html".to_string())
        } else {
            return Err(format!(
                "No PDF or HTML found for '{}' (tried {pdf_key} and {html_key})",
                p.stem
            ));
        };
        let duration_secs = start.elapsed().as_secs_f64();

        let (md, truncations) = hs_scribe::postprocess::clean_repetitions(&md);
        if truncations > 0 {
            tracing::info!("{}: cleaned {} repetition site(s)", p.stem, truncations);
        }

        let page_offsets = hs_common::catalog::compute_page_offsets(&md);
        let total_pages = page_offsets.len() as u64;

        if hs_scribe::postprocess::qc_verdict(truncations, total_pages)
            == hs_scribe::postprocess::QcVerdict::RejectLoop
        {
            if let Err(e) = hs_common::catalog::update_conversion_failed_via(
                &*self.storage,
                &self.catalog_prefix,
                &p.stem,
                &server_label,
                duration_secs,
                total_pages,
                "repetition_loop",
            )
            .await
            {
                tracing::warn!("failed-conversion catalog stamp failed for {}: {e}", p.stem);
            }
            return Err(format!(
                "{}: VLM repetition loop ({} truncation site(s) across {} page(s)) — not persisted",
                p.stem, truncations, total_pages
            ));
        }

        if hs_scribe::postprocess::is_stub_pdf(total_pages, &md, duration_secs) {
            // Record the failure on the catalog so the doc isn't silently
            // retried by every backfill pass, and so it's visible in
            // `catalog_list` via the `conversion_failed` flag.
            if let Err(e) = hs_common::catalog::update_conversion_failed_via(
                &*self.storage,
                &self.catalog_prefix,
                &p.stem,
                &server_label,
                duration_secs,
                total_pages,
                "stub_document",
            )
            .await
            {
                tracing::warn!("failed-conversion catalog stamp failed for {}: {e}", p.stem);
            }
            return Err(format!(
                "{}: stub document (≤1 page, <500 non-whitespace chars or sub-second convert) — not persisted",
                p.stem
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
            topic: None,
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
        .await;

        let key = hs_common::markdown::resolve_markdown_key(
            &self.markdown_prefix,
            &p.stem,
            catalog_entry
                .as_ref()
                .and_then(|e| e.markdown_path.as_deref()),
        );
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

    #[tool(
        description = "Delete every Qdrant chunk for a given doc_id. Use to clear orphaned vectors after markdown has been removed, or to force a clean re-index.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn distill_purge(
        &self,
        Parameters(p): Parameters<DistillPurgeParams>,
    ) -> Result<String, String> {
        let client = self
            .distill_client()
            .ok_or("No distill server configured")?;
        match client.delete_doc(&p.doc_id).await {
            Ok(deleted) => Ok(serde_json::to_string_pretty(&serde_json::json!({
                "doc_id": p.doc_id,
                "deleted": deleted,
            }))
            .unwrap_or_default()),
            Err(e) => Err(format!("Purge failed: {e}")),
        }
    }

    #[tool(
        description = "Reconcile Qdrant against markdown storage: find doc_ids whose markdown object is missing and optionally delete them. Defaults to dry_run=true — pass dry_run=false to actually delete.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn distill_reconcile(
        &self,
        Parameters(p): Parameters<DistillReconcileParams>,
    ) -> Result<String, String> {
        let client = self
            .distill_client()
            .ok_or("No distill server configured")?;
        let limit = p.limit.unwrap_or(100_000);
        let doc_ids = client
            .list_docs(limit)
            .await
            .map_err(|e| format!("list_docs failed: {e}"))?;

        let mut orphans: Vec<String> = Vec::new();
        for doc_id in &doc_ids {
            // Consult the catalog for an authoritative markdown_path first.
            // Pre-rc.241 rows can have an unsharded path; without this lookup
            // we'd flag them as ghost orphans even though the object exists.
            let catalog_entry = hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.catalog_prefix,
                doc_id,
            )
            .await;
            let key = hs_common::markdown::resolve_markdown_key(
                &self.markdown_prefix,
                doc_id,
                catalog_entry
                    .as_ref()
                    .and_then(|e| e.markdown_path.as_deref()),
            );
            if !self.storage.exists(&key).await.unwrap_or(false) {
                orphans.push(doc_id.clone());
            }
        }

        let mut deleted_total: u64 = 0;
        if !p.dry_run {
            for doc_id in &orphans {
                match client.delete_doc(doc_id).await {
                    Ok(n) => deleted_total += n,
                    Err(e) => tracing::warn!("purge {doc_id} failed: {e}"),
                }
            }
        }

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "dry_run": p.dry_run,
            "scanned_doc_ids": doc_ids.len(),
            "orphan_count": orphans.len(),
            "orphans": orphans,
            "points_deleted": deleted_total,
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
            let (cleaned, truncations) = hs_scribe::postprocess::clean_repetitions(&original);
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
        .await;

        let key = hs_common::markdown::resolve_markdown_key(
            &self.markdown_prefix,
            &p.stem,
            catalog_entry
                .as_ref()
                .and_then(|e| e.markdown_path.as_deref()),
        );
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
                let converted = cat.conversion.as_ref().is_some_and(|c| !c.failed);
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
            .await;
            let key = hs_common::markdown::resolve_markdown_key(
                &self.markdown_prefix,
                stem,
                catalog_entry
                    .as_ref()
                    .and_then(|e| e.markdown_path.as_deref()),
            );
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
        let snap = self.build_status_snapshot(20).await;
        Ok(serde_json::to_string_pretty(&snap).unwrap_or_default())
    }
}

impl HomeStillMcp {
    /// Build the unified StatusSnapshot used by both `system_status` (MCP) and
    /// `hs status` (CLI cloud-client mode). Truth source for pipeline state.
    pub(crate) async fn build_status_snapshot(
        &self,
        history_limit: usize,
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
            let client = hs_distill::client::DistillClient::new(url);
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
            let client = hs_scribe::client::ScribeClient::new(url);
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

        let conversion_failed = catalog_triples
            .as_ref()
            .map(|triples| {
                triples
                    .iter()
                    .filter(|(_, _, e)| e.conversion.as_ref().is_some_and(|c| c.failed))
                    .count() as u64
            })
            .unwrap_or(0);

        let mut pipeline = collect_pipeline_counts(
            &*self.storage,
            &self.papers_prefix,
            &self.markdown_prefix,
            &self.catalog_prefix,
            embedded_documents,
            embedded_chunks,
        )
        .await;
        pipeline.conversion_failed = conversion_failed;

        // Pipeline drift: rows in `documents` that the next-stage counts don't
        // claim. Saturating subtraction so under-counting a stage never yields
        // a negative (i.e. drift can only be >= 0). Testers assert the value
        // stays at or below PIPELINE_DRIFT_THRESHOLD.
        let total_in_flight: u64 = scribe_instances.iter().map(|s| s.in_flight).sum();
        pipeline.pipeline_drift = pipeline
            .documents
            .saturating_sub(pipeline.markdown)
            .saturating_sub(pipeline.conversion_failed)
            .saturating_sub(total_in_flight);
        pipeline.pipeline_drift_threshold = hs_common::status::PIPELINE_DRIFT_THRESHOLD;

        // History from the catalog (same source as `catalog_recent`).
        let history = match catalog_triples {
            Some(triples) => {
                let pairs: Vec<(String, hs_common::catalog::CatalogEntry)> =
                    triples.into_iter().map(|(s, _m, e)| (s, e)).collect();
                build_history(&pairs, history_limit)
            }
            None => Vec::new(),
        };

        StatusSnapshot {
            pipeline,
            scribe_instances,
            distill_instances,
            qdrant,
            history,
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

    let server = HomeStillMcp::new().await;

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
