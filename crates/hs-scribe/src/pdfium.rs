//! The one binding to libpdfium, and the PDF reads that need no raster:
//! opening a document and counting its pages. Rendering (`render_page`)
//! lives in `pipeline::pdf_parser` and needs the `server` feature.
//!
//! pdfium is the ONE PDF parser of the workspace — the renderer, and the
//! page counter every dispatcher consults before it sends a document
//! anywhere. A pure-Rust second parser used to count pages; it recursed once
//! per nesting level of a hostile object and its stack overflow aborted the
//! whole process, and any heuristic that tried to predict what it would
//! parse could be walked around. pdfium carries a recursion limit of its own
//! and is fuzzed continuously. libpdfium is therefore a hard requirement of
//! every host that runs the scribe server or a watcher: [`require`] is the
//! start-up check.
//!
//! With the `thread_safe` feature a [`Pdfium`] holds a process-wide lock for
//! its whole lifetime: two instances never run at once, and a second
//! `PdfParser::new()` waits for the first to drop. Hold a parser only on a
//! blocking thread, and never create a second one on a thread that already
//! holds one.

use crate::classify::{ConvertFailure, FailureCode};
use anyhow::Result;
use pdfium_render::prelude::*;
use std::path::{Path, PathBuf};

/// Most pages a document may have. The renderer addresses pages with a
/// `u16` (and `PdfPages::len()` is a `u16` cast of pdfium's page count), so
/// a larger document cannot be walked page by page — it is refused, not
/// counted modulo 65 536 and converted in part.
pub const MAX_DOCUMENT_PAGES: usize = u16::MAX as usize;

/// Closes the raw document handle when the count is done, however it ends.
struct CloseOnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for CloseOnDrop<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}

/// Load a document through the raw bindings (`$load` yields the handle), ask
/// pdfium for its page count and close it. The raw call is the point:
/// `PdfDocument::pages().len()` is a `u16` cast that turns 65 600 pages into
/// 64, and a count that lies is worse than none. A macro because the handle
/// type is not nameable outside pdfium-render.
macro_rules! count_raw {
    ($bindings:expr, $what:expr, $load:expr) => {{
        let document = $load;
        if document.is_null() {
            let code = $bindings.FPDF_GetLastError();
            return Err(
                if code == PdfiumInternalError::FileError as std::os::raw::c_ulong {
                    anyhow::anyhow!("pdfium could not open {}", $what)
                } else {
                    ConvertFailure::err(
                        FailureCode::PdfParseError,
                        format!("PDF cannot be opened (pdfium error {code})"),
                    )
                },
            );
        }
        let _close = CloseOnDrop(|| $bindings.FPDF_CloseDocument(document));
        let count = $bindings.FPDF_GetPageCount(document);
        if count < 1 {
            return Err(ConvertFailure::err(
                FailureCode::PdfParseError,
                "PDF has no pages",
            ));
        }
        if count as usize > MAX_DOCUMENT_PAGES {
            return Err(ConvertFailure::err(
                FailureCode::PdfParseError,
                format!(
                    "PDF has {count} pages, more than the {MAX_DOCUMENT_PAGES} that can be indexed"
                ),
            ));
        }
        Ok(count as u32)
    }};
}

pub struct PdfParser {
    pdfium: Pdfium,
}

/// Where libpdfium may live, in lookup order: beside the working directory,
/// then the deployment drop directories (`hs_common::service::lib_bootstrap`),
/// then the system search path (tried last by [`PdfParser::new`]).
fn library_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![Pdfium::pdfium_platform_library_name_at_path("./")];
    candidates.extend(
        hs_common::service::lib_bootstrap::pdfium_drop_dirs()
            .iter()
            .map(Pdfium::pdfium_platform_library_name_at_path),
    );
    candidates
}

impl PdfParser {
    /// Bind to libpdfium: the first library found among the candidates of
    /// [`library_candidates`], else the system's. A missing library is an
    /// error, never a panic and never a substitute parser.
    pub fn new() -> Result<Self> {
        let mut bound = None;
        for candidate in library_candidates() {
            match Pdfium::bind_to_library(&candidate) {
                Ok(bindings) => {
                    bound = Some(bindings);
                    break;
                }
                Err(PdfiumError::LoadLibraryError(_)) => continue,
                Err(e) => anyhow::bail!(
                    "libpdfium at {} could not be bound: {e:?}",
                    candidate.display()
                ),
            }
        }
        let bindings = match bound {
            Some(bindings) => bindings,
            None => Pdfium::bind_to_system_library().map_err(|e| {
                anyhow::anyhow!(
                    "libpdfium could not be loaded (searched ./, {}, and the system library path): {e:?}",
                    hs_common::service::lib_bootstrap::pdfium_drop_dirs()
                        .iter()
                        .map(|d| d.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?,
        };
        Ok(Self {
            pdfium: Pdfium::new(bindings),
        })
    }

    /// Open a document from disk, borrowing this parser. The returned
    /// document must not outlive the parser (pdfium owns the library binding
    /// it borrows). Callers hold both on one blocking thread and render
    /// pages lazily.
    pub fn open<'a>(&'a self, path: impl AsRef<Path>) -> Result<PdfDocument<'a>> {
        let path = path.as_ref();
        self.pdfium
            .load_pdf_from_file(path, None)
            .map_err(|e| document_error(e, &path.display().to_string()))
    }

    /// Page count of the document at `path`, without rasterizing anything —
    /// a cheap metadata read. See [`Self::count_pages_in_bytes`].
    pub fn page_count(&self, path: &str) -> Result<usize> {
        self.count_pages_in_file(Path::new(path))
            .map(|n| n as usize)
    }

    /// Pages of the PDF at `path`, `1..=`[`MAX_DOCUMENT_PAGES`]: pdfium
    /// reads only the parts of the file it needs. A file pdfium cannot open
    /// is the host's fault (untyped error); a file it opens and cannot parse
    /// is a typed [`FailureCode::PdfParseError`].
    pub fn count_pages_in_file(&self, path: &Path) -> Result<u32> {
        let path = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("{} is not a UTF-8 path", path.display()))?;
        let bindings = self.pdfium.bindings();
        count_raw!(bindings, path, bindings.FPDF_LoadDocument(path, None))
    }

    /// Pages of the PDF in `bytes`, read in place without a copy. Same
    /// contract as [`Self::count_pages_in_file`]; a document that does not
    /// parse is always a verdict on the bytes.
    pub fn count_pages_in_bytes(&self, bytes: &[u8]) -> Result<u32> {
        let bindings = self.pdfium.bindings();
        count_raw!(
            bindings,
            "the in-memory PDF",
            bindings.FPDF_LoadMemDocument64(bytes, None)
        )
    }
}

/// Start-up check of every host that runs the scribe server or a watcher:
/// libpdfium must bind. The error says where it was looked for.
pub fn require() -> Result<()> {
    PdfParser::new().map(drop)
}

/// `true` when pdfium is saying "this document is broken / locked", as
/// opposed to "I could not open the file" (a server fault).
fn is_document_fault(e: &PdfiumError) -> bool {
    matches!(
        e,
        PdfiumError::PdfiumLibraryInternalError(
            PdfiumInternalError::Unknown
                | PdfiumInternalError::FormatError
                | PdfiumInternalError::PasswordError
                | PdfiumInternalError::SecurityError
                | PdfiumInternalError::PageError
        )
    )
}

fn document_error(e: PdfiumError, what: &str) -> anyhow::Error {
    if is_document_fault(&e) {
        ConvertFailure::err(
            FailureCode::PdfParseError,
            format!("PDF cannot be opened: {e:?}"),
        )
    } else {
        anyhow::anyhow!("opening {what}: {e:?}")
    }
}

/// A page that cannot be loaded: a document fault where pdfium says so.
#[cfg(feature = "server")]
pub(crate) fn page_error(e: PdfiumError, idx: u16) -> anyhow::Error {
    if is_document_fault(&e) {
        ConvertFailure::err(
            FailureCode::PdfParseError,
            format!("page {idx} cannot be loaded: {e:?}"),
        )
    } else {
        anyhow::anyhow!("loading page {idx}: {e:?}")
    }
}
