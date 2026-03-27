use super::region::RegionType;
use anyhow::Result;

pub struct OpenAiBackend {
    client: reqwest::Client,
    url: String,
    model: String,
}

impl OpenAiBackend {
    pub fn new(url: &str, model: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.trim_end_matches('/').to_string(),
            model: model.strip_suffix(":latest").unwrap_or(model).to_string(),
        }
    }

    pub async fn recognize(&self, image_bytes: &[u8]) -> Result<String> {
        self.recognize_region(image_bytes, RegionType::FullPage)
            .await
    }

    pub async fn recognize_region(
        &self,
        image_bytes: &[u8],
        region_type: RegionType,
    ) -> Result<String> {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, image_bytes);
        let image_url = format!("data:image/jpeg;base64,{}", b64);

        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "image_url",
                            "image_url": { "url": image_url }
                        },
                        {
                            "type": "text",
                            "text": region_type.prompt()
                        }
                    ]
                }
            ],
            "temperature": 0.0,
            "max_tokens": 8192,
            "repetition_penalty": 1.1,
            "top_k": 40,
            "top_p": 0.9
        });

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.url))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let json: serde_json::Value = resp.json().await?;

        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        Ok(content)
    }
}
