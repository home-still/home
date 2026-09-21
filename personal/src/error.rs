use thiserror::Error;

#[derive(Debug, Error)]
pub enum PersonalError {
    #[error("unsupported file extension: {0}")]
    UnsupportedExtension(String),

    #[error("duplicate document (stem '{0}' already exists); rerun with --force to replace")]
    DuplicateStem(String),

    #[error("converter error ({format}): {source}")]
    Converter {
        format: &'static str,
        #[source]
        source: anyhow::Error,
    },

    #[error("naming service error: {0}")]
    Naming(String),

    #[error("category '{0}' is not in the configured taxonomy")]
    UnknownCategory(String),

    #[error("indexing failed: {0}")]
    Index(String),

    #[error("config error: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, PersonalError>;
