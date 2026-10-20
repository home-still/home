//! Rank fusion for hybrid retrieval.
//!
//! The dense (vector) channel and the lexical (keyword) channel rank
//! candidates independently and on different scales: a cosine similarity and
//! a text-match count are not comparable numbers, and averaging them would
//! let whichever channel happens to produce larger numbers win. Reciprocal
//! Rank Fusion (Cormack, Clarke & Buettcher, 2009) combines *ranks*
//! instead, so no calibration between the channels is needed:
//!
//! ```text
//! score(d) = Σ over channels of  1 / (k + rank_in_channel(d))
//! ```
//!
//! A document that any channel ranks highly scores well even if the other
//! channel never saw it; a document that both channels rank highly scores
//! best of all. The corpus contains the paper: `10.1145_1571941.1572114`.
//!
//! The constant `k` controls how much the head of a ranking is flattened.
//! `k = 1` keeps a confident head and lets the other channel contribute
//! only where it is also confident; larger `k` averages a strong ranking
//! with a weak one. Our own sibling crate (home-still/myelin) measured
//! `k = 1` at 0.8815 dev recall@6 against `k = 60` at 0.8252, and measured
//! BM25 alone at 0.8666 against dense alone at 0.7314 — so the dense head is
//! the weaker one here and must not be flattened. `k = 1` is the default
//! for the same reason.

use std::collections::HashMap;

/// One channel's ranking: candidate ids, best first.
#[derive(Debug, Clone, Default)]
pub struct RankedList {
    /// Candidate ids, most relevant first.
    pub ids: Vec<String>,
}

impl RankedList {
    pub fn new(ids: Vec<String>) -> Self {
        Self { ids }
    }
}

/// The RRF constant. See the module docs: `k = 1` is a measured choice, not
/// the value from the paper (Cormack et al. use 60).
pub const DEFAULT_RRF_K: f32 = 1.0;

/// Fuse ranked lists with Reciprocal Rank Fusion.
///
/// Returns the fused ranking, best first. Ids that appear in no list are
/// absent; ids that appear in several are scored by the sum of their
/// per-list contributions. The result is deduplicated: each id appears
/// once.
///
/// Ties are broken by the id, so the same inputs always produce the same
/// output — a search that reorders itself between identical calls is a
/// search nobody can debug.
pub fn rrf(lists: &[RankedList], k: f32) -> Vec<(String, f32)> {
    let mut scores: HashMap<&str, f32> = HashMap::new();
    for list in lists {
        for (i, id) in list.ids.iter().enumerate() {
            let rank = (i + 1) as f32;
            *scores.entry(id.as_str()).or_insert(0.0) += 1.0 / (k + rank);
        }
    }
    let mut out: Vec<(String, f32)> = scores
        .into_iter()
        .map(|(id, score)| (id.to_string(), score))
        .collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

/// The ids of the fused ranking, best first.
pub fn rrf_ids(lists: &[RankedList], k: f32) -> Vec<String> {
    rrf(lists, k).into_iter().map(|(id, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> RankedList {
        RankedList::new(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn an_empty_fusion_is_empty() {
        assert!(rrf(&[], DEFAULT_RRF_K).is_empty());
        assert!(rrf(&[ids(&[])], DEFAULT_RRF_K).is_empty());
    }

    #[test]
    fn a_single_list_keeps_its_order() {
        let fused = rrf(&[ids(&["a", "b", "c"])], DEFAULT_RRF_K);
        let order: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn a_document_in_both_lists_outranks_one_in_a_single_list() {
        // "both" is last in the dense list and first in the lexical list.
        // "dense-only" is first in the dense list.
        let fused = rrf(
            &[ids(&["dense-only", "both"]), ids(&["both", "lexical-only"])],
            DEFAULT_RRF_K,
        );
        assert_eq!(fused[0].0, "both");
    }

    #[test]
    fn the_score_is_the_sum_of_reciprocal_ranks() {
        // k = 1: "x" at rank 1 in one list scores 1/(1+1) = 0.5.
        let fused = rrf(&[ids(&["x"])], 1.0);
        assert!((fused[0].1 - 0.5).abs() < 1e-6);
        // At rank 1 in two lists: 2 * 0.5 = 1.0.
        let fused = rrf(&[ids(&["x"]), ids(&["x"])], 1.0);
        assert!((fused[0].1 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn each_id_appears_once() {
        let fused = rrf(
            &[ids(&["a", "b"]), ids(&["b", "a"]), ids(&["a"])],
            DEFAULT_RRF_K,
        );
        let mut seen: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        seen.sort();
        assert_eq!(seen, vec!["a", "b"]);
    }

    #[test]
    fn ties_break_on_the_id_so_the_output_is_stable() {
        // Two ids at the same rank in two different lists score equally.
        let fused = rrf(&[ids(&["b", "a"]), ids(&["a", "b"])], DEFAULT_RRF_K);
        let order: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order, vec!["a", "b"]);
    }

    #[test]
    fn a_larger_k_flattens_the_head() {
        // "z" heads the first list and appears nowhere else. "a" is second
        // in the first list and also sixth in the second. A small k
        // rewards a confident head, so "z" wins (0.500 vs 0.476). A large
        // k flattens the head until appearing in both lists matters more
        // than heading one, and "a" wins (0.0313 vs 0.0164).
        //
        // The arithmetic is the point: it is what makes "k = 1" a choice
        // about which channel's confidence to protect, and it is why the
        // constant is measured rather than copied from the paper.
        let first = ids(&["z", "a", "b", "c", "d", "e"]);
        let second = ids(&["zz", "g", "h", "i", "j", "a"]);
        let small = rrf(&[first.clone(), second.clone()], 1.0);
        assert_eq!(small[0].0, "z");
        let large = rrf(&[first, second], 60.0);
        assert_eq!(large[0].0, "a");
    }

    #[test]
    fn an_id_missing_from_one_list_still_appears() {
        let fused = rrf_ids(&[ids(&["a"]), ids(&["b"])], DEFAULT_RRF_K);
        assert_eq!(fused.len(), 2);
        assert!(fused.contains(&"a".to_string()));
        assert!(fused.contains(&"b".to_string()));
    }
}
