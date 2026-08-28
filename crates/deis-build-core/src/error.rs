use thiserror::Error;

#[derive(Error, Debug)]
pub enum DeniError {
    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Validation error: {0}")]
    ValidationError(String),

    #[error("Compilation error: {0}")]
    CompilationError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, DeniError>;
