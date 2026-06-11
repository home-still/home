use crate::config::Config;
use crate::error::{PersonalError, Result};

/// PDF → markdown by handing the bytes to the existing scribe HTTP server.
/// Personal docs use the same VLM pipeline as papers; running PDFs locally in
/// the personal crate would mean a second VLM stack on the same host.
pub async fn convert(cfg: &Config, bytes: Vec<u8>, stem_hint: &str) -> Result<String> {
    let client = hs_scribe::client::ScribeClient::new(&cfg.scribe_url).map_err(|e| {
        PersonalError::Converter {
            format: "pdf",
            source: e,
        }
    })?;
    // Same streaming endpoint the paper pipeline uses (the non-streaming
    // /scribe path was deleted — one convert path). Progress events are
    // dropped; personal ingest has no UI to drive.
    let result = client
        .convert_with_progress(bytes, None, Some(stem_hint), |_| {})
        .await
        .map_err(|e| PersonalError::Converter {
            format: "pdf",
            source: e,
        })?;
    if result.markdown.trim().is_empty() {
        return Err(PersonalError::Converter {
            format: "pdf",
            source: anyhow::anyhow!("scribe returned empty markdown"),
        });
    }
    Ok(result.markdown)
}
