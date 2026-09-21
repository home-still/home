use crate::error::{PersonalError, Result};

pub async fn run(query: String, category: Option<String>, limit: usize) -> Result<()> {
    let cfg = crate::config::Config::load()?;
    let hits = crate::services::search::search(&cfg, &query, category.as_deref(), limit)
        .await
        .map_err(|e| PersonalError::Index(e.to_string()))?;
    for h in &hits {
        println!(
            "[{:.3}] {}  ({})\n    {}\n",
            h.score,
            h.title.as_deref().unwrap_or("(untitled)"),
            h.category.as_deref().unwrap_or("(none)"),
            h.snippet,
        );
    }
    Ok(())
}
