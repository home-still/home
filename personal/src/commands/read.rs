use crate::error::Result;

pub async fn run(stem: String) -> Result<()> {
    let cfg = crate::config::Config::load()?;
    let md = crate::services::catalog::read_markdown(&cfg, &stem)?;
    print!("{md}");
    Ok(())
}
