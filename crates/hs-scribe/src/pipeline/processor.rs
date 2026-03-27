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
use crate::client::ProgressEvent;
use anyhow::Result;
use futures::stream::{self, StreamExt};
use image::DynamicImage;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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

pub struct Processor {
    ocr: Arc<OcrEngine>,
    layout_detector: Option<Arc<std::sync::Mutex<LayoutDetector>>>,
    table_recognizer: Option<Arc<std::sync::Mutex<TableStructureRecognizer>>>,
    config: AppConfig,
}

impl Processor {
    pub fn new(config: AppConfig) -> Result<Self> {
        let ocr = Arc::new(OcrEngine::from_config(&config));

        let layout_detector = if config.pipeline_mode == PipelineMode::PerRegion {
            let layout_path = config.resolved_layout_model_path();
            if layout_path.exists() {
                match LayoutDetector::new(layout_path.to_str().unwrap_or_default(), config.use_cuda)
                {
                    Ok(det) => {
                        tracing::info!("Layout detector loaded from {}", layout_path.display());
                        Some(Arc::new(std::sync::Mutex::new(det)))
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to load layout detector: {e}. Falling back to FullPage mode."
                        );
                        None
                    }
                }
            } else {
                tracing::warn!(
                    "Layout model not found at {}. Falling back to FullPage mode.",
                    layout_path.display()
                );
                None
            }
        } else {
            None
        };

        let table_recognizer = if config.pipeline_mode == PipelineMode::PerRegion {
            let slanet_path = config.resolved_table_model_path();
            if slanet_path.exists() {
                match TableStructureRecognizer::new(
                    slanet_path.to_str().unwrap_or_default(),
                    config.use_cuda,
                ) {
                    Ok(r) => {
                        tracing::info!("Table structure recognizer loaded (SLANet-Plus)");
                        Some(Arc::new(std::sync::Mutex::new(r)))
                    }
                    Err(e) => {
                        tracing::warn!("Table structure recognizer not available: {e}");
                        None
                    }
                }
            } else {
                tracing::info!(
                    "SLANet-Plus model not found at {}, tables go to VLM",
                    slanet_path.display()
                );
                None
            }
        } else {
            None
        };

        Ok(Self {
            ocr,
            layout_detector,
            table_recognizer,
            config,
        })
    }

    pub fn ocr(&self) -> Arc<OcrEngine> {
        Arc::clone(&self.ocr)
    }

    /// Is this processor running in per-region mode (i.e., has a layout detector)?
    fn is_per_region(&self) -> bool {
        self.layout_detector.is_some()
    }

    pub fn has_layout_detector(&self) -> bool {
        self.layout_detector.is_some()
    }

    pub fn has_table_recognizer(&self) -> bool {
        self.table_recognizer.is_some()
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
            let mut det = self
                .layout_detector
                .as_ref()
                .expect("per-region requires layout_detector")
                .lock()
                .map_err(|e| anyhow::anyhow!("Layout detector lock poisoned: {e}"))?;
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
                && self.table_recognizer.is_some()
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

        let region_results: Vec<Result<(BBox, String)>> = stream::iter(other_bboxes.into_iter())
            .map(|bbox| {
                let ocr = Arc::clone(&ocr);
                let image = Arc::clone(&image_arc);
                async move {
                    let region_type = RegionType::from_class(&bbox.class_name);

                    if region_type == RegionType::Figure || region_type == RegionType::Skip {
                        return Ok((bbox, String::new()));
                    }

                    let crop = crop_bbox(&image, &bbox);
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

        let mut regions: Vec<(BBox, String)> =
            region_results.into_iter().collect::<Result<Vec<_>>>()?;

        // Process table regions: SLANet-Plus structure → per-cell VLM OCR → HTML
        for bbox in table_bboxes {
            let crop = crop_bbox(image, &bbox);
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
            let mut rec = self
                .table_recognizer
                .as_ref()
                .expect("table_recognizer required")
                .lock()
                .map_err(|e| anyhow::anyhow!("Table recognizer lock poisoned: {e}"))?;
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
                    let w = ((x2 - x1) as u32).max(1);
                    let h = ((y2 - y1) as u32).max(1);
                    let cell_crop = table_img.crop_imm(x1 as u32, y1 as u32, w, h);
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
        let on_progress = Arc::new(on_progress);

        let pages = {
            let pdf_parser = PdfParser::new()?;
            pdf_parser.parse_to_pages(pdf_path, self.config.dpi)?
        };
        let total = pages.len() as u64;

        // Emit parse-complete event
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

            let page_markdowns: Vec<Result<String>> =
                stream::iter(pages.into_iter().enumerate())
                    .map(|(i, page)| {
                        let ocr = Arc::clone(&ocr);
                        let on_progress = Arc::clone(&on_progress);
                        let completed = Arc::clone(&completed);
                        async move {
                            let downscaled = maybe_downscale(&page.image, max_dim);
                            let image_bytes = encode_jpeg(&downscaled)?;
                            tracing::info!(
                                "Processing page {}/{} ({}x{}, {} bytes JPEG)",
                                i + 1,
                                total,
                                page.image.width(),
                                page.image.height(),
                                image_bytes.len()
                            );
                            let text = ocr.recognize(&image_bytes).await?;
                            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                            on_progress(ProgressEvent {
                                stage: "vlm".into(),
                                page: done,
                                total_pages: total,
                                message: format!("OCR page {done}/{total}"),
                            });
                            Ok(text)
                        }
                    })
                    .buffered(parallel)
                    .collect()
                    .await;

            let markdowns: Vec<String> = page_markdowns.into_iter().collect::<Result<Vec<_>>>()?;
            return Ok(join_pages(&markdowns));
        }

        // Per-region mode: 2-stage async pipeline
        let (tx, mut rx) = tokio::sync::mpsc::channel::<PreparedPage>(3);
        let vlm_sem = Arc::new(tokio::sync::Semaphore::new(self.config.vlm_concurrency));

        let layout = self.layout_detector.clone();
        let table = self.table_recognizer.clone();
        let config = self.config.clone();
        let on_progress_s1 = Arc::clone(&on_progress);

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
                on_progress_s1(ProgressEvent {
                    stage: "layout".into(),
                    page: (idx + 1) as u64,
                    total_pages: total,
                    message: format!("Layout page {}/{total}", idx + 1),
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
                let result =
                    execute_vlm_for_page(prepared, ocr, sem, region_parallel).await;
                let done = vlm_completed.fetch_add(1, Ordering::Relaxed) + 1;
                on_progress(ProgressEvent {
                    stage: "vlm".into(),
                    page: done,
                    total_pages: total,
                    message: format!("OCR page {done}/{total}"),
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
            let page_markdowns: Vec<Result<String>> = stream::iter(pages.into_iter().enumerate())
                .map(|(i, page)| {
                    let ocr = Arc::clone(&ocr);
                    async move {
                        let downscaled = maybe_downscale(&page.image, max_dim);
                        let image_bytes = encode_jpeg(&downscaled)?;
                        tracing::info!(
                            "Processing page {}/{} ({}x{}, {} bytes JPEG)",
                            i + 1,
                            total,
                            page.image.width(),
                            page.image.height(),
                            image_bytes.len()
                        );
                        ocr.recognize(&image_bytes).await
                    }
                })
                .buffered(parallel)
                .collect()
                .await;

            let markdowns: Vec<String> = page_markdowns.into_iter().collect::<Result<Vec<_>>>()?;
            return Ok(join_pages(&markdowns));
        }

        // Per-region mode: 2-stage async pipeline
        // Stage 1 (CPU): layout detection, cropping, JPEG encoding — sequential (ONNX mutex)
        // Stage 2 (VLM): HTTP inference — concurrent across pages

        let (tx, mut rx) = tokio::sync::mpsc::channel::<PreparedPage>(3);
        let vlm_sem = Arc::new(tokio::sync::Semaphore::new(self.config.vlm_concurrency));

        let layout = self.layout_detector.clone();
        let table = self.table_recognizer.clone();
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
            tasks.spawn(
                async move { execute_vlm_for_page(prepared, ocr, sem, region_parallel).await },
            );
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
    layout: &Option<Arc<std::sync::Mutex<LayoutDetector>>>,
    table: &Option<Arc<std::sync::Mutex<TableStructureRecognizer>>>,
    config: &AppConfig,
) -> Result<PreparedPage> {
    let bboxes = {
        let mut det = layout
            .as_ref()
            .expect("per-region requires layout_detector")
            .lock()
            .map_err(|e| anyhow::anyhow!("Layout detector lock poisoned: {e}"))?;
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

    // Prepare non-table regions: crop + JPEG encode
    let mut text_regions = Vec::with_capacity(other_bboxes.len());
    for bbox in other_bboxes {
        let region_type = RegionType::from_class(&bbox.class_name);
        let jpeg_bytes = if region_type == RegionType::Figure || region_type == RegionType::Skip {
            Vec::new()
        } else {
            let crop = crop_bbox(image, &bbox);
            encode_jpeg(&crop)?
        };
        text_regions.push(PreparedRegion {
            bbox,
            region_type,
            jpeg_bytes,
        });
    }

    // Prepare table regions: SLANet structure + per-cell crop + JPEG encode
    let mut table_regions = Vec::with_capacity(table_bboxes.len());
    for bbox in table_bboxes {
        let crop = crop_bbox(image, &bbox);
        let structure = {
            let mut rec = table
                .as_ref()
                .expect("table_recognizer required")
                .lock()
                .map_err(|e| anyhow::anyhow!("Table recognizer lock poisoned: {e}"))?;
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
            let w = ((x2 - x1) as u32).max(1);
            let h = ((y2 - y1) as u32).max(1);
            let cell_crop = crop.crop_imm(x1 as u32, y1 as u32, w, h);
            cell_jpegs.push(encode_jpeg(&cell_crop)?);
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
) -> Result<(usize, String)> {
    match prepared {
        PreparedPage::FullPage {
            page_idx,
            jpeg_bytes,
        } => {
            let _permit = sem
                .acquire()
                .await
                .map_err(|e| anyhow::anyhow!("Semaphore closed: {e}"))?;
            let text = ocr.recognize(&jpeg_bytes).await?;
            Ok((page_idx, text))
        }
        PreparedPage::Regions {
            page_idx,
            detection_order,
            text_regions,
            table_regions,
        } => {
            // Process text regions with semaphore-gated concurrency
            let region_results: Vec<Result<(BBox, String)>> =
                stream::iter(text_regions.into_iter())
                    .map(|r| {
                        let ocr = Arc::clone(&ocr);
                        let sem = Arc::clone(&sem);
                        async move {
                            if r.region_type == RegionType::Figure
                                || r.region_type == RegionType::Skip
                            {
                                return Ok((r.bbox, String::new()));
                            }
                            let _permit = sem
                                .acquire()
                                .await
                                .map_err(|e| anyhow::anyhow!("Semaphore closed: {e}"))?;
                            let text = ocr.recognize_region(&r.jpeg_bytes, r.region_type).await?;
                            Ok((r.bbox, text))
                        }
                    })
                    .buffer_unordered(region_parallel)
                    .collect()
                    .await;

            let mut regions: Vec<(BBox, String)> =
                region_results.into_iter().collect::<Result<Vec<_>>>()?;

            // Process table cells with semaphore-gated concurrency
            for table in table_regions {
                let cell_texts: Vec<String> = stream::iter(table.cell_jpegs.into_iter())
                    .map(|jpeg| {
                        let ocr = Arc::clone(&ocr);
                        let sem = Arc::clone(&sem);
                        async move {
                            let _permit = sem
                                .acquire()
                                .await
                                .map_err(|e| anyhow::anyhow!("Semaphore closed: {e}"))?;
                            match ocr.recognize_region(&jpeg, RegionType::Text).await {
                                Ok(text) => Ok(text.trim().to_string()),
                                Err(e) => {
                                    tracing::warn!("Cell OCR failed: {e}");
                                    Ok(String::new())
                                }
                            }
                        }
                    })
                    .buffer_unordered(region_parallel)
                    .collect::<Vec<Result<String>>>()
                    .await
                    .into_iter()
                    .collect::<Result<Vec<_>>>()?;

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

/// Crop a bounding box region from the image with 2px padding, clamped to bounds.
fn crop_bbox(image: &DynamicImage, bbox: &BBox) -> DynamicImage {
    let (img_w, img_h) = (image.width() as f32, image.height() as f32);
    let pad = 2.0;

    let x1 = (bbox.x1 - pad).max(0.0) as u32;
    let y1 = (bbox.y1 - pad).max(0.0) as u32;
    let x2 = (bbox.x2 + pad).min(img_w) as u32;
    let y2 = (bbox.y2 + pad).min(img_h) as u32;

    let w = x2.saturating_sub(x1).max(1);
    let h = y2.saturating_sub(y1).max(1);

    image.crop_imm(x1, y1, w, h)
}

pub(crate) fn encode_jpeg(image: &DynamicImage) -> Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85);
    image.write_with_encoder(encoder)?;
    Ok(buf.into_inner())
}
