use crate::error::Result;
use crate::services::ingest::{self, IngestOptions};
use std::path::PathBuf;

pub async fn run(
    file: PathBuf,
    category: Option<String>,
    title: Option<String>,
    force: bool,
) -> Result<()> {
    let cfg = crate::config::Config::load()?;
    let opts = IngestOptions {
        category_override: category,
        title_override: title,
        force,
    };
    let outcome = ingest::ingest(&cfg, &file, opts).await?;
    println!(
        "ingested: stem={} title={:?} category={} chunks={}",
        outcome.stem, outcome.title, outcome.category, outcome.chunk_count
    );
    Ok(())
}
