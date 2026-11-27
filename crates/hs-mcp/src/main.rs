use std::sync::Arc;

use clap::Parser;
use hs_common::event_bus::EventBus;
use hs_common::storage::Storage;
use rmcp::{
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter},
        wrapper::Parameters,
    },
    model::{
        CallToolResult, ErrorData, GetPromptRequestParams, GetPromptResult, ListPromptsResult,
        PaginatedRequestParams, ProgressNotificationParam, PromptMessage, PromptMessageRole,
        ServerCapabilities, ServerInfo,
    },
    prompt, prompt_handler, prompt_router, schemars,
    service::RequestContext,
    tool, tool_router, RoleServer, ServerHandler,
};

mod http_app;
mod links;
mod openalex;
mod snapshot;
mod stem;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tool_tests;
mod tools;

// ── MCP Server ──────────────────────────────────────────────────

#[derive(Clone)]
struct HomeStillMcp {
    // Primary read-path handle: Storage trait (local fs or Garage/S3).
    storage: Arc<dyn Storage>,
    // Event bus for cross-service notifications (`papers.ingested`).
    // Required: `HomeStillMcp::new` fails if the config has no usable
    // `events:` section.
    events: Arc<dyn EventBus>,
    /// Where the `academic` store keeps its objects: what `acquire` writes
    /// to and the dashboard snapshot counts.
    academic: hs_common::stores::DocumentLayout,
    /// The scribe servers `status` asks for their readiness.
    scribe_servers: Vec<String>,
    /// Every paper provider, with its rate limiters and circuit breakers,
    /// built once so all requests share them.
    providers: Arc<paper::providers::set::ProviderSet>,
    /// The downloader over the shared providers, storage and event bus.
    downloader: Arc<paper::providers::downloader::PaperDownloader>,
    /// The distill servers `status` asks what the index holds.
    distill_servers: Vec<String>,
    /// The local OpenAlex corpus, or why it cannot be read.
    openalex: openalex::OpenAlexCorpus,
    /// Mints the signed, expiring links that let a user open what an
    /// agent found.
    link_minter: Option<Arc<crate::links::LinkMinter>>,
    /// The short links this deployment has minted, stored in the corpus
    /// beside the documents.
    short_links: hs_common::docs::ShortLinks,
    /// The stores of the corpus: what a `store` argument and a link's
    /// `col` are checked against.
    stores: hs_common::stores::Stores,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

/// Everything a [`HomeStillMcp`] is built from. `HomeStillMcp::new` fills it
/// from the config files; tests fill it with fakes.
struct Deps {
    storage: Arc<dyn Storage>,
    events: Arc<dyn EventBus>,
    scribe_servers: Vec<String>,
    paper_config: paper::config::Config,
    distill_servers: Vec<String>,
    openalex: openalex::OpenAlexCorpus,
    link_minter: Option<Arc<crate::links::LinkMinter>>,
    stores: hs_common::stores::Stores,
}

/// Serialize a tool result. A serialization failure is a tool error, never
/// an empty "success".
fn to_json<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|e| format!("serializing the result failed: {e}"))
}

/// Sends `notifications/progress` about every 20 s while it is alive, so a
/// tool that runs for minutes without a natural progress signal keeps the
/// client's tool-call timer and the server's session idle timer from firing.
/// Does nothing when the caller sent no progress token.
struct ProgressHeartbeat(Option<tokio::task::JoinHandle<()>>);

impl ProgressHeartbeat {
    const EVERY: std::time::Duration = std::time::Duration::from_secs(20);

    fn start(context: &RequestContext<RoleServer>, label: String) -> Self {
        let Some(token) = context.meta.get_progress_token() else {
            return Self(None);
        };
        let peer = context.peer.clone();
        let started = std::time::Instant::now();
        Self(Some(tokio::spawn(hs_common::panic_guard::supervise(
            "progress-heartbeat",
            move || {
                let (peer, token, label) = (peer.clone(), token.clone(), label.clone());
                async move {
                    let mut tick = tokio::time::interval_at(
                        tokio::time::Instant::now() + Self::EVERY,
                        Self::EVERY,
                    );
                    loop {
                        tick.tick().await;
                        let secs = started.elapsed().as_secs();
                        let params = ProgressNotificationParam::new(token.clone(), secs as f64)
                            .with_message(format!("{label}: still running ({secs}s)"));
                        if let Err(e) = peer.notify_progress(params).await {
                            tracing::warn!(label = %label, error = %e, "progress heartbeat stopped");
                            break;
                        }
                    }
                }
            },
        ))))
    }
}

impl Drop for ProgressHeartbeat {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

impl HomeStillMcp {
    async fn new() -> anyhow::Result<Self> {
        // One read of ~/.home-still/config.yaml. A missing file is an empty
        // config; a malformed file or section stops the server with the
        // section named: nothing here substitutes defaults for it.
        Self::from_config_file(hs_common::config_file::ConfigFile::load()?).await
    }

    async fn from_config_file(file: hs_common::config_file::ConfigFile) -> anyhow::Result<Self> {
        let distill_cfg = hs_distill::config::DistillClientConfig::from_file(&file)?;
        let scribe_cfg = hs_scribe::config::ScribeConfig::from_file(&file)?;

        // Storage backend: honor the `storage:` section in
        // ~/.home-still/config.yaml. A missing section is fatal —
        // we do not fall back to LocalFsStorage at project_dir because that
        // masks typos and serves unrelated data silently (ONE PATH).
        let storage_cfg = file
            .section::<hs_common::storage::StorageConfig>("storage")?
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

        // Event bus: `acquire`, `capture` and `synthesis_save` announce what
        // they store on `papers.ingested`, so this server cannot run
        // without a valid `events:` section: a missing section, an invalid
        // key, an unset auth variable or a build without NATS stops the
        // start, and a bus that silently drops every publish is never
        // substituted. The broker connection itself is made on first
        // use: only those tools and `status` need it, and a broker outage
        // must not take the read-only tools (and a restart loop) with it.
        let events: Arc<dyn EventBus> =
            hs_common::event_bus::EventBusConfig::build_lazy(scribe_cfg.events.as_ref())
                .await
                .map_err(|e| {
                    e.context("hs-mcp needs a valid `events:` section in ~/.home-still/config.yaml")
                })?;

        // Config is the sole source of server URLs. To route through the
        // gateway, set the gateway URL explicitly in config (e.g.
        // `servers: [https://gateway.example/gateway/scribe]`).
        // hs-mcp only needs the URLs: `status` asks each server how it is.
        // No server configured is a valid state for the server as a whole;
        // `status` says so in the part of its answer that needed one.
        let scribe_servers: Vec<String> =
            scribe_cfg.servers.iter().map(|e| e.url.clone()).collect();
        let distill_servers = distill_cfg.servers.clone();

        // Local OpenAlex corpus. Not configured, not built or not openable
        // are states `discover` and `graph` report when asked for it; a
        // malformed `openalex:` section stops the start.
        let openalex = openalex::OpenAlexCorpus::open(&file)?;

        let paper_config = paper::config::Config::load()
            .map_err(|e| anyhow::anyhow!("paper config invalid: {e:#}"))?;

        let link_minter = Arc::new(crate::links::LinkMinter::from_config(&file)?);
        if link_minter.is_enabled() {
            tracing::info!("document links are enabled");
        } else {
            // A deployment without links still works; the tools simply
            // carry no links. Saying so at startup is how the operator
            // finds out why.
            tracing::warn!(
                "document links are not enabled: set `docs.lan_base_url` and \
                 `docs.remote_base_url` in ~/.home-still/config.yaml"
            );
        }
        Self::from_deps(Deps {
            storage,
            events,
            scribe_servers,
            distill_servers,
            openalex,
            paper_config,
            link_minter: Some(link_minter),
            stores: hs_common::stores::Stores::from_config(&file.load_system()?, &file)?,
        })
    }

    fn from_deps(deps: Deps) -> anyhow::Result<Self> {
        let Deps {
            storage,
            events,
            scribe_servers,
            paper_config,
            distill_servers,
            openalex,
            link_minter,
            stores,
        } = deps;

        // The academic store's layout, from the registry.
        let (_, academic) = stores
            .documents(hs_common::stores::ACADEMIC)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let academic = academic.clone();

        // One provider set for the life of the process: every request goes
        // through the same rate limiters and circuit breakers, and a bad
        // paper config stops the server here instead of failing each call.
        let providers = Arc::new(
            paper::providers::set::ProviderSet::new(&paper_config)
                .map_err(|e| anyhow::anyhow!("paper providers: {e:#}"))?,
        );
        let downloader = Arc::new(
            paper::providers::downloader::PaperDownloader::with_event_bus(
                storage.clone(),
                events.clone(),
                &paper_config.download,
                providers.download_resolvers(),
            )
            .map_err(|e| anyhow::anyhow!("paper downloader: {e:#}"))?,
        );

        Ok(Self {
            short_links: hs_common::docs::ShortLinks::new(storage.clone()),
            storage,
            events,
            academic,
            scribe_servers,
            providers,
            downloader,
            distill_servers,
            openalex,
            link_minter,
            stores,
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        })
    }

    /// A client for the distill server this host is configured to ask, or
    /// why there is none.
    fn distill_client(&self) -> Result<hs_distill::client::DistillClient, String> {
        let url = self.distill_servers.first().ok_or(
            "no distill server is configured: set `distill.servers` in \
             ~/.home-still/config.yaml",
        )?;
        hs_distill::client::DistillClient::new(url).map_err(|e| format!("{e:#}"))
    }

    /// A tool that announces its result on the event bus checks, before it
    /// does any work, that the bus can be reached: a document whose
    /// announcement nobody receives is worse than a refusal.
    async fn require_event_bus(&self, tool: &str) -> Result<(), String> {
        self.events.ensure_ready().await.map_err(|e| {
            format!(
                "{tool} announces its result on the event bus and the bus is unreachable, so \
                 nothing was done (read-only tools are unaffected): {e:#}"
            )
        })
    }
}

// ── Tools ───────────────────────────────────────────────────────
//
// Ten tools. Each one's arguments and logic are in its own file under
// `tools/`; this is the one place that names and describes them. Search is
// not here: it is the `myelin` server's `recall`.

#[tool_router]
impl HomeStillMcp {
    #[tool(
        description = "Check a quote against the document it is from, and return a citation a reader can verify. Call it for every passage you present as a quotation from the corpus, before you present it. The quote is matched against the document's own text: markup, line breaks, spacing, hyphenation, case and typography may differ, the words may not. When the document does not contain the quote the call FAILS and names the closest passage: quote that passage, or do not quote. On success it returns the passage as the document has it; the document's title, authors, year and DOI; where the passage is (its lines, and for a PDF whose word boxes are built its page and the rectangles around its words: `boxes.status` is `located`, `not_located`, `pending` or `not_applicable`); and signed, expiring links, each for the LAN and for away from home: `links.source` (the original document), `links.markdown` (the converted text, the passage selected) and, for a located passage, `links.viewer` (the PDF opened on the boxed passage) and `links.page` (that page as a picture, the passage boxed). `not_located` means the converted text has the passage and the words read from the PDF's pages do not, so the conversion may be wrong there: tell the reader the quote is not confirmed on the page. `image: true` also returns that picture in the result. `short` also stores a 12-character code that redirects to one of the links. Call it again for fresh links when earlier ones have expired.",
        annotations(
            // Not read-only in the `short` case: it stores the code. A client
            // that plans a call by its annotations must not be told a write is
            // free.
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn cite(
        &self,
        Parameters(p): Parameters<tools::cite::CiteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // A refusal is the tool's answer, not a protocol failure: it goes
        // back as a result flagged `isError`, as every other tool's does.
        Ok(tools::cite::tool_result(self.cite_quote(p).await))
    }

    #[tool(
        description = "Read a document: its catalog card and a slice of its text. `store` and `doc_id` are a search hit's `store` and `meta.doc_id` (the `myelin` server's `recall`), or a row of `documents`. Returns two parts: a JSON answer with the card (title, authors, year, DOI, abstract, when it was added), `converted`, the `slice` returned (`line_start`, `line_end`, `page`, `total_lines`, `total_pages`, `next_line`) and signed links that open the document; then the text, every line behind its number (`12: ...`). Select with `page` (one page of the source) or `lines: [first, last]`, not both; with neither, reading starts at line 1. At most `max_chars` characters come back (12,000 by default), in whole lines: when the selection did not fit, `slice.next_line` is the line to call again from. A document that is stored and not yet converted answers `converted: false` with its card and no text: ask `status` with its `doc_id`. The line numbers are the ones `cite` reports and takes as `near_line`. Reading is for context: anything you quote goes through `cite`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn read(
        &self,
        Parameters(p): Parameters<tools::read::ReadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(tools::read::tool_result(self.read_document(p).await))
    }

    #[tool(
        description = "List the documents of one store, newest first: `academic` (papers; the default), `personal` (the user's own records), `web` (captured pages) or `syntheses` (saved answers). Each row has the `doc_id` that `read`, `cite` and `status` take, with the title, authors, year, DOI, when it was added, and whether it is converted to text and indexed for search. Narrow with `title_contains` and `added_after`. A page holds `limit` rows (20 by default, 200 at most); `next_cursor`, when present, is the `cursor` of the next page with the same filters. This lists what the corpus holds; to find documents by what they say, search with the `myelin` server's `recall`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn documents(
        &self,
        Parameters(p): Parameters<tools::documents::DocumentsParams>,
    ) -> Result<String, String> {
        self.list_documents(p).await
    }

    #[tool(
        description = "Look outside the corpus for works worth adding to it. `source` is required: `providers` asks the online paper providers (arXiv, OpenAlex, Semantic Scholar, Europe PMC, CrossRef and, with an API key, CORE); `openalex` reads the local OpenAlex corpus without the network. Give exactly one of `query` (a search; `year_from`, `year_to` and `limit` narrow it, 10 results by default and 200 at most) or `doi` (one work's full metadata; with `openalex`, its authors and topics too). A provider search answers `{works, provider_failures}`: `provider_failures` names each provider that did not answer, and the call fails only when none did. `kind: \"authors\"` (with `source: \"openalex\"` and a `query` naming a topic by id or exact name) returns that topic's most cited authors. A combination a source cannot answer is refused, never answered in part; `source: \"openalex\"` on a host whose corpus is not built is an error that says how to build it. To bring a work found here into the corpus, call `acquire` with its DOI.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn discover(
        &self,
        Parameters(p): Parameters<tools::discover::DiscoverParams>,
    ) -> Result<String, String> {
        self.discover_works(p).await
    }

    #[tool(
        description = "Step along the citation graph from a paper's DOI. `direction` is `references` (the works the paper cites) or `citations` (the works that cite it). `source` is `semantic_scholar` (the Graph API, online) or `openalex` (the local OpenAlex corpus, no network; an error that says how to build it when the corpus is not built). Returns `{doi, direction, source, works, returned, truncated}`: `truncated` says more works matched than were returned. `limit` is 1 to 1000 (100 by default), `year_from` keeps works from that year on, and `sort` is `year` (newest first, the default) or `citations` (most cited first). The works carry their source's fields: Semantic Scholar gives DOI, title, year, authors, venue and citation count; OpenAlex gives its id, DOI, title, year and citation count. To bring a work found here into the corpus, call `acquire` with its DOI.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn graph(
        &self,
        Parameters(p): Parameters<tools::graph::GraphParams>,
    ) -> Result<String, String> {
        self.graph_neighbours(p).await
    }

    #[tool(
        description = "Download a paper by DOI into the `academic` store. Tries arXiv, MDPI and the providers' open-access locations, stores the file, writes its catalog entry and announces it; conversion to text, indexing and word boxes then run in the background. Returns `{store, doc_id, doi, state, converted}`: `state` is `queued` (stored by this call; the answer carries its size and SHA-256) or `present` (the store already held it; nothing was downloaded). When `converted` is false, ask `status` with the `doc_id` until it is, then `read` and `cite`. Fails when no open copy can be downloaded, when the metadata lookup fails or when the catalog entry cannot be written; calling again repairs a half-finished acquisition.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn acquire(
        &self,
        Parameters(p): Parameters<tools::acquire::AcquireParams>,
    ) -> Result<String, String> {
        self.acquire_paper(p).await
    }

    #[tool(
        description = "Keep a web page you have read, in the `web` store, so it can be searched, read and cited like a paper. You fetch the page; pass its `url` (http or https), its whole `content` (2 MiB at most: citations are checked against this text, so not a summary) and the `content_type` the content is written in (`html`, `markdown` or `text`), with a `title` when you know it. The page's `doc_id` is made from its address, so capturing the same address again REPLACES the page captured before. Refused: content over the limit, an address that is not http(s), and HTML that is a paywall or login page rather than the article. Returns `{store, doc_id, source_url, size_bytes, replaced, next}`; conversion and indexing run in the background, and `status` with the `doc_id` says when the page can be read and cited.",
        annotations(
            read_only_hint = false,
            // Capturing an address again replaces the earlier capture.
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn capture(
        &self,
        Parameters(p): Parameters<tools::capture::CaptureParams>,
    ) -> Result<String, String> {
        self.capture_page(p).await
    }

    #[tool(
        description = "Ingest a file already staged in the personal inbox. Provide just the FILENAME (e.g. 'medical-record.pdf') — absolute paths and path separators are rejected. The user drops the file under the configured inbox directory; this tool resolves the name, runs the same ingest pipeline as `hs personal add`, and reports the assigned doc_id/title/category. PDF conversion of long documents can take many minutes; the tool blocks until indexing finishes.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn personal_add(
        &self,
        Parameters(p): Parameters<tools::personal_add::PersonalAddParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        // Runs longer than the session idle timeout can tolerate without a
        // progress signal; see `ProgressHeartbeat`.
        let _heartbeat = ProgressHeartbeat::start(&context, "personal_add".to_string());
        self.add_personal_document(p).await
    }

    #[tool(
        description = "Save an answer you wrote as a document in the `syntheses` store, so it can be found, read and cited later. Pass its `title`, its `body` (markdown, 1 MiB at most), the `citations` it stands on and optional `tags`. Every citation is `{store, doc_id, quote}` and is checked exactly as `cite` checks it: when a document does not contain its quote, NOTHING is saved and the refusal names the citation, so call `cite` on each quote first. At least one citation is required. The saved document starts with front matter listing the title, the creation time, the tags and each verified citation (store, doc_id, page, lines, quote). Returns `{store, doc_id, citations_verified, next}`; indexing runs in the background. Each call saves a new document.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn synthesis_save(
        &self,
        Parameters(p): Parameters<tools::synthesis_save::SynthesisSaveParams>,
    ) -> Result<String, String> {
        self.save_synthesis(p).await
    }

    #[tool(
        description = "How the pipeline is, or how one document is. Without `doc_id`: the queues (sources waiting to be converted, converted documents waiting to be indexed, PDFs waiting for their word boxes; each with `waiting` and `in_progress`), what the index holds for `store` (`academic` by default), and each converter's readiness. It reads only sources that answer at once and never lists the storage. With `doc_id`: whether the document's source is stored and in what format, whether it is converted, how many records each index holds for it, whether its word boxes are `built`, `pending` or `not_applicable`, and the failure recorded for it, if any. A part whose source could not answer carries `error` in place of its value; the rest of the answer stands. Use it after `acquire` or `capture` to learn when a document can be read and cited.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn status(
        &self,
        Parameters(p): Parameters<tools::status::StatusParams>,
    ) -> Result<String, String> {
        self.report_status(p).await
    }
}

// ── Prompts ─────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GuidancePromptParams {
    #[schemars(description = "The question to answer from the corpus")]
    question: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct VerifyPromptParams {
    #[schemars(description = "The claim to check against the corpus")]
    claim: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BriefPromptParams {
    #[schemars(description = "The topic to write a brief on")]
    topic: String,
}

#[prompt_router]
impl HomeStillMcp {
    #[prompt(
        description = "Answer a question from the corpus: profile, recall, cite each claim, answer with links"
    )]
    fn guidance(&self, Parameters(p): Parameters<GuidancePromptParams>) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            format!(
                "Answer this question from the user's corpus: \"{}\"\n\n\
                 1. `profile` (myelin): read what the user is known to prefer, and let it shape \
                 the form of the answer.\n\
                 2. `stores` (myelin), then `recall` over the stores the question needs: \
                 `academic` for papers, `abstracts` for a broad first pass, `web` for captured \
                 pages, `syntheses` for earlier answers, `memory` for what was said before, and \
                 `personal` only when the question is about the user's own records. Use \
                 `investigate` when one search is not enough.\n\
                 3. `read` (home-still) a hit's document when the passage needs its context.\n\
                 4. `cite` (home-still) every passage you intend to quote, with the hit's \
                 `store`, its `meta.doc_id` and the exact words. A quote that `cite` refuses is \
                 not in the document: quote the passage it names instead, or drop the claim.\n\
                 5. Answer. Give each quotation with its citation's link (`links.viewer` for a \
                 located passage, otherwise `links.markdown`), say so when `boxes.status` is \
                 `not_located`, and keep what the corpus says apart from what you infer. When \
                 the corpus does not answer the question, say that instead of answering from \
                 memory.",
                p.question
            ),
        )]
    }

    #[prompt(
        description = "Check a claim against the corpus: find support and contradiction, cite both, give a verdict"
    )]
    fn verify(&self, Parameters(p): Parameters<VerifyPromptParams>) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            format!(
                "Check this claim against the user's corpus: \"{}\"\n\n\
                 1. `recall` (myelin) twice: once for passages that support the claim, once \
                 for passages that contradict or qualify it.\n\
                 2. `read` (home-still) around the strongest passages, so none is taken out \
                 of its context.\n\
                 3. `cite` (home-still) each passage you rely on. Only a passage `cite` \
                 accepts counts as evidence.\n\
                 4. Give a verdict: supported, contradicted, mixed, or not in the corpus. \
                 List the cited passages for and against, each with its link. \"Not in the \
                 corpus\" is an answer: do not fill the gap from memory. `discover` can find \
                 works that would settle it, and `acquire` brings one in.",
                p.claim
            ),
        )]
    }

    #[prompt(
        description = "Write a cited brief on a topic from the corpus, widen the corpus if it is thin, and save the brief"
    )]
    fn brief(&self, Parameters(p): Parameters<BriefPromptParams>) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            format!(
                "Write a brief on \"{}\" from the user's corpus.\n\n\
                 1. `recall` (myelin) over `abstracts` to find the documents that matter, \
                 then over `academic`, `web` and `syntheses` for their passages.\n\
                 2. `read` (home-still) the central documents where the passages are.\n\
                 3. If the corpus is thin: `discover` and `graph` to find the central works, \
                 `acquire` them (or `capture` a page you have read), and ask `status` until \
                 they are converted.\n\
                 4. `cite` every quotation.\n\
                 5. Write the brief: the question, what the sources agree on, where they \
                 differ, what is not known. Each claim carries its citation's link.\n\
                 6. `synthesis_save` the brief with the citations it stands on, so it can be \
                 recalled later.",
                p.topic
            ),
        )]
    }
}

// ── ServerHandler ───────────────────────────────────────────────

#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for HomeStillMcp {
    /// What `#[tool_handler]` generates, plus a panic guard. A panic inside
    /// a tool would otherwise unwind the rmcp session task, which ends the
    /// whole session with no answer to the call (the HTTP catch layer cannot
    /// see it: the session runs on its own task). It becomes a tool error,
    /// logged with the tool name, and the session keeps serving.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let tool = request.name.to_string();
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        match hs_common::panic_guard::catch_panic(self.tool_router.call(tcc)).await {
            Ok(result) => result,
            Err(message) => {
                tracing::error!(tool = %tool, panic = %message, "tool handler panicked; answering with a tool error");
                Ok(rmcp::model::CallToolResult::error(vec![
                    rmcp::model::Content::text(format!(
                        "{tool} failed: internal error (the server logged the details)"
                    )),
                ]))
            }
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, ErrorData> {
        Ok(rmcp::model::ListToolsResult {
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        self.tool_router.get(name).cloned()
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_instructions(
            "home-still: the user's document corpus, with citations a reader can check.\n\n\
             Search and memory are the `myelin` server's, not this one's: `recall` (or \
             `investigate`) searches the stores, `remember` keeps what the user tells you, \
             `profile` says what they prefer. A hit names its `store` and its `meta.doc_id`; \
             every tool here takes those two.\n\n\
             - `cite` every passage you quote from the corpus BEFORE you present it. A quote \
             it refuses is not in the document.\n\
             - `read` a document for the context around a hit; `documents` lists what a \
             store holds.\n\
             - Widen the corpus: `discover` finds works, `graph` follows citations, \
             `acquire` downloads a paper by DOI, `capture` keeps a web page you have read; \
             `status` says when a new document can be read and cited.\n\
             - `synthesis_save` keeps an answer, with the citations it stands on, so it can \
             be recalled later.\n\
             - `personal_add` ingests a file the user staged in the personal inbox: it takes \
             a FILENAME, never a path. Deleting a personal document is `hs personal delete` \
             on the command line, on purpose.\n\n\
             Stores: `academic` (papers), `references` and `abstracts` (derived from the \
             papers), `personal` (the user's own records), `web` (captured pages), \
             `syntheses` (saved answers) and `memory`.\n\n\
             Prompts: guidance (answer a question), verify (check a claim), brief (write a \
             cited brief).",
        )
    }
}

// ── Entrypoint ──────────────────────────────────────────────────

/// hs-mcp — MCP server for the home-still research pipeline
#[derive(Parser)]
#[command(name = "hs-mcp", version = env!("HS_VERSION"))]
struct Args {
    /// Run as HTTP/SSE server on this address (default: stdio mode). The HTTP
    /// transport requires HS_BACKEND_TOKEN (at least 32 bytes): every request
    /// must carry `Authorization: Bearer <HS_BACKEND_TOKEN>`.
    /// Example: --serve 127.0.0.1:7445
    #[arg(long)]
    serve: Option<String>,
    /// HTTP only: drop an MCP session after this many seconds without any
    /// message. Must exceed the longest tool call that sends no progress.
    #[arg(long, default_value_t = http_app::DEFAULT_SESSION_IDLE_TIMEOUT.as_secs())]
    session_idle_timeout_secs: u64,
}

/// How this process talks MCP, resolved before anything is started so that a
/// misconfiguration stops the process instead of running it half-protected.
enum Transport {
    Stdio,
    Http {
        addr: String,
        token: hs_common::auth::backend::BackendToken,
        session_idle_timeout: std::time::Duration,
    },
}

impl Transport {
    /// `env` looks a variable up (the process environment in `main`).
    fn resolve(
        args: &Args,
        env: impl Fn(&str) -> Result<String, std::env::VarError>,
    ) -> anyhow::Result<Self> {
        let Some(addr) = args.serve.clone() else {
            return Ok(Self::Stdio);
        };
        anyhow::ensure!(
            args.session_idle_timeout_secs > 0,
            "--session-idle-timeout-secs must be greater than zero"
        );
        let token = hs_common::auth::backend::BackendToken::from_lookup(env)
            .map_err(|e| e.context("refusing to serve MCP over HTTP without a backend token"))?;
        Ok(Self::Http {
            addr,
            token,
            session_idle_timeout: std::time::Duration::from_secs(args.session_idle_timeout_secs),
        })
    }
}

include!("../../../build-support/version_marker.rs");

fn main() -> anyhow::Result<()> {
    keep_version_marker();
    use anyhow::Context as _;
    // Secrets are exported into the environment, which is only sound while
    // this is the only thread: load them before the runtime (and its worker
    // threads) exist, and refuse to start if they cannot be read. The
    // backend token resolved in `async_main` depends on this having run.
    hs_common::secrets::load_default_secrets().context("loading ~/.home-still/secrets.env")?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let args = Args::parse();
    let transport = Transport::resolve(&args, |name| std::env::var(name))?;

    // In stdio mode, stdout is the MCP protocol — never write human-readable
    // lines to stderr either, so logs are spool-only and ship to the logs
    // bucket like every other service.
    let logging_handle = install_logging(args.serve.is_some()).await;

    let server = HomeStillMcp::new().await?;

    let result: anyhow::Result<()> = match transport {
        Transport::Http {
            addr,
            token,
            session_idle_timeout,
        } => {
            tracing::info!("Starting MCP HTTP server on {addr}");
            http_app::serve(&addr, http_app::build(server, token, session_idle_timeout)).await
        }
        Transport::Stdio => {
            let transport = rmcp::transport::io::stdio();
            let ct = rmcp::service::serve_server(server, transport).await?;
            ct.waiting().await?;
            Ok(())
        }
    };

    let _ = logging_handle.shutdown().await;
    result
}

async fn install_logging(is_sse: bool) -> hs_common::logging::LoggingHandle {
    use hs_common::logging::{self, StderrOutput};
    let (service, stderr) = if is_sse {
        ("hs-mcp-sse", StderrOutput::EnvFilter("info".into()))
    } else {
        ("hs-mcp-stdio", StderrOutput::Disabled)
    };
    let sections = logging::load_config_sections()
        .unwrap_or_else(|e| logging::exit_on_config_error(service, e));
    let cfg = sections
        .logging_config(service, stderr)
        .unwrap_or_else(|e| logging::exit_on_config_error(service, e));
    let mut handle = logging::init(cfg);
    handle
        .start_shipping(sections.storage.as_ref(), &sections.logs.bucket)
        .await;
    handle
}

#[cfg(test)]
mod startup_tests {
    use super::openalex::OpenAlexCorpus;
    use super::HomeStillMcp;

    /// Regression for P0-4: when `~/.home-still/config.yaml` has no
    /// `storage:` section, the MCP server must refuse to start instead
    /// of silently falling back to LocalFsStorage rooted at project_dir.
    #[tokio::test]
    async fn refuses_to_start_without_storage_config() {
        let tmp = tempfile::tempdir().unwrap();
        let file = hs_common::config_file::ConfigFile::load_in(tmp.path()).unwrap();
        let result = HomeStillMcp::from_config_file(file).await;
        let err = result
            .err()
            .expect("new() must fail without storage config");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("storage"),
            "error should mention storage; got: {msg}"
        );
    }

    fn config_with(yaml: &str) -> (tempfile::TempDir, hs_common::config_file::ConfigFile) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".home-still");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), yaml).unwrap();
        let file = hs_common::config_file::ConfigFile::load_in(tmp.path()).unwrap();
        (tmp, file)
    }

    /// Why the corpus is unavailable; panics when it is ready.
    fn unavailable(corpus: OpenAlexCorpus) -> String {
        match corpus {
            OpenAlexCorpus::Unavailable(reason) => reason,
            OpenAlexCorpus::Ready(_) => panic!("the corpus must not be ready"),
        }
    }

    /// RA-121: a malformed `openalex:` section stops the server; it is not
    /// read as "OpenAlex is not configured".
    #[test]
    fn a_malformed_openalex_section_is_an_error() {
        for yaml in [
            "openalex:\n  snapshot_dir: /tmp/snap\n",
            "openalex:\n  db_pth: /typo\n",
            "openalex: [1, 2]\n",
            "openalex:\n  db_path: \"  \"\n",
        ] {
            let (_tmp, file) = config_with(yaml);
            let err = OpenAlexCorpus::open(&file)
                .err()
                .unwrap_or_else(|| panic!("must refuse: {yaml}"));
            assert!(format!("{err:#}").contains("openalex"), "{err:#}");
        }
    }

    #[test]
    fn an_absent_section_or_database_says_what_to_do() {
        let (_tmp, file) = config_with("storage:\n  backend: local\n");
        let reason = unavailable(OpenAlexCorpus::open(&file).unwrap());
        assert!(reason.contains("not configured"), "{reason}");
        assert!(reason.contains("`openalex:` section"), "{reason}");

        // A stale key another tool once read (live configs carry
        // `ingest_workers`) is warned about, never fatal.
        let (_tmp, file) = config_with(&format!(
            "openalex:\n  db_path: {}\n  ingest_workers: 8\n",
            "/definitely/not/there/openalex.db"
        ));
        let reason = unavailable(OpenAlexCorpus::open(&file).unwrap());
        assert!(reason.contains("has not been built"), "{reason}");
        assert!(reason.contains("hs openalex load-works"), "{reason}");
    }
}

#[cfg(test)]
mod transport_tests {
    use super::{Args, Transport};
    use clap::Parser;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn args(extra: &[&str]) -> Args {
        Args::parse_from(std::iter::once("hs-mcp").chain(extra.iter().copied()))
    }

    fn env_with(
        value: Option<&'static str>,
    ) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        move |name| match value {
            Some(v) if name == "HS_BACKEND_TOKEN" => Ok(v.to_string()),
            _ => Err(std::env::VarError::NotPresent),
        }
    }

    /// RA-24: the network-facing transport must not come up unprotected.
    #[test]
    fn http_refuses_to_start_without_a_backend_token() {
        let err = Transport::resolve(&args(&["--serve", "127.0.0.1:0"]), env_with(None))
            .err()
            .expect("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("HS_BACKEND_TOKEN"), "{msg}");
    }

    #[test]
    fn http_refuses_a_short_token_without_printing_it() {
        let err = Transport::resolve(
            &args(&["--serve", "127.0.0.1:0"]),
            env_with(Some("correct-horse-battery")),
        )
        .err()
        .expect("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("HS_BACKEND_TOKEN"), "{msg}");
        assert!(!msg.contains("correct-horse"), "the secret leaked: {msg}");
    }

    #[test]
    fn http_starts_with_a_valid_token() {
        let transport =
            Transport::resolve(&args(&["--serve", "127.0.0.1:0"]), env_with(Some(SECRET))).unwrap();
        assert!(matches!(transport, Transport::Http { .. }));
    }

    #[test]
    fn stdio_does_not_need_a_token() {
        let transport = Transport::resolve(&args(&[]), env_with(None)).unwrap();
        assert!(matches!(transport, Transport::Stdio));
    }

    #[test]
    fn a_zero_session_timeout_is_refused() {
        let result = Transport::resolve(
            &args(&["--serve", "127.0.0.1:0", "--session-idle-timeout-secs", "0"]),
            env_with(Some(SECRET)),
        );
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod docs_tests {
    use std::collections::BTreeSet;

    use super::HomeStillMcp;

    const REFERENCE: &str = include_str!("../../../docs/reference/mcp-tools.md");

    /// The whole surface. A tool is added or removed here on purpose, with
    /// its reference entry.
    const TOOLS: [&str; 10] = [
        "acquire",
        "capture",
        "cite",
        "discover",
        "documents",
        "graph",
        "personal_add",
        "read",
        "status",
        "synthesis_save",
    ];

    /// Names in headings of the form ``### `tool_name` ``.
    fn documented_tools(markdown: &str) -> BTreeSet<String> {
        markdown
            .lines()
            .filter_map(|line| {
                let name = line.trim_end().strip_prefix("### `")?.strip_suffix('`')?;
                (!name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
                    .then(|| name.to_string())
            })
            .collect()
    }

    fn defined_tools() -> BTreeSet<String> {
        HomeStillMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    #[test]
    fn the_server_offers_exactly_the_ten_tools() {
        let expected: BTreeSet<String> = TOOLS.iter().map(|name| name.to_string()).collect();
        assert_eq!(defined_tools(), expected);
    }

    #[test]
    fn every_tool_has_a_reference_entry_and_every_entry_is_a_tool() {
        let defined = defined_tools();
        let documented = documented_tools(REFERENCE);

        let missing: Vec<&String> = defined.difference(&documented).collect();
        let extra: Vec<&String> = documented.difference(&defined).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "docs/reference/mcp-tools.md disagrees with the tools the server defines.\n\
             missing from docs: {missing:?}\n\
             documents no tool: {extra:?}"
        );
    }
}
