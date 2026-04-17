//! Comprehensive error types for the model system
//!
//! Replaces scattered anyhow::Error usage with structured, actionable errors
//! that enable proper recovery, retry logic, and user-friendly messages.

use serde::{Deserialize, Serialize};
use std::fmt;

/// User-facing error information with actionable suggestions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserFacingError {
    /// Short summary for status bar (e.g., "Connection failed")
    pub summary: String,
    /// Detailed message for chat display
    pub message: String,
    /// Actionable suggestion for the user
    pub suggestion: String,
    /// Error category for styling/icons
    pub category: ErrorCategory,
    /// Whether this error is recoverable (user can retry)
    pub recoverable: bool,
}

/// Error categories for visual differentiation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCategory {
    /// Connection/network issues
    Connection,
    /// Authentication/authorization issues
    Auth,
    /// Configuration issues
    Config,
    /// Resource not found
    NotFound,
    /// Temporary issue (rate limit, timeout)
    Temporary,
    /// Internal/unexpected error
    Internal,
}

/// Top-level error type for all model operations
#[derive(Debug)]
pub enum ModelError {
    /// Backend-specific error (connection, API, etc)
    Backend(BackendError),

    /// Configuration error (invalid settings, missing keys, etc)
    Config(ConfigError),

    /// Model not found or unavailable
    ModelNotFound {
        model: String,
        searched: Vec<String>,
    },

    /// Request timeout
    Timeout {
        operation: String,
        duration_secs: u64,
    },

    /// Rate limit exceeded
    RateLimit { retry_after: Option<u64> },

    /// Invalid request (malformed input, bad parameters)
    InvalidRequest(String),

    /// Response parsing error
    ParseError {
        message: String,
        raw: Option<String>,
    },

    /// Stream error (connection dropped, incomplete response)
    StreamError(String),

    /// Authentication error
    Authentication(String),

    /// The adapter does not implement the requested feature (e.g. an
    /// Anthropic adapter has no `list_models` endpoint, so the trait's
    /// default impl returns this).
    Unsupported { feature: String },
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelError::Backend(e) => write!(f, "Backend error: {}", e),
            ModelError::Config(e) => write!(f, "Configuration error: {}", e),
            ModelError::ModelNotFound { model, searched } => {
                write!(
                    f,
                    "Model '{}' not found. Searched: {}",
                    model,
                    searched.join(", ")
                )
            },
            ModelError::Timeout {
                operation,
                duration_secs,
            } => {
                if *duration_secs == 0 {
                    write!(f, "Operation '{}' timed out", operation)
                } else {
                    write!(
                        f,
                        "Operation '{}' timed out after {} seconds",
                        operation, duration_secs
                    )
                }
            },
            ModelError::RateLimit { retry_after } => {
                if let Some(secs) = retry_after {
                    write!(f, "Rate limit exceeded. Retry after {} seconds", secs)
                } else {
                    write!(f, "Rate limit exceeded")
                }
            },
            ModelError::InvalidRequest(msg) => write!(f, "Invalid request: {}", msg),
            ModelError::ParseError { message, raw } => {
                if let Some(r) = raw {
                    write!(f, "Parse error: {} (raw: {})", message, r)
                } else {
                    write!(f, "Parse error: {}", message)
                }
            },
            ModelError::StreamError(msg) => write!(f, "Stream error: {}", msg),
            ModelError::Authentication(msg) => write!(f, "Authentication error: {}", msg),
            ModelError::Unsupported { feature } => {
                write!(f, "Feature not supported by this adapter: {}", feature)
            },
        }
    }
}

impl std::error::Error for ModelError {}

impl ModelError {
    /// Convert to user-facing error with actionable suggestions
    pub fn to_user_facing(&self) -> UserFacingError {
        match self {
            ModelError::Backend(BackendError::ConnectionFailed { backend, url, .. }) => {
                UserFacingError {
                    summary: format!("{} connection failed", backend),
                    message: format!("Could not connect to {} at {}", backend, url),
                    suggestion: if backend == "ollama" {
                        "Run 'ollama serve' to start Ollama, or check if it's running on the correct port".to_string()
                    } else {
                        format!("Check if {} is running and accessible", backend)
                    },
                    category: ErrorCategory::Connection,
                    recoverable: true,
                }
            },
            ModelError::Backend(BackendError::NotAvailable { backend, reason }) => {
                UserFacingError {
                    summary: format!("{} unavailable", backend),
                    message: format!("{} is not available: {}", backend, reason),
                    suggestion: if backend == "ollama" {
                        "Start Ollama with 'ollama serve' or pull the model with 'ollama pull <model>'".to_string()
                    } else {
                        format!("Ensure {} service is running and healthy", backend)
                    },
                    category: ErrorCategory::Connection,
                    recoverable: true,
                }
            },
            ModelError::Backend(BackendError::HttpError { status, message }) => {
                let (summary, suggestion) = match status {
                    401 | 403 => (
                        "Authentication failed",
                        "Check your API key in ~/.config/mermaid/config.toml",
                    ),
                    404 => (
                        "Model not found",
                        "Use :model <name> to switch models (auto-pulls if needed), or pull manually with 'ollama pull <name>'",
                    ),
                    429 => (
                        "Rate limited",
                        "Wait a moment before retrying, or switch to a local model",
                    ),
                    500..=599 => (
                        "Server error",
                        "The backend service is experiencing issues - try again later",
                    ),
                    _ => (
                        "Request failed",
                        "Check your network connection and backend configuration",
                    ),
                };
                UserFacingError {
                    summary: summary.to_string(),
                    message: format!("HTTP {}: {}", status, message),
                    suggestion: suggestion.to_string(),
                    category: if *status == 401 || *status == 403 {
                        ErrorCategory::Auth
                    } else if *status == 429 {
                        ErrorCategory::Temporary
                    } else {
                        ErrorCategory::Internal
                    },
                    recoverable: *status == 429 || *status >= 500,
                }
            },
            ModelError::Backend(BackendError::UnexpectedResponse { backend, message }) => {
                UserFacingError {
                    summary: "Unexpected response".to_string(),
                    message: format!("Received unexpected response from {}: {}", backend, message),
                    suggestion: "This might be a version mismatch - try updating the backend"
                        .to_string(),
                    category: ErrorCategory::Internal,
                    recoverable: false,
                }
            },
            ModelError::Backend(BackendError::ProviderError {
                provider,
                code,
                message,
            }) => {
                let code_str = code.as_deref().unwrap_or("unknown");
                UserFacingError {
                    summary: format!("{} error", provider),
                    message: format!("{} returned error {}: {}", provider, code_str, message),
                    suggestion: format!(
                        "Check {} documentation for error code {}",
                        provider, code_str
                    ),
                    category: ErrorCategory::Internal,
                    recoverable: false,
                }
            },
            ModelError::Config(ConfigError::MissingRequired(field)) => UserFacingError {
                summary: "Missing configuration".to_string(),
                message: format!("Required configuration '{}' is missing", field),
                suggestion: format!("Add '{}' to ~/.config/mermaid/config.toml", field),
                category: ErrorCategory::Config,
                recoverable: false,
            },
            ModelError::Config(ConfigError::InvalidValue {
                field,
                value,
                reason,
            }) => UserFacingError {
                summary: "Invalid configuration".to_string(),
                message: format!("Invalid value '{}' for '{}': {}", value, field, reason),
                suggestion: format!("Fix '{}' in ~/.config/mermaid/config.toml", field),
                category: ErrorCategory::Config,
                recoverable: false,
            },
            ModelError::Config(ConfigError::FileError { path, reason }) => UserFacingError {
                summary: "Config file error".to_string(),
                message: format!("Cannot read config file '{}': {}", path, reason),
                suggestion: "Check file permissions and syntax".to_string(),
                category: ErrorCategory::Config,
                recoverable: false,
            },
            ModelError::ModelNotFound { model, searched } => UserFacingError {
                summary: "Model not found".to_string(),
                message: format!("Model '{}' not found in: {}", model, searched.join(", ")),
                suggestion: format!(
                    "Pull the model with 'ollama pull {}' or check if the model name is correct",
                    model
                ),
                category: ErrorCategory::NotFound,
                recoverable: false,
            },
            ModelError::Timeout {
                operation,
                duration_secs,
            } => UserFacingError {
                summary: "Request timed out".to_string(),
                message: if *duration_secs == 0 {
                    format!("'{}' timed out", operation)
                } else {
                    format!("'{}' timed out after {} seconds", operation, duration_secs)
                },
                suggestion: "The model might be overloaded - try a smaller model or wait and retry"
                    .to_string(),
                category: ErrorCategory::Temporary,
                recoverable: true,
            },
            ModelError::RateLimit { retry_after } => {
                let wait_msg = retry_after
                    .map(|s| format!("Wait {} seconds", s))
                    .unwrap_or_else(|| "Wait a moment".to_string());
                UserFacingError {
                    summary: "Rate limited".to_string(),
                    message: "Too many requests - rate limit exceeded".to_string(),
                    suggestion: format!(
                        "{}. Consider using a local Ollama model to avoid rate limits",
                        wait_msg
                    ),
                    category: ErrorCategory::Temporary,
                    recoverable: true,
                }
            },
            ModelError::InvalidRequest(msg) => UserFacingError {
                summary: "Invalid request".to_string(),
                message: format!("The request was invalid: {}", msg),
                suggestion: "Check your message format or try rephrasing".to_string(),
                category: ErrorCategory::Internal,
                recoverable: false,
            },
            ModelError::ParseError { message, .. } => UserFacingError {
                summary: "Parse error".to_string(),
                message: format!("Failed to parse response: {}", message),
                suggestion:
                    "The model returned an unexpected format - try sending the message again"
                        .to_string(),
                category: ErrorCategory::Internal,
                recoverable: true,
            },
            ModelError::StreamError(msg) => UserFacingError {
                summary: "Stream interrupted".to_string(),
                message: format!("Connection lost during streaming: {}", msg),
                suggestion: "Check your network connection and try again".to_string(),
                category: ErrorCategory::Connection,
                recoverable: true,
            },
            ModelError::Authentication(msg) => UserFacingError {
                summary: "Authentication failed".to_string(),
                message: format!("Authentication error: {}", msg),
                suggestion:
                    "Check your API key in ~/.config/mermaid/config.toml or environment variables"
                        .to_string(),
                category: ErrorCategory::Auth,
                recoverable: false,
            },
            ModelError::Unsupported { feature } => UserFacingError {
                summary: "Unsupported feature".to_string(),
                message: format!("The current model adapter does not support '{}'.", feature),
                suggestion: format!(
                    "Switch to a provider/model that supports '{}', or omit this operation.",
                    feature
                ),
                category: ErrorCategory::Internal,
                recoverable: false,
            },
        }
    }
}

/// Backend-specific errors
#[derive(Debug)]
pub enum BackendError {
    /// Connection failed (network, DNS, etc)
    ConnectionFailed {
        backend: String,
        url: String,
        reason: String,
    },

    /// Backend not available (not running, health check failed)
    NotAvailable { backend: String, reason: String },

    /// HTTP error from backend
    HttpError { status: u16, message: String },

    /// Backend returned unexpected response format
    UnexpectedResponse { backend: String, message: String },

    /// Provider-specific error
    ProviderError {
        provider: String,
        code: Option<String>,
        message: String,
    },
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::ConnectionFailed {
                backend,
                url,
                reason,
            } => {
                write!(f, "Failed to connect to {} at {}: {}", backend, url, reason)
            },
            BackendError::NotAvailable { backend, reason } => {
                write!(f, "Backend '{}' not available: {}", backend, reason)
            },
            BackendError::HttpError { status, message } => {
                write!(f, "HTTP error {}: {}", status, message)
            },
            BackendError::UnexpectedResponse { backend, message } => {
                write!(f, "Unexpected response from {}: {}", backend, message)
            },
            BackendError::ProviderError {
                provider,
                code,
                message,
            } => {
                if let Some(c) = code {
                    write!(f, "{} error {}: {}", provider, c, message)
                } else {
                    write!(f, "{} error: {}", provider, message)
                }
            },
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
    InvalidValue {
        field: String,
        value: String,
        reason: String,
    },

    /// File operation error (read, parse, etc)
    FileError { path: String, reason: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::MissingRequired(field) => {
                write!(f, "Missing required configuration: {}", field)
            },
            ConfigError::InvalidValue {
                field,
                value,
                reason,
            } => {
                write!(f, "Invalid value for '{}': '{}' ({})", field, value, reason)
            },
            ConfigError::FileError { path, reason } => {
                write!(f, "Error reading config file '{}': {}", path, reason)
            },
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
            // reqwest::Error doesn't expose the actual elapsed duration,
            // and the adapter only sets a connect_timeout (no global
            // request timeout), so there is no truthful number to report.
            // 0 is a sentinel meaning "unknown" — the Display and
            // to_user_facing impls for ModelError::Timeout omit the
            // "after N seconds" suffix when duration_secs == 0.
            ModelError::Timeout {
                operation: "HTTP request".to_string(),
                duration_secs: 0,
            }
        } else if err.is_connect() {
            ModelError::Backend(BackendError::ConnectionFailed {
                backend: "unknown".to_string(),
                url: err
                    .url()
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_display_omits_zero_duration() {
        let err = ModelError::Timeout {
            operation: "HTTP request".to_string(),
            duration_secs: 0,
        };
        let rendered = err.to_string();
        assert_eq!(rendered, "Operation 'HTTP request' timed out");
        assert!(!rendered.contains("0 seconds"));
    }

    #[test]
    fn timeout_display_shows_nonzero_duration() {
        let err = ModelError::Timeout {
            operation: "HTTP request".to_string(),
            duration_secs: 45,
        };
        let rendered = err.to_string();
        assert_eq!(
            rendered,
            "Operation 'HTTP request' timed out after 45 seconds"
        );
    }

    #[test]
    fn timeout_user_facing_omits_zero_duration() {
        let err = ModelError::Timeout {
            operation: "HTTP request".to_string(),
            duration_secs: 0,
        };
        let ufe = err.to_user_facing();
        assert_eq!(ufe.message, "'HTTP request' timed out");
        assert!(!ufe.message.contains("0 seconds"));
    }
}
