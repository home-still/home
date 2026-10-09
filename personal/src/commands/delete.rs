use crate::error::Result;

pub async fn run(stem: String) -> Result<()> {
    let cfg = crate::config::Config::load()?;
    crate::services::catalog::delete(&cfg, &stem).await?;
    println!("deleted: {stem}");
    Ok(())
}
