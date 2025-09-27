use thiserror::Error;

/// Main error type for Mermaid
#[derive(Error, Debug)]
pub enum MermaidError {
    #[error("Model error: {0}")]
    ModelError(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Git error: {0}")]
    GitError(#[from] git2::Error),

    #[error("Context loading error: {0}")]
    ContextError(String),

    #[error("Agent execution error: {0}")]
    AgentError(String),

    #[error("UI error: {0}")]
    UIError(String),

    #[error("Network error: {0}")]
    NetworkError(String),

    #[error("API error: {0}")]
    ApiError(String),

    #[error("Unknown error: {0}")]
    Unknown(String),
}
