use crate::error::{PersonalError, Result};

pub async fn run(stem: String) -> Result<()> {
    let cfg = crate::config::Config::load()?;
    let chunks = crate::services::catalog::reindex(&cfg, &stem)
        .await
        .map_err(|e| PersonalError::Index(e.to_string()))?;
    println!("reindexed: stem={stem} chunks={chunks}");
    Ok(())
}
