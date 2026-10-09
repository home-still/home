use crate::error::Result;

pub async fn run(stem: String) -> Result<()> {
    let cfg = crate::config::Config::load()?;
    let chunks = crate::services::catalog::reindex(&cfg, &stem).await?;
    println!("reindexed: stem={stem} chunks={chunks}");
    Ok(())
}
