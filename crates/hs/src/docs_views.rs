//! The rendered views of a PDF: one page as a picture with the passage a
//! link highlights boxed on it (`page.png`), that picture in a page of
//! HTML with the neighbouring pages a click away (`page`), and the whole
//! document in a viewer that draws it in the browser (`viewer`).
//!
//! All three are answered for a signed link, checked exactly as a link to
//! the document itself is ([`verify_request`]), and only for a document
//! whose source is a PDF: anything else has no pages to show, and is a
//! 404.
//!
//! # What a highlight is
//!
//! A link's highlight names runs of words in the document's word-box
//! sidecar. Each run becomes one rectangle per text line
//! ([`hs_common::quote::line_rects`]), and those are what is drawn: on the
//! picture itself for `page.png`, and by the viewer over its own rendering
//! of the page. A highlight the sidecar cannot answer (there is no
//! sidecar, or it lacks a page or a word the highlight names) is a 404: a
//! page shown without the passage it was linked for would claim to show a
//! quote and not show it.
//!
//! # The links between the views
//!
//! Every link one view carries to another is signed here
//! ([`hs_common::docs::resign`]) for the subject of the link the request
//! came in on, with that link's issue time and expiry: nothing a view
//! links to is open to anyone else, or for longer.
//!
//! # Assets
//!
//! The viewer's scripts and the stylesheet are compiled into the binary
//! and served without a signature under `/docs/assets/<version>/`: they
//! hold nothing of the corpus. `<version>` is a hash of the assets, so a
//! browser may keep them for good ([`ASSET_CACHE_CONTROL`]) and still gets
//! the new ones from a newer service, whose pages name another version.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, LazyLock};

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_common::docs::{resign, Highlight, LinkFormat, LinkKey, LinkQuery, SignedFields};
use hs_common::quote::line_rects;
use hs_common::storage::Storage;
use hs_common::stores::DocumentLayout;
use hs_common::word_boxes::{self, WordBoxes, BOX_UNITS_PER_PAGE_SIDE};
use hs_scribe::config::DEFAULT_MAX_RENDER_PIXELS;
use hs_scribe::pdfium::{guarded_call, with_parser, PdfParser};
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, Rgb, RgbImage};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::docs_service::{
    filename_for, find_source_key, private_cache_control, verify_request, DocsState, Refusal,
    Verified,
};

/// How wide the picture of a page is, whatever the page's size: enough
/// that the body text of a two-column paper is sharp on a dense screen.
const PAGE_IMAGE_WIDTH_PX: u32 = 1_400;

/// The yellow a highlighted passage is tinted with, and how opaque it is
/// (out of 255): the text under it stays readable.
const HIGHLIGHT_FILL_RGBA: [u8; 4] = [255, 221, 0, 90];

/// The amber of the line drawn around a highlighted passage, which is
/// what finds it on a page seen whole.
const HIGHLIGHT_OUTLINE_RGB: [u8; 3] = [230, 138, 0];

/// How thick that line is, in pixels of the picture.
const HIGHLIGHT_OUTLINE_PX: u32 = 3;

/// A fully opaque alpha.
const OPAQUE: u16 = u8::MAX as u16;

/// The only source a page view is made from.
const PDF_EXTENSION: &str = "pdf";

/// The content security policy of the two HTML views: scripts, the
/// stylesheet, pictures and fetches from this origin and nowhere else,
/// and nothing inline.
///
/// `'wasm-unsafe-eval'` is for pdf.js, which decodes the JPEG 2000 images
/// of a scan with a WebAssembly module: without it those pages are blank.
/// It allows compiling WebAssembly, not `eval`.
const VIEW_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self' \
     'wasm-unsafe-eval'; style-src 'self'; img-src 'self'; connect-src 'self'; worker-src 'self'";

/// How long a browser may keep an asset: a year, without asking again. The
/// asset's URL holds a hash of its content, so it never changes under one.
const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// The route the assets are served under.
const ASSET_ROUTE_PREFIX: &str = "/docs/assets";

/// How many bytes of the assets' hash make their version: 64 bits, far
/// more than telling two builds apart needs.
const ASSET_VERSION_BYTES: usize = 8;

const JAVASCRIPT: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const HTML: &str = "text/html; charset=utf-8";
const PNG: &str = "image/png";

/// The stylesheet both HTML views load.
const STYLESHEET: &str = "viewer.css";
/// The script the viewer runs.
const VIEWER_SCRIPT: &str = "viewer.js";
/// The `id` of the element the viewer's script reads the link's data from
/// (`viewer.js` names it too).
const VIEWER_DATA_ID: &str = "hs-cite";

/// One file served under [`ASSET_ROUTE_PREFIX`].
struct Asset {
    name: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
}

/// Everything the HTML views load. pdf.js is Mozilla's, Apache-2.0: its
/// licence is beside it in `assets/docs/pdfjs-4.10.38/`. `viewer.js`
/// imports the two pdf.js files by these names.
const ASSETS: [Asset; 4] = [
    Asset {
        name: "pdf.min.mjs",
        content_type: JAVASCRIPT,
        bytes: include_bytes!("../assets/docs/pdfjs-4.10.38/pdf.min.mjs"),
    },
    Asset {
        name: "pdf.worker.min.mjs",
        content_type: JAVASCRIPT,
        bytes: include_bytes!("../assets/docs/pdfjs-4.10.38/pdf.worker.min.mjs"),
    },
    Asset {
        name: VIEWER_SCRIPT,
        content_type: JAVASCRIPT,
        bytes: include_bytes!("../assets/docs/viewer.js"),
    },
    Asset {
        name: STYLESHEET,
        content_type: CSS,
        bytes: include_bytes!("../assets/docs/viewer.css"),
    },
];

/// The version in the assets' URLs: the start of a SHA-256 over every
/// asset's name and bytes, in hexadecimal.
static ASSET_VERSION: LazyLock<String> = LazyLock::new(|| {
    let mut hasher = Sha256::new();
    for asset in &ASSETS {
        hasher.update(asset.name.as_bytes());
        hasher.update(asset.bytes);
    }
    let mut version = String::new();
    for byte in hasher.finalize().iter().take(ASSET_VERSION_BYTES) {
        // Writing to a `String` cannot fail.
        let _ = write!(version, "{byte:02x}");
    }
    version
});

fn asset_version() -> &'static str {
    &ASSET_VERSION
}

/// The URL path of an asset of this build.
fn asset_path(name: &str) -> String {
    format!("{ASSET_ROUTE_PREFIX}/{}/{name}", asset_version())
}

/// `GET /docs/assets/{version}/{file}`: a script or the stylesheet.
///
/// Any version but this build's is a 404: the answer is cacheable for a
/// year, so it must be the file the URL names and no other.
pub(crate) async fn serve_asset(Path((version, file)): Path<(String, String)>) -> Response {
    let asset = ASSETS
        .iter()
        .find(|asset| asset.name == file)
        .filter(|_| version == asset_version());
    let Some(asset) = asset else {
        return Refusal::NotFound.into_response();
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(ASSET_CACHE_CONTROL),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    (StatusCode::OK, headers, asset.bytes).into_response()
}

/// `GET /docs/v1/{stem}/page.png`: the picture of one page.
pub(crate) async fn serve_page_image(
    State(state): State<Arc<DocsState>>,
    Path(stem): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    page_image(&state, &stem, &query)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

/// `GET /docs/v1/{stem}/page`: one page, in a page of HTML.
pub(crate) async fn serve_page(
    State(state): State<Arc<DocsState>>,
    Path(stem): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    page_view(&state, &stem, &query)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

/// `GET /docs/v1/{stem}/viewer`: the whole document, in the viewer.
pub(crate) async fn serve_viewer(
    State(state): State<Arc<DocsState>>,
    Path(stem): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    viewer(&state, &stem, &query)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

/// A verified request for a view of a document whose source is a PDF.
struct PdfRequest<'a> {
    link: LinkQuery,
    layout: &'a DocumentLayout,
    /// Where the PDF is in storage.
    pdf_key: String,
}

/// Check the link, then that the document it names is a PDF.
async fn pdf_request<'a>(
    state: &'a DocsState,
    stem: &str,
    query: &HashMap<String, String>,
    format: LinkFormat,
) -> Result<PdfRequest<'a>, Refusal> {
    let Verified { link, layout } = verify_request(state, stem, query, format)?;
    let Some((pdf_key, _, extension)) = find_source_key(&*state.storage, layout, stem).await else {
        return Err(Refusal::NotFound);
    };
    if extension != PDF_EXTENSION {
        return Err(Refusal::NotFound);
    }
    Ok(PdfRequest {
        link,
        layout,
        pdf_key,
    })
}

/// Read an object a view is made from. One that is not there is a 404;
/// storage that cannot be read is the service's failure.
async fn read_object(storage: &dyn Storage, key: &str) -> Result<Vec<u8>, Refusal> {
    match storage.get(key).await {
        Ok(bytes) => Ok(bytes),
        Err(e) if hs_common::storage::is_not_found(&e) => Err(Refusal::NotFound),
        Err(e) => {
            tracing::error!("{key} could not be read: {e:#}");
            Err(Refusal::Failed)
        }
    }
}

/// The rectangles to draw on each page, `[x0, y0, x1, y1]` in box units.
type PageRects = BTreeMap<u32, Vec<[u16; 4]>>;

/// The rectangles a link's highlight covers, read from the document's
/// word-box sidecar. A link without a highlight covers none, and its
/// document needs no sidecar.
async fn highlight_rects(
    state: &DocsState,
    layout: &DocumentLayout,
    stem: &str,
    highlight: Option<&Highlight>,
) -> Result<PageRects, Refusal> {
    let Some(highlight) = highlight else {
        return Ok(PageRects::new());
    };
    let key = word_boxes::boxes_key(layout, stem);
    let bytes = read_object(&*state.storage, &key).await?;
    let boxes = word_boxes::decode(&bytes).map_err(|e| {
        tracing::error!("the word boxes at {key} could not be read: {e}");
        Refusal::Failed
    })?;
    rects_by_page(&boxes, highlight).ok_or(Refusal::NotFound)
}

/// One rectangle per text line of every run a highlight names. `None`
/// when it names a page or a word the sidecar does not hold.
fn rects_by_page(boxes: &WordBoxes, highlight: &Highlight) -> Option<PageRects> {
    let mut rects = PageRects::new();
    for span in highlight.spans() {
        let page = boxes.page(span.page)?;
        if usize::try_from(span.end).ok()? > page.boxes.len() {
            return None;
        }
        rects
            .entry(span.page)
            .or_default()
            .extend(line_rects(page, span.start..span.end));
    }
    Some(rects)
}

/// Run pdfium work off the async threads. pdfium holds a process-wide
/// lock and a render takes as long as the page is complex.
///
/// A failure is logged here; the client is told only that the document
/// could not be read.
async fn on_pdfium<T: Send + 'static>(
    stem: &str,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, Refusal> {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => {
            tracing::error!("pdfium could not read {stem}: {e:#}");
            Err(Refusal::Failed)
        }
        Err(e) => {
            tracing::error!("the pdfium work for {stem} did not finish: {e}");
            Err(Refusal::Failed)
        }
    }
}

/// How many pages a PDF has.
fn count_pages(pdf: &[u8]) -> anyhow::Result<u32> {
    with_parser(|parser| guarded_call(|| parser.count_pages_in_bytes(pdf)))
}

/// The picture of page `page` (1-based) of a PDF, [`PAGE_IMAGE_WIDTH_PX`]
/// wide. `None` when the document has no such page.
fn render_page(pdf: &[u8], page: u32) -> anyhow::Result<Option<RgbImage>> {
    with_parser(|parser| {
        let pages = guarded_call(|| parser.count_pages_in_bytes(pdf))?;
        let Some(index) = page.checked_sub(1).filter(|_| page <= pages) else {
            return Ok(None);
        };
        let index = u16::try_from(index)?;
        let document = guarded_call(|| parser.open_bytes(pdf))?;
        let rendered = guarded_call(|| {
            PdfParser::render_page_to_width(
                &document,
                index,
                PAGE_IMAGE_WIDTH_PX,
                DEFAULT_MAX_RENDER_PIXELS,
            )
        })?;
        Ok(Some(rendered.image.into_rgb8()))
    })
}

/// A box coordinate as a pixel offset along a side `side_px` long.
fn to_pixels(units: u16, side_px: u32) -> u32 {
    let units = u64::from(units.min(BOX_UNITS_PER_PAGE_SIDE));
    let offset = units * u64::from(side_px) / u64::from(BOX_UNITS_PER_PAGE_SIDE);
    // At most `side_px`: `units` is at most the whole side.
    u32::try_from(offset).unwrap_or(side_px)
}

/// A pixel under the highlight's tint.
fn tinted(pixel: Rgb<u8>) -> Rgb<u8> {
    let [red, green, blue, alpha] = HIGHLIGHT_FILL_RGBA;
    let alpha = u16::from(alpha);
    let mix = |under: u8, over: u8| {
        let mixed =
            (u16::from(under) * (OPAQUE - alpha) + u16::from(over) * alpha + OPAQUE / 2) / OPAQUE;
        // A weighted mean of two bytes is a byte.
        u8::try_from(mixed).unwrap_or(u8::MAX)
    };
    Rgb([
        mix(pixel[0], red),
        mix(pixel[1], green),
        mix(pixel[2], blue),
    ])
}

/// Draw a highlight on the picture of a page: each rectangle tinted
/// [`HIGHLIGHT_FILL_RGBA`], with a line [`HIGHLIGHT_OUTLINE_PX`] thick
/// just inside its edge.
fn draw_highlights(image: &mut RgbImage, rects: &[[u16; 4]]) {
    let (width, height) = image.dimensions();
    for &[x0, y0, x1, y1] in rects {
        let (left, right) = (to_pixels(x0, width), to_pixels(x1, width));
        let (top, bottom) = (to_pixels(y0, height), to_pixels(y1, height));
        for y in top..bottom {
            for x in left..right {
                let from_edge = (x - left)
                    .min(right - 1 - x)
                    .min(y - top)
                    .min(bottom - 1 - y);
                let Some(pixel) = image.get_pixel_mut_checked(x, y) else {
                    continue;
                };
                *pixel = if from_edge < HIGHLIGHT_OUTLINE_PX {
                    Rgb(HIGHLIGHT_OUTLINE_RGB)
                } else {
                    tinted(*pixel)
                };
            }
        }
    }
}

fn encode_png(image: &RgbImage) -> anyhow::Result<Vec<u8>> {
    let mut png = Vec::new();
    PngEncoder::new(&mut png).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgb8,
    )?;
    Ok(png)
}

/// The headers every view carries: what it is, that it is the reader's
/// own and expires with the link, and that it tells no other site where
/// the reader came from.
fn view_headers(content_type: &'static str, expires_at: u64) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, private_cache_control(expires_at));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers
}

/// A page of HTML, under [`VIEW_CONTENT_SECURITY_POLICY`].
fn html_response(html: String, expires_at: u64) -> Response {
    let mut headers = view_headers(HTML, expires_at);
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(VIEW_CONTENT_SECURITY_POLICY),
    );
    (StatusCode::OK, headers, html).into_response()
}

async fn page_image(
    state: &DocsState,
    stem: &str,
    query: &HashMap<String, String>,
) -> Result<Response, Refusal> {
    let request = pdf_request(state, stem, query, LinkFormat::PageImage).await?;
    // A link to a page is signed with its page (`hs_common::docs::mint`
    // refuses to sign one without); a signed link that lacks it was not
    // made by this deployment's code.
    let page = request.link.page.ok_or(Refusal::Refused)?;
    let rects = highlight_rects(state, request.layout, stem, request.link.highlight.as_ref())
        .await?
        .remove(&page)
        .unwrap_or_default();
    let pdf = read_object(&*state.storage, &request.pdf_key).await?;
    let png = on_pdfium(stem, move || {
        let Some(mut image) = render_page(&pdf, page)? else {
            return Ok(None);
        };
        draw_highlights(&mut image, &rects);
        encode_png(&image).map(Some)
    })
    .await?
    .ok_or(Refusal::NotFound)?;

    let mut headers = view_headers(PNG, request.link.expires_at);
    let filename = filename_for(stem, &format!("p{page}.png"));
    if let Ok(value) = HeaderValue::from_str(&format!("inline; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    Ok((StatusCode::OK, headers, png).into_response())
}

/// The links from one view of a document to its others.
struct Siblings<'a> {
    key: &'a LinkKey,
    stem: &'a str,
    link: &'a LinkQuery,
}

impl Siblings<'_> {
    /// A rendered view, highlighting what the request's link highlights.
    fn view(&self, format: LinkFormat, page: Option<u32>) -> Result<String, Refusal> {
        self.sign(format, page, self.link.highlight.clone())
    }

    /// The stored source or markdown, whole.
    fn stored(&self, format: LinkFormat) -> Result<String, Refusal> {
        self.sign(format, None, None)
    }

    fn sign(
        &self,
        format: LinkFormat,
        page: Option<u32>,
        highlight: Option<Highlight>,
    ) -> Result<String, Refusal> {
        let fields = SignedFields {
            stem: self.stem.to_string(),
            format,
            subject: self.link.subject.clone(),
            collection: self.link.collection.clone(),
            page,
            highlight,
            issued_at: self.link.issued_at,
            expires_at: self.link.expires_at,
        };
        resign(self.key, &fields)
            .map(|link| link.to_path())
            .map_err(|e| {
                tracing::error!(
                    "the link to the {format} of {} could not be signed: {e}",
                    self.stem
                );
                Refusal::Failed
            })
    }
}

/// What a view calls the document: the catalog's title, or the stem of a
/// document the catalog has no title for.
async fn title_of(
    state: &DocsState,
    layout: &DocumentLayout,
    stem: &str,
) -> Result<String, Refusal> {
    let entry =
        hs_common::catalog::read_catalog_entry_via(&*state.storage, &layout.catalog_prefix, stem)
            .await
            .map_err(|e| {
                tracing::error!("the catalog entry of {stem} could not be read: {e:#}");
                Refusal::Failed
            })?;
    Ok(entry
        .and_then(|entry| entry.title)
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| stem.to_string()))
}

/// The link to a document's converted text, when it has been converted.
async fn markdown_link(
    state: &DocsState,
    layout: &DocumentLayout,
    stem: &str,
    siblings: &Siblings<'_>,
) -> Result<Option<String>, Refusal> {
    let key = layout.markdown_key(stem);
    match state.storage.head(&key).await {
        Ok(Some(_)) => siblings.stored(LinkFormat::Markdown).map(Some),
        Ok(None) => Ok(None),
        Err(e) => {
            tracing::error!("{key} could not be looked up: {e:#}");
            Err(Refusal::Failed)
        }
    }
}

/// Text as it may stand in HTML, between tags or in a quoted attribute.
fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// One link of a view's navigation.
struct NavLink {
    href: String,
    text: String,
    /// The link's relation to this page (`prev`, `next`), if it has one.
    rel: Option<&'static str>,
}

impl NavLink {
    fn new(href: String, text: impl Into<String>) -> Self {
        Self {
            href,
            text: text.into(),
            rel: None,
        }
    }

    fn rel(mut self, rel: &'static str) -> Self {
        self.rel = Some(rel);
        self
    }
}

/// The start of a view's HTML, through the bar that names the document
/// and holds the links to its other views.
fn html_head(page_title: &str, heading: &str, place: Option<&str>, nav: &[NavLink]) -> String {
    let mut html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"robots\" content=\"noindex\">\n<title>{}</title>\n\
         <link rel=\"stylesheet\" href=\"{}\">\n</head>\n<body>\n\
         <header class=\"bar\">\n<h1>{}</h1>\n",
        escape_html(page_title),
        escape_html(&asset_path(STYLESHEET)),
        escape_html(heading),
    );
    // Writing to a `String` cannot fail.
    if let Some(place) = place {
        let _ = writeln!(html, "<p>{}</p>", escape_html(place));
    }
    html.push_str("<nav>\n");
    for link in nav {
        let rel = link
            .rel
            .map_or_else(String::new, |rel| format!(" rel=\"{rel}\""));
        let _ = writeln!(
            html,
            "<a href=\"{}\"{rel}>{}</a>",
            escape_html(&link.href),
            escape_html(&link.text)
        );
    }
    html.push_str("</nav>\n</header>\n");
    html
}

async fn page_view(
    state: &DocsState,
    stem: &str,
    query: &HashMap<String, String>,
) -> Result<Response, Refusal> {
    let request = pdf_request(state, stem, query, LinkFormat::Page).await?;
    // Signed with its page, like a page image's link.
    let page = request.link.page.ok_or(Refusal::Refused)?;
    // The picture this page shows is refused for a highlight the sidecar
    // cannot answer, so the page is too.
    highlight_rects(state, request.layout, stem, request.link.highlight.as_ref()).await?;
    let pdf = read_object(&*state.storage, &request.pdf_key).await?;
    let pages = on_pdfium(stem, move || count_pages(&pdf)).await?;
    if page > pages {
        return Err(Refusal::NotFound);
    }
    let title = title_of(state, request.layout, stem).await?;
    let siblings = Siblings {
        key: &state.key,
        stem,
        link: &request.link,
    };

    let mut nav = Vec::new();
    if let Some(previous) = page.checked_sub(1).filter(|&previous| previous >= 1) {
        nav.push(
            NavLink::new(
                siblings.view(LinkFormat::Page, Some(previous))?,
                "Previous page",
            )
            .rel("prev"),
        );
    }
    if page < pages {
        nav.push(
            NavLink::new(
                siblings.view(LinkFormat::Page, Some(page + 1))?,
                "Next page",
            )
            .rel("next"),
        );
    }
    nav.push(NavLink::new(
        siblings.view(LinkFormat::Viewer, Some(page))?,
        "Open in viewer",
    ));
    // `#page=` is what a browser's own PDF viewer opens a document at.
    nav.push(NavLink::new(
        format!("{}#page={page}", siblings.stored(LinkFormat::Source)?),
        "Open original PDF",
    ));
    if let Some(markdown) = markdown_link(state, request.layout, stem, &siblings).await? {
        nav.push(NavLink::new(markdown, "Converted text"));
    }

    let place = format!("Page {page} of {pages}");
    let mut html = html_head(&format!("{title}, page {page}"), &title, Some(&place), &nav);
    // Writing to a `String` cannot fail.
    let _ = write!(
        html,
        "<main class=\"sheet\">\n<img src=\"{}\" alt=\"{}\">\n</main>\n</body>\n</html>\n",
        escape_html(&siblings.view(LinkFormat::PageImage, Some(page))?),
        escape_html(&format!("Page {page} of {title}")),
    );
    Ok(html_response(html, request.link.expires_at))
}

/// What the viewer's script is told: the page's `hs-cite` data block.
#[derive(Serialize)]
struct ViewerData {
    /// The signed link to the PDF, as a path on this origin.
    source: String,
    /// The page the link names, 1-based.
    page: Option<u32>,
    /// How many units a page side is divided into: what `rects` are in.
    units: u16,
    /// The rectangles to box, by page.
    highlights: Vec<PageHighlight>,
}

#[derive(Serialize)]
struct PageHighlight {
    page: u32,
    /// `[x0, y0, x1, y1]`, the origin at the page's top left.
    rects: Vec<[u16; 4]>,
}

/// JSON as it may stand in a `<script>` data block: nothing in it can
/// close the element or open a comment, because no `<` is left in it.
/// `\u003c` is the same character to a JSON parser.
fn json_for_script(json: &str) -> String {
    json.replace('<', "\\u003c")
}

async fn viewer(
    state: &DocsState,
    stem: &str,
    query: &HashMap<String, String>,
) -> Result<Response, Refusal> {
    let request = pdf_request(state, stem, query, LinkFormat::Viewer).await?;
    let rects =
        highlight_rects(state, request.layout, stem, request.link.highlight.as_ref()).await?;
    let title = title_of(state, request.layout, stem).await?;
    let siblings = Siblings {
        key: &state.key,
        stem,
        link: &request.link,
    };
    let source = siblings.stored(LinkFormat::Source)?;

    let mut nav = Vec::new();
    // The page a reader of the link is taken to: the first one with a
    // highlighted passage, or the one the link names.
    if let Some(page) = rects.keys().next().copied().or(request.link.page) {
        nav.push(NavLink::new(
            siblings.view(LinkFormat::Page, Some(page))?,
            format!("Page {page} as an image"),
        ));
    }
    nav.push(NavLink::new(source.clone(), "Open original PDF"));
    if let Some(markdown) = markdown_link(state, request.layout, stem, &siblings).await? {
        nav.push(NavLink::new(markdown, "Converted text"));
    }

    let data = ViewerData {
        source,
        page: request.link.page,
        units: BOX_UNITS_PER_PAGE_SIDE,
        highlights: rects
            .into_iter()
            .map(|(page, rects)| PageHighlight { page, rects })
            .collect(),
    };
    let data = serde_json::to_string(&data).map_err(|e| {
        tracing::error!("the viewer's data for {stem} could not be written: {e}");
        Refusal::Failed
    })?;

    let mut html = html_head(&title, &title, None, &nav);
    // Writing to a `String` cannot fail.
    let _ = write!(
        html,
        "<p id=\"status\" class=\"status\" role=\"status\">Loading the document…</p>\n\
         <noscript><p class=\"status\">The viewer needs JavaScript. The links above open the \
         document without it.</p></noscript>\n\
         <main id=\"pages\" class=\"pages\"></main>\n\
         <script type=\"application/json\" id=\"{VIEWER_DATA_ID}\">{}</script>\n\
         <script type=\"module\" src=\"{}\"></script>\n</body>\n</html>\n",
        json_for_script(&data),
        escape_html(&asset_path(VIEWER_SCRIPT)),
    );
    Ok(html_response(html, request.link.expires_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs_service::tests::{call, service, TestResponse};
    use axum::Router;
    use hs_common::docs::HighlightSpan;
    use hs_common::storage::LocalFsStorage;
    use hs_common::word_boxes::{PageWords, WordOrigin, WORD_BOXES_VERSION};

    const STEM: &str = "10.1234_views";
    const SUBJECT: &str = "laptop";
    const STORE: &str = "academic";
    const PAGES: usize = 3;
    /// US Letter, in points: the size of every page of the fixture.
    const PAGE_SIZE_PT: (f32, f32) = (612.0, 792.0);
    /// The picture of a Letter page at [`PAGE_IMAGE_WIDTH_PX`].
    const PAGE_IMAGE_HEIGHT_PX: u32 = 1_811;
    const WHITE: Rgb<u8> = Rgb([255, 255, 255]);
    /// The words of page 2 of the fixture's sidecar: two on one line, one
    /// on the next.
    const PAGE_TWO_WORDS: [(&str, [u16; 4]); 3] = [
        ("strand", [2_000, 3_000, 3_000, 3_200]),
        ("accurate", [3_100, 3_000, 4_500, 3_200]),
        ("capture", [2_000, 3_300, 3_200, 3_500]),
    ];

    /// `true`, and says so, when libpdfium cannot be bound here: the tests
    /// that render need it, and CI has it.
    fn without_pdfium(test: &str) -> bool {
        match hs_scribe::pdfium::require() {
            Ok(()) => false,
            Err(e) => {
                eprintln!("SKIPPED {test}: libpdfium cannot be bound here ({e:#})");
                true
            }
        }
    }

    /// A well-formed PDF of [`PAGES`] blank Letter pages, classic xref.
    fn blank_pdf() -> Vec<u8> {
        let kids = (0..PAGES)
            .map(|i| format!("{} 0 R", i + 3))
            .collect::<Vec<_>>()
            .join(" ");
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [{kids}] /Count {PAGES} >>"),
        ];
        objects.extend((0..PAGES).map(|_| {
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] >>",
                PAGE_SIZE_PT.0, PAGE_SIZE_PT.1
            )
        }));
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = pdf.len();
        let mut tail = format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
        for offset in offsets {
            writeln!(tail, "{offset:010} 00000 n ").unwrap();
        }
        write!(
            tail,
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .unwrap();
        pdf.extend_from_slice(tail.as_bytes());
        pdf
    }

    /// The fixture's sidecar: [`PAGE_TWO_WORDS`] on page 2, no word on the
    /// other pages.
    fn sidecar(pdf: &[u8]) -> WordBoxes {
        let pages = (1..=PAGES as u32)
            .map(|number| {
                let mut page = PageWords::new(
                    number,
                    WordOrigin::TextLayer,
                    PAGE_SIZE_PT.0,
                    PAGE_SIZE_PT.1,
                );
                if number == 2 {
                    for (word, bounds) in PAGE_TWO_WORDS {
                        page.push(word.to_string(), bounds);
                    }
                }
                page
            })
            .collect();
        WordBoxes {
            version: WORD_BOXES_VERSION,
            source_sha256: word_boxes::source_sha256(pdf),
            pages,
        }
    }

    /// Put the fixture in the corpus: the PDF, its markdown, its catalog
    /// entry and (when asked) its word boxes.
    async fn put_fixture(dir: &std::path::Path, state: &DocsState, title: &str, boxes: bool) {
        let storage = LocalFsStorage::new(dir);
        let (_, layout) = state.stores.documents(STORE).unwrap();
        let pdf = blank_pdf();
        if boxes {
            storage
                .put(
                    &word_boxes::boxes_key(layout, STEM),
                    word_boxes::encode(&sidecar(&pdf)).unwrap(),
                )
                .await
                .unwrap();
        }
        storage
            .put(&layout.source_key(STEM, PDF_EXTENSION), pdf)
            .await
            .unwrap();
        storage
            .put(&layout.markdown_key(STEM), b"# the text\n".to_vec())
            .await
            .unwrap();
        hs_common::catalog::write_catalog_entry_via(
            &storage,
            &layout.catalog_prefix,
            STEM,
            &hs_common::catalog::CatalogEntry {
                title: Some(title.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    /// A highlight of all three words of page 2.
    fn page_two_highlight() -> Highlight {
        Highlight::new(vec![HighlightSpan {
            page: 2,
            start: 0,
            end: 3,
        }])
        .unwrap()
    }

    /// The link to a view, as a client would hold it.
    fn link(
        key: &LinkKey,
        format: LinkFormat,
        page: Option<u32>,
        highlight: Option<&Highlight>,
    ) -> String {
        hs_common::docs::mint(key, STEM, format, SUBJECT, STORE, page, highlight, 3600)
            .unwrap()
            .to_path()
    }

    /// Every `href` and `src` of a page of HTML, as a browser would
    /// follow them.
    fn links_in(html: &str) -> Vec<String> {
        let mut links = Vec::new();
        for attribute in [" href=\"", " src=\""] {
            for (at, _) in html.match_indices(attribute) {
                let rest = &html[at + attribute.len()..];
                let end = rest.find('"').unwrap();
                links.push(rest[..end].replace("&amp;", "&"));
            }
        }
        links
    }

    fn text(response: &TestResponse) -> &str {
        std::str::from_utf8(&response.body).unwrap()
    }

    /// The one link of a page of HTML that leads to `route`.
    fn link_to(html: &str, route: &str) -> String {
        let found: Vec<_> = links_in(html)
            .into_iter()
            .filter(|link| link.contains(&format!("/{route}?")))
            .collect();
        assert_eq!(found.len(), 1, "one link to {route} in {html}");
        found[0].clone()
    }

    async fn get(app: &Router, uri: &str) -> TestResponse {
        call(app, uri, None).await
    }

    #[test]
    fn a_highlight_is_tinted_inside_a_line_around_it_and_touches_nothing_else() {
        let mut image = RgbImage::from_pixel(1_000, 500, WHITE);
        // 200..600 px across, 100..200 px down.
        draw_highlights(&mut image, &[[2_000, 2_000, 6_000, 4_000]]);

        let outline = Rgb(HIGHLIGHT_OUTLINE_RGB);
        assert_eq!(*image.get_pixel(400, 150), tinted(WHITE), "the middle");
        assert_ne!(tinted(WHITE), WHITE, "a tint that cannot be seen");
        for (x, y) in [(200, 150), (599, 150), (400, 100), (400, 199), (202, 102)] {
            assert_eq!(*image.get_pixel(x, y), outline, "the edge at {x},{y}");
        }
        assert_eq!(*image.get_pixel(203, 103), tinted(WHITE), "inside the line");
        for (x, y) in [(199, 150), (600, 150), (400, 99), (400, 200), (0, 0)] {
            assert_eq!(*image.get_pixel(x, y), WHITE, "outside at {x},{y}");
        }
    }

    #[test]
    fn a_highlight_at_the_edges_of_the_page_stays_on_the_picture() {
        let mut image = RgbImage::from_pixel(333, 77, WHITE);
        let whole = [0, 0, BOX_UNITS_PER_PAGE_SIDE, BOX_UNITS_PER_PAGE_SIDE];
        // The second box has no area, and a coordinate past the page.
        draw_highlights(&mut image, &[whole, [9_000, 5_000, u16::MAX, 5_000]]);

        let outline = Rgb(HIGHLIGHT_OUTLINE_RGB);
        assert_eq!(*image.get_pixel(0, 0), outline);
        assert_eq!(*image.get_pixel(332, 76), outline);
        assert_eq!(*image.get_pixel(160, 40), tinted(WHITE));
    }

    #[test]
    fn a_highlight_becomes_one_rectangle_a_text_line_on_its_page() {
        let boxes = sidecar(b"any");
        let rects = rects_by_page(&boxes, &page_two_highlight()).unwrap();
        assert_eq!(rects.keys().copied().collect::<Vec<_>>(), [2]);
        assert_eq!(rects[&2].len(), 2, "two text lines: {rects:?}");

        // A page the sidecar lacks, and a word past a page's last.
        for (page, end) in [(4, 1), (2, 4), (1, 1)] {
            let beyond = Highlight::new(vec![HighlightSpan {
                page,
                start: 0,
                end,
            }])
            .unwrap();
            assert!(
                rects_by_page(&boxes, &beyond).is_none(),
                "page {page}, words 0..{end}"
            );
        }
    }

    #[tokio::test]
    async fn the_picture_of_a_page_is_a_png_with_the_passage_boxed() {
        if without_pdfium("the_picture_of_a_page_is_a_png_with_the_passage_boxed") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, state) = service(tmp.path()).await;
        put_fixture(tmp.path(), &state, "A paper", true).await;

        let highlight = page_two_highlight();
        let response = get(
            &app,
            &link(&key, LinkFormat::PageImage, Some(2), Some(&highlight)),
        )
        .await;

        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(response.header("content-type"), Some(PNG));
        assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
        assert!(response
            .header("cache-control")
            .is_some_and(|value| value.starts_with("private, max-age=")));
        let image = image::load_from_memory(&response.body).unwrap().into_rgb8();
        assert_eq!(
            image.dimensions(),
            (PAGE_IMAGE_WIDTH_PX, PAGE_IMAGE_HEIGHT_PX)
        );
        // The middle of "accurate" (38 % across, 31 % down) is tinted; the
        // page's corner is the blank page.
        let (x, y) = (
            PAGE_IMAGE_WIDTH_PX * 38 / 100,
            PAGE_IMAGE_HEIGHT_PX * 31 / 100,
        );
        assert_eq!(*image.get_pixel(x, y), tinted(WHITE));
        assert_eq!(*image.get_pixel(10, 10), WHITE);

        // The same page without a highlight is the blank page there, and
        // another page of the same link's highlight is too.
        for uri in [
            link(&key, LinkFormat::PageImage, Some(2), None),
            link(&key, LinkFormat::PageImage, Some(3), Some(&highlight)),
        ] {
            let response = get(&app, &uri).await;
            assert_eq!(response.status, 200, "{uri}");
            let image = image::load_from_memory(&response.body).unwrap().into_rgb8();
            assert_eq!(*image.get_pixel(x, y), WHITE, "{uri}");
        }
    }

    #[tokio::test]
    async fn a_page_the_document_does_not_have_is_404() {
        if without_pdfium("a_page_the_document_does_not_have_is_404") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, state) = service(tmp.path()).await;
        put_fixture(tmp.path(), &state, "A paper", true).await;

        for format in [LinkFormat::PageImage, LinkFormat::Page] {
            let beyond = get(&app, &link(&key, format, Some(PAGES as u32 + 1), None)).await;
            assert_eq!(beyond.status, 404, "{format}: {beyond:?}");
            let last = get(&app, &link(&key, format, Some(PAGES as u32), None)).await;
            assert_eq!(last.status, 200, "{format}: {last:?}");
        }
    }

    #[tokio::test]
    async fn a_highlight_the_sidecar_cannot_answer_is_404() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, state) = service(tmp.path()).await;
        put_fixture(tmp.path(), &state, "A paper", true).await;
        let beyond = Highlight::new(vec![HighlightSpan {
            page: 2,
            start: 1,
            end: 9,
        }])
        .unwrap();

        for format in [LinkFormat::PageImage, LinkFormat::Page, LinkFormat::Viewer] {
            let response = get(&app, &link(&key, format, Some(2), Some(&beyond))).await;
            assert_eq!(response.status, 404, "{format}: {response:?}");
        }

        // The same for a document that has no sidecar at all.
        let bare = tempfile::tempdir().unwrap();
        let (app, key, state) = service(bare.path()).await;
        put_fixture(bare.path(), &state, "A paper", false).await;
        for format in [LinkFormat::PageImage, LinkFormat::Page, LinkFormat::Viewer] {
            let uri = link(&key, format, Some(2), Some(&page_two_highlight()));
            let response = get(&app, &uri).await;
            assert_eq!(response.status, 404, "{format}: {response:?}");
        }
        // Without a highlight nothing needs the sidecar.
        let plain = get(&app, &link(&key, LinkFormat::Viewer, Some(2), None)).await;
        assert_eq!(plain.status, 200, "{plain:?}");
    }

    #[tokio::test]
    async fn a_link_opens_the_view_it_was_signed_for_and_no_other() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, state) = service(tmp.path()).await;
        put_fixture(tmp.path(), &state, "A paper", true).await;

        let viewer = link(&key, LinkFormat::Viewer, Some(2), None);
        for route in ["page", "page.png", "source", "markdown"] {
            // The same signed query, and the query with `fmt` rewritten.
            for uri in [
                viewer.replace("/viewer?", &format!("/{route}?")),
                viewer
                    .replace("/viewer?", &format!("/{route}?"))
                    .replace("fmt=viewer", &format!("fmt={route}")),
            ] {
                let response = get(&app, &uri).await;
                assert_eq!(response.status, 403, "{uri}: {response:?}");
            }
        }
        // Another page than the one that was signed.
        let other_page = get(&app, &viewer.replace("page=2", "page=3")).await;
        assert_eq!(other_page.status, 403, "{other_page:?}");
    }

    #[tokio::test]
    async fn a_document_that_is_not_a_pdf_has_no_page_views() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, state) = service(tmp.path()).await;
        let (_, layout) = state.stores.documents(STORE).unwrap();
        LocalFsStorage::new(tmp.path())
            .put(
                &layout.source_key(STEM, "html"),
                b"<html><body>a web page</body></html>".to_vec(),
            )
            .await
            .unwrap();

        for format in [LinkFormat::PageImage, LinkFormat::Page, LinkFormat::Viewer] {
            let response = get(&app, &link(&key, format, Some(1), None)).await;
            assert_eq!(response.status, 404, "{format}: {response:?}");
        }
        // The document itself is still served.
        let source = get(&app, &link(&key, LinkFormat::Source, None, None)).await;
        assert_eq!(source.status, 200, "{source:?}");
    }

    #[tokio::test]
    async fn the_page_view_names_its_place_and_every_link_on_it_works() {
        if without_pdfium("the_page_view_names_its_place_and_every_link_on_it_works") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, state) = service(tmp.path()).await;
        put_fixture(tmp.path(), &state, "Strands <of> hair & light", true).await;
        let highlight = page_two_highlight();

        let response = get(
            &app,
            &link(&key, LinkFormat::Page, Some(2), Some(&highlight)),
        )
        .await;

        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(response.header("content-type"), Some(HTML));
        assert_eq!(
            response.header("content-security-policy"),
            Some(VIEW_CONTENT_SECURITY_POLICY)
        );
        let html = text(&response);
        assert!(html.contains("Page 2 of 3"), "{html}");
        assert!(
            html.contains("<h1>Strands &lt;of&gt; hair &amp; light</h1>"),
            "{html}"
        );
        for needed in [
            "Previous page",
            "Next page",
            "Open in viewer",
            "Open original PDF",
            "Converted text",
        ] {
            assert!(html.contains(needed), "{needed} in {html}");
        }

        let links = links_in(html);
        assert_eq!(links.len(), 7, "{links:?}");
        for uri in &links {
            let followed = get(&app, uri).await;
            assert_eq!(followed.status, 200, "{uri}: {followed:?}");
        }
        // The picture is this page's, with the highlight the link carries.
        let picture = link_to(html, "page.png");
        assert!(
            picture.contains("page=2") && picture.contains("hl="),
            "{picture}"
        );
        assert_eq!(get(&app, &picture).await.header("content-type"), Some(PNG));
        // The neighbours are the pages either side.
        let pages: Vec<_> = links
            .iter()
            .filter(|link| link.contains("/page?"))
            .collect();
        assert!(
            pages[0].contains("page=1") && pages[1].contains("page=3"),
            "{pages:?}"
        );
        // The PDF opens at the page, and is the document, not a view of it.
        let source = link_to(html, "source");
        assert!(source.ends_with("#page=2"), "{source}");
        assert!(!source.contains("hl="), "{source}");
    }

    #[tokio::test]
    async fn the_first_and_the_last_page_link_to_the_one_neighbour_they_have() {
        if without_pdfium("the_first_and_the_last_page_link_to_the_one_neighbour_they_have") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, state) = service(tmp.path()).await;
        put_fixture(tmp.path(), &state, "A paper", true).await;

        let first = get(&app, &link(&key, LinkFormat::Page, Some(1), None)).await;
        let html = text(&first);
        assert!(!html.contains("Previous page"), "{html}");
        assert!(link_to(html, "page").contains("page=2"), "{html}");

        let last = get(&app, &link(&key, LinkFormat::Page, Some(3), None)).await;
        let html = text(&last);
        assert!(!html.contains("Next page"), "{html}");
        assert!(link_to(html, "page").contains("page=2"), "{html}");
    }

    #[tokio::test]
    async fn the_viewer_is_told_the_source_and_the_rectangles_to_box() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, state) = service(tmp.path()).await;
        // A title that would run as a script of its own if it were written
        // into the page as it is.
        put_fixture(
            tmp.path(),
            &state,
            "</script><script>alert(1)</script>",
            true,
        )
        .await;
        let highlight = page_two_highlight();

        let response = get(
            &app,
            &link(&key, LinkFormat::Viewer, None, Some(&highlight)),
        )
        .await;

        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(
            response.header("content-security-policy"),
            Some(VIEW_CONTENT_SECURITY_POLICY)
        );
        let html = text(&response);
        assert_eq!(html.matches("<script").count(), 2, "{html}");
        assert!(!html.contains("alert(1)</script>"), "{html}");

        let opening = format!("<script type=\"application/json\" id=\"{VIEWER_DATA_ID}\">");
        let block = html.split(&opening).nth(1).unwrap();
        let block = &block[..block.find("</script>").unwrap()];
        let data: serde_json::Value = serde_json::from_str(block).unwrap();
        assert_eq!(data["units"], 10_000);
        assert_eq!(data["page"], serde_json::Value::Null);
        assert_eq!(
            data["highlights"],
            serde_json::json!([{
                "page": 2,
                "rects": [[1_980, 2_980, 4_520, 3_220], [1_980, 3_280, 3_220, 3_520]],
            }])
        );
        // The source is a link this service answers with the PDF.
        let source = get(&app, data["source"].as_str().unwrap()).await;
        assert_eq!(source.status, 200, "{source:?}");
        assert_eq!(source.header("content-type"), Some("application/pdf"));
        assert_eq!(source.body, blank_pdf());

        // The page view it offers is the highlighted page's; the script
        // and the stylesheet are this build's assets.
        assert!(html.contains("Page 2 as an image"), "{html}");
        for uri in links_in(html) {
            if uri.starts_with(ASSET_ROUTE_PREFIX) {
                assert_eq!(get(&app, &uri).await.status, 200, "{uri}");
            }
        }
        assert!(
            links_in(html).contains(&asset_path(VIEWER_SCRIPT)),
            "{html}"
        );
    }

    #[test]
    fn json_in_a_script_block_cannot_close_the_block() {
        let json = serde_json::to_string("</script><!-- a < b").unwrap();
        let safe = json_for_script(&json);
        assert!(!safe.contains('<'), "{safe}");
        assert_eq!(
            serde_json::from_str::<String>(&safe).unwrap(),
            "</script><!-- a < b"
        );
    }

    #[tokio::test]
    async fn an_asset_is_served_for_this_builds_version_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _key, _state) = service(tmp.path()).await;

        for asset in &ASSETS {
            let response = get(&app, &asset_path(asset.name)).await;
            assert_eq!(response.status, 200, "{}", asset.name);
            assert_eq!(response.header("content-type"), Some(asset.content_type));
            assert_eq!(response.header("cache-control"), Some(ASSET_CACHE_CONTROL));
            assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
            assert_eq!(response.body, asset.bytes, "{}", asset.name);
        }

        let version = asset_version();
        assert_eq!(version.len(), ASSET_VERSION_BYTES * 2);
        for uri in [
            format!("{ASSET_ROUTE_PREFIX}/0000000000000000/{VIEWER_SCRIPT}"),
            format!("{ASSET_ROUTE_PREFIX}/{version}/viewer.map"),
            format!("{ASSET_ROUTE_PREFIX}/{version}/..%2Fdocs_views.rs"),
        ] {
            assert_eq!(get(&app, &uri).await.status, 404, "{uri}");
        }
    }

    /// The viewer's script and stylesheet name things this module also
    /// names: the data block's id, the two pdf.js files, and the
    /// highlight's colours.
    #[test]
    fn the_assets_agree_with_the_service_on_what_they_share() {
        let asset = |name: &str| {
            let asset = ASSETS.iter().find(|asset| asset.name == name).unwrap();
            std::str::from_utf8(asset.bytes).unwrap()
        };
        let script = asset(VIEWER_SCRIPT);
        assert!(script.contains(&format!("getElementById(\"{VIEWER_DATA_ID}\")")));
        for imported in ["./pdf.min.mjs", "./pdf.worker.min.mjs"] {
            assert!(script.contains(&format!("\"{imported}\"")), "{imported}");
            assert!(ASSETS.iter().any(|asset| imported.ends_with(asset.name)));
        }
        let css = asset(STYLESHEET);
        let [red, green, blue, _] = HIGHLIGHT_FILL_RGBA;
        assert!(
            css.contains(&format!("rgba({red}, {green}, {blue}, ")),
            "{css}"
        );
        let [red, green, blue] = HIGHLIGHT_OUTLINE_RGB;
        assert!(
            css.contains(&format!("rgb({red}, {green}, {blue})")),
            "{css}"
        );
    }
}
