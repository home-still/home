use crate::config::{AppConfig, BackendChoice};
use anyhow::Result;

use super::cloud::CloudBackend;
use super::ollama::OllamaBackend;
use super::openai_compatible::OpenAiBackend;
use super::region::RegionType;

pub enum OcrEngine {
    Ollama(OllamaBackend),
    Cloud(CloudBackend),
    OpenAi(OpenAiBackend),
}

impl OcrEngine {
    pub fn from_config(config: &AppConfig) -> Self {
        match config.backend {
            BackendChoice::Ollama => {
                OcrEngine::Ollama(OllamaBackend::new(&config.ollama_url, &config.model))
            }
            BackendChoice::Cloud => OcrEngine::Cloud(CloudBackend::new(
                &config.cloud_url,
                config.cloud_api_key.clone(),
            )),
            BackendChoice::OpenAi => {
                OcrEngine::OpenAi(OpenAiBackend::new(&config.openai_url, &config.model))
            }
        }
    }

    pub async fn recognize(&self, image_bytes: &[u8]) -> Result<String> {
        self.recognize_region(image_bytes, RegionType::FullPage).await
    }

    pub async fn recognize_region(
        &self,
        image_bytes: &[u8],
        region_type: RegionType,
    ) -> Result<String> {
        match self {
            OcrEngine::Ollama(backend) => backend.recognize_region(image_bytes, region_type).await,
            OcrEngine::Cloud(backend) => backend.recognize_region(image_bytes, region_type).await,
            OcrEngine::OpenAi(backend) => backend.recognize_region(image_bytes, region_type).await,
        }
    }
}
