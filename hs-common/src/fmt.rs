//! One byte-count formatter for every surface that shows a size.
//!
//! `hs status`'s History detail, its Documents/Markdown sizes and the live
//! download progress all describe the same kind of quantity, so they must
//! agree. Two copies of this format is how one panel came to say `7.0 MB`
//! about a file another called `7.3 MB`.
//!
//! Decimal units, matching the repository's existing rendering: the `papers/`
//! tree and the catalog both speak in MB/GB.

/// Render `bytes` as a short human size: `512 B`, `1.5 KB`, `7 MB`, `2.4 GB`.
pub fn fmt_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.0} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.0} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_bytes;

    #[test]
    fn each_magnitude_uses_its_own_unit() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1_500), "2 KB");
        assert_eq!(fmt_bytes(7_340_032), "7 MB");
        assert_eq!(fmt_bytes(2_400_000_000), "2.4 GB");
    }

    #[test]
    fn the_unit_changes_exactly_at_its_own_threshold() {
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(1_000), "1 KB");
        assert_eq!(fmt_bytes(999_999), "1000 KB");
        assert_eq!(fmt_bytes(1_000_000), "1 MB");
    }
}
