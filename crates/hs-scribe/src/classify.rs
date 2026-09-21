//! Single source of truth for convert-failure classification.
//!
//! The watch-events chain needs two decisions from one error:
//!
//! 1. `event_watch::convert_and_upload` — is this worth NAK-retrying on
//!    the same backend (`Transient`), or is retrying the same backend
//!    pointless (`Permanent` / `Escalate`)?
//! 2. `hs`'s chain dispatcher — for non-transient failures, should the
//!    chain stop and stamp (`Permanent`), or hand the document to the
//!    next backend (`Escalate`)?
//!
//! These used to be two independent substring tables that had to agree
//! and didn't: "olmocr reported 0 completed pages" was Escalate in one
//! and (by omission) Transient in the other, so the Escalate arm was
//! dead at runtime and poison PDFs NAK-redelivered through the full
//! `max_deliver` budget. Both layers now consult this one table.

/// Three-way verdict for a convert failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The document itself can never convert (broken PDF, paywall stub,
    /// wrong content type). Stop the chain, stamp `conversion_failed`
    /// with the reason token.
    Permanent(&'static str),
    /// This backend can't handle the document, but a different backend
    /// might (VLM repetition loop, olmocr output validation rejecting
    /// every page). Try the next backend in the chain.
    Escalate(&'static str),
    /// Cluster-state problem (network, backend down, unknown error).
    /// NAK and let JetStream redeliver.
    Transient,
}

/// Classify a convert-failure message. Matches on substrings because the
/// error crosses an HTTP boundary as formatted text (`{e:#}`) — the
/// scribe server's typed error is gone by the time the client sees it.
pub fn classify_failure(msg: &str) -> FailureClass {
    use FailureClass::*;
    if msg.contains("unsupported_content_type:html") {
        // HTTP 415 from the /scribe gate (server.rs::verify_pdf_content):
        // the object is HTML, no VLM will change that.
        Permanent("unsupported_content_type:html")
    } else if msg.contains("unsupported_content_type:binary") {
        Permanent("unsupported_content_type:binary")
    } else if msg.contains("paywall") {
        Permanent("paywall_html")
    } else if msg.contains("FormatError")
        || msg.contains("Invalid image size")
        || msg.contains("PdfiumLibrary")
    {
        // The PDF itself is structurally broken; every renderer-backed
        // backend fails identically.
        Permanent("pdf_parse_error")
    } else if msg.contains("EPUB parse failed") {
        Permanent("epub_parse_error")
    } else if msg.contains("not valid UTF-8") {
        Permanent("html_not_utf8")
    } else if msg.contains("unsupported source type") {
        Permanent("unsupported_extension")
    } else if msg.contains("source bytes missing") {
        // storage.get returned NotFound — the object doesn't exist, so
        // escalating would re-GET the same absent key on every backend
        // and the exhaustion stamp would clobber the true reason with
        // the generic token. Stop the chain at the first backend.
        Permanent("source_missing")
    } else if msg.contains("has no extension") {
        // Event key carries no extension; no backend can pick a parser.
        Permanent("missing_extension")
    } else if msg.contains("VLM repetition loop") {
        // Covers both the server's streaming-abort message ("VLM
        // repetition loop detected") and the client-side QC reject
        // ("VLM repetition loop on <stem> ..."). A different VLM
        // produces different output for the same page, so escalate.
        Escalate("vlm_repetition_loop")
    } else if msg.contains("connection closed before message completed") {
        // llama-server evicting a slot mid-stream after its own
        // repetition guard fires — same VLM-class failure family.
        Escalate("vlm_transport_error")
    } else if msg.contains("olmocr reported 0 completed pages") {
        // olmocr ran but produced nothing (output validation rejected
        // every page, or its renderer couldn't open the PDF). Not proof
        // the PDF is broken — observed on a clean-text-layer book. A
        // genuinely broken PDF fails fast on the next backend with a
        // real FormatError.
        Escalate("olmocr_zero_pages")
    } else {
        Transient
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_failure, FailureClass};

    #[test]
    fn olmocr_zero_pages_escalates() {
        // The exact string olmocr_subprocess returns, wrapped the way the
        // server/client round-trip presents it.
        let msg = "Server error: olmocr reported 0 completed pages (failed=0); \
                   content may need a different backend";
        assert_eq!(
            classify_failure(msg),
            FailureClass::Escalate("olmocr_zero_pages")
        );
    }

    #[test]
    fn repetition_loop_escalates_for_both_message_shapes() {
        // Server streaming-abort shape.
        assert_eq!(
            classify_failure("VLM repetition loop detected on page 41"),
            FailureClass::Escalate("vlm_repetition_loop")
        );
        // Client QC reject shape.
        assert_eq!(
            classify_failure("VLM repetition loop on beck_tdd (truncations=40)"),
            FailureClass::Escalate("vlm_repetition_loop")
        );
    }

    #[test]
    fn unknown_errors_are_transient() {
        assert_eq!(
            classify_failure("error sending request for url (http://big:7435/scribe)"),
            FailureClass::Transient
        );
    }

    #[test]
    fn source_missing_is_permanent_not_escalate() {
        // A missing source object must short-circuit the chain — every
        // backend GETs the same storage, so escalation just re-fails
        // N-1 more times and the exhaustion stamp replaces the true
        // reason with the generic token.
        assert_eq!(
            classify_failure("source bytes missing for papers/mc/code.pdf"),
            FailureClass::Permanent("source_missing")
        );
    }

    #[test]
    fn missing_extension_is_permanent() {
        assert_eq!(
            classify_failure("key papers/mc/noext has no extension"),
            FailureClass::Permanent("missing_extension")
        );
    }

    #[test]
    fn broken_pdf_is_permanent() {
        assert_eq!(
            classify_failure("FormatError: cross-reference table is broken"),
            FailureClass::Permanent("pdf_parse_error")
        );
    }
}
