//! UTF-8 helpers shared by the metadata, abstract and chunking code.

/// Largest char boundary of `s` that is `<= idx`. Equivalent to the
/// standard library's `str::floor_char_boundary`, which this workspace does
/// not rely on because it was unstable on the toolchains it has been built
/// with. `idx` past the end clamps to `s.len()`.
pub fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary of `s` that is `>= idx`, clamped to `s.len()`.
pub fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// The longest prefix of `s` that is at most `max_bytes` long and ends on a
/// char boundary.
pub fn prefix_at_most(s: &str, max_bytes: usize) -> &str {
    &s[..floor_char_boundary(s, max_bytes)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_never_lands_inside_a_char() {
        // "€" is 3 bytes: boundaries at 0, 3, 6.
        let s = "€€";
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 1), 0);
        assert_eq!(floor_char_boundary(s, 2), 0);
        assert_eq!(floor_char_boundary(s, 3), 3);
        assert_eq!(floor_char_boundary(s, 5), 3);
        assert_eq!(floor_char_boundary(s, 6), 6);
        assert_eq!(floor_char_boundary(s, 600), 6);
    }

    #[test]
    fn ceil_never_lands_inside_a_char() {
        let s = "€€";
        assert_eq!(ceil_char_boundary(s, 1), 3);
        assert_eq!(ceil_char_boundary(s, 3), 3);
        assert_eq!(ceil_char_boundary(s, 4), 6);
        assert_eq!(ceil_char_boundary(s, 600), 6);
    }

    #[test]
    fn prefix_at_most_keeps_whole_chars_only() {
        assert_eq!(prefix_at_most("€€€", 7), "€€");
        assert_eq!(prefix_at_most("abc", 2), "ab");
        assert_eq!(prefix_at_most("abc", 99), "abc");
        assert_eq!(prefix_at_most("", 4), "");
    }
}
