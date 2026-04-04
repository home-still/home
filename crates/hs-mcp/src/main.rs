use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_router, ServerHandler,
};
use std::path::PathBuf;

// ── Tool parameter types ────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchParams {
    #[schemars(description = "Search query for academic papers")]
    query: String,
    #[schemars(description = "Maximum results to return (default 10)")]
    max_results: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DownloadParams {
    #[schemars(description = "Search query for papers to download")]
    query: String,
    #[schemars(description = "Maximum papers to download (default 5)")]
    max_results: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ConvertParams {
    #[schemars(description = "Path to the PDF file")]
    pdf_path: String,
    #[schemars(description = "Output markdown file path (optional)")]
    output_path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CatalogReadParams {
    #[schemars(description = "Paper stem name (filename without .pdf extension)")]
    stem: String,
}

// ── MCP Server ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct HomeStillMcp {
    catalog_dir: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl HomeStillMcp {
    fn new(catalog_dir: PathBuf) -> Self {
        Self {
            catalog_dir,
            tool_router: Self::tool_router(),
        }
    }
}

fn run_hs_sync(args: &[&str]) -> (String, String, bool) {
    match std::process::Command::new("hs").args(args).output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            (stdout, stderr, output.status.success())
        }
        Err(e) => (String::new(), format!("Failed to run hs: {e}"), false),
    }
}

#[tool_router]
impl HomeStillMcp {
    #[tool(
        description = "Search academic papers across 6 providers (arXiv, OpenAlex, Semantic Scholar, Europe PMC, CrossRef, CORE). Returns JSON."
    )]
    fn paper_search(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let n = p.max_results.unwrap_or(10).to_string();
        let (stdout, stderr, ok) =
            run_hs_sync(&["paper", "search", &p.query, "-n", &n, "--output", "json"]);
        if ok {
            stdout
        } else {
            format!("{stdout}\n{stderr}")
        }
    }

    #[tool(
        description = "Download academic papers matching a search query. Returns JSON with download results."
    )]
    fn paper_download(&self, Parameters(p): Parameters<DownloadParams>) -> String {
        let n = p.max_results.unwrap_or(5).to_string();
        let (stdout, stderr, _) =
            run_hs_sync(&["paper", "download", &p.query, "-n", &n, "--output", "json"]);
        format!("{stdout}\n{stderr}")
    }

    #[tool(
        description = "Convert a PDF to markdown using the scribe server. Returns the markdown content or writes to file."
    )]
    fn scribe_convert(&self, Parameters(p): Parameters<ConvertParams>) -> String {
        let mut args = vec!["scribe", "convert", &p.pdf_path];
        let out;
        if let Some(ref o) = p.output_path {
            args.push("--out");
            out = o.clone();
            args.push(&out);
        }
        let (stdout, stderr, ok) = run_hs_sync(&args);
        if ok {
            if stdout.is_empty() {
                "Conversion complete.".to_string()
            } else {
                stdout
            }
        } else {
            format!("Conversion failed:\n{stderr}")
        }
    }

    #[tool(description = "Check the status of the scribe watch service")]
    fn scribe_status(&self) -> String {
        let (stdout, stderr, _) = run_hs_sync(&["scribe", "status"]);
        format!("{stdout}{stderr}")
    }

    #[tool(
        description = "Read a catalog entry for a paper. Returns YAML with metadata, conversion info, and page index."
    )]
    fn catalog_read(&self, Parameters(p): Parameters<CatalogReadParams>) -> String {
        let path = self.catalog_dir.join(format!("{}.yaml", p.stem));
        match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(_) => format!("No catalog entry found for '{}'", p.stem),
        }
    }

    #[tool(description = "List all papers in the catalog directory")]
    fn catalog_list(&self) -> String {
        let mut entries = Vec::new();
        if let Ok(dir) = std::fs::read_dir(&self.catalog_dir) {
            for entry in dir.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "yaml") {
                    if let Some(stem) = path.file_stem() {
                        entries.push(stem.to_string_lossy().to_string());
                    }
                }
            }
        }
        entries.sort();
        if entries.is_empty() {
            "No catalog entries found.".to_string()
        } else {
            format!(
                "{} papers in catalog:\n{}",
                entries.len(),
                entries.join("\n")
            )
        }
    }
}

impl ServerHandler for HomeStillMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "home-still: Academic research tools. Search papers, download PDFs, \
                 convert to markdown, and browse the paper catalog.",
        )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let catalog_dir = hs_common::resolve_project_dir().join("catalog");
    let server = HomeStillMcp::new(catalog_dir);

    let transport = rmcp::transport::io::stdio();
    let ct = rmcp::service::serve_server(server, transport).await?;
    let _ = ct.waiting().await;

    Ok(())
}
