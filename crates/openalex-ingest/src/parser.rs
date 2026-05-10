//! Parsing helpers shared by every entity loader.
//!
//! Lifted from `paper/src/providers/openalex.rs` so the snapshot ingest produces
//! IDs/abstracts byte-identical to what `paper_search` would surface for the
//! same record from the live API.

use std::collections::HashMap;

pub fn strip_openalex_id(id: &str) -> &str {
    id.strip_prefix("https://openalex.org/").unwrap_or(id)
}

pub fn strip_doi(doi: &str) -> &str {
    doi.strip_prefix("https://doi.org/").unwrap_or(doi)
}

/// Parse the integer portion of an OpenAlex work ID as `u64`. Accepts both
/// the URL form (`"https://openalex.org/W2741809807"`) and the bare form
/// (`"W2741809807"`). Returns `None` if the prefix is wrong, the prefix
/// letter isn't `W`, or the trailing digits don't parse.
///
/// Used by the streaming-pre-dedupe seen-set: integer IDs collide-free
/// at corpus scale (vs hashing the string, which has ~3-in-1000 collision
/// risk at 250M items via xxhash64).
pub fn parse_work_id_u64(id: &str) -> Option<u64> {
    let bare = strip_openalex_id(id);
    let digits = bare.strip_prefix('W')?;
    digits.parse::<u64>().ok()
}

pub fn reconstruct_abstract(inverted_index: &HashMap<String, Vec<u32>>) -> Option<String> {
    if inverted_index.is_empty() {
        return None;
    }

    let max_pos = inverted_index
        .values()
        .flat_map(|positions| positions.iter())
        .max()
        .copied()? as usize;

    let mut words: Vec<&str> = vec![""; max_pos + 1];

    for (word, positions) in inverted_index {
        for pos in positions {
            if let Some(slot) = words.get_mut(*pos as usize) {
                *slot = word.as_str();
            }
        }
    }

    let text = words.join(" ");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_openalex_id_prefix() {
        assert_eq!(strip_openalex_id("https://openalex.org/W123"), "W123");
        assert_eq!(strip_openalex_id("W123"), "W123");
    }

    #[test]
    fn strips_doi_prefix() {
        assert_eq!(strip_doi("https://doi.org/10.1/x"), "10.1/x");
        assert_eq!(strip_doi("10.1/x"), "10.1/x");
    }

    #[test]
    fn reconstructs_simple_abstract() {
        let mut idx = HashMap::new();
        idx.insert("the".to_string(), vec![0]);
        idx.insert("quick".to_string(), vec![1]);
        idx.insert("fox".to_string(), vec![2]);
        assert_eq!(reconstruct_abstract(&idx).as_deref(), Some("the quick fox"));
    }

    #[test]
    fn reconstructs_repeated_words() {
        let mut idx = HashMap::new();
        idx.insert("a".to_string(), vec![0, 4]);
        idx.insert("b".to_string(), vec![1]);
        idx.insert("c".to_string(), vec![2]);
        idx.insert("d".to_string(), vec![3]);
        assert_eq!(reconstruct_abstract(&idx).as_deref(), Some("a b c d a"));
    }

    #[test]
    fn empty_index_returns_none() {
        let idx: HashMap<String, Vec<u32>> = HashMap::new();
        assert!(reconstruct_abstract(&idx).is_none());
    }

    #[test]
    fn parses_work_id_u64_url_form() {
        assert_eq!(
            parse_work_id_u64("https://openalex.org/W2741809807"),
            Some(2_741_809_807)
        );
    }

    #[test]
    fn parses_work_id_u64_bare_form() {
        assert_eq!(parse_work_id_u64("W2741809807"), Some(2_741_809_807));
    }

    #[test]
    fn parses_work_id_u64_short_id() {
        // Some early-corpus works have short IDs.
        assert_eq!(parse_work_id_u64("W123"), Some(123));
    }

    #[test]
    fn parses_work_id_u64_rejects_non_w_prefix() {
        // Authors/sources/etc. use A/S/I prefixes; this helper is works-only.
        assert_eq!(parse_work_id_u64("A123"), None);
        assert_eq!(parse_work_id_u64("https://openalex.org/A123"), None);
    }

    #[test]
    fn parses_work_id_u64_rejects_garbage() {
        assert_eq!(parse_work_id_u64(""), None);
        assert_eq!(parse_work_id_u64("W"), None);
        assert_eq!(parse_work_id_u64("Wabc"), None);
    }
}
