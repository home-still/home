use super::region::RegionType;
use anyhow::Result;

pub struct CloudBackend {
    url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl CloudBackend {
    pub fn new(url: &str, api_key: Option<String>) -> Self {
        Self {
            url: url.to_string(),
            api_key,
            client: reqwest::Client::new(),
        }
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

        let md = body
            .get("md_results")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        Ok(md)
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
