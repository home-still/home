//! The one definition of a downloaded paper's storage identity (its "stem").
//!
//! Every artifact the pipeline stores for a paper — `papers/XX/<stem>.pdf`,
//! `markdown/…`, `catalog/<stem>.yaml`, the Qdrant `doc_id` — is keyed by this
//! string, so two spellings of one paper are two documents: converted twice,
//! indexed twice, returned twice by one query.
//!
//! # Scheme (matches the production corpus)
//!
//! * A paper with a DOI is keyed by the lowercased DOI with `/` → `_`
//!   (`10.1016/J.RASD.2010.03.002` → `10.1016_j.rasd.2010.03.002`). DOIs are
//!   case-insensitive (ISO 26324), and `hs migrate canonicalize-doi-stems`
//!   already converged the corpus on exactly this form, so it is the
//!   canonical one. Resolver prefixes (`https://doi.org/`, `doi:` …) are not
//!   part of the identity.
//! * A paper without a DOI is keyed by the provider's own `id` with `/`, `\`
//!   and `:` replaced by `_`, case preserved (`W2102450255`, `2301.00001v1`).
//!   Identifiers such as OpenAlex work ids are case-significant and the
//!   corpus holds stems of this shape; they are never touched.
//!
//! Both forms are checked with [`hs_common::validate_stem`], so a stem can
//! never be empty, `.`/`..`, or carry a path separator or NUL. This module is
//! the only place that builds one; the downloader and the batch service both
//! call it.

use crate::error::PaperError;
use crate::models::Paper;

/// Spellings under which resolvers hand out the same DOI.
const DOI_PREFIXES: [&str; 5] = [
    "https://doi.org/",
    "http://doi.org/",
    "https://dx.doi.org/",
    "http://dx.doi.org/",
    "doi:",
];

/// Reduce `doi` to its bare form (`10.xxxx/suffix`, original case): trimmed,
/// without a resolver prefix. Rejects anything that is not DOI-shaped —
/// a registrant code, a `/`, a non-empty suffix, no control characters, no
/// `.`/`..` path segments (the DOI becomes URL path segments, where those
/// would be dropped or resolved instead of sent).
pub fn normalize_doi(doi: &str) -> Result<String, PaperError> {
    let mut rest = doi.trim();
    for prefix in DOI_PREFIXES {
        if let Some(head) = rest.get(..prefix.len()) {
            if head.eq_ignore_ascii_case(prefix) {
                rest = rest.get(prefix.len()..).unwrap_or_default();
                break;
            }
        }
    }

    let well_formed = rest.strip_prefix("10.").is_some_and(|after| {
        after
            .split_once('/')
            .is_some_and(|(registrant, suffix)| !registrant.is_empty() && !suffix.is_empty())
    }) && !rest.chars().any(char::is_control)
        && !rest
            .split('/')
            .any(|segment| segment == "." || segment == "..");

    if !well_formed {
        return Err(PaperError::InvalidInput(format!(
            "{doi:?} is not a DOI (expected 10.<registrant>/<suffix>)"
        )));
    }
    Ok(rest.to_string())
}

/// Stem of a paper known by its DOI.
pub fn doi_stem(doi: &str) -> Result<String, PaperError> {
    let doi = normalize_doi(doi)?;
    checked(doi.to_lowercase().replace('/', "_"))
}

/// Stem of a paper that has no DOI, from the provider's own id.
pub fn id_stem(id: &str) -> Result<String, PaperError> {
    checked(id.trim().replace(['/', '\\', ':'], "_"))
}

/// Stem of a search result: its DOI when it has one (so a paper found by
/// search and the same paper requested by DOI land on one key), else its id.
///
/// A present-but-malformed DOI is an error, not a reason to quietly switch to
/// the id: the two schemes disagreeing about one paper is how the corpus got
/// its duplicate pairs.
pub fn paper_stem(paper: &Paper) -> Result<String, PaperError> {
    match paper
        .doi
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(doi) => doi_stem(doi),
        None => id_stem(&paper.id),
    }
}

/// Validate a caller-supplied stem (the `DownloadService::download_by_url`
/// boundary) against the shared stem rules.
pub fn check_stem(stem: &str) -> Result<(), PaperError> {
    hs_common::validate_stem(stem)
        .map_err(|e| PaperError::InvalidInput(format!("invalid stem {stem:?}: {e}")))
}

fn checked(stem: String) -> Result<String, PaperError> {
    check_stem(&stem)?;
    Ok(stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paper(id: &str, doi: Option<&str>) -> Paper {
        Paper {
            id: id.to_string(),
            title: "t".to_string(),
            authors: vec![],
            abstract_text: None,
            publication_date: None,
            doi: doi.map(str::to_string),
            download_urls: vec![],
            cited_by_count: None,
            source: "test".to_string(),
        }
    }

    #[test]
    fn every_spelling_of_one_doi_is_one_stem() {
        let spellings = [
            "10.1016/j.rasd.2010.03.002",
            "10.1016/J.RASD.2010.03.002",
            "  10.1016/j.rasd.2010.03.002\n",
            "https://doi.org/10.1016/J.RASD.2010.03.002",
            "HTTP://DX.DOI.ORG/10.1016/j.rasd.2010.03.002",
            "doi:10.1016/j.rasd.2010.03.002",
            "DOI:10.1016/J.Rasd.2010.03.002",
        ];
        for s in spellings {
            assert_eq!(
                doi_stem(s).unwrap(),
                "10.1016_j.rasd.2010.03.002",
                "spelling {s:?}"
            );
        }
    }

    #[test]
    fn a_search_result_and_its_doi_share_one_stem() {
        // Same paper reached by search (id + DOI as the provider spelled it)
        // and by an explicit DOI request.
        let by_search = paper(
            "W2102450255",
            Some("https://doi.org/10.1017/S0033291718004038"),
        );
        let crossref = paper(
            "10.1017/S0033291718004038",
            Some("10.1017/S0033291718004038"),
        );
        let by_doi = doi_stem("10.1017/s0033291718004038").unwrap();
        assert_eq!(paper_stem(&by_search).unwrap(), by_doi);
        assert_eq!(paper_stem(&crossref).unwrap(), by_doi);
    }

    #[test]
    fn doi_stems_match_the_stems_the_corpus_already_holds() {
        // Corpus shape pinned by the 2026-08 canonicalisation: lowercase,
        // only `/` rewritten (dots, parentheses, colons, angle brackets stay).
        assert_eq!(
            doi_stem("10.1002/(SICI)1097-4571(199806)49:8<693::AID-ASI4>3.0.CO;2-O").unwrap(),
            "10.1002_(sici)1097-4571(199806)49:8<693::aid-asi4>3.0.co;2-o"
        );
        assert_eq!(
            doi_stem("10.48550/arXiv.2410.07095").unwrap(),
            "10.48550_arxiv.2410.07095"
        );
    }

    #[test]
    fn papers_without_a_doi_keep_their_case_sensitive_id() {
        assert_eq!(
            paper_stem(&paper("W2102450255", None)).unwrap(),
            "W2102450255"
        );
        assert_eq!(
            paper_stem(&paper("hep-th/9901001v1", None)).unwrap(),
            "hep-th_9901001v1"
        );
        assert_eq!(
            paper_stem(&paper("arxiv:2301.1", Some("  "))).unwrap(),
            "arxiv_2301.1"
        );
    }

    #[test]
    fn no_input_can_produce_a_dot_segment_or_a_separator() {
        let hostile_ids = [
            "", "  ", ".", "..", " .. ", "/", "\\", "\0", "a\0b", "../x", "..\\x", "a/../b",
        ];
        for id in hostile_ids {
            match id_stem(id) {
                Ok(stem) => {
                    assert!(
                        hs_common::validate_stem(&stem).is_ok(),
                        "id {id:?} -> {stem:?}"
                    );
                    assert!(!stem.contains(['/', '\\', '\0']), "id {id:?} -> {stem:?}");
                    assert!(stem != "." && stem != "..", "id {id:?} -> {stem:?}");
                }
                Err(PaperError::InvalidInput(_)) => {}
                Err(other) => panic!("unexpected error kind for {id:?}: {other}"),
            }
        }
        // The two the old `sanitize_filename` let through: `..` and the empty id.
        assert!(id_stem("..").is_err());
        assert!(id_stem("").is_err());
    }

    #[test]
    fn hostile_dois_are_rejected_not_sanitised() {
        for doi in [
            "",
            "10.",
            "10.1234",
            "10.1234/",
            "10./abc",
            "11.1234/abc",
            "..",
            "10.1234/a\0b",
            "10.1234/a\nb",
            "10.1234/a\\b",
            "10.1234/../../etc",
            "10.1234/./x",
            "10.1234/x/..",
            "https://doi.org/",
            "doi:",
        ] {
            assert!(doi_stem(doi).is_err(), "{doi:?} must be rejected");
        }
    }

    #[test]
    fn a_malformed_doi_does_not_fall_back_to_the_id() {
        let p = paper("W1", Some("not-a-doi"));
        assert!(matches!(paper_stem(&p), Err(PaperError::InvalidInput(_))));
    }

    #[test]
    fn stems_survive_the_shard_function_for_any_script() {
        // `hs_common::sharded_key` is panic-free on multi-byte stems; the
        // canonical stem of a non-ASCII DOI must go through it unharmed.
        for doi in ["10.1234/Ünïcode", "10.1234/日本語", "10.1234/😀"] {
            let stem = doi_stem(doi).unwrap();
            assert!(hs_common::sharded_key(&stem, "pdf").ends_with(".pdf"));
        }
    }
}
