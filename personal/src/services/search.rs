use crate::config::Config;
use crate::error::Result;
use crate::services::distill::PersonalDistill;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub stem: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub snippet: String,
    pub score: f32,
}

pub async fn search(
    cfg: &Config,
    query: &str,
    category: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let distill = PersonalDistill::new(cfg)?;
    let raw = distill.search(query, limit as u64, category).await?;
    Ok(raw
        .into_iter()
        .map(|h| SearchHit {
            stem: h.doc_id,
            title: h.title,
            category: h.category,
            snippet: snippet(&h.chunk_text),
            score: h.score,
        })
        .collect())
}

fn snippet(text: &str) -> String {
    let cleaned = text.replace('\n', " ");
    if cleaned.chars().count() <= 200 {
        cleaned
    } else {
        let mut s: String = cleaned.chars().take(200).collect();
        s.push('…');
        s
    }
}
