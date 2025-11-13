/// Comprehensive error types for the model system
///
/// Replaces scattered anyhow::Error usage with structured, actionable errors
/// that enable proper recovery, retry logic, and user-friendly messages.

use std::fmt;

/// Top-level error type for all model operations
#[derive(Debug)]
pub enum ModelError {
    /// Backend-specific error (connection, API, etc)
    Backend(BackendError),

    /// Configuration error (invalid settings, missing keys, etc)
    Config(ConfigError),

    /// Model not found or unavailable
    ModelNotFound { model: String, searched: Vec<String> },

    /// Request timeout
    Timeout { operation: String, duration_secs: u64 },

    /// Rate limit exceeded
    RateLimit { retry_after: Option<u64> },

    /// Invalid request (malformed input, bad parameters)
    InvalidRequest(String),

    /// Response parsing error
    ParseError { message: String, raw: Option<String> },

    /// Stream error (connection dropped, incomplete response)
    StreamError(String),

    /// Authentication error
    Authentication(String),
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelError::Backend(e) => write!(f, "Backend error: {}", e),
            ModelError::Config(e) => write!(f, "Configuration error: {}", e),
            ModelError::ModelNotFound { model, searched } => {
                write!(f, "Model '{}' not found. Searched: {}", model, searched.join(", "))
            }
            ModelError::Timeout { operation, duration_secs } => {
                write!(f, "Operation '{}' timed out after {} seconds", operation, duration_secs)
            }
            ModelError::RateLimit { retry_after } => {
                if let Some(secs) = retry_after {
                    write!(f, "Rate limit exceeded. Retry after {} seconds", secs)
                } else {
                    write!(f, "Rate limit exceeded")
                }
            }
            ModelError::InvalidRequest(msg) => write!(f, "Invalid request: {}", msg),
            ModelError::ParseError { message, raw } => {
                if let Some(r) = raw {
                    write!(f, "Parse error: {} (raw: {})", message, r)
                } else {
                    write!(f, "Parse error: {}", message)
                }
            }
            ModelError::StreamError(msg) => write!(f, "Stream error: {}", msg),
            ModelError::Authentication(msg) => write!(f, "Authentication error: {}", msg),
        }
    }
}

impl std::error::Error for ModelError {}

/// Backend-specific errors
#[derive(Debug)]
pub enum BackendError {
    /// Connection failed (network, DNS, etc)
    ConnectionFailed { backend: String, url: String, reason: String },

    /// Backend not available (not running, health check failed)
    NotAvailable { backend: String, reason: String },

    /// HTTP error from backend
    HttpError { status: u16, message: String },

    /// Backend returned unexpected response format
    UnexpectedResponse { backend: String, message: String },

    /// Provider-specific error
    ProviderError { provider: String, code: Option<String>, message: String },
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::ConnectionFailed { backend, url, reason } => {
                write!(f, "Failed to connect to {} at {}: {}", backend, url, reason)
            }
            BackendError::NotAvailable { backend, reason } => {
                write!(f, "Backend '{}' not available: {}", backend, reason)
            }
            BackendError::HttpError { status, message } => {
                write!(f, "HTTP error {}: {}", status, message)
            }
            BackendError::UnexpectedResponse { backend, message } => {
                write!(f, "Unexpected response from {}: {}", backend, message)
            }
            BackendError::ProviderError { provider, code, message } => {
                if let Some(c) = code {
                    write!(f, "{} error {}: {}", provider, c, message)
                } else {
                    write!(f, "{} error: {}", provider, message)
                }
            }
        }
    }
}

impl std::error::Error for BackendError {}

/// Configuration errors
#[derive(Debug)]
pub enum ConfigError {
    /// Missing required configuration
    MissingRequired(String),

    /// Invalid value for configuration
    InvalidValue { field: String, value: String, reason: String },

    /// File operation error (read, parse, etc)
    FileError { path: String, reason: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::MissingRequired(field) => {
                write!(f, "Missing required configuration: {}", field)
            }
            ConfigError::InvalidValue { field, value, reason } => {
                write!(f, "Invalid value for '{}': '{}' ({})", field, value, reason)
            }
            ConfigError::FileError { path, reason } => {
                write!(f, "Error reading config file '{}': {}", path, reason)
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Result type alias for model operations
pub type Result<T> = std::result::Result<T, ModelError>;

/// Conversion from anyhow::Error (for gradual migration)
impl From<anyhow::Error> for ModelError {
    fn from(err: anyhow::Error) -> Self {
        ModelError::InvalidRequest(err.to_string())
    }
}

/// Conversion from reqwest::Error
impl From<reqwest::Error> for ModelError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            ModelError::Timeout {
                operation: "HTTP request".to_string(),
                duration_secs: 120,
            }
        } else if err.is_connect() {
            ModelError::Backend(BackendError::ConnectionFailed {
                backend: "unknown".to_string(),
                url: err.url().map(|u| u.to_string()).unwrap_or_else(|| "unknown".to_string()),
                reason: err.to_string(),
            })
        } else if err.is_status() {
            let status = err.status().map(|s| s.as_u16()).unwrap_or(500);
            ModelError::Backend(BackendError::HttpError {
                status,
                message: err.to_string(),
            })
        } else {
            ModelError::Backend(BackendError::UnexpectedResponse {
                backend: "unknown".to_string(),
                message: err.to_string(),
            })
        }
    }
}

/// Conversion from serde_json::Error
impl From<serde_json::Error> for ModelError {
    fn from(err: serde_json::Error) -> Self {
        ModelError::ParseError {
            message: err.to_string(),
            raw: None,
        }
    }
}
