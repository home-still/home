pub fn is_phrase_query(query: &str) -> bool {
    let words: Vec<&str> = query.split_whitespace().collect();
    words.len() > 1
        && !query.contains(" AND ")
        && !query.contains(" OR ")
        && !query.contains(" NOT ")
        && !query.contains('"')
}

pub fn maybe_quote_phrase(query: &str) -> String {
    if is_phrase_query(query) {
        format!("\"{}\"", query)
    } else {
        query.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_word_is_not_phrase() {
        assert!(!is_phrase_query("CRISPR"));
    }

    #[test]
    fn multi_word_is_phrase() {
        assert!(is_phrase_query("autistic female"));
        assert!(is_phrase_query("transformer attention mechanism"));
    }

    #[test]
    fn boolean_operators_not_phrase() {
        assert!(!is_phrase_query("neural AND networks"));
        assert!(!is_phrase_query("autism OR ASD"));
        assert!(!is_phrase_query("transformer NOT vision"));
    }

    #[test]
    fn already_quoted_not_phrase() {
        assert!(!is_phrase_query("\"autistic female\""));
    }

    #[test]
    fn maybe_quote_wraps_phrases() {
        assert_eq!(maybe_quote_phrase("autistic female"), "\"autistic female\"");
    }

    #[test]
    fn maybe_quote_preserves_single_words() {
        assert_eq!(maybe_quote_phrase("CRISPR"), "CRISPR");
    }

    #[test]
    fn maybe_quote_preserves_boolean() {
        assert_eq!(
            maybe_quote_phrase("neural AND networks"),
            "neural AND networks"
        );
    }
}
