use crate::classify::{ConvertFailure, FailureCode};
use crate::client::ProgressEvent;
use crate::config::{AppConfig, PipelineMode};
use crate::models::layout::{BBox, LayoutDetector};
use crate::models::table_structure::{
    build_html_from_structure, TableStructure, TableStructureRecognizer,
};
use crate::ocr::region::RegionType;
use crate::ocr::OcrEngine;
use crate::pipeline::markdown_generator::{assemble_page_markdown, join_pages};
use crate::pipeline::pdf_parser::PageData;
use crate::pipeline::PdfParser;
use crate::utils::deduplication::{deduplicate_boxes, filter_contained_regions};
use anyhow::{Context, Result};
use futures::stream::{self, StreamExt, TryStreamExt};
use hs_common::hardware_profile::HardwareProfile;
use image::DynamicImage;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// The number of pages to convert, or the permanent PDF fault that makes
/// the document unconvertible. pdfium addresses pages with a `u16`, so a
/// document of more than 65 536 pages used to be walked as
/// `0..total as u16` and silently convert only `total mod 65536` of them.
fn checked_page_count(count: usize) -> Result<usize> {
    if count == 0 || count > crate::pdf_meta::MAX_PDF_PAGES {
        return Err(ConvertFailure::err(
            FailureCode::PdfParseError,
            format!(
                "PDF has {count} pages; the renderer converts documents of 1..={} pages",
                crate::pdf_meta::MAX_PDF_PAGES
            ),
        ));
    }
    Ok(count)
}

/// A single region's OCR output with its layout classification.
#[derive(Debug, Clone)]
pub struct RegionResult {
    pub class_name: String,
    pub text: String,
}

/// Structured output from processing a single page image.
#[derive(Debug, Clone)]
pub struct ProcessedPage {
    pub markdown: String,
    pub regions: Vec<RegionResult>,
}

// --- PreparedPage types for the 2-stage async pipeline ---

struct PreparedRegion {
    bbox: BBox,
    region_type: RegionType,
    jpeg_bytes: Vec<u8>,
}

struct PreparedTable {
    bbox: BBox,
    structure: TableStructure,
    cell_jpegs: Vec<Vec<u8>>,
}

struct PreparedPage {
    page_idx: usize,
    /// PP-DocLayout-V3 class names for every region detected on this page,
    /// in detection (read) order. Threaded through to the document-level
    /// `ConversionResult` so the QC layer can apply the bibliography
    /// multiplier without an additional layout-detection pass. Empty for
    /// pages with no detected regions (the empty-page early return).
    region_classes: Vec<String>,
    /// Original page raster dimensions captured pre-crop. Carried purely
    /// for the diag JSONL — not used by any production code path.
    image_width: u32,
    image_height: u32,
    detection_order: Vec<usize>,
    text_regions: Vec<PreparedRegion>,
    table_regions: Vec<PreparedTable>,
    /// Shadow-mode defensive-column-detection record. Populated by
    /// `prepare_page`, threaded through to the per-page diag JSONL by
    /// `execute_vlm_for_page`. `None` only on the empty-page early
    /// return — every other path computes and records a verdict.
    column_split_shadow: Option<crate::diag::ColumnSplitShadow>,
    /// Regions (text crops and whole tables) dropped from this page
    /// because they could not be processed. Carried into the page's diag
    /// record; the QC gate refuses a conversion with any.
    skipped_regions: u32,
}

/// Round-robin pool of ONNX detectors. Each detector holds an `ort::Session`
/// that serializes calls through `Session::run(&mut self)`, so a single
/// shared detector was the dominant bottleneck in per-region mode — every
/// concurrent page's layout detection queued on one Mutex. Sizing comes
/// from `HardwareProfile::detector_pool_size()`.
pub struct DetectorPool<T> {
    slots: Vec<Mutex<T>>,
    next: AtomicUsize,
}

impl<T> DetectorPool<T> {
    fn new(slots: Vec<T>) -> Self {
        assert!(!slots.is_empty(), "DetectorPool requires at least one slot");
        Self {
            slots: slots.into_iter().map(Mutex::new).collect(),
            next: AtomicUsize::new(0),
        }
    }

    fn acquire(&self) -> Result<MutexGuard<'_, T>> {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        self.slots[idx]
            .lock()
            .map_err(|e| anyhow::anyhow!("DetectorPool lock poisoned: {e}"))
    }
}

/// What the Legacy converter does with a page. A per-region processor owns
/// both ONNX pools or does not exist — there is no half-loaded state and
/// no fallback from one mode to the other at runtime.
enum Pipeline {
    /// `pipeline_mode: full_page`, an explicit operator choice: the whole
    /// page goes to the VLM, no ONNX models are loaded.
    FullPage,
    PerRegion {
        layout: Arc<DetectorPool<LayoutDetector>>,
        table: Arc<DetectorPool<TableStructureRecognizer>>,
    },
}

/// Reason `/health` reports for the layout and table models when they are
/// not loaded: the operator selected full-page mode.
const MODELS_DISABLED_REASON: &str = "disabled (pipeline_mode != per_region)";

pub struct Processor {
    ocr: Arc<OcrEngine>,
    pipeline: Pipeline,
    /// Shared VLM concurrency semaphore — one per Processor, reused across
    /// every convert call. Size = the effective cap (see
    /// `effective_vlm_concurrency`). Server-side callers consume the same
    /// semaphore as watch/CLI callers; `/readiness` reports its live
    /// permit count via `Processor::vlm_sem()`.
    vlm_sem: Arc<tokio::sync::Semaphore>,
    /// The actual semaphore capacity. Equals `config.vlm_concurrency`.
    effective_vlm_concurrency: usize,
    config: AppConfig,
}

impl Processor {
    /// Build the Legacy converter. In `per_region` mode this loads the
    /// layout and table ONNX pools and FAILS if either cannot be loaded (a
    /// missing file, a CUDA provider that will not register, a model whose
    /// shapes are wrong): the server must not come up healthy in a mode
    /// that produces different output than the one configured. Nothing here
    /// is needed by the olmocr converter, which therefore never builds one.
    pub fn new(config: AppConfig) -> Result<Self> {
        config.validate()?;
        let ocr = Arc::new(OcrEngine::from_config(&config)?);

        let pipeline = match config.pipeline_mode {
            PipelineMode::PerRegion => {
                let pool_size = HardwareProfile::detect().class.detector_pool_size();
                Pipeline::PerRegion {
                    layout: build_layout_pool(&config, pool_size)?,
                    table: build_table_pool(&config, pool_size)?,
                }
            }
            PipelineMode::FullPage => Pipeline::FullPage,
        };

        let effective_vlm_concurrency = config.vlm_concurrency;
        let vlm_sem = Arc::new(tokio::sync::Semaphore::new(effective_vlm_concurrency));

        Ok(Self {
            ocr,
            pipeline,
            vlm_sem,
            effective_vlm_concurrency,
            config,
        })
    }

    /// Effective semaphore capacity — what `/readiness` reports as
    /// `vlm_slots_total`. Equals `config.vlm_concurrency`.
    pub fn effective_vlm_concurrency(&self) -> usize {
        self.effective_vlm_concurrency
    }

    /// Why the layout model is not loaded; `None` when it is.
    pub fn layout_model_reason(&self) -> Option<&str> {
        match self.pipeline {
            Pipeline::FullPage => Some(MODELS_DISABLED_REASON),
            Pipeline::PerRegion { .. } => None,
        }
    }

    /// Why the table model is not loaded; `None` when it is.
    pub fn table_model_reason(&self) -> Option<&str> {
        self.layout_model_reason()
    }

    pub fn ocr(&self) -> Arc<OcrEngine> {
        Arc::clone(&self.ocr)
    }

    /// Shared VLM semaphore. `/readiness` reports `available_permits()` so
    /// the pool load-balancer sees the true free-slot count, not a
    /// request-level approximation.
    pub fn vlm_sem(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::clone(&self.vlm_sem)
    }

    pub fn has_layout_detector(&self) -> bool {
        matches!(self.pipeline, Pipeline::PerRegion { .. })
    }

    pub fn has_table_recognizer(&self) -> bool {
        self.has_layout_detector()
    }

    pub async fn process_image(&self, image: &DynamicImage) -> Result<String> {
        Ok(self.process_image_full(image).await?.markdown)
    }

    pub async fn process_image_full(&self, image: &DynamicImage) -> Result<ProcessedPage> {
        match &self.pipeline {
            Pipeline::PerRegion { layout, table } => {
                self.process_image_regions_full(image, layout, table).await
            }
            Pipeline::FullPage => {
                let downscaled = maybe_downscale(image, self.config.max_image_dim);
                let image_bytes = encode_jpeg(&downscaled)?;
                let text = self.ocr.recognize(&image_bytes).await?;
                Ok(ProcessedPage {
                    markdown: text.clone(),
                    regions: vec![RegionResult {
                        class_name: "text".into(),
                        text,
                    }],
                })
            }
        }
    }

    async fn process_image_regions_full(
        &self,
        image: &DynamicImage,
        layout: &DetectorPool<LayoutDetector>,
        table: &DetectorPool<TableStructureRecognizer>,
    ) -> Result<ProcessedPage> {
        let bboxes = {
            let mut det = layout.acquire()?;
            det.detect(image)?
        };

        let bboxes = deduplicate_boxes(bboxes);
        let bboxes = filter_contained_regions(bboxes);

        if bboxes.is_empty() {
            // ONE PATH: zero layout regions => no VLM call, emit empty page.
            // Full-page VLM with no spatial constraints is exactly the path
            // that rides one phrase to num_predict on dense multi-column text
            // (see ollama#10767, #14493 — penalty params silently dropped on
            // the Go VLM runner). Real blank pages legitimately produce zero
            // regions and should produce zero markdown, not invented VLM text.
            // A pathological layout regression that drops every page is caught
            // by qc_verdict's bad-page-ratio gate.
            tracing::info!("layout returned zero regions; emitting empty page (no VLM call)");
            return Ok(ProcessedPage {
                markdown: String::new(),
                regions: vec![],
            });
        }

        // Filter out Skip regions (headers, footers, page numbers, formula numbers)
        let bboxes: Vec<BBox> = bboxes
            .into_iter()
            .filter(|b| RegionType::from_class(&b.class_name) != RegionType::Skip)
            .collect();

        // Save detection order (native read_order from PP-DocLayout-V3) before splitting
        let detection_order: Vec<usize> = bboxes.iter().map(|b| b.unique_id).collect();

        // Separate table bboxes (need SLANet-Plus, synchronous) from others (VLM, parallel)
        let mut table_bboxes = Vec::new();
        let mut other_bboxes = Vec::new();
        for bbox in bboxes {
            if RegionType::from_class(&bbox.class_name) == RegionType::Table {
                table_bboxes.push(bbox);
            } else {
                other_bboxes.push(bbox);
            }
        }

        // Process non-table regions in parallel via VLM
        let region_parallel = self.config.region_parallel;
        let ocr = Arc::clone(&self.ocr);
        let image_arc = Arc::new(image.clone());

        let region_results: Vec<Result<(BBox, String)>> = stream::iter(other_bboxes)
            .map(|bbox| {
                let ocr = Arc::clone(&ocr);
                let image = Arc::clone(&image_arc);
                async move {
                    let region_type = RegionType::from_class(&bbox.class_name);

                    if region_type == RegionType::Figure || region_type == RegionType::Skip {
                        return Ok((bbox, String::new()));
                    }

                    let Some(crop) = crop_bbox(&image, &bbox) else {
                        tracing::warn!(
                            class = %bbox.class_name,
                            x1 = bbox.x1, y1 = bbox.y1, x2 = bbox.x2, y2 = bbox.y2,
                            img_w = image.width(), img_h = image.height(),
                            "region crop 0-dim after clamp — skipping region (page continues)"
                        );
                        return Ok((bbox, String::new()));
                    };
                    let image_bytes = encode_jpeg(&crop)?;

                    tracing::debug!(
                        "Region {:?} '{}' ({}x{}) -> {:?}",
                        bbox.class_name,
                        bbox.confidence,
                        crop.width(),
                        crop.height(),
                        region_type,
                    );

                    let text = ocr.recognize_region(&image_bytes, region_type).await?;
                    Ok((bbox, text))
                }
            })
            .buffered(region_parallel)
            .collect()
            .await;

        // Per-region failures (bad JPEG encode, VLM error on one region)
        // degrade the page to partial markdown instead of killing the
        // whole paper. Mirrors the table-cell handling in
        // `recognize_table_html` where individual cell encode failures
        // return an empty string. If every region fails we fall through
        // with an empty `regions` vec; the outer caller will surface
        // "no regions produced output" as a page-level failure.
        let total_regions = region_results.len();
        let mut regions: Vec<(BBox, String)> = Vec::with_capacity(total_regions);
        let mut failed_regions: usize = 0;
        for r in region_results {
            match r {
                Ok(pair) => regions.push(pair),
                Err(e) => {
                    failed_regions += 1;
                    tracing::warn!(
                        error = %e,
                        "region failed; dropping from page output (paper continues)"
                    );
                }
            }
        }
        if failed_regions > 0 {
            tracing::warn!(
                failed_regions,
                total_regions,
                "page produced partial markdown due to per-region failures"
            );
        }

        // Process table regions: SLANet-Plus structure → per-cell VLM OCR → HTML
        for bbox in table_bboxes {
            let Some(crop) = crop_bbox(image, &bbox) else {
                tracing::warn!(
                    class = %bbox.class_name,
                    x1 = bbox.x1, y1 = bbox.y1, x2 = bbox.x2, y2 = bbox.y2,
                    "table crop 0-dim after clamp — skipping table (page continues)"
                );
                continue;
            };
            let html = self.recognize_table_html(table, &crop).await?;
            regions.push((bbox, html));
        }

        // Re-sort by native read_order from PP-DocLayout-V3
        let order_map: std::collections::HashMap<usize, usize> = detection_order
            .iter()
            .enumerate()
            .map(|(pos, &id)| (id, pos))
            .collect();
        regions.sort_by_key(|(bbox, _)| *order_map.get(&bbox.unique_id).unwrap_or(&usize::MAX));

        let region_results: Vec<RegionResult> = regions
            .iter()
            .map(|(bbox, text)| RegionResult {
                class_name: bbox.class_name.clone(),
                text: text.clone(),
            })
            .collect();

        Ok(ProcessedPage {
            markdown: assemble_page_markdown(&regions),
            regions: region_results,
        })
    }

    /// Recognize table structure with SLANet-Plus, OCR each cell with VLM, return HTML.
    async fn recognize_table_html(
        &self,
        table: &DetectorPool<TableStructureRecognizer>,
        table_image: &DynamicImage,
    ) -> Result<String> {
        let structure = {
            let mut rec = table.acquire()?;
            rec.recognize(table_image)?
        };

        tracing::debug!(
            "Table: {} tokens, {} cells",
            structure.tokens.len(),
            structure.cells.len()
        );

        // OCR each cell in parallel via VLM
        let ocr = Arc::clone(&self.ocr);
        let table_arc = Arc::new(table_image.clone());
        let region_parallel = self.config.region_parallel;

        let cell_texts: Vec<String> = stream::iter(structure.cells.iter().cloned())
            .map(|cell| {
                let ocr = Arc::clone(&ocr);
                let table_img = Arc::clone(&table_arc);
                async move {
                    let [x1, y1, x2, y2] = cell.bbox;
                    let x1u = x1.max(0.0) as u32;
                    let y1u = y1.max(0.0) as u32;
                    let w = (x2 - x1).max(0.0) as u32;
                    let h = (y2 - y1).max(0.0) as u32;
                    let Some(cell_crop) = crop_image_checked(&table_img, x1u, y1u, w, h) else {
                        tracing::warn!(
                            cell_x1 = x1,
                            cell_y1 = y1,
                            cell_x2 = x2,
                            cell_y2 = y2,
                            table_w = table_img.width(),
                            table_h = table_img.height(),
                            "cell crop 0-dim — emitting empty cell (table continues)"
                        );
                        return String::new();
                    };
                    match encode_jpeg(&cell_crop) {
                        Ok(bytes) => match ocr.recognize_region(&bytes, RegionType::Text).await {
                            Ok(text) => text.trim().to_string(),
                            Err(e) => {
                                tracing::warn!("Cell OCR failed: {e}");
                                String::new()
                            }
                        },
                        Err(e) => {
                            tracing::warn!("Cell JPEG encode failed: {e}");
                            String::new()
                        }
                    }
                }
            })
            .buffered(region_parallel)
            .collect()
            .await;

        Ok(build_html_from_structure(&structure, &cell_texts))
    }

    /// Convert the PDF at `pdf_path` to markdown, one page of raster resident
    /// at a time. This is the ONE conversion path of the Legacy converter
    /// (the server's `/scribe/stream`); every failure of a page — a VLM
    /// error, a JPEG encode error — fails the conversion instead of
    /// becoming an empty page, so a backend outage mid-book cannot yield a
    /// gapped "successful" markdown. Regions the per-region pipeline has to
    /// drop are counted in the page diag records for the QC gate.
    pub async fn process_pdf_with_progress<F>(
        &self,
        pdf_path: &str,
        on_progress: F,
    ) -> Result<crate::client::ConversionResult>
    where
        F: Fn(ProgressEvent) + Send + Sync + 'static,
    {
        let on_progress: Arc<dyn Fn(ProgressEvent) + Send + Sync> = Arc::new(on_progress);

        on_progress(ProgressEvent {
            stage: "parse".into(),
            page: 0,
            total_pages: 0,
            message: "Parsing PDF...".into(),
        });

        // pdfium takes a process-wide lock: never wait for it on an async
        // worker (another conversion's render thread may hold it for its
        // whole run).
        let page_count = {
            let path = pdf_path.to_string();
            let counted = tokio::task::spawn_blocking(move || {
                crate::pdfium::with_parser(|parser| parser.page_count(&path))
            })
            .await??;
            checked_page_count(counted)?
        };
        let total = page_count as u64;

        on_progress(ProgressEvent {
            stage: "parse".into(),
            page: 0,
            total_pages: total,
            message: format!("Parsed {total} pages"),
        });

        let max_render_pixels = self.config.max_render_pixels;

        let (layout, table) = match &self.pipeline {
            Pipeline::FullPage => {
                let max_dim = self.config.max_image_dim;
                let parallel = self.config.parallel;

                // Stage 1: render pages one at a time on a blocking thread into a
                // small bounded channel — at most a couple of rendered pages are
                // resident, so a large book no longer materialises its whole raster
                // set up front (the all-pages-in-RAM OOM that froze the host).
                let pdf_path_owned = pdf_path.to_string();
                let render_dpi = self.config.dpi;
                let (tx, rx) = tokio::sync::mpsc::channel::<(usize, PageData)>(2);
                let render = tokio::task::spawn_blocking(move || {
                    crate::pdfium::with_parser(|parser| {
                        let document = parser.open(&pdf_path_owned)?;
                        for idx in 0..page_count {
                            let idx = u16::try_from(idx)?;
                            let page = PdfParser::render_page(
                                &document,
                                idx,
                                render_dpi,
                                max_render_pixels,
                            )?;
                            if tx.blocking_send((idx as usize, page)).is_err() {
                                break;
                            }
                        }
                        Ok::<_, anyhow::Error>(())
                    })
                });

                // Stage 2: full-page OCR, bounded to `parallel` concurrent VLM calls.
                let markdowns = recognize_full_pages(
                    Arc::clone(&self.ocr),
                    rx,
                    total,
                    max_dim,
                    parallel,
                    Arc::clone(&on_progress),
                )
                .await?;
                render.await??;

                // FullPage mode bypasses layout detection — no per-page region
                // class info exists. Empty class lists tell QC "not bibliography",
                // which means strict default ceiling everywhere — the safer choice
                // when layout context is absent.
                let markdown = join_pages(&markdowns);
                let per_page_region_classes = vec![Vec::new(); markdowns.len()];
                // FullPage mode bypasses prepare_page so we don't have layout
                // metadata. Emit minimal diag rows so downstream JSONL alignment
                // (one record per page) still holds.
                let backend_name = self.ocr.backend_name().to_string();
                let dpi = self.config.dpi;
                let per_page_diags: Vec<crate::diag::PageDiagRecord> = markdowns
                    .iter()
                    .enumerate()
                    .map(|(i, md)| crate::diag::PageDiagRecord {
                        page_index: i,
                        dpi,
                        routing_path: "full-page".to_string(),
                        backend: backend_name.clone(),
                        raw_vlm_output_len: md.len(),
                        raw_vlm_output_first_1k: crate::diag::truncate_at_char_boundary(md, 1024),
                        output_byte_count: md.len(),
                        ..Default::default()
                    })
                    .collect();
                return Ok(crate::client::ConversionResult {
                    markdown,
                    per_page_region_classes,
                    per_page_diags,
                });
            }
            Pipeline::PerRegion { layout, table } => (Arc::clone(layout), Arc::clone(table)),
        };

        // Per-region mode: 2-stage async pipeline
        let (tx, rx) = tokio::sync::mpsc::channel::<PreparedPage>(3);
        let vlm_sem = Arc::clone(&self.vlm_sem);
        let on_progress_s1 = Arc::clone(&on_progress);

        // Stage 1 renders each page just before it detects layout and drops it
        // right after, so only one page of raster is resident at a time — the
        // whole-document rasterization that OOM-froze the host is gone.
        let pdf_path_owned = pdf_path.to_string();
        let render_dpi = self.config.dpi;
        let stage1 = tokio::task::spawn_blocking(move || {
            crate::pdfium::with_parser(|parser| {
                let document = parser.open(&pdf_path_owned)?;
                for idx in 0..page_count {
                    let idx = u16::try_from(idx)?;
                    let page_no = idx as usize;
                    on_progress_s1(ProgressEvent {
                        stage: "layout".into(),
                        page: idx as u64,
                        total_pages: total,
                        message: format!("Detecting layout page {}/{total}", page_no + 1),
                    });
                    let page =
                        PdfParser::render_page(&document, idx, render_dpi, max_render_pixels)?;
                    tracing::info!(
                        "Preparing page {}/{} ({}x{}, per-region)",
                        page_no + 1,
                        total,
                        page.image.width(),
                        page.image.height(),
                    );
                    let prepared = prepare_page(page_no, &page.image, &layout, &table)?;
                    on_progress_s1(ProgressEvent {
                        stage: "layout".into(),
                        page: (page_no + 1) as u64,
                        total_pages: total,
                        message: format!("Layout done page {}/{total}", page_no + 1),
                    });
                    if tx.blocking_send(prepared).is_err() {
                        break;
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
        });

        // Stage 2: VLM inference
        let ocr = Arc::clone(&self.ocr);
        let region_parallel = self.config.region_parallel;
        let dpi = self.config.dpi;
        let results = execute_prepared_pages(
            ocr,
            vlm_sem,
            rx,
            total,
            region_parallel,
            dpi,
            self.config.page_parallel,
            Arc::clone(&on_progress),
        )
        .await?;

        stage1.await??;

        on_progress(ProgressEvent {
            stage: "done".into(),
            page: total,
            total_pages: total,
            message: "Assembling markdown...".into(),
        });

        let mut markdowns = Vec::with_capacity(results.len());
        let mut per_page_region_classes = Vec::with_capacity(results.len());
        let mut per_page_diags = Vec::with_capacity(results.len());
        for (_, md, classes, diag) in results {
            markdowns.push(md);
            per_page_region_classes.push(classes);
            per_page_diags.push(diag);
        }
        Ok(crate::client::ConversionResult {
            markdown: join_pages(&markdowns),
            per_page_region_classes,
            per_page_diags,
        })
    }
}

/// Full-page stage 2: OCR each rendered page, at most `parallel` VLM calls
/// at once, and return the pages' markdown in page order. A page whose JPEG
/// encode or VLM call fails fails the whole conversion — it is never turned
/// into an empty page — and it does so as soon as it is noticed, not after
/// the rest of the book has been OCR'd against a backend that is down.
async fn recognize_full_pages(
    ocr: Arc<OcrEngine>,
    mut rx: tokio::sync::mpsc::Receiver<(usize, PageData)>,
    total: u64,
    max_dim: u32,
    parallel: usize,
    on_progress: Arc<dyn Fn(ProgressEvent) + Send + Sync>,
) -> Result<Vec<String>> {
    let completed = Arc::new(AtomicU64::new(0));
    let page_sem = Arc::new(tokio::sync::Semaphore::new(parallel));
    let mut tasks = tokio::task::JoinSet::new();
    let mut collected: Vec<(usize, String)> = Vec::with_capacity(total as usize);
    while let Some((i, page)) = rx.recv().await {
        while let Some(done) = tasks.try_join_next() {
            collected.push(done??);
        }
        let permit = Arc::clone(&page_sem)
            .acquire_owned()
            .await
            .map_err(|e| anyhow::anyhow!("page semaphore closed mid-paper: {e}"))?;
        let ocr = Arc::clone(&ocr);
        let on_progress = Arc::clone(&on_progress);
        let completed = Arc::clone(&completed);
        tasks.spawn(async move {
            let _permit = permit;
            on_progress(ProgressEvent {
                stage: "vlm".into(),
                page: i as u64,
                total_pages: total,
                message: format!("Starting OCR page {}/{total}", i + 1),
            });
            let downscaled = maybe_downscale(&page.image, max_dim);
            let image_bytes = encode_jpeg(&downscaled)
                .with_context(|| format!("full-page JPEG encode failed on page {}", i + 1))?;
            tracing::info!(
                "Processing page {}/{} ({}x{}, {} bytes JPEG)",
                i + 1,
                total,
                page.image.width(),
                page.image.height(),
                image_bytes.len()
            );
            let text = ocr
                .recognize(&image_bytes)
                .await
                .with_context(|| format!("full-page VLM failed on page {}", i + 1))?;
            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            on_progress(ProgressEvent {
                stage: "vlm".into(),
                page: done,
                total_pages: total,
                message: format!("OCR page {done}/{total}"),
            });
            Ok::<_, anyhow::Error>((i, text))
        });
    }
    while let Some(res) = tasks.join_next().await {
        collected.push(res??);
    }
    collected.sort_by_key(|(i, _)| *i);
    Ok(collected.into_iter().map(|(_, md)| md).collect())
}

type PageOutput = (usize, String, Vec<String>, crate::diag::PageDiagRecord);

/// Per-region stage 2: run the VLM over each prepared page and return the
/// pages' `(index, markdown, region classes, diag)` in page order.
///
/// Per-paper page-fanout cap: without `page_parallel` the JoinSet below
/// would spawn one task per page as fast as stage 1 produces them, and a
/// single 50-page paper × `region_parallel` regions could put 100+
/// concurrent VLM calls against llama-server's small slot pool — every new
/// request evicts another slot's 50–64 MB prompt cache, which is what
/// manifests as `prompt cache update took 32993 ms` and 1.55 t/s eval. Cap
/// pages-in-flight per paper so the worst-case fanout stays inside the slot
/// pool's capacity. Tune via `HS_SCRIBE_PAGE_PARALLEL`; default 2 in
/// `AppConfig`.
///
/// A page-fatal error ends the conversion as soon as it is noticed.
#[allow(clippy::too_many_arguments)]
async fn execute_prepared_pages(
    ocr: Arc<OcrEngine>,
    vlm_sem: Arc<tokio::sync::Semaphore>,
    mut rx: tokio::sync::mpsc::Receiver<PreparedPage>,
    total: u64,
    region_parallel: usize,
    dpi: u16,
    page_parallel: usize,
    on_progress: Arc<dyn Fn(ProgressEvent) + Send + Sync>,
) -> Result<Vec<PageOutput>> {
    let vlm_completed = Arc::new(AtomicU64::new(0));
    let page_sem = Arc::new(tokio::sync::Semaphore::new(page_parallel));
    let mut tasks = tokio::task::JoinSet::new();
    let mut results: Vec<PageOutput> = Vec::with_capacity(total as usize);

    while let Some(prepared) = rx.recv().await {
        while let Some(done) = tasks.try_join_next() {
            results.push(done??);
        }
        let page_permit = Arc::clone(&page_sem)
            .acquire_owned()
            .await
            .map_err(|e| anyhow::anyhow!("page semaphore closed mid-paper: {e}"))?;
        let ocr = Arc::clone(&ocr);
        let sem = Arc::clone(&vlm_sem);
        let on_progress = Arc::clone(&on_progress);
        let vlm_completed = Arc::clone(&vlm_completed);
        tasks.spawn(async move {
            let _page_permit = page_permit; // released when this page's VLM finishes
            let done = vlm_completed.fetch_add(1, Ordering::Relaxed) + 1;
            let result = execute_vlm_for_page(
                prepared,
                ocr,
                sem,
                region_parallel,
                Arc::clone(&on_progress),
                done,
                total,
                dpi,
            )
            .await;
            on_progress(ProgressEvent {
                stage: "vlm".into(),
                page: done,
                total_pages: total,
                message: format!("Completed page {done}/{total}"),
            });
            result
        });
    }

    while let Some(res) = tasks.join_next().await {
        results.push(res??);
    }
    results.sort_by_key(|(idx, _, _, _)| *idx);
    Ok(results)
}

/// Load `pool_size` layout detectors, or say why the server cannot start.
/// There is no partial pool and no degraded mode: per-region conversion
/// needs the layout model, and a pipeline that quietly fell back to
/// full-page VLM (the repetition-loop path this mode exists to avoid)
/// would serve a healthy `/health` while producing the wrong thing.
fn build_layout_pool(
    config: &AppConfig,
    pool_size: usize,
) -> Result<Arc<DetectorPool<LayoutDetector>>> {
    let layout_path = config.resolved_layout_model_path();
    if !layout_path.exists() {
        anyhow::bail!(
            "layout model not found at {} (pipeline_mode is per_region)",
            layout_path.display()
        );
    }
    let path_str = layout_path
        .to_str()
        .context("layout model path is not valid UTF-8")?;
    let mut slots: Vec<LayoutDetector> = Vec::with_capacity(pool_size);
    for i in 0..pool_size {
        slots.push(
            LayoutDetector::new(path_str, config.use_cuda).with_context(|| {
                format!(
                    "loading layout detector {}/{pool_size} from {}",
                    i + 1,
                    layout_path.display()
                )
            })?,
        );
    }

    tracing::info!(
        "Layout detector pool loaded from {} (N={pool_size})",
        layout_path.display()
    );
    Ok(Arc::new(DetectorPool::new(slots)))
}

/// Load `pool_size` table-structure recognizers, or say why the server
/// cannot start. Same rule as [`build_layout_pool`]: no "tables go to the
/// VLM" degraded path.
fn build_table_pool(
    config: &AppConfig,
    pool_size: usize,
) -> Result<Arc<DetectorPool<TableStructureRecognizer>>> {
    let slanet_path = config.resolved_table_model_path();
    if !slanet_path.exists() {
        anyhow::bail!(
            "table structure model not found at {} (pipeline_mode is per_region)",
            slanet_path.display()
        );
    }
    let path_str = slanet_path
        .to_str()
        .context("table model path is not valid UTF-8")?;
    let mut slots: Vec<TableStructureRecognizer> = Vec::with_capacity(pool_size);
    for i in 0..pool_size {
        slots.push(
            TableStructureRecognizer::new(path_str, config.use_cuda).with_context(|| {
                format!(
                    "loading table recognizer {}/{pool_size} from {}",
                    i + 1,
                    slanet_path.display()
                )
            })?,
        );
    }

    tracing::info!("Table structure recognizer pool loaded (SLANet-Plus, N={pool_size})");
    Ok(Arc::new(DetectorPool::new(slots)))
}

/// Downscale an image if its longest dimension exceeds max_dim.
/// Region crops are already small so this is typically a no-op for them.
fn maybe_downscale(image: &DynamicImage, max_dim: u32) -> DynamicImage {
    let (w, h) = (image.width(), image.height());
    if w.max(h) <= max_dim {
        return image.clone();
    }
    let scale = max_dim as f64 / w.max(h) as f64;
    image.resize(
        (w as f64 * scale) as u32,
        (h as f64 * scale) as u32,
        image::imageops::FilterType::Lanczos3,
    )
}

/// CPU-bound page preparation: layout detection → dedup → crop → JPEG encode.
/// Called from a blocking thread in the 2-stage pipeline.
fn prepare_page(
    page_idx: usize,
    image: &DynamicImage,
    layout: &DetectorPool<LayoutDetector>,
    table: &DetectorPool<TableStructureRecognizer>,
) -> Result<PreparedPage> {
    let bboxes = {
        let mut det = layout.acquire()?;
        det.detect(image)?
    };

    let bboxes = deduplicate_boxes(bboxes);
    let bboxes = filter_contained_regions(bboxes);

    let (image_width, image_height) = (image.width(), image.height());

    if bboxes.is_empty() {
        // ONE PATH: zero layout regions => no VLM call, emit empty page.
        // Same rationale as process_image_regions_full above — full-page VLM
        // on dense multi-column text is the path that triggers repetition
        // loops (ollama#10767, #14493). A pathological layout regression
        // dropping every page is caught by qc_verdict's bad-page-ratio gate.
        tracing::info!(
            "Page {}: layout returned zero regions; emitting empty page (no VLM call)",
            page_idx + 1
        );
        return Ok(PreparedPage {
            page_idx,
            region_classes: vec![],
            image_width,
            image_height,
            detection_order: vec![],
            text_regions: vec![],
            table_regions: vec![],
            column_split_shadow: None,
            skipped_regions: 0,
        });
    }

    // Snapshot per-page region class names BEFORE the table/text split
    // and Skip-filter consume `bboxes`. These are surfaced to the
    // document-level QC gate via `ConversionResult.per_page_region_classes`.
    let region_classes: Vec<String> = bboxes.iter().map(|b| b.class_name.clone()).collect();

    // Filter out Skip regions
    let bboxes: Vec<BBox> = bboxes
        .into_iter()
        .filter(|b| RegionType::from_class(&b.class_name) != RegionType::Skip)
        .collect();

    // Defensive column detection. Phase 1: shadow_decide records what
    // the system *would* do (clean / detector_only / split) into the
    // per-page diag JSONL. Phase 2: when the operator flips
    // `HS_SCRIBE_COLUMN_SPLIT_ACTIVE=1` on the scribe-server unit,
    // confirmed splits are also applied — the suspect bbox is replaced
    // with two half-width bboxes at the gutter. Default is shadow-only;
    // the gate flips per host so a regression on the active path can
    // be reverted by clearing the env var, no rebuild needed.
    let column_split_shadow = Some(crate::pipeline::column_detect::shadow_decide(
        image, &bboxes,
    ));
    let mut bboxes = bboxes;
    if let Some(shadow) = column_split_shadow.as_ref() {
        if shadow.would_action == "split" && crate::pipeline::column_detect::active_split_enabled()
        {
            if let (Some(idx), Some(gx)) = (shadow.suspect_idx, shadow.gutter_x) {
                tracing::info!(
                    page = page_idx + 1,
                    suspect_idx = idx,
                    gutter_x = gx,
                    "column-split: replacing wide suspect with left/right pair"
                );
                crate::pipeline::column_detect::split_suspect_at_gutter(&mut bboxes, idx, gx);
            }
        }
    }

    let detection_order: Vec<usize> = bboxes.iter().map(|b| b.unique_id).collect();

    let mut table_bboxes = Vec::new();
    let mut other_bboxes = Vec::new();
    for bbox in bboxes {
        if RegionType::from_class(&bbox.class_name) == RegionType::Table {
            table_bboxes.push(bbox);
        } else {
            other_bboxes.push(bbox);
        }
    }

    // Prepare non-table regions: crop + JPEG encode. A 0-dim crop
    // (Fix A's layout guard missed it, or the bbox lands past an
    // image edge after padding) or a per-region encode failure drops
    // the region and emits a WARN; the page keeps the surviving
    // regions instead of failing the whole PDF.
    let mut text_regions = Vec::with_capacity(other_bboxes.len());
    let mut skipped_regions: usize = 0;
    for bbox in other_bboxes {
        let region_type = RegionType::from_class(&bbox.class_name);
        let jpeg_bytes = if region_type == RegionType::Figure || region_type == RegionType::Skip {
            Vec::new()
        } else {
            let Some(crop) = crop_bbox(image, &bbox) else {
                tracing::warn!(
                    class = %bbox.class_name,
                    x1 = bbox.x1, y1 = bbox.y1, x2 = bbox.x2, y2 = bbox.y2,
                    img_w = image.width(), img_h = image.height(),
                    "region crop 0-dim after clamp — skipping region (page continues)"
                );
                skipped_regions += 1;
                continue;
            };
            match encode_jpeg(&crop) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        err = %e,
                        class = %bbox.class_name,
                        "region JPEG encode failed — skipping region (page continues)"
                    );
                    skipped_regions += 1;
                    continue;
                }
            }
        };
        text_regions.push(PreparedRegion {
            bbox,
            region_type,
            jpeg_bytes,
        });
    }
    // Prepare table regions: SLANet structure + per-cell crop + JPEG
    // encode. A 0-dim table crop drops the whole table (counted as a
    // skipped region); a 0-dim cell crop emits an empty Vec for that cell
    // (SLANet's structure still needs one slot per cell so alignment is
    // preserved).
    let mut table_regions = Vec::with_capacity(table_bboxes.len());
    for bbox in table_bboxes {
        let Some(crop) = crop_bbox(image, &bbox) else {
            tracing::warn!(
                class = %bbox.class_name,
                x1 = bbox.x1, y1 = bbox.y1, x2 = bbox.x2, y2 = bbox.y2,
                "table crop 0-dim after clamp — skipping table"
            );
            skipped_regions += 1;
            continue;
        };
        let structure = {
            let mut rec = table.acquire()?;
            rec.recognize(&crop)?
        };

        tracing::debug!(
            "Table: {} tokens, {} cells",
            structure.tokens.len(),
            structure.cells.len()
        );

        let mut cell_jpegs = Vec::with_capacity(structure.cells.len());
        for cell in &structure.cells {
            let [x1, y1, x2, y2] = cell.bbox;
            let x1u = x1.max(0.0) as u32;
            let y1u = y1.max(0.0) as u32;
            let w = (x2 - x1).max(0.0) as u32;
            let h = (y2 - y1).max(0.0) as u32;
            let bytes = match crop_image_checked(&crop, x1u, y1u, w, h) {
                Some(cell_crop) => match encode_jpeg(&cell_crop) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            err = %e,
                            cell_x1 = x1, cell_y1 = y1,
                            "cell JPEG encode failed — emitting empty cell (table continues)"
                        );
                        Vec::new()
                    }
                },
                None => {
                    tracing::warn!(
                        cell_x1 = x1,
                        cell_y1 = y1,
                        cell_x2 = x2,
                        cell_y2 = y2,
                        table_w = crop.width(),
                        table_h = crop.height(),
                        "cell crop 0-dim — emitting empty cell (table continues)"
                    );
                    Vec::new()
                }
            };
            cell_jpegs.push(bytes);
        }

        table_regions.push(PreparedTable {
            bbox,
            structure,
            cell_jpegs,
        });
    }

    if skipped_regions > 0 {
        // These regions are missing from the page's markdown. The count
        // travels to the client in the page's diag record and the QC gate
        // refuses to record the conversion (`QcVerdict::RejectGapped`).
        tracing::warn!(
            skipped_regions,
            kept_regions = text_regions.len() + table_regions.len(),
            "page {} has skipped regions; the conversion will be rejected as gapped",
            page_idx + 1
        );
    }

    Ok(PreparedPage {
        page_idx,
        region_classes,
        image_width,
        image_height,
        detection_order,
        text_regions,
        table_regions,
        column_split_shadow,
        skipped_regions: skipped_regions as u32,
    })
}

/// Stage 2: execute VLM inference for a single prepared page.
/// Returns `(page_idx, markdown, region_classes, diag)`. `region_classes`
/// is the per-page list of PP-DocLayout-V3 class names threaded straight
/// through from `prepare_page` — used by the document-level QC gate to
/// apply the bibliography multiplier. `diag` is the per-page record
/// surfaced to the optional `<output_dir>/<stem>.diag.jsonl`.
#[allow(clippy::too_many_arguments)]
async fn execute_vlm_for_page(
    prepared: PreparedPage,
    ocr: Arc<OcrEngine>,
    sem: Arc<tokio::sync::Semaphore>,
    region_parallel: usize,
    on_progress: Arc<dyn Fn(ProgressEvent) + Send + Sync>,
    page_num: u64,
    total_pages: u64,
    dpi: u16,
) -> Result<(usize, String, Vec<String>, crate::diag::PageDiagRecord)> {
    let started = std::time::Instant::now();
    let PreparedPage {
        page_idx,
        region_classes,
        image_width,
        image_height,
        detection_order,
        text_regions,
        table_regions,
        column_split_shadow,
        skipped_regions,
    } = prepared;

    let total_regions = text_regions.len();
    let region_done = Arc::new(AtomicU64::new(0));
    // Counter for streaming-detector aborts. Threaded into the diag
    // record so post-mortems can gauge per-page and per-corpus abort
    // rates without re-running OCR.
    let aborted_count = Arc::new(AtomicU64::new(0));

    on_progress(ProgressEvent {
        stage: "vlm".into(),
        page: page_num,
        total_pages,
        message: format!(
            "OCR page {page_num}/{total_pages} ({total_regions} regions, {} tables)",
            table_regions.len()
        ),
    });

    // Process text regions with semaphore-gated concurrency.
    //
    // **Failure handling.** Two distinct paths:
    //
    // - **Repetition-loop abort** (RepetitionLoopError, raised by the
    //   streaming OCR backend mid-generation): the region's output is a
    //   degenerate cycle that the VLM was about to spam to max_tokens.
    //   Treat as a per-region drop, NOT a page failure. The page assembles
    //   markdown from the surviving regions; the doc continues. Increment
    //   `aborted_count` so the per-page diag records the abort.
    //
    // - **Other VLM errors** (transport, server 5xx, semaphore closed):
    //   propagate as page-fatal via `?`. Silent fallthrough on these is
    //   the bug class CLAUDE.md prohibits — invisible holes in the
    //   markdown that an operator can't tell from a clean convert.
    //
    // Figure/Skip and empty-jpeg regions return empty placeholders
    // because those are intentional non-VLM slots, not failures.
    let region_results: Vec<(BBox, String)> = stream::iter(text_regions)
        .map(|r| {
            let ocr = Arc::clone(&ocr);
            let sem = Arc::clone(&sem);
            let on_progress = Arc::clone(&on_progress);
            let region_done = Arc::clone(&region_done);
            let aborted_count = Arc::clone(&aborted_count);
            async move {
                if r.region_type == RegionType::Figure || r.region_type == RegionType::Skip {
                    region_done.fetch_add(1, Ordering::Relaxed);
                    return Ok::<_, anyhow::Error>((r.bbox, String::new()));
                }
                if r.jpeg_bytes.is_empty() {
                    // prepare_page logged the skip; keep the slot empty so
                    // read-order assembly still has a placeholder.
                    region_done.fetch_add(1, Ordering::Relaxed);
                    return Ok((r.bbox, String::new()));
                }
                let permit = sem
                    .acquire()
                    .await
                    .map_err(|e| anyhow::anyhow!("VLM semaphore closed mid-page: {e}"))?;
                let text = match ocr.recognize_region(&r.jpeg_bytes, r.region_type).await {
                    Ok(t) => t,
                    Err(e) => {
                        // Discriminate the controlled streaming-abort from
                        // a transport/server failure. Aborts produce an
                        // empty region (page continues); other errors
                        // bubble up as page-fatal.
                        if let Some(loop_err) = e.downcast_ref::<crate::ocr::RepetitionLoopError>()
                        {
                            tracing::warn!(
                                page = page_num,
                                region_type = ?r.region_type,
                                reason = %loop_err.reason,
                                bytes_at_abort = loop_err.bytes_at_abort,
                                "streaming repetition detector aborted region; \
                                 dropping from page output (paper continues)"
                            );
                            aborted_count.fetch_add(1, Ordering::Relaxed);
                            drop(permit);
                            region_done.fetch_add(1, Ordering::Relaxed);
                            return Ok((r.bbox, String::new()));
                        }
                        return Err(e.context(format!(
                            "region VLM failed on page {page_num} (region_type={:?})",
                            r.region_type
                        )));
                    }
                };
                drop(permit);
                let done = region_done.fetch_add(1, Ordering::Relaxed) + 1;
                on_progress(ProgressEvent {
                    stage: "vlm".into(),
                    page: page_num,
                    total_pages,
                    message: format!("OCR region {done}/{total_regions} on page {page_num}"),
                });
                Ok((r.bbox, text))
            }
        })
        .buffer_unordered(region_parallel)
        .try_collect()
        .await?;
    let aborted_count = aborted_count.load(Ordering::Relaxed) as u32;

    let mut regions: Vec<(BBox, String)> = region_results;

    // Process table cells with semaphore-gated concurrency
    for (t_idx, table) in table_regions.into_iter().enumerate() {
        let total_cells = table.cell_jpegs.len();
        let cell_done = Arc::new(AtomicU64::new(0));

        on_progress(ProgressEvent {
            stage: "vlm".into(),
            page: page_num,
            total_pages,
            message: format!(
                "OCR table {}/{} ({total_cells} cells) on page {page_num}",
                t_idx + 1,
                t_idx + 1
            ),
        });

        // Text-region VLM failures abort the whole paper (fail-loud, ONE
        // PATH). Table cells are different: each cell is a single-image
        // VLM call whose output (often "0.5", "<.001", "Mean (SD)") is
        // pathologically prone to tripping the streaming repetition
        // detector. The detector firing on a 4-gram cycle ≥3 times is
        // easy on tabular numerics — and aborting an entire 30-page
        // paper because one cell on one page emitted "0.5 0.5 0.5 0.5"
        // is a worse outcome than emitting an empty cell.
        //
        // So: a single cell's `RepetitionLoopError` degrades to an empty
        // cell, the rest of the table is preserved, and the paper
        // continues. This is NOT a generic "swallow VLM errors" path —
        // only the repetition-loop class downgrades; transport errors
        // and other failures still fail the conversion. The text-region
        // path remains strict.
        let cell_texts: Vec<String> = stream::iter(table.cell_jpegs)
            .map(|jpeg| {
                let ocr = Arc::clone(&ocr);
                let sem = Arc::clone(&sem);
                let on_progress = Arc::clone(&on_progress);
                let cell_done = Arc::clone(&cell_done);
                async move {
                    if jpeg.is_empty() {
                        cell_done.fetch_add(1, Ordering::Relaxed);
                        return Ok::<String, anyhow::Error>(String::new());
                    }
                    let permit = sem
                        .acquire()
                        .await
                        .map_err(|e| anyhow::anyhow!("VLM semaphore closed mid-table: {e}"))?;
                    let text = match ocr
                        .recognize_region(&jpeg, RegionType::Text)
                        .await
                    {
                        Ok(t) => t,
                        Err(e) => {
                            if e.downcast_ref::<crate::ocr::RepetitionLoopError>().is_some() {
                                tracing::warn!(
                                    page = page_num,
                                    error = %e,
                                    "table cell VLM repetition loop — emitting empty cell (table and paper continue)"
                                );
                                cell_done.fetch_add(1, Ordering::Relaxed);
                                return Ok(String::new());
                            }
                            return Err(e.context(format!(
                                "table cell VLM failed on page {page_num}"
                            )));
                        }
                    };
                    drop(permit);
                    let done = cell_done.fetch_add(1, Ordering::Relaxed) + 1;
                    on_progress(ProgressEvent {
                        stage: "vlm".into(),
                        page: page_num,
                        total_pages,
                        message: format!("OCR table cell {done}/{total_cells} on page {page_num}"),
                    });
                    Ok(text.trim().to_string())
                }
            })
            .buffer_unordered(region_parallel)
            .try_collect::<Vec<String>>()
            .await?;

        let html = build_html_from_structure(&table.structure, &cell_texts);
        regions.push((table.bbox, html));
    }

    // Re-sort by detection order
    let order_map: std::collections::HashMap<usize, usize> = detection_order
        .iter()
        .enumerate()
        .map(|(pos, &id)| (id, pos))
        .collect();
    regions.sort_by_key(|(bbox, _)| *order_map.get(&bbox.unique_id).unwrap_or(&usize::MAX));

    let markdown = assemble_page_markdown(&regions);
    let routing_path = if region_classes.is_empty() {
        "empty-page"
    } else {
        "per-region"
    };
    let has_tables = region_classes
        .iter()
        .any(|c| RegionType::from_class(c) == RegionType::Table);
    let has_formulas = region_classes.iter().any(|c| {
        matches!(
            RegionType::from_class(c),
            RegionType::Formula | RegionType::InlineFormula
        )
    });
    let diag = crate::diag::PageDiagRecord {
        page_index: page_idx,
        dpi,
        image_width,
        image_height,
        layout_region_count: region_classes.len(),
        layout_region_classes: region_classes.clone(),
        has_tables,
        has_formulas,
        routing_path: routing_path.to_string(),
        backend: ocr.backend_name().to_string(),
        sampling_params: serde_json::Value::Null,
        prompt: String::new(),
        raw_vlm_output_len: markdown.len(),
        raw_vlm_output_first_1k: crate::diag::truncate_at_char_boundary(&markdown, 1024),
        output_byte_count: markdown.len(),
        wall_clock_ms: started.elapsed().as_millis() as u64,
        column_split_shadow,
        repetition_aborted_regions: aborted_count,
        skipped_regions,
    };
    Ok((page_idx, markdown, region_classes, diag))
}

/// Crop a rectangular subimage with explicit bounds checking. Returns
/// `None` when the requested rectangle has zero width or height after
/// being clipped to the source image. This is the single chokepoint
/// for bbox-driven crops feeding `encode_jpeg` — a 0-dim `DynamicImage`
/// crashes the JPEG encoder with `Invalid image size (NxM)`, which
/// pre-rc.305 terminated the whole PDF convert. Callers that can skip
/// a bad region (table cell, layout region) match on `None` and move
/// on with an empty payload.
pub(crate) fn crop_image_checked(
    image: &DynamicImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Option<DynamicImage> {
    let img_w = image.width();
    let img_h = image.height();
    if x >= img_w || y >= img_h {
        return None;
    }
    let w = w.min(img_w - x);
    let h = h.min(img_h - y);
    if w == 0 || h == 0 {
        return None;
    }
    Some(image.crop_imm(x, y, w, h))
}

/// Crop a bounding-box region with 2px padding, clamped to image
/// bounds. Returns `None` if the clamped rect is 0-dim — this happens
/// when a layout bbox that slipped past Fix A lands entirely past
/// an image edge, or when SLANet emits a bbox with reversed corners.
pub(crate) fn crop_bbox(image: &DynamicImage, bbox: &BBox) -> Option<DynamicImage> {
    let (img_w, img_h) = (image.width() as f32, image.height() as f32);
    let pad = 2.0;

    let x1 = (bbox.x1 - pad).max(0.0) as u32;
    let y1 = (bbox.y1 - pad).max(0.0) as u32;
    let x2 = (bbox.x2 + pad).min(img_w) as u32;
    let y2 = (bbox.y2 + pad).min(img_h) as u32;

    let w = x2.saturating_sub(x1);
    let h = y2.saturating_sub(y1);

    crop_image_checked(image, x1, y1, w, h)
}

pub(crate) fn encode_jpeg(image: &DynamicImage) -> Result<Vec<u8>> {
    // Final backstop: the `image` crate's Invalid-image-size error
    // propagates as "Format error encoding Jpeg: Invalid image size
    // (NxM)" and before rc.305 terminated the enclosing convert.
    // Reject 0-dim at this boundary with a greppable error string;
    // the happy path never hits this because `crop_image_checked`
    // already filtered the crop out.
    if image.width() == 0 || image.height() == 0 {
        anyhow::bail!(
            "encode_jpeg refused 0-dim image {}x{} — crop_image_checked should have filtered this upstream",
            image.width(),
            image.height()
        );
    }
    let mut buf = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85);
    image.write_with_encoder(encoder)?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn img(w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::new(w, h))
    }

    fn bbox(x1: f32, y1: f32, x2: f32, y2: f32) -> BBox {
        BBox {
            x1,
            y1,
            x2,
            y2,
            confidence: 0.9,
            class_id: 22,
            class_name: "text".to_string(),
            unique_id: 0,
            read_order: 0.0,
        }
    }

    #[test]
    fn crop_image_checked_rejects_0_dim() {
        let image = img(100, 80);
        // x past right edge → None
        assert!(crop_image_checked(&image, 100, 0, 10, 10).is_none());
        assert!(crop_image_checked(&image, 200, 0, 10, 10).is_none());
        // y past bottom edge → None
        assert!(crop_image_checked(&image, 0, 80, 10, 10).is_none());
        // w == 0 → None
        assert!(crop_image_checked(&image, 0, 0, 0, 10).is_none());
        // h == 0 → None
        assert!(crop_image_checked(&image, 0, 0, 10, 0).is_none());
        // normal crop → Some
        let c = crop_image_checked(&image, 10, 10, 50, 30).unwrap();
        assert_eq!(c.width(), 50);
        assert_eq!(c.height(), 30);
        // crop extending past right → clipped, not None
        let c = crop_image_checked(&image, 90, 0, 50, 10).unwrap();
        assert_eq!(c.width(), 10);
    }

    #[test]
    fn crop_bbox_none_when_beyond_edge() {
        let image = img(1600, 2400);
        // bbox entirely past right edge — slipped past Fix A
        assert!(crop_bbox(&image, &bbox(2000.0, 100.0, 2010.0, 200.0)).is_none());
        // bbox entirely past bottom edge
        assert!(crop_bbox(&image, &bbox(100.0, 2500.0, 200.0, 2510.0)).is_none());
        // reversed corners (x2 < x1) — Fix A should catch but defense in depth
        assert!(crop_bbox(&image, &bbox(500.0, 100.0, 400.0, 200.0)).is_none());
        // normal bbox within bounds → Some
        let c = crop_bbox(&image, &bbox(100.0, 100.0, 300.0, 400.0)).unwrap();
        assert!(c.width() > 0 && c.height() > 0);
    }

    #[test]
    fn encode_jpeg_rejects_0_dim_backstop() {
        // 0×1 and 1×0 must not reach the `image` crate encoder.
        let zero_w = img(0, 10);
        let err = encode_jpeg(&zero_w).expect_err("0-width image must be rejected");
        assert!(
            err.to_string().contains("encode_jpeg refused"),
            "err: {err}"
        );
        let zero_h = img(10, 0);
        let err = encode_jpeg(&zero_h).expect_err("0-height image must be rejected");
        assert!(
            err.to_string().contains("encode_jpeg refused"),
            "err: {err}"
        );
        // 1×1 encodes fine
        let tiny = img(1, 1);
        assert!(encode_jpeg(&tiny).is_ok());
    }

    #[test]
    fn page_counts_beyond_what_pdfium_can_index_are_refused_not_truncated() {
        // `0..total as u16` used to convert only `total mod 65536` pages.
        assert_eq!(checked_page_count(1).unwrap(), 1);
        assert_eq!(checked_page_count(65_536).unwrap(), 65_536);
        for bad in [0usize, 65_537, 70_000, usize::MAX] {
            let e = checked_page_count(bad).unwrap_err();
            assert_eq!(
                crate::classify::failure_code(&e),
                Some(FailureCode::PdfParseError),
                "{bad}"
            );
        }
    }

    /// A VLM stand-in on loopback: answers every request with `reply`.
    async fn fake_vlm(reply: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let reply = reply.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 256 * 1024];
                    let mut got = 0;
                    // Read the whole (single-write) request before answering.
                    while let Ok(Ok(n)) = tokio::time::timeout(
                        std::time::Duration::from_millis(150),
                        sock.read(&mut buf[got..]),
                    )
                    .await
                    {
                        if n == 0 {
                            break;
                        }
                        got += n;
                    }
                    let _ = sock.write_all(reply.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        url
    }

    fn sse_reply(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    async fn full_page_run(vlm_url: &str, pages: usize) -> Result<Vec<String>> {
        let config = AppConfig {
            backend: crate::config::BackendChoice::OpenAi,
            openai_url: vlm_url.to_string(),
            pipeline_mode: PipelineMode::FullPage,
            ..AppConfig::default()
        };
        let ocr = Arc::new(OcrEngine::from_config(&config)?);
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tokio::spawn(async move {
            for idx in 0..pages {
                let page = PageData {
                    page_idx: idx,
                    image: img(8, 8),
                    width: 8.0,
                    height: 8.0,
                    text: None,
                };
                if tx.send((idx, page)).await.is_err() {
                    break;
                }
            }
        });
        recognize_full_pages(ocr, rx, pages as u64, 1800, 2, Arc::new(|_| {})).await
    }

    #[tokio::test]
    async fn a_vlm_failure_on_a_full_page_fails_the_conversion_instead_of_an_empty_page() {
        let url = fake_vlm(sse_reply("400 Bad Request", "{\"error\":\"bad\"}")).await;
        let err = full_page_run(&url, 3).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("full-page VLM failed on page"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_truncated_vlm_stream_on_a_full_page_fails_the_conversion() {
        // A clean EOF with no [DONE] / finish_reason=stop used to be a short success.
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"half a pa\"}}]}\n\n";
        let url = fake_vlm(sse_reply("200 OK", body)).await;
        let err = full_page_run(&url, 2).await.unwrap_err();
        assert_eq!(
            crate::classify::failure_code(&err),
            Some(FailureCode::VlmTransportError),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn complete_vlm_answers_come_back_in_page_order() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"page text\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let url = fake_vlm(sse_reply("200 OK", body)).await;
        let pages = full_page_run(&url, 4).await.unwrap();
        assert_eq!(pages, vec!["page text"; 4]);
    }

    #[tokio::test]
    async fn regions_the_pipeline_dropped_reach_the_page_diag_record() {
        // No VLM call is made for a page with no text regions; the count
        // prepare_page recorded must come out in the diag the QC gate reads.
        let config = AppConfig::default();
        let ocr = Arc::new(OcrEngine::from_config(&config).unwrap());
        let prepared = PreparedPage {
            page_idx: 4,
            region_classes: vec![],
            image_width: 10,
            image_height: 10,
            detection_order: vec![],
            text_regions: vec![],
            table_regions: vec![],
            column_split_shadow: None,
            skipped_regions: 2,
        };
        let (_, _, _, diag) = execute_vlm_for_page(
            prepared,
            ocr,
            Arc::new(tokio::sync::Semaphore::new(1)),
            1,
            Arc::new(|_| {}),
            1,
            1,
            200,
        )
        .await
        .unwrap();
        assert_eq!(diag.skipped_regions, 2);
        let result = crate::client::ConversionResult {
            per_page_diags: vec![diag],
            ..Default::default()
        };
        assert_eq!(result.skipped_regions(), 2);
    }
}
