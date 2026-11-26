//! `personal_add`: ingest a file the user staged in the personal inbox.
//!
//! The one write into the `personal` store an agent can make, and it takes
//! a file name, never a path: the user puts the file in the inbox, the
//! agent names it. Deleting a personal document is `hs personal delete`,
//! on the command line only.

use rmcp::schemars;

use crate::{to_json, HomeStillMcp};

/// The arguments of `personal_add`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PersonalAddParams {
    #[schemars(
        description = "Filename inside the personal-store inbox (no path separators, no leading dot, no absolute paths). Drop the file under cfg.inbox_dir() yourself before calling."
    )]
    pub filename: String,
    #[schemars(
        description = "Override the LLM-picked category. Must be one of the configured categories."
    )]
    pub category: Option<String>,
    #[schemars(description = "Override the LLM-picked title.")]
    pub title: Option<String>,
    #[schemars(
        description = "If true, replace an existing document with the same stem or the same file content."
    )]
    #[serde(default)]
    pub force: bool,
}

impl HomeStillMcp {
    /// Run the same ingest as `hs personal add` on a file in the inbox.
    pub(crate) async fn add_personal_document(
        &self,
        p: PersonalAddParams,
    ) -> Result<String, String> {
        let cfg = personal::config::Config::load().map_err(|e| format!("{e:#}"))?;
        let path = personal::services::inbox::resolve_inbox_path(&cfg, &p.filename)
            .map_err(|e| format!("{e:#}"))?;
        let opts = personal::services::ingest::IngestOptions {
            category_override: p.category,
            title_override: p.title,
            force: p.force,
        };
        let outcome = personal::services::ingest::ingest(&cfg, &path, opts)
            .await
            .map_err(|e| format!("{e:#}"))?;
        to_json(&serde_json::json!({
            "store": hs_common::stores::PERSONAL,
            "doc_id": outcome.stem,
            "title": outcome.title,
            "category": outcome.category,
            "chunks_indexed": outcome.chunk_count,
            "source_file": p.filename,
        }))
    }
}
