use figment::{
    providers::{Env, Serialized},
    Figment,
};
use hs_common::config_file::{ConfigError, ConfigFile};
use hs_common::event_bus::{EventBus, EventBusConfig};
use hs_common::hardware_profile::HardwareProfile;
use hs_common::storage::{Storage, StorageConfig};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Default ceiling on the pixels of one rendered PDF page
/// (`AppConfig::max_render_pixels`). A US-letter page at the default
/// 200 dpi is 3.7 Mpx; 36 Mpx is ~6000x6000, so an A0 poster still
/// renders (at a reduced dpi) while a page that asks for gigapixels never
/// gets its bitmap allocated. At 4 bytes per pixel the bitmap is 144 MB,
/// plus the image copy made from it.
pub const DEFAULT_MAX_RENDER_PIXELS: u64 = 36_000_000;

/// Section of `~/.home-still/config.yaml` holding the scribe SERVER's
/// settings ([`AppConfig`]); the client's live under `scribe:`
/// ([`ScribeConfig`]). Mirrors `distill_server:` / `distill:`.
pub const SERVER_SECTION: &str = "scribe_server";

/// Section of `~/.home-still/config.yaml` holding the client's settings.
pub const CLIENT_SECTION: &str = "scribe";

/// `HS_SCRIBE_<KEY>` overrides `<section>.<key>` for every scalar key of
/// the section's struct (`HS_SCRIBE_VLM_CONCURRENCY` →
/// `scribe_server.vlm_concurrency`, `HS_SCRIBE_CONVERT_TIMEOUT_SECS` →
/// `scribe.convert_timeout_secs`). The two structs have disjoint keys, so
/// one prefix serves both; a name that is no key of the struct being
/// loaded is ignored (`HS_SCRIBE_DIAG_DIR` and friends are read directly).
fn env_overrides(section: &'static str) -> Env {
    Env::prefixed("HS_SCRIBE_").map(move |key| format!("{section}.{key}").into())
}

/// Layer `defaults`, the `section` of the config file and the `HS_SCRIBE_*`
/// environment (later wins) and extract `T`. A section that is present but
/// does not fit `T` is an error naming the file, the section and the key.
fn extract_section<T>(
    file: &ConfigFile,
    section: &'static str,
    defaults: &T,
) -> Result<T, ConfigError>
where
    T: Serialize + serde::de::DeserializeOwned,
{
    let invalid = |e: figment::Error| ConfigError::section(file.path(), section, e);
    let mut figment = Figment::from(Serialized::default(section, defaults));
    if let Some(from_file) = file.section_json(section)? {
        figment = figment.merge(Serialized::default(section, from_file));
    }
    figment
        .merge(env_overrides(section))
        .focus(section)
        .extract()
        .map_err(invalid)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendChoice {
    #[default]
    Ollama,
    Cloud,
    OpenAi,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum PipelineMode {
    FullPage,
    #[default]
    PerRegion,
}

/// Which converter the scribe server uses for the inbound `/scribe` POST.
///
/// `Legacy` is the per-region OcrEngine pipeline (render → layout-detect
/// → per-region VLM via `BackendChoice`) — what scribe has always done.
///
/// `OlmOcr` shells out to the `olmocr` CLI (allenai/olmOCR-2-7B-1025-FP8
/// via vLLM). Olmocr does its own rendering, anchoring against the PDF's
/// text layer, and produces flat markdown. The per-region pipeline,
/// streaming repetition detector, and QC postprocess are all bypassed —
/// olmocr returns assembled markdown which the server returns as-is.
///
/// Selected at server startup via `HS_SCRIBE_CONVERTER=olmocr` so a
/// single binary serves both backends (different scribe-server processes
/// on different ports, each with its own env-selected converter).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConverterMode {
    #[default]
    Legacy,
    Olmocr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub ollama_url: String,
    pub model: String,
    pub cloud_api_key: Option<String>,
    pub cloud_url: String,
    pub openai_url: String,
    /// Bearer token for the OpenAI-compatible VLM backend. `None` when the
    /// backend is unauthenticated (e.g. a bare `llama-server` on big). Set
    /// via `HS_SCRIBE_OPENAI_API_KEY` when routing through an auth-gated
    /// proxy such as `llama-swap` on a daily-driver host.
    pub openai_api_key: Option<String>,
    pub backend: BackendChoice,
    /// Wall-clock deadline (seconds) for a single PDF convert on the server.
    /// The handler wraps `process_pdf_*` in `tokio::time::timeout()` — when
    /// this fires, the inner future chain is dropped, which cancels every
    /// in-flight VLM request to Ollama (reqwest is cancel-safe) and releases
    /// the VLM semaphore permit. Without this, a slow Ollama backend (e.g.
    /// Apple Silicon on big_mac / mac_air) could leave the handler polling
    /// forever after the client disconnected, wedging all convert slots until
    /// the process was restarted. Matches the client-side
    /// `ScribeConfig::convert_timeout_secs` default (900s); tune via
    /// `HS_SCRIBE_CONVERT_DEADLINE_SECS`.
    pub convert_deadline_secs: u64,
    /// Ceiling (seconds) on the per-request `X-Convert-Deadline-Secs` a
    /// caller may ask for. A larger request is clamped to this, so one
    /// client cannot pin a converter slot (and its temp file) for longer
    /// than the operator allows. Must be at least `convert_deadline_secs`
    /// and should match the event consumer's `ack_wait` (default 7200 s):
    /// a conversion that outlives it is redelivered anyway. Override via
    /// `HS_SCRIBE_MAX_CONVERT_DEADLINE_SECS`.
    pub max_convert_deadline_secs: u64,
    pub dpi: u16,
    /// Ceiling on the pixels of one rendered PDF page. A page whose
    /// `MediaBox x dpi` exceeds it is rendered at a proportionally lower
    /// dpi, and a box that is not a plausible page is refused. Applied
    /// before the bitmap is allocated. Override via
    /// `HS_SCRIBE_MAX_RENDER_PIXELS`.
    pub max_render_pixels: u64,
    pub parallel: usize,
    pub pipeline_mode: PipelineMode,
    pub layout_model_path: String,
    pub table_model_path: String,
    pub region_parallel: usize,
    /// Cap on concurrent pages-in-flight per single PDF conversion.
    /// Inserted between stage-1 (CPU layout) and stage-2 (GPU VLM) so a
    /// single large paper can't fan out to dozens of pages × N regions
    /// against a llama-server slot pool sized for the whole cluster.
    /// `dispatcher_concurrency × page_parallel × region_parallel` is the
    /// real worst-case concurrent VLM call count — keep that ≤ a small
    /// multiple of llama-server's `--parallel` to avoid prompt-cache
    /// eviction thrash. Empirically: 2 keeps a 3090 + 8-slot llama
    /// healthy under sustained load.
    pub page_parallel: usize,
    pub use_cuda: bool,
    pub max_image_dim: u32,
    /// Converter capacity. In `Legacy` mode: concurrent VLM calls across
    /// all conversions (the shared VLM semaphore). In `Olmocr` mode:
    /// concurrent `olmocr` CLI runs on this host. It is also the most
    /// conversions the server admits at once (further uploads are refused
    /// with 503), and the slot total `/readiness` advertises.
    pub vlm_concurrency: usize,
    /// Longest silence tolerated on a VLM backend connection, in seconds:
    /// the wait for the first byte (cold model load, prompt-cache rebuild)
    /// and the gap between reads of a streaming answer. A backend that goes
    /// quiet for longer fails the region (and so the conversion) instead of
    /// holding a VLM permit until the whole-convert deadline. Override via
    /// `HS_SCRIBE_VLM_IDLE_TIMEOUT_SECS`.
    pub vlm_idle_timeout_secs: u64,
    /// Longest one Ollama generate call may take, in seconds. The Ollama
    /// backend is non-streaming, so this bounds the whole generation (a
    /// stalled Ollama otherwise pinned a VLM permit until the convert
    /// deadline). Must be at least 1 and, with the Ollama backend, no more
    /// than `max_convert_deadline_secs`. Override via
    /// `HS_SCRIBE_OLLAMA_REQUEST_TIMEOUT_SECS`.
    pub ollama_request_timeout_secs: u64,
    /// Which converter implements `/scribe`. Defaults to `Legacy` so
    /// existing deployments are unaffected; set `HS_SCRIBE_CONVERTER=olmocr`
    /// on hosts running the olmocr/vLLM scribe instance.
    #[serde(default)]
    pub converter: ConverterMode,
    /// vLLM endpoint serving olmocr (OpenAI-compatible). Consumed only
    /// when `converter == Olmocr`. Override via `HS_SCRIBE_OLMOCR_ENDPOINT`.
    pub olmocr_endpoint: String,
    /// Model name vLLM advertises for olmocr (matches its
    /// `--served-model-name`). Override via `HS_SCRIBE_OLMOCR_MODEL`.
    pub olmocr_model: String,
    /// Path to the `olmocr` CLI binary. Override via `HS_SCRIBE_OLMOCR_BIN`.
    /// Default `"olmocr"` lets the OS PATH lookup find it; on big the
    /// pinned location is `~/.local/share/olmocr-vllm/venv/bin/olmocr`.
    pub olmocr_bin: String,
    /// Free VRAM required to cold-start the VLM backend, MB. Skipped
    /// when the model is already resident. vLLM at
    /// `--gpu-memory-utilization 0.60` on a 24 GiB card needs ~14.7 GiB,
    /// so dispatching below this only buys a `healthCheckTimeout` stall.
    /// Hosts without an NVIDIA GPU have no free-VRAM signal and skip the
    /// gate entirely. Override via `HS_SCRIBE_VRAM_HEADROOM_MB`.
    pub vram_headroom_mb: u64,
}
impl Default for AppConfig {
    fn default() -> Self {
        let class = HardwareProfile::detect().class;
        Self {
            ollama_url: "http://localhost:11434".into(),
            model: "glm-ocr:latest".into(),
            cloud_api_key: None,
            cloud_url: "https://api.z.ai/api/paas/v4/layout_parsing".into(),
            openai_url: "http://localhost:8080".into(),
            openai_api_key: None,
            backend: BackendChoice::Ollama,
            convert_deadline_secs: 900,
            max_convert_deadline_secs: 7200,
            max_render_pixels: DEFAULT_MAX_RENDER_PIXELS,
            dpi: 200,
            parallel: 1,
            pipeline_mode: PipelineMode::PerRegion,
            layout_model_path: "pp-doclayoutv3.onnx".into(),
            table_model_path: "slanet-plus.onnx".into(),
            region_parallel: class.region_parallel(),
            page_parallel: 2,
            use_cuda: true,
            max_image_dim: 1800,
            vlm_concurrency: class.vlm_concurrency(),
            vlm_idle_timeout_secs: 300,
            ollama_request_timeout_secs: 600,
            converter: ConverterMode::default(),
            olmocr_endpoint: "http://localhost:8081/v1".into(),
            olmocr_model: "olmocr".into(),
            olmocr_bin: "olmocr".into(),
            vram_headroom_mb: 15000,
        }
    }
}

impl AppConfig {
    /// The scribe server's effective settings: the documented defaults,
    /// then the `scribe_server:` section of `~/.home-still/config.yaml`,
    /// then `HS_SCRIBE_*` environment variables. Any problem (unreadable or
    /// malformed file or section, a bad env value, a value the server cannot
    /// run with) is an `Err`; the server refuses to start on it.
    pub fn load() -> anyhow::Result<Self> {
        Ok(Self::from_file(&ConfigFile::load()?)?)
    }

    /// [`Self::load`] against an already-read config file.
    pub fn from_file(file: &ConfigFile) -> Result<Self, ConfigError> {
        let cfg: Self = extract_section(file, SERVER_SECTION, &Self::default())?;
        cfg.validate()
            .map_err(|e| ConfigError::section(file.path(), SERVER_SECTION, format!("{e:#}")))?;
        Ok(cfg)
    }

    /// Reject settings that would hang or panic the server: a zero
    /// semaphore size or `buffered(0)` never makes progress, a zero
    /// dpi/pixel budget renders nothing. Called by the server binary on the
    /// effective config before anything is started.
    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, value) in [
            ("vlm_concurrency", self.vlm_concurrency as u64),
            ("vlm_idle_timeout_secs", self.vlm_idle_timeout_secs),
            (
                "ollama_request_timeout_secs",
                self.ollama_request_timeout_secs,
            ),
            ("page_parallel", self.page_parallel as u64),
            ("region_parallel", self.region_parallel as u64),
            ("parallel", self.parallel as u64),
            ("dpi", u64::from(self.dpi)),
            ("max_image_dim", u64::from(self.max_image_dim)),
            ("max_render_pixels", self.max_render_pixels),
            ("convert_deadline_secs", self.convert_deadline_secs),
        ] {
            if value == 0 {
                anyhow::bail!(
                    "scribe config: `{name}` must be at least 1 (0 would hang or render nothing)"
                );
            }
        }
        if self.max_convert_deadline_secs < self.convert_deadline_secs {
            anyhow::bail!(
                "scribe config: `max_convert_deadline_secs` ({}) is below `convert_deadline_secs` ({})",
                self.max_convert_deadline_secs,
                self.convert_deadline_secs
            );
        }
        if self.backend == BackendChoice::Ollama
            && self.ollama_request_timeout_secs > self.max_convert_deadline_secs
        {
            anyhow::bail!(
                "scribe config: `ollama_request_timeout_secs` ({}) exceeds `max_convert_deadline_secs` ({}): \
                 a single request cannot outlive the longest conversion",
                self.ollama_request_timeout_secs,
                self.max_convert_deadline_secs
            );
        }
        if self.converter == ConverterMode::Olmocr {
            if self.olmocr_bin.trim().is_empty() {
                anyhow::bail!("scribe config: `olmocr_bin` is empty but converter is olmocr");
            }
            if self.olmocr_endpoint.trim().is_empty() || self.olmocr_model.trim().is_empty() {
                anyhow::bail!(
                    "scribe config: `olmocr_endpoint` and `olmocr_model` are required when converter is olmocr"
                );
            }
        }
        Ok(())
    }

    /// Resolve a model filename to an absolute path.
    /// If already absolute and exists, use as-is.
    /// Otherwise look in `~/.home-still/models/`.
    pub fn resolve_model_path(name: &str) -> PathBuf {
        let p = PathBuf::from(name);
        if p.is_absolute() && p.exists() {
            return p;
        }
        if p.exists() {
            return p;
        }
        let models_dir = dirs::home_dir()
            .unwrap_or_default()
            .join(".home-still")
            .join("models");
        models_dir.join(name)
    }

    pub fn resolved_layout_model_path(&self) -> PathBuf {
        Self::resolve_model_path(&self.layout_model_path)
    }

    pub fn resolved_table_model_path(&self) -> PathBuf {
        Self::resolve_model_path(&self.table_model_path)
    }
}

/// A scribe HTTP server entry. YAML accepts either the legacy bare URL
/// string (which defaults the backend to `glm_ocr`, matching every
/// pre-chain deployment) or a struct with explicit per-entry backend
/// metadata:
///
/// ```yaml
/// scribe:
///   servers:
///     - http://host-a.example.local:7433       # legacy: backend = glm_ocr
///     - url: http://host-a.example.local:7434  # new chain entry
///       backend: olmocr
///     - url: http://host-a.example.local:7433
///       backend: glm_ocr
/// ```
///
/// Chain order is the list order — `cmd_watch_events` walks entries
/// top-to-bottom, escalating from one backend to the next on
/// `ConvertClassification::Escalate`. The `backend` field is recorded
/// in the catalog as `ConversionMeta.converted_by` (success) and each
/// `AttemptEntry.backend` (chain audit log).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "ScribeServerEntryRepr", into = "ScribeServerEntryRepr")]
pub struct ScribeServerEntry {
    pub url: String,
    pub backend: String,
    /// Max conversions the dispatcher runs against THIS backend at once.
    /// Tuned per-backend because models differ wildly in footprint: olmocr
    /// (vLLM + a per-conversion CLI subprocess rendering pages) is RAM/VRAM
    /// heavy and wants a low cap; glm (scans, rarely hit) can run more. The
    /// dispatcher holds a semaphore of this size per backend, so a heavy
    /// model can't fan out and exhaust host RAM. Default 4.
    pub concurrency: usize,
}

/// Default backend identifier when an entry comes in as a bare URL
/// string — preserves the pre-chain semantics for every existing config
/// file in the fleet.
fn default_backend() -> String {
    "glm_ocr".to_string()
}

/// Default per-backend conversion concurrency when an entry doesn't set one.
/// Conservative so an unconfigured heavy backend can't blow up RAM/VRAM.
fn default_concurrency() -> usize {
    4
}

/// Serde wire shape for `ScribeServerEntry`. The untagged enum lets YAML
/// accept either form transparently; the type-level `from` / `into`
/// converters above keep `ScribeServerEntry`'s call-site API ergonomic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum ScribeServerEntryRepr {
    Bare(String),
    Struct {
        url: String,
        #[serde(default = "default_backend")]
        backend: String,
        #[serde(default = "default_concurrency")]
        concurrency: usize,
    },
}

impl From<ScribeServerEntryRepr> for ScribeServerEntry {
    fn from(repr: ScribeServerEntryRepr) -> Self {
        match repr {
            ScribeServerEntryRepr::Bare(url) => Self {
                url,
                backend: default_backend(),
                concurrency: default_concurrency(),
            },
            ScribeServerEntryRepr::Struct {
                url,
                backend,
                concurrency,
            } => Self {
                url,
                backend,
                concurrency,
            },
        }
    }
}

impl From<ScribeServerEntry> for ScribeServerEntryRepr {
    fn from(entry: ScribeServerEntry) -> Self {
        Self::Struct {
            url: entry.url,
            backend: entry.backend,
            concurrency: entry.concurrency,
        }
    }
}

/// Client-side scribe configuration (server list, directories).
/// Loaded from ~/.home-still/config.yaml under the "scribe" section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScribeConfig {
    pub output_dir: PathBuf,
    pub watch_dir: PathBuf,
    pub corrupted_dir: PathBuf,
    pub catalog_dir: PathBuf,
    pub servers: Vec<ScribeServerEntry>,
    /// When false, skip local scribe server init/start (client-only mode).
    /// Machines that only run the watcher and forward to remote scribe servers
    /// should set this to false.
    pub local_server: bool,
    /// Polling interval for the client-side inbox watcher
    /// (`hs scribe inbox run`). The inbox daemon sweeps
    /// `papers/manually_downloaded/` on the configured Storage at this
    /// cadence, relocates eligible files to `papers/<shard>/...`, and
    /// publishes `papers.ingested` on NATS. Default 30 seconds; a short
    /// value drains drops quickly, a long value is gentler on S3 list
    /// cost. Must be ≥ 1.
    #[serde(default = "default_inbox_poll_interval_secs")]
    pub inbox_poll_interval_secs: u64,
    /// Request timeout (seconds) for `ScribeClient::convert` /
    /// `convert_with_progress` when the subscriber could not determine
    /// the PDF page count. Acts as the reqwest client's baseline
    /// timeout; per-request overrides come from `timeout_policy`. Raise
    /// via `HS_SCRIBE_CONVERT_TIMEOUT_SECS` for outlier workloads.
    #[serde(default = "default_convert_timeout_secs")]
    pub convert_timeout_secs: u64,
    /// Caps on what an EPUB archive may expand to before it is converted
    /// (`scribe.epub.max_entries`, `max_entry_bytes`, `max_total_bytes`).
    /// An archive over any cap is refused, not truncated.
    #[serde(default)]
    pub epub: crate::epub::EpubLimits,
    /// Page-count-aware timeout policy for PDF conversion. Each
    /// dispatch reads the PDF page count (lopdf), feeds it into the
    /// policy formula (`clamp(base + pages × per_page, floor, ceiling)`),
    /// and sends that deadline both as reqwest's per-request timeout
    /// and as the `X-Convert-Deadline-Secs` header. The server mirrors
    /// the header when present so client and server agree on the
    /// deadline and neither gives up prematurely.
    #[serde(default)]
    pub timeout_policy: TimeoutPolicy,
    /// Storage backend (loaded from top-level `storage:` section, not `scribe.storage`).
    #[serde(skip)]
    pub storage: StorageConfig,
    /// Event bus (loaded from the top-level `events:` section). `None` when
    /// the config has no such section; components that publish or consume
    /// events then refuse to start ([`ScribeConfig::build_event_bus`]).
    #[serde(skip)]
    pub events: Option<EventBusConfig>,
}

fn default_inbox_poll_interval_secs() -> u64 {
    30
}

fn default_convert_timeout_secs() -> u64 {
    900
}

/// Page-count-aware timeout formula for PDF conversion. The subscriber
/// reads the page count from the raw PDF before dispatching and sizes
/// the per-request deadline as
/// `clamp(base + pages × per_page, floor, ceiling)`. A PDF whose page
/// count cannot be read is refused before dispatch, so there is no
/// "unknown size" deadline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TimeoutPolicy {
    /// Constant overhead per convert (model load, PDF parse on server,
    /// network + S3 fetch). Independent of page count.
    pub base_secs: u64,
    /// Budget per PDF page for VLM + layout + table extraction. A
    /// generous estimate — a slow Metal VLM takes ~10s/page at 1800px
    /// and `per_page_secs=15` leaves headroom for queueing and retries.
    pub per_page_secs: u64,
    /// Minimum deadline regardless of page count. A 1-page paper
    /// shouldn't time out at 75s when the actual convert takes 200s on
    /// a saturated cluster.
    pub floor_secs: u64,
    /// Maximum deadline regardless of page count. Caps a truly huge
    /// book so a poison input can't hold a delivery slot for hours.
    /// JetStream `ack_wait` must be ≥ this value.
    pub ceiling_secs: u64,
}

impl TimeoutPolicy {
    /// `clamp(floor, ceiling)` panics when `floor > ceiling`, and a zero
    /// floor lets a deadline of zero seconds through.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.floor_secs == 0 {
            anyhow::bail!("scribe.timeout_policy.floor_secs must be at least 1");
        }
        if self.floor_secs > self.ceiling_secs {
            anyhow::bail!(
                "scribe.timeout_policy.floor_secs ({}) is above ceiling_secs ({})",
                self.floor_secs,
                self.ceiling_secs
            );
        }
        Ok(())
    }
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            base_secs: 60,
            per_page_secs: 15,
            floor_secs: 300,
            ceiling_secs: 3600,
        }
    }
}

impl Default for ScribeConfig {
    /// The documented built-in defaults, rooted at `~/home-still`. Loading
    /// (`ScribeConfig::load`) roots them at the configured
    /// `home.project_dir` instead and never falls back to this value.
    fn default() -> Self {
        Self::with_project_dir(&hs_common::default_project_dir())
    }
}

impl ScribeConfig {
    /// Defaults with the directories rooted at `project` (`home.project_dir`).
    /// `servers` is empty: there is no default server to guess at.
    pub fn with_project_dir(project: &Path) -> Self {
        Self {
            output_dir: project.join("markdown"),
            watch_dir: project.join("papers"),
            corrupted_dir: project.join("corrupted"),
            catalog_dir: project.join("catalog"),
            servers: Vec::new(),
            local_server: true,
            inbox_poll_interval_secs: default_inbox_poll_interval_secs(),
            convert_timeout_secs: default_convert_timeout_secs(),
            timeout_policy: TimeoutPolicy::default(),
            epub: crate::epub::EpubLimits::default(),
            storage: StorageConfig::default(),
            events: None,
        }
    }

    /// The client's effective settings: the documented defaults (rooted at
    /// `home.project_dir`), then the `scribe:` section of
    /// `~/.home-still/config.yaml`, then `HS_SCRIBE_*` environment variables
    /// (`HS_SCRIBE_CONVERT_TIMEOUT_SECS` → `scribe.convert_timeout_secs`).
    /// `storage:` and `events:` are read from their own sections.
    ///
    /// A missing config file is a valid, empty config. A file or section that
    /// is present but malformed is an `Err` naming it; nothing here
    /// substitutes defaults for a failed load.
    pub fn load() -> anyhow::Result<Self> {
        Ok(Self::from_file(&ConfigFile::load()?)?)
    }

    /// [`Self::load`] against an already-read config file.
    pub fn from_file(file: &ConfigFile) -> Result<Self, ConfigError> {
        let defaults = Self::with_project_dir(&file.project_dir()?);
        let mut cfg: Self = extract_section(file, CLIENT_SECTION, &defaults)?;
        cfg.storage = file.section("storage")?.unwrap_or_default();
        cfg.events = EventBusConfig::from_file(file)?;
        cfg.validate()
            .map_err(|e| ConfigError::section(file.path(), CLIENT_SECTION, format!("{e:#}")))?;
        Ok(cfg)
    }

    /// The configured scribe servers, or an error telling the operator how
    /// to configure one. Commands that talk to a scribe server call this
    /// instead of guessing a `localhost` address.
    pub fn require_servers(&self) -> anyhow::Result<&[ScribeServerEntry]> {
        if self.servers.is_empty() {
            anyhow::bail!(
                "no scribe server is configured: set `scribe.servers` in {} \
                 (for example `servers: [http://<host>:7433]`; `hs config init` writes a template)",
                hs_common::CONFIG_REL_PATH
            );
        }
        Ok(&self.servers)
    }

    /// Reject values that would panic or hang the dispatcher: a deadline
    /// floor above its ceiling, a backend tier of zero concurrency (its
    /// semaphore never grants a permit), zero timeouts or poll intervals.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.timeout_policy.validate()?;
        self.epub.validate()?;
        if self.convert_timeout_secs == 0 {
            anyhow::bail!("scribe.convert_timeout_secs must be at least 1");
        }
        if self.inbox_poll_interval_secs == 0 {
            anyhow::bail!("scribe.inbox_poll_interval_secs must be at least 1");
        }
        for entry in &self.servers {
            if entry.concurrency == 0 {
                anyhow::bail!(
                    "scribe.servers entry {} has concurrency 0: nothing would ever be dispatched to it",
                    entry.url
                );
            }
        }
        Ok(())
    }

    /// Build the configured storage backend.
    pub fn build_storage(&self) -> anyhow::Result<Arc<dyn Storage>> {
        self.storage.build()
    }

    /// Build the configured event bus. A config without an `events:` section
    /// has none: this is an error, never a bus that drops everything.
    pub async fn build_event_bus(&self) -> anyhow::Result<Arc<dyn EventBus>> {
        EventBusConfig::build_required(self.events.as_ref()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_url_deserializes_with_default_backend() {
        // Legacy form — every existing config.yaml in the fleet looks
        // like this. Must continue to parse without operator action.
        let yaml = "- http://192.0.2.110:7433\n";
        let entries: Vec<ScribeServerEntry> =
            serde_yaml_ng::from_str(yaml).expect("bare URL must parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].url, "http://192.0.2.110:7433");
        assert_eq!(entries[0].backend, "glm_ocr");
    }

    #[test]
    fn struct_form_deserializes_with_explicit_backend() {
        let yaml = "\
- url: http://192.0.2.110:7434
  backend: olmocr
";
        let entries: Vec<ScribeServerEntry> =
            serde_yaml_ng::from_str(yaml).expect("struct form must parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].url, "http://192.0.2.110:7434");
        assert_eq!(entries[0].backend, "olmocr");
    }

    #[test]
    fn struct_form_with_missing_backend_defaults_to_glm_ocr() {
        // Operator wrote `url: ...` but forgot `backend:` — fall back to
        // the same default the bare form uses rather than fail loudly,
        // since that matches the legacy meaning.
        let yaml = "\
- url: http://192.0.2.110:7433
";
        let entries: Vec<ScribeServerEntry> =
            serde_yaml_ng::from_str(yaml).expect("backend-less struct must parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].backend, "glm_ocr");
    }

    #[test]
    fn mixed_list_preserves_order_and_backends() {
        // The chain semantics in Step 2d rely on this exact ordering —
        // primary backend first, fallbacks after.
        let yaml = "\
- url: http://192.0.2.110:7434
  backend: olmocr
- http://192.0.2.110:7433
- url: http://192.0.2.233:7433
  backend: glm_ocr
";
        let entries: Vec<ScribeServerEntry> =
            serde_yaml_ng::from_str(yaml).expect("mixed list must parse");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].backend, "olmocr");
        assert_eq!(entries[1].backend, "glm_ocr"); // bare → default
        assert_eq!(entries[2].backend, "glm_ocr");
    }

    #[test]
    fn roundtrip_serializes_as_struct_form() {
        // Serializing always emits the explicit form so operators can
        // see exactly which backend each URL routes to. Round-trip
        // through Vec because the From/Into pair only fires on Vec
        // elements, not on the wrapper itself.
        let entries = vec![ScribeServerEntry {
            url: "http://x:7433".into(),
            backend: "glm_ocr".into(),
            concurrency: default_concurrency(),
        }];
        let yaml = serde_yaml_ng::to_string(&entries).expect("serialize");
        assert!(yaml.contains("url: http://x:7433"));
        assert!(yaml.contains("backend: glm_ocr"));
        let parsed: Vec<ScribeServerEntry> = serde_yaml_ng::from_str(&yaml).expect("re-parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].url, "http://x:7433");
        assert_eq!(parsed[0].backend, "glm_ocr");
    }

    #[test]
    fn the_default_configs_are_valid() {
        AppConfig::default().validate().unwrap();
        ScribeConfig::default().validate().unwrap();
        TimeoutPolicy::default().validate().unwrap();
    }

    #[test]
    fn zero_concurrency_and_budgets_are_errors_not_hangs() {
        // Semaphore(0) never grants a permit and buffered(0) never polls.
        type Break = fn(&mut AppConfig);
        let cases: [(&str, Break); 9] = [
            ("vlm_concurrency", |c| c.vlm_concurrency = 0),
            ("page_parallel", |c| c.page_parallel = 0),
            ("region_parallel", |c| c.region_parallel = 0),
            ("parallel", |c| c.parallel = 0),
            ("dpi", |c| c.dpi = 0),
            ("max_render_pixels", |c| c.max_render_pixels = 0),
            ("vlm_idle_timeout_secs", |c| c.vlm_idle_timeout_secs = 0),
            ("ollama_request_timeout_secs", |c| {
                c.ollama_request_timeout_secs = 0
            }),
            ("convert_deadline_secs", |c| c.convert_deadline_secs = 0),
        ];
        for (key, break_it) in cases {
            let mut c = AppConfig::default();
            break_it(&mut c);
            let err = c.validate().unwrap_err().to_string();
            assert!(err.contains(key), "{key}: {err}");
        }
    }

    #[test]
    fn an_ollama_request_cannot_outlive_the_longest_convert_deadline() {
        let ollama = AppConfig {
            backend: BackendChoice::Ollama,
            ollama_request_timeout_secs: 7201,
            ..AppConfig::default()
        };
        assert!(ollama
            .validate()
            .unwrap_err()
            .to_string()
            .contains("ollama_request_timeout_secs"));
        // Irrelevant (and so not checked against the ceiling) for other backends.
        let other = AppConfig {
            backend: BackendChoice::OpenAi,
            ..ollama
        };
        other.validate().unwrap();
    }

    #[test]
    fn the_server_deadline_ceiling_cannot_be_below_the_default_deadline() {
        let c = AppConfig {
            convert_deadline_secs: 900,
            max_convert_deadline_secs: 600,
            ..AppConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn a_timeout_floor_above_its_ceiling_is_an_error_at_load_not_a_dispatch_panic() {
        let mut c = ScribeConfig::default();
        c.timeout_policy.floor_secs = 4000;
        c.timeout_policy.ceiling_secs = 3600;
        assert!(c.validate().unwrap_err().to_string().contains("floor_secs"));
        c.timeout_policy.floor_secs = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn a_backend_tier_with_zero_concurrency_is_refused() {
        let mut c = ScribeConfig::default();
        c.servers.push(ScribeServerEntry {
            url: "http://host-a.example:7433".into(),
            backend: default_backend(),
            concurrency: 0,
        });
        assert!(c
            .validate()
            .unwrap_err()
            .to_string()
            .contains("concurrency 0"));
        let c = ScribeConfig {
            convert_timeout_secs: 0,
            ..ScribeConfig::default()
        };
        assert!(c.validate().is_err());
        let c = ScribeConfig {
            inbox_poll_interval_secs: 0,
            ..ScribeConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn olmocr_mode_requires_its_binary_and_endpoint() {
        let mut c = AppConfig {
            converter: ConverterMode::Olmocr,
            ..AppConfig::default()
        };
        c.validate().unwrap();
        c.olmocr_bin = "  ".into();
        assert!(c.validate().is_err());
    }

    // ── Loading (RA-6, RA-97) ──────────────────────────────────────────
    //
    // These go through `from_file` against a temporary home directory, with
    // the process environment emptied and then set per test (figment's
    // `Jail`, which serialises tests that touch the environment).

    #[allow(clippy::result_large_err)] // figment's `Jail` closure type
    fn with_env<R>(vars: &[(&str, &str)], f: impl FnOnce() -> R) -> R {
        let mut out = None;
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            for (k, v) in vars {
                jail.set_env(k, v);
            }
            out = Some(f());
            Ok(())
        });
        out.expect("closure ran")
    }

    fn home_with(yaml: Option<&str>) -> (tempfile::TempDir, ConfigFile) {
        let home = tempfile::tempdir().unwrap();
        if let Some(yaml) = yaml {
            let path = home.path().join(hs_common::CONFIG_REL_PATH);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, yaml).unwrap();
        }
        let file = ConfigFile::load_in(home.path()).unwrap();
        (home, file)
    }

    #[test]
    fn no_config_file_gives_documented_defaults_with_no_server_and_no_bus() {
        let (home, file) = home_with(None);
        let cfg = with_env(&[], || ScribeConfig::from_file(&file)).unwrap();
        assert!(cfg.servers.is_empty(), "no server is guessed");
        assert!(cfg.events.is_none(), "no bus is guessed");
        assert_eq!(cfg.output_dir, home.path().join("home-still/markdown"));
        assert_eq!(cfg.convert_timeout_secs, 900);
        let err = cfg.require_servers().unwrap_err().to_string();
        assert!(err.contains("scribe.servers"), "{err}");
        // The server side needs no file at all.
        with_env(&[], || AppConfig::from_file(&file)).unwrap();
    }

    #[test]
    fn the_scribe_section_and_the_project_dir_are_honoured() {
        let (_home, file) = home_with(Some(
            "home:\n  project_dir: /srv/hs\nscribe:\n  convert_timeout_secs: 1234\n  servers:\n    - http://host-a.example:7433\n    - url: http://host-b.example:7435\n      backend: olmocr\n      concurrency: 2\nevents:\n  backend: nats\n",
        ));
        let cfg = with_env(&[], || ScribeConfig::from_file(&file)).unwrap();
        assert_eq!(cfg.convert_timeout_secs, 1234);
        assert_eq!(cfg.output_dir, PathBuf::from("/srv/hs/markdown"));
        let urls: Vec<_> = cfg.servers.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(
            urls,
            ["http://host-a.example:7433", "http://host-b.example:7435"]
        );
        assert_eq!(cfg.servers[1].backend, "olmocr");
        assert_eq!(cfg.servers[1].concurrency, 2);
        assert_eq!(
            cfg.events.as_ref().map(|e| e.backend.clone()),
            Some(hs_common::event_bus::EventsBackend::Nats)
        );
        assert_eq!(cfg.require_servers().unwrap().len(), 2);
    }

    #[test]
    fn the_documented_env_override_reaches_the_client_config() {
        // `HS_SCRIBE_CONVERT_TIMEOUT_SECS` is documented on the field; the
        // loader used to merge the variable at the root and then read the
        // `scribe` key, so it never applied.
        let (_home, file) = home_with(Some("scribe:\n  convert_timeout_secs: 1234\n"));
        let cfg = with_env(&[("HS_SCRIBE_CONVERT_TIMEOUT_SECS", "77")], || {
            ScribeConfig::from_file(&file)
        })
        .unwrap();
        assert_eq!(cfg.convert_timeout_secs, 77, "env beats the file");
        let cfg = with_env(&[], || ScribeConfig::from_file(&file)).unwrap();
        assert_eq!(cfg.convert_timeout_secs, 1234);
        // A value that is not a number is an error, not the default.
        let err = with_env(&[("HS_SCRIBE_CONVERT_TIMEOUT_SECS", "soon")], || {
            ScribeConfig::from_file(&file)
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.to_ascii_lowercase().contains("convert_timeout_secs"),
            "{err}"
        );
    }

    #[test]
    fn a_malformed_section_is_an_error_naming_it_never_the_defaults() {
        for (yaml, section) in [
            ("scribe:\n  convert_timeout_secs: soon\n", "scribe"),
            ("scribe:\n  servers: not-a-list\n", "scribe"),
            (
                "scribe:\n  timeout_policy:\n    floor_secs: 4000\n    ceiling_secs: 3600\n",
                "scribe",
            ),
            ("storage:\n  backend: carrier-pigeon\n", "storage"),
            ("events:\n  backend: carrier-pigeon\n", "events"),
            ("events:\n  nats: {url: nats://x:4222}\n", "events"),
            ("home:\n  project_dir: [a, b]\n", "home"),
        ] {
            let (_home, file) = home_with(Some(yaml));
            let err = with_env(&[], || ScribeConfig::from_file(&file))
                .expect_err(yaml)
                .to_string();
            assert!(err.contains(&format!("`{section}`")), "{yaml}: {err}");
        }
    }

    #[test]
    fn the_server_config_reads_scribe_server_and_env_wins() {
        let (_home, file) = home_with(Some(
            "scribe:\n  vlm_concurrency: 99\nscribe_server:\n  vlm_concurrency: 9\n  backend: OpenAi\n  openai_url: http://llm.example:8080\n",
        ));
        let cfg = with_env(&[], || AppConfig::from_file(&file)).unwrap();
        assert_eq!(
            cfg.vlm_concurrency, 9,
            "the scribe: section is the client's"
        );
        assert_eq!(cfg.backend, BackendChoice::OpenAi);
        assert_eq!(cfg.openai_url, "http://llm.example:8080");

        let cfg = with_env(
            &[
                ("HS_SCRIBE_VLM_CONCURRENCY", "3"),
                ("HS_SCRIBE_CONVERTER", "olmocr"),
            ],
            || AppConfig::from_file(&file),
        )
        .unwrap();
        assert_eq!(cfg.vlm_concurrency, 3);
        assert_eq!(cfg.converter, ConverterMode::Olmocr);
        assert_eq!(
            cfg.backend,
            BackendChoice::OpenAi,
            "untouched keys keep the file's value"
        );
    }

    #[test]
    fn a_bad_server_setting_stops_the_load_wherever_it_comes_from() {
        let (_home, ok) = home_with(Some("scribe_server:\n  vlm_concurrency: 9\n"));
        for (vars, what) in [
            (
                vec![("HS_SCRIBE_VLM_CONCURRENCY", "many")],
                "env value of the wrong type",
            ),
            (
                vec![("HS_SCRIBE_VLM_CONCURRENCY", "0")],
                "env value the server cannot run with",
            ),
            (
                vec![("HS_SCRIBE_BACKEND", "sglang")],
                "env backend that does not exist",
            ),
        ] {
            let err = with_env(&vars, || AppConfig::from_file(&ok)).expect_err(what);
            assert!(err.to_string().contains("scribe_server"), "{what}: {err}");
        }
        for yaml in [
            "scribe_server:\n  vlm_concurrency: 0\n",
            "scribe_server:\n  vlm_concurrency: lots\n",
            "scribe_server:\n  backend: sglang\n",
            "scribe_server: 7\n",
        ] {
            let (_home, file) = home_with(Some(yaml));
            let err = with_env(&[], || AppConfig::from_file(&file)).expect_err(yaml);
            assert!(err.to_string().contains("`scribe_server`"), "{yaml}: {err}");
        }
    }
}
