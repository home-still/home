use crate::classify::{ConvertFailure, FailureCode};
use anyhow::Result;
use pdfium_render::prelude::*;

/// Largest page side a PDF may declare, in points. 14 400 pt (200 in) is
/// the limit the PDF format itself documents; a box beyond it, or one that
/// is not a finite positive number, is not a page.
pub const MAX_PAGE_POINTS: f32 = 14_400.0;

/// Pixel dimensions a page will be rendered at, after the pixel cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderSize {
    pub width: u32,
    pub height: u32,
}

/// Where a page is rasterized: `box_pts` at `dpi`, scaled down uniformly
/// until it fits `max_pixels`. Pure arithmetic, evaluated *before* any
/// bitmap exists — `max_image_dim` downscaling happens after rendering and
/// cannot protect memory.
///
/// A box that is not finite, not positive, wider than [`MAX_PAGE_POINTS`],
/// or so thin that the cap leaves it under one pixel is refused as a
/// permanent PDF fault: no backend can render it.
pub fn plan_render(
    width_pts: f32,
    height_pts: f32,
    dpi: u16,
    max_pixels: u64,
) -> Result<RenderSize, ConvertFailure> {
    let bad = |why: String| ConvertFailure::new(FailureCode::PdfParseError, why);
    for (side, name) in [(width_pts, "width"), (height_pts, "height")] {
        if !side.is_finite() || side <= 0.0 || side > MAX_PAGE_POINTS {
            return Err(bad(format!(
                "page {name} of {side} pt is not a renderable size (0 < side <= {MAX_PAGE_POINTS} pt)"
            )));
        }
    }
    let scale = f64::from(dpi) / 72.0;
    let (w, h) = (f64::from(width_pts) * scale, f64::from(height_pts) * scale);
    let pixels = w * h;
    let fit = if pixels > max_pixels as f64 {
        (max_pixels as f64 / pixels).sqrt()
    } else {
        1.0
    };
    let (width, height) = ((w * fit).floor(), (h * fit).floor());
    if width < 1.0 || height < 1.0 {
        return Err(bad(format!(
            "a {width_pts}x{height_pts} pt page renders to {width}x{height} px at {dpi} dpi under \
             the {max_pixels}-pixel limit"
        )));
    }
    Ok(RenderSize {
        width: width as u32,
        height: height as u32,
    })
}

pub use crate::pdfium::PdfParser;

impl PdfParser {
    /// Render a single page to a raster image at `dpi`, never larger than
    /// `max_pixels` (see [`plan_render`]). Streaming callers render one page,
    /// consume it, and drop it before rendering the next — so peak raster
    /// memory is a single page, not the whole document. This is the safeguard
    /// against the all-pages-in-RAM OOM that froze the host: a 900-page book
    /// is bounded to one page of raster at a time instead of ~14 GB.
    pub fn render_page(
        document: &PdfDocument,
        idx: u16,
        dpi: u16,
        max_pixels: u64,
    ) -> Result<PageData> {
        let page = document
            .pages()
            .get(idx)
            .map_err(|e| crate::pdfium::page_error(e, idx))?;

        let (width_pts, height_pts) = (page.width().value, page.height().value);
        let size = plan_render(width_pts, height_pts, dpi, max_pixels)
            .map_err(|e| anyhow::Error::new(e).context(format!("page {idx}")))?;

        let config = PdfRenderConfig::new()
            .set_target_width(size.width as i32)
            .set_target_height(size.height as i32);

        let bitmap = page
            .render_with_config(&config)
            .map_err(|e| crate::pdfium::render_error(e, idx))?;
        let image = bitmap.as_image();

        Ok(PageData {
            page_idx: idx as usize,
            image,
            width: width_pts,
            height: height_pts,
            text: None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct PageData {
    pub page_idx: usize,
    pub image: image::DynamicImage,
    pub width: f32,
    pub height: f32,
    pub text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_MAX_RENDER_PIXELS;

    const LETTER: (f32, f32) = (612.0, 792.0);

    #[test]
    fn a_normal_page_renders_at_exactly_the_requested_dpi() {
        let s = plan_render(LETTER.0, LETTER.1, 200, DEFAULT_MAX_RENDER_PIXELS).unwrap();
        assert_eq!((s.width, s.height), (1700, 2200));
    }

    #[test]
    fn a_page_over_the_pixel_cap_is_scaled_to_fit_and_keeps_its_aspect() {
        // 14400 pt square at 200 dpi is 40 000 x 40 000 = 1.6 Gpx (~6 GB).
        let s = plan_render(14_400.0, 14_400.0, 200, DEFAULT_MAX_RENDER_PIXELS).unwrap();
        assert!(
            u64::from(s.width) * u64::from(s.height) <= DEFAULT_MAX_RENDER_PIXELS,
            "{s:?}"
        );
        assert_eq!(s.width, s.height);
        assert!(s.width >= 5_900, "should use most of the budget: {s:?}");

        let wide = plan_render(14_400.0, 7_200.0, 300, 1_000_000).unwrap();
        assert!(u64::from(wide.width) * u64::from(wide.height) <= 1_000_000);
        let ratio = wide.width as f64 / wide.height as f64;
        assert!((ratio - 2.0).abs() < 0.01, "{wide:?}");
    }

    #[test]
    fn the_pixel_cap_is_never_exceeded_for_any_box_that_is_accepted() {
        for &(w, h) in &[
            (1.0, 1.0),
            (612.0, 792.0),
            (2384.0, 3370.0),
            (14_400.0, 14_400.0),
            (14_400.0, 30.0),
        ] {
            for dpi in [1u16, 72, 200, 600, u16::MAX] {
                for cap in [10_000u64, 1_000_000, DEFAULT_MAX_RENDER_PIXELS] {
                    if let Ok(s) = plan_render(w, h, dpi, cap) {
                        assert!(s.width >= 1 && s.height >= 1);
                        assert!(
                            u64::from(s.width) * u64::from(s.height) <= cap,
                            "{w}x{h} @ {dpi} cap {cap} -> {s:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn absurd_page_boxes_are_permanent_pdf_errors() {
        for (w, h) in [
            (f32::NAN, 792.0),
            (612.0, f32::NAN),
            (f32::INFINITY, 792.0),
            (612.0, f32::NEG_INFINITY),
            (0.0, 792.0),
            (612.0, 0.0),
            (-612.0, 792.0),
            (14_400.5, 792.0),
            (612.0, 1.0e9),
            (f32::MAX, f32::MAX),
        ] {
            let e = plan_render(w, h, 200, DEFAULT_MAX_RENDER_PIXELS).unwrap_err();
            assert_eq!(e.code(), FailureCode::PdfParseError, "{w}x{h}");
        }
    }

    #[test]
    fn a_sliver_that_the_cap_would_squash_below_one_pixel_is_refused() {
        // 14400 x 0.01 pt: any dpi leaves a zero-height bitmap.
        let e = plan_render(14_400.0, 0.01, 200, DEFAULT_MAX_RENDER_PIXELS).unwrap_err();
        assert_eq!(e.code(), FailureCode::PdfParseError);
    }

    #[test]
    fn a_cap_too_small_for_one_pixel_per_side_is_refused() {
        assert!(plan_render(612.0, 792.0, 200, 0).is_err());
    }

    // ── pdfium-backed: skipped (loudly) when libpdfium cannot be bound ──
    // CI needs libpdfium on the library path for these to run.

    fn pdfium_or_skip(test: &str) -> Option<PdfParser> {
        match PdfParser::new() {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("SKIPPED {test}: libpdfium cannot be bound here ({e:#})");
                None
            }
        }
    }

    /// A one-page PDF with the given MediaBox text, written to a temp file.
    fn page_with_media_box(dir: &std::path::Path, media_box: &str) -> String {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [{media_box}] >>"),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = vec![];
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        let path = dir.join("page.pdf");
        std::fs::write(&path, out).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_maximal_media_box_renders_within_the_pixel_cap_not_at_gigapixels() {
        let Some(parser) = pdfium_or_skip("a_maximal_media_box_renders_within_the_pixel_cap")
        else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // 14400 pt square at 200 dpi would be 40 000 x 40 000 px (~6 GB).
        let path = page_with_media_box(dir.path(), "0 0 14400 14400");
        let doc = parser.open(&path).unwrap();
        let cap = 1_000_000u64;
        let page = PdfParser::render_page(&doc, 0, 200, cap).expect("renders at reduced dpi");
        let px = u64::from(page.image.width()) * u64::from(page.image.height());
        assert!(
            px <= cap && px > cap / 2,
            "rendered {px} px under a {cap} cap"
        );
    }

    #[test]
    fn an_absurd_media_box_is_an_error_or_stays_within_the_cap_never_an_allocation() {
        let Some(parser) = pdfium_or_skip("an_absurd_media_box_is_an_error") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let cap = 1_000_000u64;
        for media_box in [
            "0 0 1000000000 1000000000",
            "0 0 99999999999999 5",
            "0 0 14401 14401",
        ] {
            let path = page_with_media_box(dir.path(), media_box);
            let Ok(doc) = parser.open(&path) else {
                continue;
            };
            match PdfParser::render_page(&doc, 0, 200, cap) {
                Err(e) => assert_eq!(
                    crate::classify::failure_code(&e),
                    Some(FailureCode::PdfParseError),
                    "{media_box}: {e:#}"
                ),
                Ok(p) => {
                    let px = u64::from(p.image.width()) * u64::from(p.image.height());
                    assert!(px <= cap, "{media_box}: rendered {px} px");
                }
            }
        }
    }
}
