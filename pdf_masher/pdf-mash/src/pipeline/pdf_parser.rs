use anyhow::Result;
use pdfium_render::prelude::*;

pub struct PdfParser {
    pdfium: Pdfium,
}

impl PdfParser {
    pub fn new() -> Result<Self> {
        let pdfium = Pdfium::default();
        Ok(Self { pdfium })
    }

    pub fn parse_to_pages(&self, path: &str, dpi: u16) -> Result<Vec<PageData>> {
        let document = self.pdfium.load_pdf_from_file(path, None)?;
        let mut pages = Vec::new();

        for (idx, page) in document.pages().iter().enumerate() {
            let width = (page.width().value * dpi as f32 / 72.0) as i32;
            let height = (page.height().value * dpi as f32 / 72.0) as i32;

            let config = PdfRenderConfig::new()
                .set_target_width(width)
                .set_target_height(height);

            let bitmap = page.render_with_config(&config)?;
            let image = bitmap.as_image();

            pages.push(PageData {
                page_idx: idx,
                image,
                width: page.width().value,
                height: page.height().value,
                text: None,
            });
        }
        Ok(pages)
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
