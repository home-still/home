use thiserror::Error;

#[derive(Error, Debug)]
pub enum DistillError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Embedding error: {0}")]
    Embedding(String),

    #[error("Qdrant error: {0}")]
    Qdrant(String),

    #[error("Metadata extraction error: {0}")]
    Metadata(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}
