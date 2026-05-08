use crate::error::Result;

pub async fn run(category: Option<String>, limit: usize) -> Result<()> {
    let cfg = crate::config::Config::load()?;
    let entries = crate::services::catalog::list_entries(&cfg, category.as_deref(), limit)?;
    for e in &entries {
        println!(
            "{:60}  {:14}  {}",
            e.title.as_deref().unwrap_or("(untitled)"),
            e.category.as_deref().unwrap_or("(none)"),
            e.stem,
        );
    }
    println!(
        "\n{} entr{} shown",
        entries.len(),
        if entries.len() == 1 { "y" } else { "ies" }
    );
    Ok(())
}
