use serde::{Deserialize, Serialize};

use crate::models::Paper;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedPaper {
    pub paper: Paper,
    pub contributing_sources: Vec<String>,
    pub score: f64,
    /// Content-only relevance (term coverage, phrase match, title density).
    /// Kept separately from `score` so downstream can apply a relevance floor
    /// when the user picks a sort mode that can drown relevance (e.g.
    /// `sort=citations` amplifies high-citation off-topic papers; pairing
    /// with a floor keeps the target paper in the top slots).
    #[serde(default)]
    pub relevance: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DedupStats {
    pub total_raw: usize,
    pub unique: usize,
    pub doi_matches: usize,
    pub fuzzy_matches: usize,
}
