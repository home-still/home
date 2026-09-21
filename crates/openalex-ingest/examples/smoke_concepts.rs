use openalex_ingest::{OpenAlexDb, SimpleEntity};
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let snapshot = PathBuf::from("/home/ladvien/data/academic_papers/openalex-snapshot/data");
    let db_path = std::env::temp_dir().join(format!("oa-smoke-{}.duckdb", std::process::id()));
    println!("DB at {}", db_path.display());

    let db = OpenAlexDb::open(&db_path)?;
    let stats = db.load_simple_entity(SimpleEntity::Concepts, &snapshot)?;
    println!("concepts: {:?}", stats);

    for (table, n) in db.row_counts()? {
        println!("  {:>20}: {}", table, n);
    }

    println!("\nSample query — top 5 concepts by works_count:");
    let mut stmt = db.raw().prepare(
        "SELECT openalex_id, display_name, level, works_count
         FROM concepts ORDER BY works_count DESC NULLS LAST LIMIT 5",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<u8>>(2)?,
            r.get::<_, Option<u64>>(3)?,
        ))
    })?;
    for row in rows {
        println!("  {:?}", row?);
    }

    std::fs::remove_file(&db_path).ok();
    Ok(())
}
