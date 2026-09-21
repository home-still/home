use anyhow::{Context, Result};
use pdfium_render::prelude::*;

pub struct PdfParser {
    pdfium: Pdfium,
}

impl PdfParser {
    pub fn new() -> Result<Self> {
        let pdfium = Pdfium::default();
        Ok(Self { pdfium })
    }

    /// Open a document, borrowing this parser. The returned document must not
    /// outlive the parser (pdfium owns the library binding it borrows). Callers
    /// hold both on one blocking thread and render pages lazily.
    pub fn open<'a>(&'a self, path: &str) -> Result<PdfDocument<'a>> {
        self.pdfium
            .load_pdf_from_file(path, None)
            .with_context(|| format!("opening PDF {path}"))
    }

    /// Page count without rasterizing anything — a cheap metadata read. Used to
    /// size progress/order bookkeeping before the streaming render begins.
    pub fn page_count(&self, path: &str) -> Result<usize> {
        Ok(self.open(path)?.pages().len() as usize)
    }

    /// Render a single page to a raster image at `dpi`. Streaming callers render
    /// one page, consume it, and drop it before rendering the next — so peak
    /// raster memory is a single page, not the whole document. This is the
    /// safeguard against the all-pages-in-RAM OOM that froze the host: a
    /// 900-page book is bounded to one page of raster at a time instead of ~14 GB.
    pub fn render_page(document: &PdfDocument, idx: u16, dpi: u16) -> Result<PageData> {
        let page = document
            .pages()
            .get(idx)
            .with_context(|| format!("loading page {idx}"))?;

        let width = (page.width().value * dpi as f32 / 72.0) as i32;
        let height = (page.height().value * dpi as f32 / 72.0) as i32;

        let config = PdfRenderConfig::new()
            .set_target_width(width)
            .set_target_height(height);

        let bitmap = page.render_with_config(&config)?;
        let image = bitmap.as_image();

        Ok(PageData {
            page_idx: idx as usize,
            image,
            width: page.width().value,
            height: page.height().value,
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
