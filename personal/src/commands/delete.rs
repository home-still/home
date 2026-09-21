use crate::error::{PersonalError, Result};

pub async fn run(stem: String) -> Result<()> {
    let cfg = crate::config::Config::load()?;
    crate::services::catalog::delete(&cfg, &stem)
        .await
        .map_err(|e| PersonalError::Index(e.to_string()))?;
    println!("deleted: {stem}");
    Ok(())
}
