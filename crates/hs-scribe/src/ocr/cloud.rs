use super::region::RegionType;
use anyhow::{Context, Result};
use std::time::Duration;

/// How long a connection attempt to the cloud endpoint may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct CloudBackend {
    url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl CloudBackend {
    /// `request_timeout` bounds one whole recognize call (the reply is a
    /// single JSON document, not a stream), so a stalled endpoint cannot
    /// pin a VLM permit until the whole-convert deadline.
    pub fn new(url: &str, api_key: Option<String>, request_timeout: Duration) -> Result<Self> {
        let client = hs_common::http::client_builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(request_timeout)
            .build()
            .context("failed to build the cloud OCR HTTP client")?;
        Ok(Self {
            url: url.to_string(),
            api_key,
            client,
        })
    }

    pub async fn recognize(&self, image_bytes: &[u8]) -> Result<String> {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, image_bytes);
        let image_data = format!("data:image/jpeg;base64,{}", b64);

        let mut req = self.client.post(&self.url).json(&serde_json::json!({
            "image": image_data,
        }));

        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await?.error_for_status()?;
        let body: serde_json::Value = resp.json().await?;
        md_results(&body)
    }

    pub async fn recognize_region(
        &self,
        image_bytes: &[u8],
        region_type: RegionType,
    ) -> Result<String> {
        if region_type != RegionType::FullPage {
            tracing::warn!(
                "Cloud backend does not support task-specific prompts; \
                 falling back to generic OCR for {:?}",
                region_type
            );
        }
        self.recognize(image_bytes).await
    }
}

/// The recognized markdown of a cloud reply. A reply without `md_results`
/// is a failed call (an error document that arrived with a 200), not a
/// blank page.
fn md_results(body: &serde_json::Value) -> Result<String> {
    body.get("md_results")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .with_context(|| {
            let head: String = body.to_string().chars().take(200).collect();
            format!("cloud OCR reply has no `md_results` string: {head}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_with_md_results_is_the_text_even_when_blank() {
        assert_eq!(
            md_results(&serde_json::json!({"md_results": "# Title"})).unwrap(),
            "# Title"
        );
        assert_eq!(
            md_results(&serde_json::json!({"md_results": ""})).unwrap(),
            ""
        );
    }

    #[test]
    fn a_reply_without_md_results_is_an_error_not_an_empty_page() {
        for body in [
            serde_json::json!({"error": {"message": "quota exceeded"}}),
            serde_json::json!({"md_results": null}),
            serde_json::json!({"md_results": 7}),
            serde_json::json!([]),
        ] {
            assert!(md_results(&body).is_err(), "{body}");
        }
    }
}
