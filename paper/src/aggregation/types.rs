use serde::{Deserialize, Serialize};

use crate::models::Paper;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedPaper {
    pub paper: Paper,
    pub contributing_sources: Vec<String>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DedupStats {
    pub total_raw: usize,
    pub unique: usize,
    pub doi_matches: usize,
    pub fuzzy_matches: usize,
}
