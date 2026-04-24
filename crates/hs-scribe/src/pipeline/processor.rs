use crate::client::ProgressEvent;
use crate::config::{AppConfig, PipelineMode};
use crate::models::layout::{BBox, LayoutDetector};
use crate::models::table_structure::{
    build_html_from_structure, TableStructure, TableStructureRecognizer,
};
use crate::ocr::region::RegionType;
use crate::ocr::OcrEngine;
use crate::pipeline::markdown_generator::{assemble_page_markdown, join_pages};
use crate::pipeline::PdfParser;
use crate::utils::deduplication::{deduplicate_boxes, filter_contained_regions};
use anyhow::Result;
use futures::stream::{self, StreamExt};
use hs_common::hardware_profile::HardwareProfile;
use image::DynamicImage;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

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

enum PreparedPage {
    FullPage {
        page_idx: usize,
        jpeg_bytes: Vec<u8>,
    },
    Regions {
        page_idx: usize,
        detection_order: Vec<usize>,
        text_regions: Vec<PreparedRegion>,
        table_regions: Vec<PreparedTable>,
    },
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

pub struct Processor {
    ocr: Arc<OcrEngine>,
    layout_detectors: Option<Arc<DetectorPool<LayoutDetector>>>,
    layout_model_reason: Option<String>,
    table_recognizers: Option<Arc<DetectorPool<TableStructureRecognizer>>>,
    table_model_reason: Option<String>,
    /// Shared VLM concurrency semaphore — one per Processor, reused across
    /// every convert call. Size = the effective cap (see
    /// `effective_vlm_concurrency`). Server-side callers consume the same
    /// semaphore as watch/CLI callers; `/readiness` reports its live
    /// permit count via `Processor::vlm_sem()`.
    vlm_sem: Arc<tokio::sync::Semaphore>,
    /// The actual semaphore capacity. Equals `config.vlm_concurrency`
    /// unless clamped by live `OLLAMA_NUM_PARALLEL` detection at startup —
    /// Rust-layer concurrency above Ollama's parallel capacity just
    /// oversubscribes Ollama's internal queue, inflates per-request
    /// latency past the convert deadline, and produces cascading
    /// timeouts. Clamping at startup keeps Rust in lockstep with Ollama.
    effective_vlm_concurrency: usize,
    config: AppConfig,
}

impl Processor {
    pub fn new(config: AppConfig) -> Result<Self> {
        let ocr = Arc::new(OcrEngine::from_config(&config));

        let pool_size = HardwareProfile::detect().class.detector_pool_size();

        let (layout_detectors, layout_model_reason) =
            if config.pipeline_mode == PipelineMode::PerRegion {
                build_layout_pool(&config, pool_size)
            } else {
                (
                    None,
                    Some("disabled (pipeline_mode != per_region)".to_string()),
                )
            };

        let (table_recognizers, table_model_reason) =
            if config.pipeline_mode == PipelineMode::PerRegion {
                build_table_pool(&config, pool_size)
            } else {
                (
                    None,
                    Some("disabled (pipeline_mode != per_region)".to_string()),
                )
            };

        let effective_vlm_concurrency = resolve_effective_vlm_concurrency(config.vlm_concurrency);
        let vlm_sem = Arc::new(tokio::sync::Semaphore::new(effective_vlm_concurrency));

        Ok(Self {
            ocr,
            layout_detectors,
            layout_model_reason,
            table_recognizers,
            table_model_reason,
            vlm_sem,
            effective_vlm_concurrency,
            config,
        })
    }

    /// Effective semaphore capacity — what `/readiness` reports as
    /// `vlm_slots_total`. Equals `config.vlm_concurrency` except when
    /// clamped by live `OLLAMA_NUM_PARALLEL`.
    pub fn effective_vlm_concurrency(&self) -> usize {
        self.effective_vlm_concurrency
    }

    pub fn layout_model_reason(&self) -> Option<&str> {
        self.layout_model_reason.as_deref()
    }

    pub fn table_model_reason(&self) -> Option<&str> {
        self.table_model_reason.as_deref()
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

    /// Is this processor running in per-region mode (i.e., has a layout detector pool)?
    fn is_per_region(&self) -> bool {
        self.layout_detectors.is_some()
    }

    pub fn has_layout_detector(&self) -> bool {
        self.layout_detectors.is_some()
    }

    pub fn has_table_recognizer(&self) -> bool {
        self.table_recognizers.is_some()
    }

    pub async fn process_image(&self, image: &DynamicImage) -> Result<String> {
        Ok(self.process_image_full(image).await?.markdown)
    }

    pub async fn process_image_full(&self, image: &DynamicImage) -> Result<ProcessedPage> {
        if self.is_per_region() {
            self.process_image_regions_full(image).await
        } else {
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

    async fn process_image_regions_full(&self, image: &DynamicImage) -> Result<ProcessedPage> {
        let bboxes = {
            let pool = self
                .layout_detectors
                .as_ref()
                .expect("per-region requires layout_detectors");
            let mut det = pool.acquire()?;
            det.detect(image)?
        };

        let bboxes = deduplicate_boxes(bboxes);
        let bboxes = filter_contained_regions(bboxes);

        if bboxes.is_empty() {
            tracing::info!("No layout detections, falling back to full-page OCR");
            let downscaled = maybe_downscale(image, self.config.max_image_dim);
            let image_bytes = encode_jpeg(&downscaled)?;
            let text = self.ocr.recognize(&image_bytes).await?;
            return Ok(ProcessedPage {
                markdown: text.clone(),
                regions: vec![RegionResult {
                    class_name: "text".into(),
                    text,
                }],
            });
        }

        // Filter out Skip regions (headers, footers, page numbers, formula numbers)
        let bboxes: Vec<BBox> = bboxes
            .into_iter()
            .filter(|b| RegionType::from_class(&b.class_name) != RegionType::Skip)
            .collect();

        // Detect text-heavy pages where full-page VLM is better than per-region cropping.
        // Dense multi-column pages (newspapers) get garbled by per-region crops that split
        // mid-sentence or merge columns. Full-page VLM handles these natively.
        let has_tables = bboxes
            .iter()
            .any(|b| RegionType::from_class(&b.class_name) == RegionType::Table);
        let has_formulas = bboxes.iter().any(|b| {
            matches!(
                RegionType::from_class(&b.class_name),
                RegionType::Formula | RegionType::InlineFormula
            )
        });
        if !has_tables && !has_formulas {
            tracing::info!("Text-only page (no tables/formulas) → full-page VLM");
            let downscaled = maybe_downscale(image, self.config.max_image_dim);
            let image_bytes = encode_jpeg(&downscaled)?;
            let text = self.ocr.recognize(&image_bytes).await?;
            return Ok(ProcessedPage {
                markdown: text.clone(),
                regions: vec![RegionResult {
                    class_name: "text".into(),
                    text,
                }],
            });
        }

        // Save detection order (native read_order from PP-DocLayout-V3) before splitting
        let detection_order: Vec<usize> = bboxes.iter().map(|b| b.unique_id).collect();

        // Separate table bboxes (need SLANet-Plus, synchronous) from others (VLM, parallel)
        let mut table_bboxes = Vec::new();
        let mut other_bboxes = Vec::new();
        for bbox in bboxes {
            if RegionType::from_class(&bbox.class_name) == RegionType::Table
                && self.table_recognizers.is_some()
            {
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
            let html = self.recognize_table_html(&crop).await?;
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
    async fn recognize_table_html(&self, table_image: &DynamicImage) -> Result<String> {
        let structure = {
            let pool = self
                .table_recognizers
                .as_ref()
                .expect("table_recognizers required");
            let mut rec = pool.acquire()?;
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

    pub async fn process_pdf_with_progress<F>(
        &self,
        pdf_path: &str,
        on_progress: F,
    ) -> Result<String>
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

        let pages = {
            let pdf_parser = PdfParser::new()?;
            pdf_parser.parse_to_pages(pdf_path, self.config.dpi)?
        };
        let total = pages.len() as u64;

        on_progress(ProgressEvent {
            stage: "parse".into(),
            page: 0,
            total_pages: total,
            message: format!("Parsed {total} pages"),
        });

        if !self.is_per_region() {
            let ocr = Arc::clone(&self.ocr);
            let max_dim = self.config.max_image_dim;
            let parallel = self.config.parallel;
            let completed = Arc::new(AtomicU64::new(0));

            let markdowns: Vec<String> = stream::iter(pages.into_iter().enumerate())
                .map(|(i, page)| {
                    let ocr = Arc::clone(&ocr);
                    let on_progress = Arc::clone(&on_progress);
                    let completed = Arc::clone(&completed);
                    async move {
                        on_progress(ProgressEvent {
                            stage: "vlm".into(),
                            page: i as u64,
                            total_pages: total,
                            message: format!("Starting OCR page {}/{total}", i + 1),
                        });
                        let downscaled = maybe_downscale(&page.image, max_dim);
                        let image_bytes = match encode_jpeg(&downscaled) {
                            Ok(b) => b,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    page = i + 1,
                                    "full-page JPEG encode failed — emitting empty page (paper continues)"
                                );
                                return String::new();
                            }
                        };
                        tracing::info!(
                            "Processing page {}/{} ({}x{}, {} bytes JPEG)",
                            i + 1,
                            total,
                            page.image.width(),
                            page.image.height(),
                            image_bytes.len()
                        );
                        let text = match ocr.recognize(&image_bytes).await {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    page = i + 1,
                                    "full-page VLM failed — emitting empty page (paper continues)"
                                );
                                String::new()
                            }
                        };
                        let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                        on_progress(ProgressEvent {
                            stage: "vlm".into(),
                            page: done,
                            total_pages: total,
                            message: format!("OCR page {done}/{total}"),
                        });
                        text
                    }
                })
                .buffered(parallel)
                .collect()
                .await;

            return Ok(join_pages(&markdowns));
        }

        // Per-region mode: 2-stage async pipeline
        let (tx, mut rx) = tokio::sync::mpsc::channel::<PreparedPage>(3);
        let vlm_sem = Arc::clone(&self.vlm_sem);

        let layout = self.layout_detectors.clone();
        let table = self.table_recognizers.clone();
        let config = self.config.clone();
        let on_progress_s1 = Arc::clone(&on_progress);

        let stage1 = tokio::task::spawn_blocking(move || {
            for (idx, page) in pages.into_iter().enumerate() {
                on_progress_s1(ProgressEvent {
                    stage: "layout".into(),
                    page: idx as u64,
                    total_pages: total,
                    message: format!("Detecting layout page {}/{total}", idx + 1),
                });
                tracing::info!(
                    "Preparing page {}/{} ({}x{}, per-region)",
                    idx + 1,
                    total,
                    page.image.width(),
                    page.image.height(),
                );
                let prepared = prepare_page(idx, &page.image, &layout, &table, &config)?;
                on_progress_s1(ProgressEvent {
                    stage: "layout".into(),
                    page: (idx + 1) as u64,
                    total_pages: total,
                    message: format!("Layout done page {}/{total}", idx + 1),
                });
                if tx.blocking_send(prepared).is_err() {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        });

        // Stage 2: VLM inference
        let ocr = Arc::clone(&self.ocr);
        let region_parallel = self.config.region_parallel;
        let vlm_completed = Arc::new(AtomicU64::new(0));
        let mut tasks = tokio::task::JoinSet::new();

        while let Some(prepared) = rx.recv().await {
            let ocr = Arc::clone(&ocr);
            let sem = Arc::clone(&vlm_sem);
            let on_progress = Arc::clone(&on_progress);
            let vlm_completed = Arc::clone(&vlm_completed);
            tasks.spawn(async move {
                let done = vlm_completed.fetch_add(1, Ordering::Relaxed) + 1;
                let result = execute_vlm_for_page(
                    prepared,
                    ocr,
                    sem,
                    region_parallel,
                    Arc::clone(&on_progress),
                    done,
                    total,
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

        let mut results: Vec<(usize, String)> = Vec::with_capacity(total as usize);
        while let Some(res) = tasks.join_next().await {
            results.push(res??);
        }
        results.sort_by_key(|(idx, _)| *idx);

        stage1.await??;

        on_progress(ProgressEvent {
            stage: "done".into(),
            page: total,
            total_pages: total,
            message: "Assembling markdown...".into(),
        });

        Ok(join_pages(
            &results.into_iter().map(|(_, md)| md).collect::<Vec<_>>(),
        ))
    }

    pub async fn process_pdf(&self, pdf_path: &str) -> Result<String> {
        let pages = {
            let pdf_parser = PdfParser::new()?;
            pdf_parser.parse_to_pages(pdf_path, self.config.dpi)?
        };
        let total = pages.len();

        if !self.is_per_region() {
            // Full-page mode: pages can be processed in parallel with downscaling
            let ocr = Arc::clone(&self.ocr);
            let max_dim = self.config.max_image_dim;
            let parallel = self.config.parallel;
            let markdowns: Vec<String> = stream::iter(pages.into_iter().enumerate())
                .map(|(i, page)| {
                    let ocr = Arc::clone(&ocr);
                    async move {
                        let downscaled = maybe_downscale(&page.image, max_dim);
                        let image_bytes = match encode_jpeg(&downscaled) {
                            Ok(b) => b,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    page = i + 1,
                                    "full-page JPEG encode failed — emitting empty page (paper continues)"
                                );
                                return String::new();
                            }
                        };
                        tracing::info!(
                            "Processing page {}/{} ({}x{}, {} bytes JPEG)",
                            i + 1,
                            total,
                            page.image.width(),
                            page.image.height(),
                            image_bytes.len()
                        );
                        match ocr.recognize(&image_bytes).await {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    page = i + 1,
                                    "full-page VLM failed — emitting empty page (paper continues)"
                                );
                                String::new()
                            }
                        }
                    }
                })
                .buffered(parallel)
                .collect()
                .await;

            return Ok(join_pages(&markdowns));
        }

        // Per-region mode: 2-stage async pipeline
        // Stage 1 (CPU): layout detection, cropping, JPEG encoding — pool-gated
        // Stage 2 (VLM): HTTP inference — concurrent across pages

        let (tx, mut rx) = tokio::sync::mpsc::channel::<PreparedPage>(3);
        let vlm_sem = Arc::clone(&self.vlm_sem);

        let layout = self.layout_detectors.clone();
        let table = self.table_recognizers.clone();
        let config = self.config.clone();

        let stage1 = tokio::task::spawn_blocking(move || {
            for (idx, page) in pages.into_iter().enumerate() {
                tracing::info!(
                    "Preparing page {}/{} ({}x{}, per-region)",
                    idx + 1,
                    total,
                    page.image.width(),
                    page.image.height(),
                );
                let prepared = prepare_page(idx, &page.image, &layout, &table, &config)?;
                if tx.blocking_send(prepared).is_err() {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        });

        // Stage 2: VLM inference (concurrent across pages)
        let ocr = Arc::clone(&self.ocr);
        let region_parallel = self.config.region_parallel;
        let mut tasks = tokio::task::JoinSet::new();

        while let Some(prepared) = rx.recv().await {
            let ocr = Arc::clone(&ocr);
            let sem = Arc::clone(&vlm_sem);
            let noop: Arc<dyn Fn(ProgressEvent) + Send + Sync> = Arc::new(|_| {});
            tasks.spawn(async move {
                execute_vlm_for_page(prepared, ocr, sem, region_parallel, noop, 0, 0).await
            });
        }

        // Collect results, sort by page index
        let mut results: Vec<(usize, String)> = Vec::with_capacity(total);
        while let Some(res) = tasks.join_next().await {
            results.push(res??);
        }
        results.sort_by_key(|(idx, _)| *idx);

        stage1.await??;
        Ok(join_pages(
            &results.into_iter().map(|(_, md)| md).collect::<Vec<_>>(),
        ))
    }
}

/// Clamp `requested` (the configured `vlm_concurrency`) to the live
/// `OLLAMA_NUM_PARALLEL` on this host when both are detectable and the
/// requested value is larger. This prevents Rust-layer oversubscription
/// of Ollama's internal parallel queue, which on production showed up as
/// every in-flight request stalling past the 900s convert deadline and
/// getting cancelled without counting as success. If `OLLAMA_NUM_PARALLEL`
/// is not explicitly set, we fall back to the requested value — the
/// operator has opted out of the guardrail.
fn resolve_effective_vlm_concurrency(requested: usize) -> usize {
    let detected = match crate::ollama_tuner::detect_ollama_control() {
        Ok(ctrl) => ctrl.detect_current(),
        Err(e) => {
            tracing::debug!(
                "No Ollama launcher found for vlm_concurrency guardrail: {e} — \
                 using configured vlm_concurrency={requested} as-is"
            );
            return requested;
        }
    };
    let Some(num_parallel) = detected else {
        tracing::info!(
            vlm_concurrency = requested,
            "OLLAMA_NUM_PARALLEL not explicitly set — using configured \
             vlm_concurrency as-is. If Ollama's default is smaller than this, \
             expect queue oversubscription; set OLLAMA_NUM_PARALLEL via the \
             platform launcher (systemd drop-in or launchctl setenv) or run \
             `hs scribe autotune`."
        );
        return requested;
    };
    let num_parallel_usize = num_parallel as usize;
    if num_parallel_usize < requested {
        tracing::warn!(
            requested,
            ollama_num_parallel = num_parallel,
            effective = num_parallel_usize,
            "vlm_concurrency clamped to OLLAMA_NUM_PARALLEL — Rust-layer \
             concurrency above Ollama's parallel capacity oversubscribes its \
             internal queue and inflates per-request latency past the convert \
             deadline. Raise OLLAMA_NUM_PARALLEL (e.g. via `hs scribe autotune`) \
             to use the full requested concurrency."
        );
        num_parallel_usize
    } else {
        tracing::info!(
            vlm_concurrency = requested,
            ollama_num_parallel = num_parallel,
            "vlm_concurrency within OLLAMA_NUM_PARALLEL — no clamp"
        );
        requested
    }
}

fn build_layout_pool(
    config: &AppConfig,
    pool_size: usize,
) -> (Option<Arc<DetectorPool<LayoutDetector>>>, Option<String>) {
    let layout_path = config.resolved_layout_model_path();
    if !layout_path.exists() {
        let reason = format!("model file not found at {}", layout_path.display());
        tracing::warn!("{reason}. Falling back to FullPage mode.");
        return (None, Some(reason));
    }

    let path_str = layout_path.to_str().unwrap_or_default();
    let mut slots: Vec<LayoutDetector> = Vec::with_capacity(pool_size);
    for i in 0..pool_size {
        match LayoutDetector::new(path_str, config.use_cuda) {
            Ok(det) => slots.push(det),
            Err(e) => {
                // One path: any slot failure aborts the whole pool. No
                // "at least one loaded" partial state to reason about.
                let reason = format!("load failed on slot {i}/{pool_size}: {e}");
                tracing::warn!(
                    "Failed to load layout detector: {e}. Falling back to FullPage mode."
                );
                return (None, Some(reason));
            }
        }
    }

    tracing::info!(
        "Layout detector pool loaded from {} (N={pool_size})",
        layout_path.display()
    );
    (Some(Arc::new(DetectorPool::new(slots))), None)
}

fn build_table_pool(
    config: &AppConfig,
    pool_size: usize,
) -> (
    Option<Arc<DetectorPool<TableStructureRecognizer>>>,
    Option<String>,
) {
    let slanet_path = config.resolved_table_model_path();
    if !slanet_path.exists() {
        let reason = format!("model file not found at {}", slanet_path.display());
        tracing::info!("{reason}, tables go to VLM");
        return (None, Some(reason));
    }

    let path_str = slanet_path.to_str().unwrap_or_default();
    let mut slots: Vec<TableStructureRecognizer> = Vec::with_capacity(pool_size);
    for i in 0..pool_size {
        match TableStructureRecognizer::new(path_str, config.use_cuda) {
            Ok(r) => slots.push(r),
            Err(e) => {
                let reason = format!("load failed on slot {i}/{pool_size}: {e}");
                tracing::warn!("Table structure recognizer not available: {e}");
                return (None, Some(reason));
            }
        }
    }

    tracing::info!("Table structure recognizer pool loaded (SLANet-Plus, N={pool_size})");
    (Some(Arc::new(DetectorPool::new(slots))), None)
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
    layout: &Option<Arc<DetectorPool<LayoutDetector>>>,
    table: &Option<Arc<DetectorPool<TableStructureRecognizer>>>,
    config: &AppConfig,
) -> Result<PreparedPage> {
    let bboxes = {
        let pool = layout
            .as_ref()
            .expect("per-region requires layout_detectors");
        let mut det = pool.acquire()?;
        det.detect(image)?
    };

    let bboxes = deduplicate_boxes(bboxes);
    let bboxes = filter_contained_regions(bboxes);

    if bboxes.is_empty() {
        tracing::info!(
            "Page {}: no layout detections → full-page VLM",
            page_idx + 1
        );
        let downscaled = maybe_downscale(image, config.max_image_dim);
        let jpeg_bytes = encode_jpeg(&downscaled)?;
        return Ok(PreparedPage::FullPage {
            page_idx,
            jpeg_bytes,
        });
    }

    // Filter out Skip regions
    let bboxes: Vec<BBox> = bboxes
        .into_iter()
        .filter(|b| RegionType::from_class(&b.class_name) != RegionType::Skip)
        .collect();

    // Text-only page check
    let has_tables = bboxes
        .iter()
        .any(|b| RegionType::from_class(&b.class_name) == RegionType::Table);
    let has_formulas = bboxes.iter().any(|b| {
        matches!(
            RegionType::from_class(&b.class_name),
            RegionType::Formula | RegionType::InlineFormula
        )
    });
    if !has_tables && !has_formulas {
        tracing::info!("Page {}: text-only → full-page VLM", page_idx + 1);
        let downscaled = maybe_downscale(image, config.max_image_dim);
        let jpeg_bytes = encode_jpeg(&downscaled)?;
        return Ok(PreparedPage::FullPage {
            page_idx,
            jpeg_bytes,
        });
    }

    let detection_order: Vec<usize> = bboxes.iter().map(|b| b.unique_id).collect();

    let mut table_bboxes = Vec::new();
    let mut other_bboxes = Vec::new();
    for bbox in bboxes {
        if RegionType::from_class(&bbox.class_name) == RegionType::Table && table.is_some() {
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
    if skipped_regions > 0 {
        tracing::warn!(
            skipped_regions,
            kept_regions = text_regions.len(),
            "page {} has skipped regions; page output will be partial",
            page_idx + 1
        );
    }

    // Prepare table regions: SLANet structure + per-cell crop + JPEG
    // encode. A 0-dim table crop drops the whole table; a 0-dim cell
    // crop emits an empty Vec for that cell (SLANet's structure still
    // needs one slot per cell so alignment is preserved).
    let mut table_regions = Vec::with_capacity(table_bboxes.len());
    for bbox in table_bboxes {
        let Some(crop) = crop_bbox(image, &bbox) else {
            tracing::warn!(
                class = %bbox.class_name,
                x1 = bbox.x1, y1 = bbox.y1, x2 = bbox.x2, y2 = bbox.y2,
                "table crop 0-dim after clamp — skipping table (page continues)"
            );
            continue;
        };
        let structure = {
            let pool = table.as_ref().expect("table_recognizers required");
            let mut rec = pool.acquire()?;
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

    Ok(PreparedPage::Regions {
        page_idx,
        detection_order,
        text_regions,
        table_regions,
    })
}

/// Stage 2: execute VLM inference for a single prepared page.
async fn execute_vlm_for_page(
    prepared: PreparedPage,
    ocr: Arc<OcrEngine>,
    sem: Arc<tokio::sync::Semaphore>,
    region_parallel: usize,
    on_progress: Arc<dyn Fn(ProgressEvent) + Send + Sync>,
    page_num: u64,
    total_pages: u64,
) -> Result<(usize, String)> {
    match prepared {
        PreparedPage::FullPage {
            page_idx,
            jpeg_bytes,
        } => {
            let permit = match sem.acquire().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        page_idx,
                        "semaphore closed on full-page VLM — emitting empty page"
                    );
                    return Ok((page_idx, String::new()));
                }
            };
            let text = match ocr.recognize(&jpeg_bytes).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        page_idx,
                        "full-page VLM failed — emitting empty page (paper continues)"
                    );
                    String::new()
                }
            };
            drop(permit);
            Ok((page_idx, text))
        }
        PreparedPage::Regions {
            page_idx,
            detection_order,
            text_regions,
            table_regions,
        } => {
            let total_regions = text_regions.len();
            let region_done = Arc::new(AtomicU64::new(0));

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
            // Inner closure is infallible — per-region failures degrade
            // to empty string, the paper continues.
            let region_results: Vec<(BBox, String)> = stream::iter(text_regions)
                .map(|r| {
                    let ocr = Arc::clone(&ocr);
                    let sem = Arc::clone(&sem);
                    let on_progress = Arc::clone(&on_progress);
                    let region_done = Arc::clone(&region_done);
                    async move {
                        if r.region_type == RegionType::Figure || r.region_type == RegionType::Skip
                        {
                            region_done.fetch_add(1, Ordering::Relaxed);
                            return (r.bbox, String::new());
                        }
                        if r.jpeg_bytes.is_empty() {
                            // prepare_page logged the skip; keep the slot empty so
                            // read-order assembly still has a placeholder.
                            region_done.fetch_add(1, Ordering::Relaxed);
                            return (r.bbox, String::new());
                        }
                        let permit = match sem.acquire().await {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "semaphore closed — emitting empty region"
                                );
                                return (r.bbox, String::new());
                            }
                        };
                        let text = match ocr.recognize_region(&r.jpeg_bytes, r.region_type).await {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    region_type = ?r.region_type,
                                    "region VLM failed — emitting empty region (paper continues)"
                                );
                                String::new()
                            }
                        };
                        drop(permit);
                        let done = region_done.fetch_add(1, Ordering::Relaxed) + 1;
                        on_progress(ProgressEvent {
                            stage: "vlm".into(),
                            page: page_num,
                            total_pages,
                            message: format!(
                                "OCR region {done}/{total_regions} on page {page_num}"
                            ),
                        });
                        (r.bbox, text)
                    }
                })
                .buffer_unordered(region_parallel)
                .collect()
                .await;

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

                let cell_texts: Vec<String> = stream::iter(table.cell_jpegs)
                    .map(|jpeg| {
                        let ocr = Arc::clone(&ocr);
                        let sem = Arc::clone(&sem);
                        let on_progress = Arc::clone(&on_progress);
                        let cell_done = Arc::clone(&cell_done);
                        async move {
                            if jpeg.is_empty() {
                                cell_done.fetch_add(1, Ordering::Relaxed);
                                return String::new();
                            }
                            let permit = match sem.acquire().await {
                                Ok(p) => p,
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "semaphore closed — emitting empty cell"
                                    );
                                    return String::new();
                                }
                            };
                            let result = match ocr.recognize_region(&jpeg, RegionType::Text).await {
                                Ok(text) => text.trim().to_string(),
                                Err(e) => {
                                    tracing::warn!("Cell OCR failed: {e}");
                                    String::new()
                                }
                            };
                            drop(permit);
                            let done = cell_done.fetch_add(1, Ordering::Relaxed) + 1;
                            on_progress(ProgressEvent {
                                stage: "vlm".into(),
                                page: page_num,
                                total_pages,
                                message: format!(
                                    "OCR table cell {done}/{total_cells} on page {page_num}"
                                ),
                            });
                            result
                        }
                    })
                    .buffer_unordered(region_parallel)
                    .collect::<Vec<String>>()
                    .await;

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

            Ok((page_idx, assemble_page_markdown(&regions)))
        }
    }
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
}
