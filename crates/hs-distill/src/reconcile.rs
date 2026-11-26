//! Reconcile markdown ↔ myelin store ↔ catalog.
//!
//! The embed pipeline can diverge in two ways when a stamp write or an
//! event handler silently fails:
//!
//! - **Stamp lost**: the doc is in the store (search works) but the catalog
//!   YAML never got the `embedding:` block, so the TUI reports the doc
//!   as "not embedded" even though it is. The `hs status` Markdown →
//!   Embedded percentage is phantom-low as a result.
//! - **Embed lost**: the markdown exists on storage but the doc never
//!   reached the store. Usually because the `scribe.completed` NATS event
//!   was dropped or the distill server errored and the handler swallowed
//!   the failure.
//!
//! `partition` is the pure decision function: given the three sets, it
//! returns the stems that need stamp-backfill vs. re-embed. It is
//! intentionally IO-free so the accounting logic can be unit-tested.

use std::collections::HashSet;

/// Classification of a markdown stem after comparing catalog state to
/// the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Catalog and store agree, or the catalog intentionally recorded a
    /// skip/failure. Nothing to do.
    Ok,
    /// Doc is in the store but catalog has no embedding stamp. Backfill the
    /// stamp so the TUI counts it correctly.
    StampMissing,
    /// Markdown exists, catalog has no prior embed outcome (neither a
    /// success stamp nor a skip/failure), but the store doesn't have it.
    /// Re-embed is safe.
    EmbedMissing,
}

/// Per-stem input for `partition`: what the catalog says about this
/// stem today.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogState {
    pub has_embedding_stamp: bool,
    /// Reason from `embedding_skip`, if any. `"zero_chunks_or_empty"` is an
    /// intentional skip (treat as Ok). `"embed_failed: ..."` is a dropped-
    /// event marker the reconciler itself writes — these must remain
    /// retryable (treat as EmbedMissing) so the next reconcile pass picks
    /// them up.
    pub embedding_skip_reason: Option<String>,
}

/// Classify every markdown stem. `indexed_docs` is the set of doc_ids
/// the store currently has at least one chunk for. `catalog` maps stem →
/// state (absent entries are treated as all-false).
pub fn partition<'a>(
    markdown_stems: &'a [String],
    indexed_docs: &HashSet<String>,
    catalog: &std::collections::HashMap<String, CatalogState>,
) -> Vec<(&'a str, Classification)> {
    markdown_stems
        .iter()
        .map(|stem| {
            let state = catalog.get(stem).cloned().unwrap_or_default();
            let in_store = indexed_docs.contains(stem);
            let class = classify(in_store, &state);
            (stem.as_str(), class)
        })
        .collect()
}

fn classify(in_store: bool, state: &CatalogState) -> Classification {
    // A recorded embed failure outranks whatever the store holds for the stem:
    // indexing upserts batch by batch, so a failed run can leave a document
    // with only some of its chunks (or a stale generation) in the store. Stamping
    // those as embedded would record a partial index as a success; only a
    // fresh index (which replaces the document's chunks) settles it.
    if !state.has_embedding_stamp && is_embed_failure(state.embedding_skip_reason.as_deref()) {
        return Classification::EmbedMissing;
    }
    match (in_store, state.has_embedding_stamp) {
        (true, true) => Classification::Ok,
        (true, false) => Classification::StampMissing,
        (false, _) => {
            // Not in the store. Embedding skips are intentional *unless* the
            // reason is an embed failure we recorded ourselves — those
            // must stay retryable, otherwise a transient error
            // permanently hides the doc from the reconciler.
            if is_intentional_skip(state.embedding_skip_reason.as_deref()) {
                Classification::Ok
            } else {
                Classification::EmbedMissing
            }
        }
    }
}

fn is_embed_failure(reason: Option<&str>) -> bool {
    reason.is_some_and(|r| r.starts_with("embed_failed"))
}

fn is_intentional_skip(reason: Option<&str>) -> bool {
    reason.is_some() && !is_embed_failure(reason)
}

/// Counts for each bucket. Used for the reconcile summary report.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileCounts {
    pub ok: usize,
    pub stamp_missing: usize,
    pub embed_missing: usize,
}

impl ReconcileCounts {
    pub fn from_partitions(parts: &[(&str, Classification)]) -> Self {
        let mut c = Self::default();
        for (_, class) in parts {
            match class {
                Classification::Ok => c.ok += 1,
                Classification::StampMissing => c.stamp_missing += 1,
                Classification::EmbedMissing => c.embed_missing += 1,
            }
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn s(xs: &[&str]) -> HashSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn stems(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stamp_missing_when_in_store_but_no_stamp() {
        // The real-world case we observed: markdown exists, the store has the
        // doc, catalog just never got stamped.
        let md = stems(&["a", "b"]);
        let indexed = s(&["a", "b"]);
        let mut cat = HashMap::new();
        cat.insert(
            "a".into(),
            CatalogState {
                has_embedding_stamp: true,
                ..Default::default()
            },
        );
        // "b" has no stamp → StampMissing
        let parts = partition(&md, &indexed, &cat);
        assert_eq!(parts[0].1, Classification::Ok);
        assert_eq!(parts[1].1, Classification::StampMissing);
    }

    #[test]
    fn embed_missing_when_not_in_store_and_no_skip() {
        let md = stems(&["a"]);
        let indexed = HashSet::new();
        let cat = HashMap::new();
        let parts = partition(&md, &indexed, &cat);
        assert_eq!(parts[0].1, Classification::EmbedMissing);
    }

    #[test]
    fn intentional_embedding_skip_is_ok_even_without_store_entry() {
        let md = stems(&["a"]);
        let indexed = HashSet::new();
        let mut cat = HashMap::new();
        cat.insert(
            "a".into(),
            CatalogState {
                embedding_skip_reason: Some("zero_chunks_or_empty".into()),
                ..Default::default()
            },
        );
        let parts = partition(&md, &indexed, &cat);
        assert_eq!(parts[0].1, Classification::Ok);
    }

    #[test]
    fn embed_failed_skip_stays_retryable() {
        // An "embed_failed:*" skip is a marker the reconciler itself (or
        // the event-watcher) wrote when indexing errored. Future runs must
        // still classify it as EmbedMissing, otherwise a transient storage
        // error or network blip hides the doc forever.
        let md = stems(&["a"]);
        let indexed = HashSet::new();
        let mut cat = HashMap::new();
        cat.insert(
            "a".into(),
            CatalogState {
                embedding_skip_reason: Some("embed_failed: Failed to read key".into()),
                ..Default::default()
            },
        );
        let parts = partition(&md, &indexed, &cat);
        assert_eq!(parts[0].1, Classification::EmbedMissing);
    }

    #[test]
    fn embed_failed_doc_present_in_store_is_reindexed_not_stamped() {
        // A failed index can leave some chunks (or a stale tail) behind;
        // backfilling a success stamp from that count would record a
        // partial index as complete.
        let md = stems(&["a", "b"]);
        let indexed = s(&["a", "b"]);
        let mut cat = HashMap::new();
        cat.insert(
            "a".into(),
            CatalogState {
                embedding_skip_reason: Some("embed_failed: upsert".into()),
                ..Default::default()
            },
        );
        // Stamped AND failed (a later re-index failed): the stamp stands.
        cat.insert(
            "b".into(),
            CatalogState {
                has_embedding_stamp: true,
                embedding_skip_reason: Some("embed_failed: upsert".into()),
            },
        );
        let parts = partition(&md, &indexed, &cat);
        assert_eq!(parts[0].1, Classification::EmbedMissing);
        assert_eq!(parts[1].1, Classification::Ok);
    }

    #[test]
    fn counts_tally_partitions() {
        let parts = vec![
            ("a", Classification::Ok),
            ("b", Classification::StampMissing),
            ("c", Classification::StampMissing),
            ("d", Classification::EmbedMissing),
        ];
        let counts = ReconcileCounts::from_partitions(&parts);
        assert_eq!(counts.ok, 1);
        assert_eq!(counts.stamp_missing, 2);
        assert_eq!(counts.embed_missing, 1);
    }

    #[test]
    fn mixed_real_world_sample_partitions_correctly() {
        // Mirrors the observed sample: 8 docs with markdown, the store has
        // them, catalog never stamped — all should be StampMissing.
        let md = stems(&[
            "10.1016_j.bbi.2020.08.034",
            "10.1016_j.cobeha.2017.07.018",
            "10.1186_s13229-018-0226-4",
        ]);
        let mut indexed = HashSet::new();
        indexed.insert("10.1016_j.bbi.2020.08.034".into());
        indexed.insert("10.1016_j.cobeha.2017.07.018".into());
        // Third stem (Risk markers) is genuinely missing from the store.
        let cat = HashMap::new();

        let parts = partition(&md, &indexed, &cat);
        assert_eq!(parts[0].1, Classification::StampMissing);
        assert_eq!(parts[1].1, Classification::StampMissing);
        assert_eq!(parts[2].1, Classification::EmbedMissing);

        let counts = ReconcileCounts::from_partitions(&parts);
        assert_eq!(counts.stamp_missing, 2);
        assert_eq!(counts.embed_missing, 1);
    }
}
