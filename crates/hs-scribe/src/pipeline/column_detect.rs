//! Defensive column detection for PP-DocLayoutV3 misfires.
//!
//! Failure mode: PP-DocLayoutV3 sometimes returns a single wide `text`
//! bbox covering most of a 2-column academic page. The per-region path
//! crops that bbox, and the crop is effectively the full page — the VLM
//! gets its hardest possible input (long structured output, dense
//! reference-list patterns) and enters a repetition loop. Per-region
//! routing is supposed to bound output to ~50–500 tokens per call; one
//! page-wide bbox bypasses that bound.
//!
//! The fix runs in three steps after the layout detector returns:
//!
//! 1. **Detect a suspect**: scan bboxes for a single wide text-class
//!    region (`relative_width >= 0.62`) on a page that does NOT already
//!    show two narrow text columns. Wide regions on classes that
//!    legitimately span (figure, table, document title, image_caption)
//!    are NOT suspect — leave them alone.
//!
//! 2. **Verify with projection profile** (1-D, ~1–2 ms): binarize the
//!    suspect region's pixels, sum dark pixels per x-column, smooth with
//!    a small Gaussian, find the deepest sustained minimum in the gutter
//!    zone `[0.40·W, 0.60·W]`. Two columns confirmed when the minimum's
//!    mean value < 15% of the profile's 80th percentile AND the minimum
//!    spans ≥1% of bbox width.
//!
//! 3. **Split at the gutter**: emit two bboxes (left, right) at the
//!    confirmed gutter x. Reading order is left-then-right within the
//!    block (PP-DocLayoutV3's outer read-order still drives across-block
//!    ordering).
//!
//! Phase 1 (this file as initially shipped) runs the detector + verifier
//! in **shadow mode** — log decisions to the per-page diag JSONL but do
//! not mutate the bbox vector. After a corpus run confirms low false-
//! positive rate, Phase 2 wires the split into the routing path.

use crate::diag::ColumnSplitShadow;
use crate::models::layout::BBox;
use image::{DynamicImage, GenericImageView};

/// Layout classes that legitimately occupy a wide band of the page when
/// they're a single block (full-width title, two-column abstract that's
/// rendered as one block, the references section header). On a 2-column
/// body page these would be unusual — but they happen, and we should not
/// split them.
const SUSPECT_CLASSES: &[&str] = &["text", "paragraph_title", "reference_content"];

/// Wide-bbox threshold: bboxes spanning at least this fraction of page
/// width are candidates for the verifier. 0.62 is the brief's
/// recommendation — narrow enough to skip clearly-mid-column blocks
/// (typical 2-col text width is ~0.42·W per column), wide enough to
/// catch anything that has clearly absorbed both columns.
const WIDE_RATIO_MIN: f32 = 0.62;

/// Page-area gutter zone — projection-profile minimum must land within
/// this band of the bbox width to count. 2-col academic layouts have
/// gutters at the page midline; values outside this band are usually
/// false signals (figures with a vertical white strip, table column
/// boundaries, inline column gutters in marginalia).
const GUTTER_ZONE: (f32, f32) = (0.40, 0.60);

/// Two-column verdict: minimum mean must be below this fraction of the
/// profile's 80th-percentile density. Tuning trade: too low → misses
/// gutters with stray ascenders; too high → mistakes a wide block of
/// whitespace mid-paragraph for a column boundary. 0.15 is the brief's
/// recommendation.
const GUTTER_DEPTH_RATIO: f32 = 0.15;

/// Two-column verdict: minimum must span at least this fraction of bbox
/// width. Filters out single-column-of-whitespace artifacts (a 1-pixel
/// wide gap caused by a hyphen offset is not a gutter).
const MIN_GUTTER_SPAN_RATIO: f32 = 0.01;

fn clean_shadow() -> ColumnSplitShadow {
    ColumnSplitShadow {
        would_action: "clean".to_string(),
        ..Default::default()
    }
}

/// Find a single wide text-class region on this page that's a candidate
/// for column-split. Returns the index of the suspect into `bboxes` if
/// the page has *exactly one* such region (multiple wide regions =
/// already a multi-block layout, the model handles those fine; zero
/// wide regions = clean per-region routing, no intervention needed).
///
/// Pre-condition: `bboxes` is the post-dedupe, post-Skip-filter list as
/// returned by `prepare_page`'s prep stage. `page_width` is the source
/// image width in pixels.
pub fn detect_suspect_wide_region(bboxes: &[BBox], page_width: u32) -> Option<usize> {
    if page_width == 0 {
        return None;
    }
    let page_w = page_width as f32;
    let mut suspect_idx: Option<usize> = None;
    let mut suspect_count = 0usize;
    for (i, b) in bboxes.iter().enumerate() {
        if !is_suspect_class(&b.class_name) {
            continue;
        }
        let relative_width = (b.x2 - b.x1).max(0.0) / page_w;
        if relative_width < WIDE_RATIO_MIN {
            continue;
        }
        suspect_count += 1;
        if suspect_count > 1 {
            // Multiple wide text regions on the same page is unusual but
            // not a misfire — the model already has structure to work
            // with. Don't intervene.
            return None;
        }
        suspect_idx = Some(i);
    }
    suspect_idx
}

fn is_suspect_class(class: &str) -> bool {
    SUSPECT_CLASSES.iter().any(|c| *c == class)
}

/// Run the projection-profile verifier on the suspect bbox. Returns the
/// confirmed gutter x-coordinate in absolute image pixels if the page is
/// two-column, or `None` if the verifier rejects the call.
///
/// The check binarizes the bbox crop (Otsu), sums dark pixels along
/// each x column, smooths the profile with a small Gaussian, then looks
/// for a deep sustained minimum in the gutter zone `[0.40·W, 0.60·W]`.
/// 1-2 ms on a 2200×3000 px page at 200 DPI on this hardware class.
pub fn verify_two_column_via_profile(image: &DynamicImage, bbox: &BBox) -> Option<u32> {
    let (img_w, img_h) = image.dimensions();
    if img_w == 0 || img_h == 0 {
        return None;
    }

    // Clamp the bbox to image bounds (PP-DocLayoutV3 occasionally returns
    // sub-pixel float coords that, when rounded, land just past an edge).
    let x1 = bbox.x1.max(0.0).round() as u32;
    let y1 = bbox.y1.max(0.0).round() as u32;
    let x2 = bbox.x2.min(img_w as f32).round() as u32;
    let y2 = bbox.y2.min(img_h as f32).round() as u32;
    if x2 <= x1 || y2 <= y1 {
        return None;
    }
    let bbox_w = (x2 - x1) as usize;
    let bbox_h = (y2 - y1) as usize;
    if bbox_w < 32 || bbox_h < 32 {
        // Tiny suspects are layout noise, not 2-column bodies. The
        // smoothing window would dominate the signal anyway.
        return None;
    }

    let crop = image.crop_imm(x1, y1, x2 - x1, y2 - y1);
    let gray = crop.to_luma8();

    // Otsu's method picks a threshold by maximizing between-class
    // variance over the 8-bit histogram. Captures the typical paper
    // (light bg, dark glyphs) and inverted reverse-print (dark bg, light
    // glyphs) without per-page tuning.
    let threshold = otsu_threshold_8bit(&gray);

    // Density profile: per-column count of dark pixels (pixels at or
    // below `threshold`). `<=` is intentional: a bimodal page where the
    // dark mode is at exactly the threshold value should still register
    // as foreground. Otsu's threshold sits between the two modes; using
    // `<=` keeps the dark mode on the dark side of the cut.
    let mut profile = vec![0u32; bbox_w];
    for (x, y, pixel) in gray.enumerate_pixels() {
        if pixel.0[0] <= threshold {
            profile[x as usize] = profile[x as usize].saturating_add(1);
            // y unused but enumerate_pixels returns it; keep destructure
            let _ = y;
        }
    }

    // Smoothing: σ ≈ 1% of bbox width, kernel half-width = 3σ. Damps
    // single-column-of-pixels noise without eating real gutters.
    let sigma = (bbox_w as f32 * 0.01).max(1.0);
    let smoothed = gaussian_smooth_1d(&profile, sigma);

    let p80 = percentile(&smoothed, 0.80);
    if p80 == 0.0 {
        // Profile is uniformly zero — the crop has no dark content.
        // Likely a blank region the layout detector spuriously emitted.
        return None;
    }

    let gutter_lo = (bbox_w as f32 * GUTTER_ZONE.0) as usize;
    let gutter_hi = (bbox_w as f32 * GUTTER_ZONE.1).min(bbox_w as f32) as usize;
    if gutter_hi <= gutter_lo {
        return None;
    }

    // Find the longest run within the gutter zone where the smoothed
    // profile sits below `GUTTER_DEPTH_RATIO * p80`. The mean depth of
    // that run must also clear the threshold — keeps a single noisy
    // pixel from anchoring an otherwise-busy slice.
    let depth_threshold = GUTTER_DEPTH_RATIO * p80;
    let min_span = (bbox_w as f32 * MIN_GUTTER_SPAN_RATIO).ceil() as usize;
    let min_span = min_span.max(2);

    let mut best_run: Option<(usize, usize)> = None;
    let mut current_start: Option<usize> = None;
    for x in gutter_lo..gutter_hi {
        if smoothed[x] <= depth_threshold {
            if current_start.is_none() {
                current_start = Some(x);
            }
        } else if let Some(start) = current_start.take() {
            let len = x - start;
            if len >= min_span {
                match best_run {
                    None => best_run = Some((start, x)),
                    Some((bs, be)) if (x - start) > (be - bs) => best_run = Some((start, x)),
                    _ => {}
                }
            }
        }
    }
    if let Some(start) = current_start {
        let len = gutter_hi - start;
        if len >= min_span {
            match best_run {
                None => best_run = Some((start, gutter_hi)),
                Some((bs, be)) if (gutter_hi - start) > (be - bs) => {
                    best_run = Some((start, gutter_hi))
                }
                _ => {}
            }
        }
    }

    let (run_start, run_end) = best_run?;
    let mean: f32 = smoothed[run_start..run_end].iter().sum::<f32>() / (run_end - run_start) as f32;
    if mean > depth_threshold {
        return None;
    }

    let gutter_center_in_bbox = (run_start + run_end) / 2;
    let gutter_x = x1 + gutter_center_in_bbox as u32;
    Some(gutter_x)
}

/// Build a `ColumnSplitShadow` for a given page's bbox vector. Calls the
/// detector and (if a suspect was found) the verifier. Pure read — does
/// NOT mutate `bboxes`. Phase 2 will replace the suspect with the split
/// pair in routing; until then the shadow record is the only output.
pub fn shadow_decide(image: &DynamicImage, bboxes: &[BBox]) -> ColumnSplitShadow {
    let (page_w, _) = image.dimensions();
    let Some(idx) = detect_suspect_wide_region(bboxes, page_w) else {
        return clean_shadow();
    };
    let suspect = &bboxes[idx];
    let suspect_relative_width = if page_w == 0 {
        0.0
    } else {
        (suspect.x2 - suspect.x1) / page_w as f32
    };

    match verify_two_column_via_profile(image, suspect) {
        Some(gutter_x) => ColumnSplitShadow {
            suspect_idx: Some(idx),
            suspect_class: Some(suspect.class_name.clone()),
            suspect_relative_width: Some(suspect_relative_width),
            verifier_passed: true,
            gutter_x: Some(gutter_x),
            would_action: "split".to_string(),
        },
        None => ColumnSplitShadow {
            suspect_idx: Some(idx),
            suspect_class: Some(suspect.class_name.clone()),
            suspect_relative_width: Some(suspect_relative_width),
            verifier_passed: false,
            gutter_x: None,
            would_action: "detector_only".to_string(),
        },
    }
}

/// Phase 2: replace the suspect bbox with two split bboxes at the
/// confirmed gutter x. Reading order: left first, then right (within
/// the suspect's vertical span — PP-DocLayoutV3's outer read-order
/// drives across-block ordering, so we don't need to renumber other
/// bboxes). Only the `read_order` field on the new pair gets a small
/// epsilon offset to keep the assembler's stable sort deterministic.
///
/// Caller responsibility: only invoke when `shadow_decide` returned
/// `would_action == "split"` AND the active-split feature gate is
/// enabled. This function is a pure mutation helper — it does not
/// re-verify the decision.
pub fn split_suspect_at_gutter(bboxes: &mut Vec<BBox>, suspect_idx: usize, gutter_x_abs: u32) {
    if suspect_idx >= bboxes.len() {
        return;
    }
    let suspect = bboxes[suspect_idx].clone();
    // Reject pathological gutter coords that would produce zero-width
    // children — safer to leave the suspect intact than emit a degenerate
    // bbox that the cropper would later reject.
    let gx = gutter_x_abs as f32;
    if gx <= suspect.x1 + 1.0 || gx >= suspect.x2 - 1.0 {
        return;
    }
    let mut left = suspect.clone();
    let mut right = suspect.clone();
    left.x2 = gx;
    right.x1 = gx;
    // Tiny offsets so the page assembler's stable sort places left
    // before right when their nominal y-bands overlap.
    right.read_order = suspect.read_order + 0.5;
    // unique_id collisions are fine for downstream code (the unique_id
    // is the row in detection order, used only for read-order recovery
    // post-VLM) but we keep the suspect's id on the left half and bump
    // the right by one — trivial uniqueness within a page.
    right.unique_id = suspect.unique_id.saturating_add(1);
    bboxes[suspect_idx] = left;
    bboxes.insert(suspect_idx + 1, right);
}

/// Whether the active-split feature gate is enabled. Reads
/// `HS_SCRIBE_COLUMN_SPLIT_ACTIVE` at call time so an operator can flip
/// the gate without rebuilding (set the env var to `1`/`true`/`yes` on
/// the scribe-server unit, restart, and the next conversion uses
/// active split). Default off — Phase 1 is shadow-only.
pub fn active_split_enabled() -> bool {
    match std::env::var("HS_SCRIBE_COLUMN_SPLIT_ACTIVE")
        .ok()
        .as_deref()
    {
        Some("1") | Some("true") | Some("yes") | Some("TRUE") | Some("YES") => true,
        _ => false,
    }
}

/// Otsu's threshold for an 8-bit luma image. Returns the threshold value
/// in `0..=255`. Pixels strictly darker than the returned value are
/// treated as foreground (dark) by the projection profile.
fn otsu_threshold_8bit(img: &image::GrayImage) -> u8 {
    let mut hist = [0u64; 256];
    for p in img.pixels() {
        hist[p.0[0] as usize] += 1;
    }
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 128;
    }

    let total_f = total as f64;
    let sum_total: f64 = hist
        .iter()
        .enumerate()
        .map(|(i, &c)| i as f64 * c as f64)
        .sum();

    let mut sum_b: f64 = 0.0;
    let mut w_b: f64 = 0.0;
    let mut max_var: f64 = -1.0;
    let mut plateau_start: u16 = 0;
    let mut plateau_end: u16 = 0;
    for t in 0u16..256 {
        w_b += hist[t as usize] as f64;
        if w_b == 0.0 {
            continue;
        }
        let w_f = total_f - w_b;
        if w_f == 0.0 {
            break;
        }
        sum_b += (t as f64) * hist[t as usize] as f64;
        let m_b = sum_b / w_b;
        let m_f = (sum_total - sum_b) / w_f;
        let between = w_b * w_f * (m_b - m_f) * (m_b - m_f);
        if between > max_var {
            max_var = between;
            plateau_start = t;
            plateau_end = t;
        } else if (between - max_var).abs() < f64::EPSILON {
            // Bimodal histograms with a clean valley make the variance
            // identical across the gap (no mass to redistribute). Track
            // the plateau and pick the midpoint so the threshold lands
            // in the valley, not on the dark mode itself.
            plateau_end = t;
        }
    }
    let mid = (plateau_start as u32 + plateau_end as u32) / 2;
    mid as u8
}

/// Discrete 1-D Gaussian convolution. Kernel half-width = ceil(3·σ).
/// Reflective padding at the edges (we don't want the smoothed profile
/// to bias toward zero at the gutter zone of a narrow bbox).
fn gaussian_smooth_1d(profile: &[u32], sigma: f32) -> Vec<f32> {
    if profile.is_empty() {
        return Vec::new();
    }
    let half = (3.0 * sigma).ceil() as i32;
    let kernel_len = 2 * half + 1;
    let mut kernel = vec![0.0f32; kernel_len as usize];
    let two_sigma_sq = 2.0 * sigma * sigma;
    let mut k_sum = 0.0f32;
    for i in -half..=half {
        let v = (-(i * i) as f32 / two_sigma_sq).exp();
        kernel[(i + half) as usize] = v;
        k_sum += v;
    }
    if k_sum > 0.0 {
        for v in kernel.iter_mut() {
            *v /= k_sum;
        }
    }
    let n = profile.len() as i32;
    let mut out = vec![0.0f32; profile.len()];
    for x in 0..n {
        let mut acc = 0.0f32;
        for (k_idx, k) in kernel.iter().enumerate() {
            let mut idx = x + (k_idx as i32 - half);
            // Reflect at boundaries.
            if idx < 0 {
                idx = -idx;
            }
            if idx >= n {
                idx = 2 * (n - 1) - idx;
            }
            if idx < 0 || idx >= n {
                continue;
            }
            acc += profile[idx as usize] as f32 * k;
        }
        out[x as usize] = acc;
    }
    out
}

/// Approximate p-th percentile of a slice. Sorts a copy — fine for the
/// short profiles (a few thousand entries) we operate on.
fn percentile(values: &[f32], p: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p = p.clamp(0.0, 1.0);
    let idx = ((v.len() as f32 - 1.0) * p).round() as usize;
    v[idx.min(v.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bbox(class: &str, x1: f32, y1: f32, x2: f32, y2: f32) -> BBox {
        BBox {
            x1,
            y1,
            x2,
            y2,
            confidence: 1.0,
            class_id: 0,
            class_name: class.to_string(),
            unique_id: 0,
            read_order: 0.0,
        }
    }

    #[test]
    fn detector_finds_single_wide_text() {
        // Page 1000 px wide; one text bbox 70% of width → suspect.
        let bs = vec![
            bbox("text", 50.0, 100.0, 750.0, 900.0),
            bbox("page_number", 470.0, 950.0, 530.0, 980.0),
        ];
        assert_eq!(detect_suspect_wide_region(&bs, 1000), Some(0));
    }

    #[test]
    fn detector_skips_narrow_columns() {
        // Page with two narrow text columns. No single suspect → None.
        let bs = vec![
            bbox("text", 50.0, 100.0, 480.0, 900.0),
            bbox("text", 520.0, 100.0, 950.0, 900.0),
        ];
        assert_eq!(detect_suspect_wide_region(&bs, 1000), None);
    }

    #[test]
    fn detector_skips_wide_figure() {
        // Wide figure spanning both columns is legitimate — not suspect.
        let bs = vec![bbox("figure", 50.0, 100.0, 950.0, 900.0)];
        assert_eq!(detect_suspect_wide_region(&bs, 1000), None);
    }

    #[test]
    fn detector_skips_two_wide_text_regions() {
        // Two wide text regions on one page = unusual but explicit; we
        // don't intervene because the structure is already there.
        let bs = vec![
            bbox("text", 50.0, 100.0, 950.0, 400.0),
            bbox("text", 50.0, 500.0, 950.0, 900.0),
        ];
        assert_eq!(detect_suspect_wide_region(&bs, 1000), None);
    }

    #[test]
    fn detector_handles_zero_width_page() {
        let bs = vec![bbox("text", 50.0, 100.0, 750.0, 900.0)];
        assert_eq!(detect_suspect_wide_region(&bs, 0), None);
    }

    #[test]
    fn otsu_separates_bimodal_histogram() {
        // Build a 8x8 image with a clear dark/light split.
        let mut img = image::GrayImage::new(8, 8);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            *p = image::Luma([if x < 4 { 30u8 } else { 220u8 }]);
        }
        let t = otsu_threshold_8bit(&img);
        // The threshold should land between the two modes.
        assert!(t > 30 && t < 220, "otsu picked t={t}");
    }

    #[test]
    fn gaussian_smooth_idempotent_under_zero_input() {
        let p = vec![0u32; 100];
        let s = gaussian_smooth_1d(&p, 2.0);
        assert!(s.iter().all(|v| (*v).abs() < 1e-6));
    }

    #[test]
    fn gaussian_smooth_preserves_total_mass_approximately() {
        let p = vec![0u32, 0, 0, 100, 0, 0, 0];
        let s = gaussian_smooth_1d(&p, 1.0);
        let total: f32 = s.iter().sum();
        // Gaussian convolution conserves mass within reflection-padding
        // tolerance on a 7-element profile.
        assert!((total - 100.0).abs() < 5.0, "total={total}");
    }

    #[test]
    fn percentile_handles_basic_cases() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert!((percentile(&v, 0.0) - 1.0).abs() < 1e-6);
        assert!((percentile(&v, 1.0) - 5.0).abs() < 1e-6);
        // 80th percentile of [1..5] lands on the 4th element (idx 3).
        assert!((percentile(&v, 0.8) - 4.0).abs() < 1e-6);
    }

    #[test]
    fn percentile_handles_empty() {
        let v: Vec<f32> = Vec::new();
        assert_eq!(percentile(&v, 0.5), 0.0);
    }

    /// Build a synthetic 2-column page image: white background, two dark
    /// vertical bands separated by a clear gutter. Used to exercise the
    /// full verifier end-to-end.
    fn synthetic_two_column(width: u32, height: u32) -> image::DynamicImage {
        let mut img = image::GrayImage::new(width, height);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            let xf = x as f32 / width as f32;
            // Left column: 0.10..0.45. Right column: 0.55..0.90. Gutter
            // 0.45..0.55. Edges (margins) white.
            let in_left = xf > 0.10 && xf < 0.45;
            let in_right = xf > 0.55 && xf < 0.90;
            let dark = in_left || in_right;
            *p = image::Luma([if dark { 30u8 } else { 240u8 }]);
        }
        image::DynamicImage::ImageLuma8(img)
    }

    #[test]
    fn verifier_confirms_synthetic_two_column() {
        let img = synthetic_two_column(800, 600);
        // Suspect bbox covers the whole image (the "wide text region").
        let b = bbox("text", 0.0, 0.0, 800.0, 600.0);
        let g = verify_two_column_via_profile(&img, &b);
        assert!(g.is_some(), "verifier should confirm 2-col");
        let g = g.unwrap();
        // Gutter should land within the central [0.40, 0.60] band.
        assert!(
            g >= 320 && g <= 480,
            "gutter at {g}, expected 320..480 (mid-page band)"
        );
    }

    #[test]
    fn verifier_rejects_uniform_dark_block() {
        // Solid dark block — no gutter. Verifier should NOT confirm.
        let mut img = image::GrayImage::new(400, 300);
        for (_x, _y, p) in img.enumerate_pixels_mut() {
            *p = image::Luma([30u8]);
        }
        let img = image::DynamicImage::ImageLuma8(img);
        let b = bbox("text", 0.0, 0.0, 400.0, 300.0);
        assert!(verify_two_column_via_profile(&img, &b).is_none());
    }

    #[test]
    fn verifier_rejects_blank_block() {
        // Solid white — also no gutter (no dark content at all).
        let mut img = image::GrayImage::new(400, 300);
        for (_x, _y, p) in img.enumerate_pixels_mut() {
            *p = image::Luma([240u8]);
        }
        let img = image::DynamicImage::ImageLuma8(img);
        let b = bbox("text", 0.0, 0.0, 400.0, 300.0);
        assert!(verify_two_column_via_profile(&img, &b).is_none());
    }

    #[test]
    fn shadow_decide_clean_when_no_suspect() {
        let img = synthetic_two_column(800, 600);
        let bs = vec![
            bbox("text", 50.0, 100.0, 380.0, 500.0),
            bbox("text", 420.0, 100.0, 750.0, 500.0),
        ];
        let s = shadow_decide(&img, &bs);
        assert_eq!(s.would_action, "clean");
        assert!(s.suspect_idx.is_none());
    }

    #[test]
    fn split_replaces_suspect_with_left_right_pair() {
        let mut bs = vec![
            bbox("page_number", 470.0, 950.0, 530.0, 980.0),
            bbox("text", 50.0, 100.0, 950.0, 900.0),
            bbox("figure_title", 100.0, 920.0, 900.0, 940.0),
        ];
        // Gutter at x=500 (midline); suspect is index 1.
        split_suspect_at_gutter(&mut bs, 1, 500);
        // Vec grew by one — left+right replaced the single suspect.
        assert_eq!(bs.len(), 4);
        // Untouched bboxes still surround the split pair.
        assert_eq!(bs[0].class_name, "page_number");
        assert_eq!(bs[3].class_name, "figure_title");
        // Left half: original.x1..gutter.
        assert_eq!(bs[1].x1, 50.0);
        assert_eq!(bs[1].x2, 500.0);
        assert_eq!(bs[1].class_name, "text");
        // Right half: gutter..original.x2.
        assert_eq!(bs[2].x1, 500.0);
        assert_eq!(bs[2].x2, 950.0);
        assert_eq!(bs[2].class_name, "text");
        // Right read_order is offset so stable-sort places left first.
        assert!(bs[2].read_order > bs[1].read_order);
    }

    #[test]
    fn split_rejects_degenerate_gutter() {
        // Gutter at x=51 → left bbox would be 1px wide. No-op.
        let mut bs = vec![bbox("text", 50.0, 100.0, 950.0, 900.0)];
        split_suspect_at_gutter(&mut bs, 0, 51);
        assert_eq!(bs.len(), 1, "should not split when child would be ~0-wide");
        assert_eq!(bs[0].x1, 50.0);
        assert_eq!(bs[0].x2, 950.0);
    }

    #[test]
    fn split_rejects_out_of_range_index() {
        // Index past end — silent no-op rather than panic. The caller
        // gets the unchanged vec and the error is recoverable.
        let mut bs = vec![bbox("text", 50.0, 100.0, 950.0, 900.0)];
        split_suspect_at_gutter(&mut bs, 99, 500);
        assert_eq!(bs.len(), 1);
    }

    #[test]
    fn active_split_gate_default_off() {
        // Gate must default OFF — we will not silently start splitting
        // bboxes on a freshly-deployed scribe just because the env var
        // is unset.
        std::env::remove_var("HS_SCRIBE_COLUMN_SPLIT_ACTIVE");
        assert!(!active_split_enabled());
    }

    #[test]
    fn active_split_gate_on_when_env_truthy() {
        for v in ["1", "true", "yes", "TRUE", "YES"] {
            std::env::set_var("HS_SCRIBE_COLUMN_SPLIT_ACTIVE", v);
            assert!(
                active_split_enabled(),
                "expected gate ON for env value `{v}`"
            );
        }
        for v in ["0", "false", "no", "off", ""] {
            std::env::set_var("HS_SCRIBE_COLUMN_SPLIT_ACTIVE", v);
            assert!(
                !active_split_enabled(),
                "expected gate OFF for env value `{v}`"
            );
        }
        std::env::remove_var("HS_SCRIBE_COLUMN_SPLIT_ACTIVE");
    }

    #[test]
    fn shadow_decide_split_when_detector_and_verifier_agree() {
        let img = synthetic_two_column(800, 600);
        let bs = vec![bbox("text", 0.0, 0.0, 800.0, 600.0)];
        let s = shadow_decide(&img, &bs);
        assert_eq!(s.would_action, "split");
        assert_eq!(s.suspect_idx, Some(0));
        assert!(s.verifier_passed);
        assert!(s.gutter_x.is_some());
    }
}
