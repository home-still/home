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
}
