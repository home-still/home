use figment::{
    providers::{Env, Format, Serialized, Yaml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum BackendChoice {
    #[default]
    Ollama,
    Cloud,
    OpenAi,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum PipelineMode {
    FullPage,
    #[default]
    PerRegion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub ollama_url: String,
    pub model: String,
    pub cloud_api_key: Option<String>,
    pub cloud_url: String,
    pub openai_url: String,
    pub backend: BackendChoice,
    pub timeout_secs: u64,
    pub dpi: u16,
    pub parallel: usize,
    pub pipeline_mode: PipelineMode,
    pub layout_model_path: String,
    pub table_model_path: String,
    pub region_parallel: usize,
    pub use_cuda: bool,
    pub max_image_dim: u32,
    pub vlm_concurrency: usize,
}
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            ollama_url: "http://localhost:11434".into(),
            model: "glm-ocr:latest".into(),
            cloud_api_key: None,
            cloud_url: "https://api.z.ai/api/paas/v4/layout_parsing".into(),
            openai_url: "http://localhost:8080".into(),
            backend: BackendChoice::Ollama,
            timeout_secs: 120,
            dpi: 200,
            parallel: 1,
            pipeline_mode: PipelineMode::PerRegion,
            layout_model_path: "pp-doclayoutv3.onnx".into(),
            table_model_path: "slanet-plus.onnx".into(),
            region_parallel: 4,
            use_cuda: true,
            max_image_dim: 1800,
            vlm_concurrency: 4,
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self, Box<figment::Error>> {
        let config_path = dirs::config_dir()
            .map(|d| d.join("home-still").join("config.yaml"))
            .unwrap_or_default();

        Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Yaml::file_exact(config_path).nested())
            .merge(Env::prefixed("HS_SCRIBE_"))
            .extract()
            .map_err(Box::new)
    }

    /// Resolve a model filename to an absolute path.
    /// If already absolute and exists, use as-is.                          
    /// Otherwise look in `~/.local/share/home-still/models/`.              
    pub fn resolve_model_path(name: &str) -> PathBuf {
        let p = PathBuf::from(name);
        if p.is_absolute() && p.exists() {
            return p;
        }
        if p.exists() {
            return p;
        }
        let data_dir = dirs::data_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
            .join("home-still")
            .join("models");
        data_dir.join(name)
    }

    pub fn resolved_layout_model_path(&self) -> PathBuf {
        Self::resolve_model_path(&self.layout_model_path)
    }

    pub fn resolved_table_model_path(&self) -> PathBuf {
        Self::resolve_model_path(&self.table_model_path)
    }
}
