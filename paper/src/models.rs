use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    pub affiliations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paper {
    pub id: String,
    pub title: String,
    pub authors: Vec<Author>,
    pub abstract_text: Option<String>,
    pub publication_date: Option<NaiveDate>,
    pub doi: Option<String>,
    pub download_url: Option<String>,
    pub cited_by_count: Option<u64>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SearchType {
    Keywords,
    Title,
    Author,
    DOI,
    Subject,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum SortBy {
    #[default]
    Relevance,
    Date,
    Citations,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DateFilter {
    /// Inclusive lower bound (first date to include)
    pub after: Option<NaiveDate>,
    /// Exclusive upper bound (first date to exclude)
    pub before: Option<NaiveDate>,
}

impl DateFilter {
    pub fn parse(input: &str) -> Result<Self, String> {
        let mut after = None;
        let mut before = None;

        for token in input.split_whitespace() {
            let (op, date_str) = if let Some(rest) = token.strip_prefix(">=") {
                (">=", rest)
            } else if let Some(rest) = token.strip_prefix("<=") {
                ("<=", rest)
            } else if let Some(rest) = token.strip_prefix('>') {
                (">", rest)
            } else if let Some(rest) = token.strip_prefix('<') {
                ("<", rest)
            } else {
                return Err(format!(
                    "expected '>', '>=', '<', or '<=' prefix, got: {}",
                    token
                ));
            };

            let date = parse_partial_date(date_str)?;

            match op {
                ">=" => {
                    if after.is_some() {
                        return Err("duplicate lower bound".into());
                    }

                    after = Some(date);
                }
                ">" => {
                    if after.is_some() {
                        return Err("duplicate lower bound".into());
                    }

                    after = Some(
                        end_of_period(date_str, date)?
                            .succ_opt()
                            .ok_or("date overflow")?,
                    );
                }
                "<" => {
                    if before.is_some() {
                        return Err("duplicate upper bound".into());
                    }
                    before = Some(date)
                }
                "<=" => {
                    if before.is_some() {
                        return Err("duplicate upper bound".into());
                    }
                    before = Some(
                        end_of_period(date_str, date)?
                            .succ_opt()
                            .ok_or("date overflow")?,
                    )
                }
                _ => unreachable!(),
            }
        }

        if after.is_none() && before.is_none() {
            return Err("empty date filter".into());
        }
        if let (Some(a), Some(b)) = (after, before) {
            if a >= b {
                return Err(format!("lower bound {} is not before upper bound {}", a, b));
            }
        }
        Ok(DateFilter { after, before })
    }
}

fn parse_partial_date(s: &str) -> Result<NaiveDate, String> {
    match s.len() {
        4 => NaiveDate::parse_from_str(&format!("{}-01-01", s), "%Y-%m-%d"),
        7 => NaiveDate::parse_from_str(&format!("{}-01", s), "%Y-%m-%d"),
        10 => NaiveDate::parse_from_str(s, "%Y-%m-%d"),
        _ => {
            return Err(format!(
                "invalid date format '{}': use YYYY, YYYY-MM, or YYYY-MM-DD",
                s
            ))
        }
    }
    .map_err(|e| format!("invalid date '{}': {}", s, e))
}

fn end_of_period(raw: &str, start: NaiveDate) -> Result<NaiveDate, String> {
    match raw.len() {
        4 => {
            // End of year: Dec 31
            NaiveDate::from_ymd_opt(start.year(), 12, 31)
                .ok_or_else(|| format!("invalid year end for '{}'", raw))
        }
        7 => {
            // End of month: go to next month's 1st, subtract 1 day
            let next = if start.month() == 12 {
                NaiveDate::from_ymd_opt(start.year() + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(start.year(), start.month() + 1, 1)
            };
            next.and_then(|d| d.pred_opt())
                .ok_or_else(|| format!("invalid month end for '{}'", raw))
        }
        10 => Ok(start),
        _ => Err(format!("invalid date format '{}'", raw)),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    pub query: String,
    pub search_type: SearchType,
    pub max_results: usize,
    pub offset: usize,
    pub date_filter: Option<DateFilter>,
    pub sort_by: SortBy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub papers: Vec<Paper>,
    pub total_results: usize,
    pub next_offset: Option<usize>,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadResult {
    pub file_path: PathBuf,
    pub doi: Option<String>,
    pub sha256: String,
    pub size_bytes: u64,
    pub skipped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperMetadata {
    pub title: Option<String>,
    pub authors: Vec<Author>,
    pub doi: Option<String>,
    pub confidence_score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchDownloadResult {
    pub succeeded: Vec<DownloadResult>,
    pub skipped: Vec<DownloadResult>,
    pub failed: Vec<DownloadFailure>,
    pub total_requested: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadFailure {
    pub paper_id: String,
    pub title: String,
    pub error: String,
}

// --- Aggregation types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatedSearchResult {
    pub papers: Vec<RankedPaper>,
    pub source_statuses: Vec<SourceStatus>,
    pub dedup_stats: DedupStats,
    pub total_results: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedPaper {
    pub paper: Paper,
    pub contributing_sources: Vec<String>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStatus {
    pub name: String,
    pub status: SourceState,
    pub count: usize,
    pub total_available: usize,
    pub response_time_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SourceState {
    Success,
    Timeout,
    Error(String),
    CircuitOpen,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DedupStats {
    pub total_raw: usize,
    pub unique: usize,
    pub doi_matches: usize,
    pub fuzzy_matches: usize,
}
