use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use hs_common::reporter::Reporter;
use hs_distill::cli::DistillCmd;
use hs_distill::client::DistillClient;
use hs_distill::config::DistillClientConfig;

const DEFAULT_SERVER: &str = "http://localhost:7434";

fn resolve_servers(cli_server: Option<&str>) -> Vec<String> {
    if let Some(s) = cli_server {
        return vec![s.to_string()];
    }
    match DistillClientConfig::load() {
        Ok(cfg) => {
            if cfg.servers.is_empty() {
                vec![DEFAULT_SERVER.to_string()]
            } else {
                cfg.servers
            }
        }
        Err(e) => {
            eprintln!("warning: Failed to load distill config: {e}, using default server");
            vec![DEFAULT_SERVER.to_string()]
        }
    }
}

pub async fn dispatch(cmd: DistillCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        DistillCmd::Index {
            force: _,
            file,
            server,
        } => cmd_index(file, server.as_deref(), reporter).await,
        DistillCmd::Search {
            query,
            limit,
            year,
            topic,
            server,
        } => cmd_search(&query, limit, year, topic, server.as_deref()).await,
        DistillCmd::Status { server } => cmd_status(server.as_deref()).await,
    }
}

async fn cmd_index(
    files: Option<Vec<PathBuf>>,
    server: Option<&str>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let servers = resolve_servers(server);
    let client = DistillClient::new(&servers[0]);

    // Health check
    let health = client.health().await?;
    reporter.status(
        "Connected",
        &format!("distill server ({})", health.compute_device),
    );

    // Determine files to index
    let config = DistillClientConfig::load().unwrap_or_default();
    let markdown_dir = config.markdown_dir;

    let paths: Vec<PathBuf> = if let Some(files) = files {
        files
    } else {
        std::fs::read_dir(&markdown_dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
                    .collect()
            })
            .unwrap_or_default()
    };

    if paths.is_empty() {
        reporter.warn("No markdown files found to index");
        return Ok(());
    }

    reporter.status("Found", &format!("{} files to index", paths.len()));

    let mut total_chunks = 0u32;
    for path in &paths {
        let path_str = path.to_string_lossy().to_string();
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        let stage = reporter.begin_stage(&format!("Indexing {stem}"), None);

        match client
            .index_file_with_progress(&path_str, |progress| {
                tracing::debug!(
                    "[{}] {}: {}",
                    progress.stage,
                    progress.doc,
                    progress.message
                );
            })
            .await
        {
            Ok(result) => {
                total_chunks += result.chunks_indexed;
                stage.finish_with_message(&format!(
                    "{}: {} chunks ({})",
                    stem, result.chunks_indexed, result.embedding_device
                ));
            }
            Err(e) => {
                stage.finish_failed(&format!("{stem}: {e}"));
            }
        }
    }

    reporter.finish(&format!(
        "Indexed {} files, {} total chunks",
        paths.len(),
        total_chunks
    ));

    Ok(())
}

async fn cmd_search(
    query: &str,
    limit: u64,
    year: Option<String>,
    topic: Option<String>,
    server: Option<&str>,
) -> Result<()> {
    let servers = resolve_servers(server);
    let client = DistillClient::new(&servers[0]);

    let filters = hs_distill::client::SearchFilters { year, topic };
    let hits = client.search(query, limit, filters).await?;

    if hits.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    for (i, hit) in hits.iter().enumerate() {
        let title = hit.title.as_deref().unwrap_or(&hit.doc_id);
        let page_info = hit
            .page
            .map(|p| format!(" (page {})", p))
            .unwrap_or_default();
        let pdf = hit.pdf_path.as_deref().unwrap_or("?");

        println!("\n{}. {} [score: {:.3}]", i + 1, title, hit.score);
        println!(
            "   {}:{}–{}{}",
            hit.doc_id, hit.line_start, hit.line_end, page_info
        );
        println!("   PDF: {pdf}");

        let preview: String = hit.chunk_text.chars().take(200).collect();
        println!("   {preview}...");
    }

    Ok(())
}

async fn cmd_status(server: Option<&str>) -> Result<()> {
    let servers = resolve_servers(server);
    let client = DistillClient::new(&servers[0]);

    let status = client.status().await?;

    println!("Collection: {}", status.collection);
    println!("Points: {}", status.points_count);
    println!("Device: {}", status.compute_device);

    Ok(())
}
