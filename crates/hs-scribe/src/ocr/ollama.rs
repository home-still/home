use super::region::RegionType;
use anyhow::Result;
use ollama_rs::{
    generation::completion::request::GenerationRequest, generation::images::Image,
    models::ModelOptions, Ollama,
};
use reqwest::Url;

pub struct OllamaBackend {
    client: Ollama,
    model: String,
}

impl OllamaBackend {
    pub fn new(url: &str, model: &str) -> Self {
        if std::env::var("OLLAMA_NUM_PARALLEL").is_err() {
            tracing::warn!(
                "OLLAMA_NUM_PARALLEL is not set. Set it to 2 when starting Ollama \
                 for parallel processing: OLLAMA_NUM_PARALLEL=2 ollama serve"
            );
        }
        let parsed =
            Url::parse(url).unwrap_or_else(|_| Url::parse("http://localhost:11434").unwrap());
        let host = format!(
            "{}://{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or("localhost")
        );
        let port = parsed.port().unwrap_or(11434);
        Self {
            client: Ollama::new(host, port),
            model: model.to_string(),
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
        let image = Image::from_base64(&b64);

        // num_ctx=8192 is the minimum safe value: image tokens consume ~4672 for a
        // 200 DPI letter page, plus ~500 prompt tokens, plus output headroom.
        // num_ctx=4096 silently truncates the image and produces garbage.
        let options = ModelOptions::default()
            .temperature(0.0)
            .num_predict(4096)
            .num_ctx(8192);

        let request = GenerationRequest::new(self.model.clone(), region_type.prompt().to_string())
            .images(vec![image])
            .options(options);

        let response = self
            .client
            .generate(request)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(response.response)
    }
}
