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
//! Both layers ask [`classify`], which reads a typed [`ConvertFailure`]
//! out of the error chain. A failure is typed where it is *produced* —
//! the scribe server names it with a [`FailureCode`] on the wire, the
//! client turns that code back into a `ConvertFailure`, and the event
//! handler constructs one for its own verdicts. Nothing is classified by
//! searching message text: event keys and stems end up in messages
//! (`scribe convert failed for {key}`), so a document whose stem
//! contains "paywall" used to be permanently failed by a substring match.
//! An error with no `ConvertFailure` in its chain is `Transient`: it is
//! cluster state (network, backend down), not a statement about the
//! document.

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

/// Machine-readable reason for a convert failure. The [`wire`](Self::wire)
/// token is the stable name used on the HTTP boundary between scribe
/// server and client and as the `conversion_failed` reason stamped in the
/// catalog, so renaming one is a wire and data change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureCode {
    /// HTTP 415 from the `%PDF` magic-byte gate: the object is HTML.
    UnsupportedContentTypeHtml,
    /// HTTP 415 from the `%PDF` magic-byte gate: the object is not a PDF.
    UnsupportedContentTypeBinary,
    /// HTML that is a paywall / loading stub.
    PaywallHtml,
    /// The PDF is structurally broken, encrypted, or has a page box or
    /// page count the renderer refuses; every renderer fails identically.
    PdfParseError,
    /// The EPUB archive cannot be opened, or exceeds the size caps.
    EpubParseError,
    HtmlNotUtf8,
    /// HTML the converter refuses to parse: elements nested deeper than
    /// `html::MAX_HTML_NESTING`, or a conversion that did not finish in time.
    HtmlParseError,
    UnsupportedExtension,
    /// The source object does not exist in storage.
    SourceMissing,
    /// The event key carries no extension; no parser can be picked.
    MissingExtension,
    /// The event key cannot name a document (escapes the storage root,
    /// empty or dot-segment stem).
    InvalidKey,
    /// A parser (HTML / EPUB) produced output below the indexable floor.
    /// Every tier runs the same parser, so this is content-intrinsic.
    EmptyConversion,
    /// A VLM produced output below the indexable floor. A different VLM
    /// may read a scan the first could not.
    EmptyVlmConversion,
    /// A VLM looped and the output was rejected by QC (or the streaming
    /// detector). A different VLM produces different output.
    VlmRepetitionLoop,
    /// The VLM stream died mid-response (evicted slot, reset, stall,
    /// premature EOF, backend-reported error event).
    VlmTransportError,
    /// The VLM stopped at its token limit instead of finishing.
    VlmOutputTruncated,
    /// olmocr ran but reported no completed page.
    OlmocrZeroPages,
    /// olmocr reported failed pages, or fewer completed pages than the
    /// source has.
    OlmocrIncompletePages,
    /// The server panicked while converting this document. A panic on the
    /// same bytes repeats, so it is never retried.
    ConversionPanicked,
    /// Part of the document could not be processed (regions the pipeline
    /// had to skip), so the markdown has holes.
    GappedConversion,
}

impl FailureCode {
    /// The stable wire / catalog token.
    pub fn wire(self) -> &'static str {
        match self {
            Self::UnsupportedContentTypeHtml => "unsupported_content_type:html",
            Self::UnsupportedContentTypeBinary => "unsupported_content_type:binary",
            Self::PaywallHtml => "paywall_html",
            Self::PdfParseError => "pdf_parse_error",
            Self::EpubParseError => "epub_parse_error",
            Self::HtmlNotUtf8 => "html_not_utf8",
            Self::HtmlParseError => "html_parse_error",
            Self::UnsupportedExtension => "unsupported_extension",
            Self::SourceMissing => "source_missing",
            Self::MissingExtension => "missing_extension",
            Self::InvalidKey => "invalid_key",
            Self::EmptyConversion => "empty_conversion",
            Self::EmptyVlmConversion => "empty_vlm_conversion",
            Self::VlmRepetitionLoop => "vlm_repetition_loop",
            Self::VlmTransportError => "vlm_transport_error",
            Self::VlmOutputTruncated => "vlm_output_truncated",
            Self::OlmocrZeroPages => "olmocr_zero_pages",
            Self::OlmocrIncompletePages => "olmocr_incomplete_pages",
            Self::GappedConversion => "gapped_conversion",
            Self::ConversionPanicked => "conversion_panicked",
        }
    }

    const ALL: [FailureCode; 20] = [
        Self::UnsupportedContentTypeHtml,
        Self::UnsupportedContentTypeBinary,
        Self::PaywallHtml,
        Self::PdfParseError,
        Self::EpubParseError,
        Self::HtmlNotUtf8,
        Self::HtmlParseError,
        Self::UnsupportedExtension,
        Self::SourceMissing,
        Self::MissingExtension,
        Self::InvalidKey,
        Self::EmptyConversion,
        Self::EmptyVlmConversion,
        Self::VlmRepetitionLoop,
        Self::VlmTransportError,
        Self::VlmOutputTruncated,
        Self::OlmocrZeroPages,
        Self::OlmocrIncompletePages,
        Self::GappedConversion,
        Self::ConversionPanicked,
    ];

    /// Parse a wire token. Exact match only: an unknown token is not a
    /// known failure, and the caller treats it as unclassified.
    pub fn from_wire(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.wire() == token)
    }

    /// What the chain should do with a document that failed this way.
    pub fn class(self) -> FailureClass {
        let token = self.wire();
        match self {
            Self::UnsupportedContentTypeHtml
            | Self::UnsupportedContentTypeBinary
            | Self::PaywallHtml
            | Self::PdfParseError
            | Self::EpubParseError
            | Self::HtmlNotUtf8
            | Self::HtmlParseError
            | Self::UnsupportedExtension
            | Self::SourceMissing
            | Self::MissingExtension
            | Self::InvalidKey
            | Self::ConversionPanicked
            | Self::EmptyConversion => FailureClass::Permanent(token),
            Self::EmptyVlmConversion
            | Self::VlmRepetitionLoop
            | Self::VlmTransportError
            | Self::VlmOutputTruncated
            | Self::OlmocrZeroPages
            | Self::OlmocrIncompletePages
            | Self::GappedConversion => FailureClass::Escalate(token),
        }
    }
}

/// A convert failure that carries its [`FailureCode`]. Put it at the root
/// of an `anyhow::Error` (`anyhow::Error::new(ConvertFailure::new(..))`);
/// `.context(..)` layers added later keep it reachable for [`classify`].
#[derive(Debug)]
pub struct ConvertFailure {
    code: FailureCode,
    message: String,
}

impl ConvertFailure {
    pub fn new(code: FailureCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// `anyhow::Error` rooted at a failure with this code.
    pub fn err(code: FailureCode, message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self::new(code, message))
    }

    pub fn code(&self) -> FailureCode {
        self.code
    }
}

impl std::fmt::Display for ConvertFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConvertFailure {}

/// The [`FailureCode`] an error was typed with, if any.
pub fn failure_code(err: &anyhow::Error) -> Option<FailureCode> {
    err.chain()
        .find_map(|e| e.downcast_ref::<ConvertFailure>())
        .map(ConvertFailure::code)
}

/// Classify a convert failure from its typed code. No code in the chain
/// means the error says nothing about the document: `Transient`.
pub fn classify(err: &anyhow::Error) -> FailureClass {
    failure_code(err).map_or(FailureClass::Transient, FailureCode::class)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn every_code_round_trips_through_its_wire_token_and_tokens_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for code in FailureCode::ALL {
            assert_eq!(FailureCode::from_wire(code.wire()), Some(code));
            assert!(seen.insert(code.wire()), "duplicate token {}", code.wire());
        }
    }

    #[test]
    fn wire_tokens_match_exactly_and_nothing_else() {
        assert_eq!(
            FailureCode::from_wire("paywall_html"),
            Some(FailureCode::PaywallHtml)
        );
        for not_a_code in [
            "",
            "paywall",
            "Paywall_html",
            "paywall_html ",
            "scribe convert failed for paywall_html",
            "unsupported_content_type",
        ] {
            assert_eq!(FailureCode::from_wire(not_a_code), None, "{not_a_code:?}");
        }
    }

    #[test]
    fn permanent_codes_stop_the_chain_and_name_the_catalog_reason() {
        for (code, token) in [
            (
                FailureCode::UnsupportedContentTypeHtml,
                "unsupported_content_type:html",
            ),
            (
                FailureCode::UnsupportedContentTypeBinary,
                "unsupported_content_type:binary",
            ),
            (FailureCode::PaywallHtml, "paywall_html"),
            (FailureCode::PdfParseError, "pdf_parse_error"),
            (FailureCode::EpubParseError, "epub_parse_error"),
            (FailureCode::HtmlNotUtf8, "html_not_utf8"),
            (FailureCode::HtmlParseError, "html_parse_error"),
            (FailureCode::UnsupportedExtension, "unsupported_extension"),
            (FailureCode::SourceMissing, "source_missing"),
            (FailureCode::MissingExtension, "missing_extension"),
            (FailureCode::InvalidKey, "invalid_key"),
            (FailureCode::EmptyConversion, "empty_conversion"),
            (FailureCode::ConversionPanicked, "conversion_panicked"),
        ] {
            assert_eq!(
                classify(&ConvertFailure::err(code, "x")),
                FailureClass::Permanent(token)
            );
        }
    }

    #[test]
    fn backend_class_failures_escalate_to_the_next_backend() {
        for code in [
            FailureCode::EmptyVlmConversion,
            FailureCode::VlmRepetitionLoop,
            FailureCode::VlmTransportError,
            FailureCode::VlmOutputTruncated,
            FailureCode::OlmocrZeroPages,
            FailureCode::OlmocrIncompletePages,
            FailureCode::GappedConversion,
        ] {
            assert_eq!(
                classify(&ConvertFailure::err(code, "x")),
                FailureClass::Escalate(code.wire()),
                "{code:?}"
            );
        }
    }

    #[test]
    fn an_untyped_error_is_transient_even_when_its_text_names_a_verdict() {
        // The old substring table permanently failed any document whose
        // stem or key happened to contain one of its words.
        for text in [
            "scribe convert failed for papers/pa/paywall-economics.pdf",
            "error sending request for url (http://host-a:7435/scribe/stream): FormatError",
            "VLM repetition loop on beck_tdd",
            "source bytes missing for papers/mc/code.pdf",
            "key papers/mc/noext has no extension",
            "converted by html-parser to 12 non-whitespace chars, below the indexable floor",
        ] {
            assert_eq!(
                classify(&anyhow::anyhow!(text.to_string())),
                FailureClass::Transient,
                "{text}"
            );
        }
    }

    #[test]
    fn a_stem_that_names_a_verdict_does_not_change_the_typed_one() {
        let err = ConvertFailure::err(FailureCode::VlmTransportError, "stream died")
            .context("scribe convert failed for papers/pa/paywall-FormatError.pdf");
        assert_eq!(
            classify(&err),
            FailureClass::Escalate("vlm_transport_error")
        );
    }

    #[test]
    fn context_layers_added_after_the_fact_keep_the_code_reachable() {
        let err: anyhow::Result<()> = Err(ConvertFailure::err(
            FailureCode::PdfParseError,
            "broken xref",
        ));
        let err = err
            .context("opening upload")
            .context("scribe convert failed")
            .unwrap_err();
        assert_eq!(failure_code(&err), Some(FailureCode::PdfParseError));
        assert_eq!(classify(&err), FailureClass::Permanent("pdf_parse_error"));
    }
}
