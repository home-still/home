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
//!
//! Consequence for the Legacy converter: stage 1 of a conversion (open, render
//! and layout-detect every page, `Processor::process_pdf_with_progress`) holds
//! that lock from the first page to the last, so stage 1 of two conversions
//! never overlaps. Stage 2 (the per-region VLM calls) runs outside the lock;
//! `vlm_concurrency` > 1 therefore lets one conversion's VLM work overlap the
//! next one's render. A pdfium call that does not return within its budget
//! (see [`guarded_call`]) wedges that lock for good, so the watchdog `_exit`s
//! the whole server, ending every conversion in flight on it.

use crate::classify::{ConvertFailure, FailureCode};
use anyhow::Result;
use pdfium_render::prelude::*;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

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

/// Exit status of a process that stopped because pdfium is unusable.
pub const FAULT_EXIT_CODE: i32 = 70;

static POISONED: AtomicBool = AtomicBool::new(false);
static DETECTOR_POISONED: AtomicBool = AtomicBool::new(false);
/// Start of the pdfium call now in flight on the render path, in ms since
/// [`epoch`]; 0 = none. pdfium serialises its callers, so at most one.
static CALL_STARTED_MS: AtomicU64 = AtomicU64::new(0);
static CALL_BUDGET_MS: AtomicU64 = AtomicU64::new(DEFAULT_CALL_BUDGET.as_millis() as u64);
static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);

/// How long one pdfium call on the render path (open a document, render a
/// page) may run before the process is declared wedged. A healthy call takes
/// well under a second; a page of a book at 200 dpi a few seconds.
pub const DEFAULT_CALL_BUDGET: Duration = Duration::from_secs(120);

fn epoch() -> std::time::Instant {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(std::time::Instant::now)
}

fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64 + 1
}

/// Override the per-call budget (tests).
pub fn set_call_budget(budget: Duration) {
    CALL_BUDGET_MS.store(budget.as_millis().max(1) as u64, Ordering::SeqCst);
}

/// Run one pdfium call of the render path under the watchdog: if it has not
/// returned within the call budget, health goes red and the process exits
/// after the grace period (see [`install_fault_exit`]). The page-count path
/// has its own budget in `pdf_meta::bounded`; this is the same accounting for
/// the Legacy converter, which holds pdfium's lock for a whole conversion.
pub fn guarded_call<T>(call: impl FnOnce() -> T) -> T {
    /// Ends the call's accounting however `call` ends: a panic that unwinds
    /// out of it must not leave the call "in flight", or the watchdog would
    /// declare the process wedged one budget later and exit it.
    struct Finished;
    impl Drop for Finished {
        fn drop(&mut self) {
            CALL_STARTED_MS.store(0, Ordering::SeqCst);
            clear_wedged();
        }
    }
    CALL_STARTED_MS.store(now_ms(), Ordering::SeqCst);
    let _finished = Finished;
    call()
}

/// Poll the in-flight call; spawned once by [`install_fault_exit`].
fn start_watchdog() {
    if WATCHDOG_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(250));
        let started = CALL_STARTED_MS.load(Ordering::SeqCst);
        let budget = CALL_BUDGET_MS.load(Ordering::SeqCst);
        if started != 0
            && !WEDGED.load(Ordering::SeqCst)
            && now_ms().saturating_sub(started) > budget
        {
            // The call may return between the check and the flag; re-check
            // after setting so a call that just finished never leaves the
            // flag raised.
            fault_wedged(Duration::from_millis(budget));
            if CALL_STARTED_MS.load(Ordering::SeqCst) != started {
                clear_wedged();
            }
        }
    });
}

/// A pooled ONNX detector's lock was poisoned by a panic: its session is in
/// an unknown state and every later page routed to it would fail.
#[cfg(feature = "server")]
pub(crate) fn fault_detector_poisoned() {
    DETECTOR_POISONED.store(true, Ordering::SeqCst);
    tracing::error!(
        "a layout/table detector lock is poisoned (a panic unwound through ONNX inference); \
         every later page routed to it would fail — the process must restart"
    );
    arm_exit();
}
static WEDGED: AtomicBool = AtomicBool::new(false);
/// Milliseconds a faulted process lingers (health red) before it exits; 0 =
/// no exit (library default; the server and the watcher install one).
static EXIT_GRACE_MS: AtomicU64 = AtomicU64::new(0);

/// Make a pdfium fault end the process: after `grace` (during which
/// `/health` is red) the process exits with [`FAULT_EXIT_CODE`] so its
/// supervisor restarts it. A native call stuck inside pdfium cannot be
/// cancelled or killed from within the process, and a poisoned pdfium lock
/// never recovers: exit is the only recovery.
pub fn install_fault_exit(grace: Duration) {
    EXIT_GRACE_MS.store(grace.as_millis().max(1) as u64, Ordering::SeqCst);
    start_watchdog();
}

/// `Err(reason)` while pdfium is poisoned or a call is stuck inside it.
pub fn healthy() -> Result<(), &'static str> {
    if DETECTOR_POISONED.load(Ordering::SeqCst) {
        Err("a layout/table detector lock is poisoned by a panic; restart required")
    } else if POISONED.load(Ordering::SeqCst) {
        Err("pdfium's process-wide lock is poisoned by a panic; restart required")
    } else if WEDGED.load(Ordering::SeqCst) {
        Err("a pdfium call has not returned within its budget")
    } else {
        Ok(())
    }
}

pub(crate) fn fault_poisoned() {
    POISONED.store(true, Ordering::SeqCst);
    tracing::error!(
        "pdfium's process-wide lock is poisoned (a panic unwound while a PdfParser was alive); \
         every later PDF would fail — the process must restart"
    );
    arm_exit();
}

pub(crate) fn fault_wedged(budget: Duration) {
    WEDGED.store(true, Ordering::SeqCst);
    tracing::error!(
        budget_secs = budget.as_secs(),
        "a pdfium call exceeded its budget and has not returned; it cannot be killed in-process \
         — the process restarts unless it returns within the grace period"
    );
    arm_exit();
}

/// The stuck call returned after all.
pub(crate) fn clear_wedged() {
    WEDGED.store(false, Ordering::SeqCst);
}

fn arm_exit() {
    let grace = EXIT_GRACE_MS.load(Ordering::SeqCst);
    if grace == 0 {
        return;
    }
    // Detached on purpose: it must outlive a wedged runtime.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(grace));
        if let Err(reason) = healthy() {
            eprintln!("FATAL: {reason}; exiting so the supervisor restarts this process");
            // `_exit`, not `exit`: a wedged thread may hold a lock that
            // libpdfium's static destructors (run by `exit`) would wait on
            // for ever, and the supervisor would never see the process end.
            #[cfg(unix)]
            unsafe {
                libc::_exit(FAULT_EXIT_CODE)
            }
            #[cfg(not(unix))]
            std::process::exit(FAULT_EXIT_CODE);
        }
    });
}

/// Run `f` with a parser, never letting a panic unwind through a live
/// `PdfParser`: pdfium-render keeps its process-wide lock in the `Pdfium`
/// instance, and dropping that instance *while unwinding* poisons the lock
/// for the life of the process. The panic is caught, the parser dropped, and
/// the unwind resumed afterwards. A panic out of `PdfParser::new` is that
/// poisoned lock itself: recorded, health goes red, the process exits.
pub fn with_parser<T>(f: impl FnOnce(&PdfParser) -> Result<T>) -> Result<T> {
    let parser = match catch_unwind(PdfParser::new) {
        Ok(created) => created?,
        Err(_) => {
            fault_poisoned();
            anyhow::bail!("pdfium's process-wide lock is poisoned; the process is restarting");
        }
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| f(&parser)));
    drop(parser);
    match outcome {
        Ok(result) => result,
        Err(payload) => resume_unwind(payload),
    }
}

pub struct PdfParser {
    pdfium: Pdfium,
}

/// Where libpdfium may live, in lookup order: the deployment drop
/// directories (`hs_common::service::lib_bootstrap`, all under the home
/// directory), then the system search path (tried last by
/// [`PdfParser::new`]). The working directory is deliberately not a
/// candidate: a command run from an untrusted directory must not load
/// whatever library sits there.
fn library_candidates() -> Vec<PathBuf> {
    hs_common::service::lib_bootstrap::pdfium_drop_dirs()
        .iter()
        .map(Pdfium::pdfium_platform_library_name_at_path)
        .collect()
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
                    "libpdfium could not be loaded (searched {}, and the system library path): {e:?}",
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
    with_parser(|_| Ok(()))
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

/// A page that failed to render. pdfium reports a document fault (format,
/// password, security, page) as a verdict on the bytes — permanent. A bare
/// `Unknown` is not: `render_with_config` raises it when pdfium hands back
/// a null bitmap, and the size was already bounded by `plan_render`, so it
/// is an allocation failure that a later attempt can survive. Every other
/// error stays untyped, hence transient.
#[cfg(feature = "server")]
pub(crate) fn render_error(e: PdfiumError, idx: u16) -> anyhow::Error {
    let bitmap_allocation_failed = matches!(
        e,
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::Unknown)
    );
    if is_document_fault(&e) && !bitmap_allocation_failed {
        ConvertFailure::err(
            FailureCode::PdfParseError,
            format!("page {idx} cannot be rendered: {e:?}"),
        )
    } else {
        anyhow::anyhow!("rendering page {idx}: {e:?}")
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::classify::{classify, failure_code, FailureClass};

    #[test]
    fn a_render_failure_pdfium_pins_on_the_document_is_permanent() {
        for internal in [
            PdfiumInternalError::FormatError,
            PdfiumInternalError::PasswordError,
            PdfiumInternalError::SecurityError,
            PdfiumInternalError::PageError,
        ] {
            let err = render_error(PdfiumError::PdfiumLibraryInternalError(internal), 3);
            assert_eq!(failure_code(&err), Some(FailureCode::PdfParseError));
            assert_eq!(classify(&err), FailureClass::Permanent("pdf_parse_error"));
        }
    }

    #[test]
    fn a_null_bitmap_and_other_render_errors_stay_transient() {
        for e in [
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::Unknown),
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::FileError),
            PdfiumError::UnknownBitmapFormat,
        ] {
            let err = render_error(e, 3);
            assert_eq!(failure_code(&err), None);
            assert_eq!(classify(&err), FailureClass::Transient);
        }
    }
}

#[cfg(test)]
mod library_search_tests {
    use super::*;

    /// RA-137: a relative candidate resolves against the working directory,
    /// so `hs scribe convert` run from an untrusted directory would load
    /// whatever library sits there.
    #[test]
    fn no_library_candidate_depends_on_the_working_directory() {
        for candidate in library_candidates() {
            assert!(candidate.is_absolute(), "{}", candidate.display());
        }
    }
}
