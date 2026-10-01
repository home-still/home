//! Client-side PDF validation and metadata.
//!
//! Used by the subscriber to size per-request convert timeouts against
//! PDF page count, so a 500-page book gets a generous deadline while
//! small papers don't sit idle on the floor — and, because the same
//! parse runs before anything is dispatched, to refuse a PDF that no
//! backend can convert at the door instead of after an upload and a
//! deadline's worth of waiting. Depends on `lopdf` (pure Rust).
//!
//! There is no "unknown page count": every outcome is either a count or
//! a typed [`ConvertFailure`]. lopdf parses the whole file, including
//! untrusted bytes up to [`MAX_PDF_BYTES`], so the parse runs under
//! `catch_unwind` and a panic is reported as a failure of the document.
//! `catch_unwind` only helps under `panic = "unwind"`; with the release
//! profile's `panic = "abort"` a lopdf panic still ends the process.

use crate::classify::{ConvertFailure, FailureCode};

/// Largest PDF accepted anywhere in the pipeline: the scribe server's
/// upload limit, and the bound applied before lopdf sees the bytes.
pub const MAX_PDF_BYTES: usize = 256 * 1024 * 1024;

/// Pages the renderer can address (pdfium page indices are `u16`).
pub const MAX_PDF_PAGES: usize = u16::MAX as usize + 1;

/// Bytes inspected to tell what a non-PDF payload is.
pub const HEADER_PROBE_BYTES: usize = 4096;

/// Accept only bodies that start with `%PDF` (the header `PDF-1.x`
/// specifies exactly). Anything else will never convert — the VLM would
/// spend GPU time producing garbage — so say what it is instead:
/// paywall HTML renamed `.pdf` and truncated or binary downloads get
/// different codes.
pub fn check_header(head: &[u8]) -> Result<(), ConvertFailure> {
    let head = &head[..head.len().min(HEADER_PROBE_BYTES)];
    if head.starts_with(b"%PDF") {
        return Ok(());
    }
    Err(if hs_common::html::looks_like_html(head) {
        ConvertFailure::new(
            FailureCode::UnsupportedContentTypeHtml,
            "the object is HTML, not a PDF",
        )
    } else {
        ConvertFailure::new(
            FailureCode::UnsupportedContentTypeBinary,
            "the object does not start with %PDF",
        )
    })
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// Number of pages in `bytes`, or why the PDF cannot be converted.
pub fn count_pages(bytes: &[u8]) -> Result<u32, ConvertFailure> {
    parse_page_count(bytes, |b| {
        lopdf::Document::load_mem(b)
            .map(|doc| doc.get_pages().len())
            .map_err(|e| e.to_string())
    })
}

/// [`count_pages`] for a PDF on disk (blocking: reads the file). A file the
/// server cannot read is the server's fault and comes back as a plain
/// error; one it can read but cannot count is a typed [`ConvertFailure`].
pub fn count_pages_in_file(path: &std::path::Path) -> anyhow::Result<u32> {
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("reading {} to count its pages: {e}", path.display()))?;
    count_pages(&bytes).map_err(anyhow::Error::new)
}

/// Deepest `[`/`<<` nesting accepted before lopdf sees the bytes. lopdf's
/// parser recurses once per level and overflows its thread's stack on a
/// ~2 KB file nested ~1000 deep — a stack overflow aborts the process and no
/// `catch_unwind` can stop it. Real PDFs nest a few levels.
pub const MAX_NESTING_DEPTH: usize = 128;

/// Stack for the parse thread: ample for [`MAX_NESTING_DEPTH`] levels of
/// lopdf recursion even with large frames.
const PARSE_STACK_BYTES: usize = 16 * 1024 * 1024;

/// Greatest array/dictionary nesting in the raw bytes, skipping comments,
/// literal and hex strings and stream bodies. Linear, no allocation. It
/// cannot see inside compressed object streams (see the module docs of
/// the report: that residual needs process isolation).
pub fn max_nesting_depth(bytes: &[u8]) -> usize {
    let (mut depth, mut max, mut i) = (0usize, 0usize, 0usize);
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                    i += 1;
                }
            }
            b'(' => {
                let mut open = 1usize;
                i += 1;
                while i < bytes.len() && open > 0 {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'(' => open += 1,
                        b')' => open -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                continue;
            }
            b'[' => {
                depth += 1;
                max = max.max(depth);
            }
            b']' => depth = depth.saturating_sub(1),
            b'<' if bytes.get(i + 1) == Some(&b'<') => {
                depth += 1;
                max = max.max(depth);
                i += 1;
            }
            b'<' => {
                while i < bytes.len() && bytes[i] != b'>' {
                    i += 1;
                }
            }
            b'>' if bytes.get(i + 1) == Some(&b'>') => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b's' if bytes[i..].starts_with(b"stream")
                && i > 0
                && (bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b'>') =>
            {
                // Skip the stream body: arbitrary binary, not PDF syntax.
                let body = &bytes[i + 6..];
                match body.windows(9).position(|w| w == b"endstream") {
                    Some(end) => i += 6 + end + 9,
                    None => break,
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    max
}

/// The validation around a page-count parser. `parse` is lopdf in
/// production; it is a parameter so the panic path can be exercised. It
/// runs on a scoped thread with a large stack: a panic there comes back as
/// an `Err` from `join` (under `panic = "unwind"`).
fn parse_page_count(
    bytes: &[u8],
    parse: impl FnOnce(&[u8]) -> Result<usize, String> + Send,
) -> Result<u32, ConvertFailure> {
    check_header(bytes)?;
    if bytes.len() > MAX_PDF_BYTES {
        return Err(ConvertFailure::new(
            FailureCode::PdfParseError,
            format!(
                "PDF is {} bytes, over the {MAX_PDF_BYTES}-byte limit",
                bytes.len()
            ),
        ));
    }
    let depth = max_nesting_depth(bytes);
    if depth > MAX_NESTING_DEPTH {
        return Err(ConvertFailure::new(
            FailureCode::PdfParseError,
            format!("PDF objects nest {depth} levels deep (limit {MAX_NESTING_DEPTH})"),
        ));
    }
    let parsed = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("pdf-page-count".into())
            .stack_size(PARSE_STACK_BYTES)
            .spawn_scoped(scope, || parse(bytes))
            .map(|handle| handle.join())
    });
    let count = match parsed {
        Ok(Ok(Ok(count))) => count,
        Ok(Ok(Err(e))) => {
            return Err(ConvertFailure::new(
                FailureCode::PdfParseError,
                format!("PDF structure is unreadable: {e}"),
            ))
        }
        Ok(Err(payload)) => {
            return Err(ConvertFailure::new(
                FailureCode::PdfParseError,
                format!("PDF parser panicked: {}", panic_text(payload.as_ref())),
            ))
        }
        Err(e) => {
            return Err(ConvertFailure::new(
                FailureCode::PdfParseError,
                format!("could not start the PDF parse thread: {e}"),
            ))
        }
    };
    if count == 0 || count > MAX_PDF_PAGES {
        return Err(ConvertFailure::new(
            FailureCode::PdfParseError,
            format!("PDF has {count} pages; convertible documents have 1..={MAX_PDF_PAGES}"),
        ));
    }
    Ok(count as u32)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lopdf::{dictionary, Document, Object};

    /// A well-formed PDF with `pages` empty pages, built and serialised by
    /// lopdf itself.
    pub(crate) fn pdf_with_pages(pages: usize) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let kids: Vec<Object> = (0..pages)
            .map(|_| {
                doc.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                })
                .into()
            })
            .collect();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => pages as i64,
            }),
        );
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).expect("serialise");
        out
    }

    fn code(r: Result<u32, ConvertFailure>) -> FailureCode {
        r.expect_err("must be refused").code()
    }

    #[test]
    fn counts_the_pages_of_a_valid_pdf() {
        assert_eq!(count_pages(&pdf_with_pages(1)).unwrap(), 1);
        assert_eq!(count_pages(&pdf_with_pages(7)).unwrap(), 7);
    }

    #[test]
    fn html_and_binary_bodies_are_named_for_what_they_are() {
        assert_eq!(
            code(count_pages(
                b"<!DOCTYPE html><html><body>please log in</body></html>"
            )),
            FailureCode::UnsupportedContentTypeHtml
        );
        assert_eq!(
            code(count_pages(&[0u8, 1, 2, 3, 4, 5, 6, 7])),
            FailureCode::UnsupportedContentTypeBinary
        );
        assert_eq!(
            code(count_pages(b"")),
            FailureCode::UnsupportedContentTypeBinary
        );
    }

    #[test]
    fn a_broken_pdf_is_a_typed_failure_not_an_unknown_page_count() {
        for bytes in [
            &b"%PDF-1.4\n"[..],
            b"%PDF-1.4\n%%EOF",
            b"%PDF-1.7\n1 0 obj << /Type /Catalog >> endobj\nstartxref\n99999\n%%EOF",
        ] {
            assert_eq!(code(count_pages(bytes)), FailureCode::PdfParseError);
        }
    }

    #[test]
    fn truncating_a_valid_pdf_anywhere_never_panics_or_succeeds_wrongly() {
        let good = pdf_with_pages(3);
        for cut in (0..good.len()).step_by(7) {
            // Either a correct count (a prefix can still be a whole PDF
            // when only trailing bytes are cut) or a typed refusal.
            match count_pages(&good[..cut]) {
                Ok(n) => assert_eq!(n, 3, "cut at {cut}"),
                Err(e) => assert!(
                    matches!(
                        e.code(),
                        FailureCode::PdfParseError
                            | FailureCode::UnsupportedContentTypeBinary
                            | FailureCode::UnsupportedContentTypeHtml
                    ),
                    "cut at {cut}: {e}"
                ),
            }
        }
    }

    #[test]
    fn corrupting_a_valid_pdf_never_escapes_as_a_panic() {
        let good = pdf_with_pages(4);
        for i in (8..good.len()).step_by(5) {
            let mut bad = good.clone();
            bad[i] ^= 0xFF;
            // The assertion is "returns at all": a panic would fail the
            // test, a hang would time it out.
            let _ = count_pages(&bad);
        }
    }

    #[test]
    fn a_pdf_with_no_pages_is_refused() {
        assert_eq!(
            code(count_pages(&pdf_with_pages(0))),
            FailureCode::PdfParseError
        );
    }

    #[test]
    fn a_panicking_parser_is_a_failure_of_the_document_not_of_the_process() {
        let good = pdf_with_pages(1);
        let err = parse_page_count(&good, |_| panic!("index out of bounds in xref")).unwrap_err();
        assert_eq!(err.code(), FailureCode::PdfParseError);
        assert!(err.to_string().contains("panicked"), "{err}");
    }

    #[test]
    fn page_counts_outside_what_the_renderer_can_address_are_refused() {
        let good = pdf_with_pages(1);
        assert_eq!(
            parse_page_count(&good, |_| Ok(MAX_PDF_PAGES)).unwrap(),
            MAX_PDF_PAGES as u32
        );
        for n in [0, MAX_PDF_PAGES + 1, usize::MAX] {
            let err = parse_page_count(&good, |_| Ok(n)).unwrap_err();
            assert_eq!(err.code(), FailureCode::PdfParseError, "{n}");
        }
    }

    #[test]
    fn the_parser_is_not_called_for_bodies_that_fail_the_header_check() {
        let err = parse_page_count(b"<html>", |_| panic!("must not parse HTML")).unwrap_err();
        assert_eq!(err.code(), FailureCode::UnsupportedContentTypeHtml);
    }

    fn pdf_with_nested_page_entry(depth: usize) -> Vec<u8> {
        let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /X {nested} >>"),
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
        out
    }

    #[test]
    fn deeply_nested_objects_are_refused_before_lopdf_can_overflow_its_stack() {
        // lopdf 0.33 aborts the process (stack overflow) on this input at
        // depth ~1000; the preflight must refuse it without parsing.
        for depth in [MAX_NESTING_DEPTH + 1, 1_000, 100_000] {
            let err = count_pages(&pdf_with_nested_page_entry(depth)).unwrap_err();
            assert_eq!(err.code(), FailureCode::PdfParseError, "depth {depth}");
            assert!(err.to_string().contains("nest"), "{err}");
        }
    }

    #[test]
    fn ordinary_nesting_is_counted_and_accepted() {
        assert_eq!(count_pages(&pdf_with_nested_page_entry(10)).unwrap(), 1);
        assert_eq!(
            count_pages(&pdf_with_nested_page_entry(MAX_NESTING_DEPTH - 2)).unwrap(),
            1
        );
    }

    #[test]
    fn nesting_depth_ignores_strings_comments_and_stream_bodies() {
        assert_eq!(max_nesting_depth(b"<< /A [ [ 1 ] ] >>"), 3);
        assert_eq!(max_nesting_depth(b"(((([[[[ not syntax ))))) [1]"), 1);
        assert_eq!(max_nesting_depth(b"% [[[[[[[\n<< /A 1 >>"), 1);
        assert_eq!(max_nesting_depth(b"<5b5b5b5b> [1]"), 1);
        assert_eq!(
            max_nesting_depth(b"<< /Length 9 >>\nstream\n[[[[[[[[[\nendstream\n[1]"),
            1
        );
    }
}
