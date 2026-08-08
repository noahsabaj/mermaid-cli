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

    /// Rate limit exceeded. `retry_after` is the server's `Retry-After` in
    /// seconds when it sent one; `message` is the human-readable reason from
    /// the 429 response body when one could be extracted (e.g. Cloudflare's
    /// "used up your daily free allocation of 10,000 neurons") — the
    /// difference between "wait a moment" and "upgrade your plan".
    RateLimit {
        retry_after: Option<u64>,
        message: Option<String>,
    },

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

    /// The provider call was aborted by the turn's cancellation
    /// token. The effect runner swallows this silently — the
    /// terminal `Msg::TurnCancelled` is emitted from `drop_scope`
    /// after the scope's `JoinSet` drains, so no `UpstreamError`
    /// should reach the reducer for cancelled turns.
    Cancelled,
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
            ModelError::RateLimit {
                retry_after,
                message,
            } => {
                write!(f, "Rate limit exceeded")?;
                if let Some(secs) = retry_after {
                    write!(f, " (retry after {} seconds)", secs)?;
                }
                if let Some(reason) = message {
                    write!(f, ": {}", reason)?;
                }
                Ok(())
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
            ModelError::Cancelled => write!(f, "Cancelled by user"),
        }
    }
}

impl std::error::Error for ModelError {}

impl ModelError {
    /// Convert to user-facing error with actionable suggestions
    #[expect(
        clippy::too_many_lines,
        reason = "predates the lint; see .github/baselines/expect_budget.txt"
    )]
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
            ModelError::Backend(BackendError::HttpError {
                status,
                message,
                debug,
            }) => {
                let (summary, suggestion) = match status {
                    401 | 403 => (
                        "Authentication failed",
                        "Check your API key in ~/.config/mermaid/config.toml",
                    ),
                    404 => (
                        "Model not found",
                        "Use /model <name> to switch models (auto-pulls if needed), or pull manually with 'ollama pull <name>'",
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
                // Body may be a raw JSON blob from the provider (e.g., Ollama
                // Cloud emits `{"error":"Internal Server Error (ref: ...)"}`).
                // Render the extracted message when we can, fall back to the
                // raw body so we never lose information.
                let rendered = match try_extract_error_message(message) {
                    Some(clean) => format!("HTTP {}: {}", status, clean),
                    None => format!("HTTP {}: {}", status, message),
                };
                UserFacingError {
                    summary: summary.to_string(),
                    message: debug.suffix(rendered),
                    suggestion: suggestion.to_string(),
                    // 5xx errors ARE recoverable (the caller can retry) and
                    // the suggestion tells the user to try again — that's
                    // the `Temporary` category semantic. `Internal` was
                    // wrong and painted the status bar with a sterner tone
                    // than the situation warrants.
                    category: if *status == 401 || *status == 403 {
                        ErrorCategory::Auth
                    } else if *status == 429 || (500..=599).contains(status) {
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
                debug,
            }) => {
                let code_str = code.as_deref().unwrap_or("unknown");
                UserFacingError {
                    summary: format!("{} error", provider),
                    message: debug.suffix(format!(
                        "{} returned error {}: {}",
                        provider, code_str, message
                    )),
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
            ModelError::RateLimit {
                retry_after,
                message,
            } => {
                let wait_msg = retry_after
                    .map(|s| format!("Wait {} seconds and retry", s))
                    .unwrap_or_else(|| {
                        "This can be a burst limit (retry shortly) or an exhausted quota"
                            .to_string()
                    });
                UserFacingError {
                    summary: "Rate limited".to_string(),
                    // Prefer the provider's own explanation (it distinguishes
                    // "slow down" from "your daily quota is spent") over the
                    // generic phrasing.
                    message: message.clone().unwrap_or_else(|| {
                        "The provider rejected the request with 429 (too many requests)".to_string()
                    }),
                    suggestion: format!("{}. Local Ollama models have no rate limits", wait_msg),
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
            ModelError::Cancelled => UserFacingError {
                summary: "Cancelled".to_string(),
                message: "The request was cancelled.".to_string(),
                suggestion: String::new(),
                category: ErrorCategory::Temporary,
                recoverable: true,
            },
        }
    }
}

/// Correlation ids captured from a provider's HTTP response headers.
/// Appended (one plain-text line) to the user-facing error message so bug
/// reports to the provider can quote them; deliberately NOT part of
/// `Display`, which feeds logs and `try_extract_error_message`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResponseDebugContext {
    /// Provider request id: first present of `x-request-id`, `request-id`,
    /// `anthropic-request-id`.
    pub request_id: Option<String>,
    /// Cloudflare ray id (`cf-ray`) — identifies the edge PoP + request for
    /// providers fronted by Cloudflare.
    pub cf_ray: Option<String>,
}

impl ResponseDebugContext {
    /// Capture correlation ids from response headers. Cheap; call before
    /// consuming the body (`.text()` takes the response by value).
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let captured = Self {
            request_id: ["x-request-id", "request-id", "anthropic-request-id"]
                .iter()
                .find_map(|n| get(n)),
            cf_ray: get("cf-ray"),
        };
        if !captured.is_empty() {
            // Feeds the TRACE ring so `--trace` runs correlate provider-side.
            tracing::trace!(
                request_id = ?captured.request_id,
                cf_ray = ?captured.cf_ray,
                "captured provider response ids"
            );
        }
        captured
    }

    pub fn is_empty(&self) -> bool {
        self.request_id.is_none() && self.cf_ray.is_none()
    }

    /// The `(request-id: ..., cf-ray: ...)` suffix line, or `None` when
    /// nothing was captured.
    fn render(&self) -> Option<String> {
        let parts: Vec<String> = [
            self.request_id
                .as_ref()
                .map(|id| format!("request-id: {id}")),
            self.cf_ray.as_ref().map(|ray| format!("cf-ray: {ray}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if parts.is_empty() {
            None
        } else {
            Some(format!("({})", parts.join(", ")))
        }
    }

    /// Append the rendered id line to a user-facing message when present.
    fn suffix(&self, message: String) -> String {
        match self.render() {
            Some(line) => format!("{message}\n{line}"),
            None => message,
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
    HttpError {
        status: u16,
        message: String,
        /// Response-header correlation ids (empty when the error was not
        /// built from an HTTP response).
        debug: ResponseDebugContext,
    },

    /// Backend returned unexpected response format
    UnexpectedResponse { backend: String, message: String },

    /// Provider-specific error
    ProviderError {
        provider: String,
        code: Option<String>,
        message: String,
        /// Response-header correlation ids (empty when the error was not
        /// built from an HTTP response).
        debug: ResponseDebugContext,
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
            // `debug` ids are deliberately NOT printed here: Display feeds
            // logs and try_extract_error_message; the ids surface once, in
            // to_user_facing.
            BackendError::HttpError {
                status, message, ..
            } => {
                write!(f, "HTTP error {}: {}", status, message)
            },
            BackendError::UnexpectedResponse { backend, message } => {
                write!(f, "Unexpected response from {}: {}", backend, message)
            },
            BackendError::ProviderError {
                provider,
                code,
                message,
                ..
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
                debug: ResponseDebugContext::default(),
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

/// Try to extract a human-readable error message from a raw upstream
/// response body. Handles the two shapes observed in the wild across
/// Ollama, OpenAI, Groq, OpenRouter, Cerebras, DeepInfra, Together
/// (Anthropic + Gemini have their own adapter-level parsers):
///
/// - `{"error": "some string"}` — Ollama Cloud style
/// - `{"error": {"message": "...", ...}}` — OpenAI Chat Completions style
///
/// Returns `None` when the body isn't parseable JSON or doesn't match
/// either shape — callers fall back to the raw body so no information
/// is lost.
fn try_extract_error_message(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let error = value.get("error")?;

    // Shape 1: `error` is a plain string.
    if let Some(s) = error.as_str() {
        return Some(s.trim().to_string());
    }

    // Shape 2: `error` is an object with a `message` field. Prepend
    // `type:` if present (matches OpenAI's `"invalid_request_error"` +
    // message convention).
    if let Some(obj) = error.as_object() {
        let message = obj.get("message").and_then(|v| v.as_str())?;
        let kind = obj
            .get("type")
            .and_then(|v| v.as_str())
            .or_else(|| obj.get("code").and_then(|v| v.as_str()));
        let out = match kind {
            Some(k) if !k.is_empty() => format!("{}: {}", k, message),
            _ => message.to_string(),
        };
        return Some(out.trim().to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn debug_context_captures_each_request_id_alias() {
        for alias in ["x-request-id", "request-id", "anthropic-request-id"] {
            let debug = ResponseDebugContext::from_headers(&headers(&[(alias, "req_123")]));
            assert_eq!(debug.request_id.as_deref(), Some("req_123"), "{alias}");
        }
        // Precedence: x-request-id beats the later aliases.
        let debug = ResponseDebugContext::from_headers(&headers(&[
            ("anthropic-request-id", "anth"),
            ("x-request-id", "xreq"),
        ]));
        assert_eq!(debug.request_id.as_deref(), Some("xreq"));
        // cf-ray captured independently; absent headers -> empty.
        let debug = ResponseDebugContext::from_headers(&headers(&[("cf-ray", "8f3a-EWR")]));
        assert_eq!(debug.cf_ray.as_deref(), Some("8f3a-EWR"));
        assert!(debug.request_id.is_none());
        assert!(ResponseDebugContext::from_headers(&headers(&[])).is_empty());
    }

    #[test]
    fn user_facing_appends_ids_display_does_not() {
        let debug = ResponseDebugContext {
            request_id: Some("req_abc".to_string()),
            cf_ray: Some("ray_1".to_string()),
        };
        let err = ModelError::Backend(BackendError::HttpError {
            status: 500,
            message: "boom".to_string(),
            debug: debug.clone(),
        });
        let ufe = err.to_user_facing();
        assert!(
            ufe.message
                .ends_with("(request-id: req_abc, cf-ray: ray_1)"),
            "got: {}",
            ufe.message
        );
        // Display feeds logs + try_extract_error_message: no ids there.
        assert!(!err.to_string().contains("req_abc"));

        let err = ModelError::Backend(BackendError::ProviderError {
            provider: "anthropic".to_string(),
            code: Some("api_error".to_string()),
            message: "boom".to_string(),
            debug,
        });
        let ufe = err.to_user_facing();
        assert!(ufe.message.contains("(request-id: req_abc, cf-ray: ray_1)"));
        assert!(!err.to_string().contains("req_abc"));

        // Empty debug adds nothing (no trailing blank line).
        let err = ModelError::Backend(BackendError::HttpError {
            status: 500,
            message: "boom".to_string(),
            debug: ResponseDebugContext::default(),
        });
        let msg = err.to_user_facing().message;
        assert!(!msg.contains("request-id"));
        assert!(!msg.ends_with('\n'));
    }

    #[test]
    fn redaction_leaves_the_id_line_intact() {
        // The `(request-id: ...)` line must survive the secret scrubber —
        // pinned so a future redaction pattern can't silently eat it.
        let line = "HTTP 500: boom\n(request-id: req_0aF3kZ9xQ, cf-ray: 8f3ab2cd4e-EWR)";
        assert_eq!(crate::utils::redact_secrets(line), line);
    }

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

    #[test]
    fn extract_error_handles_ollama_string_shape() {
        let body = r#"{"error":"Internal Server Error (ref: 6e8ae4c7)"}"#;
        assert_eq!(
            try_extract_error_message(body).as_deref(),
            Some("Internal Server Error (ref: 6e8ae4c7)")
        );
    }

    #[test]
    fn extract_error_handles_openai_object_shape_with_type() {
        let body = r#"{"error":{"message":"Rate limit","type":"rate_limit_error","code":null}}"#;
        assert_eq!(
            try_extract_error_message(body).as_deref(),
            Some("rate_limit_error: Rate limit")
        );
    }

    /// OpenRouter emits `code` as a numeric HTTP status, not a string.
    /// `as_str()` returns None so we skip the prefix gracefully.
    #[test]
    fn extract_error_handles_openrouter_numeric_code() {
        let body = r#"{"error":{"message":"upstream timeout","code":504,"metadata":{}}}"#;
        assert_eq!(
            try_extract_error_message(body).as_deref(),
            Some("upstream timeout")
        );
    }

    #[test]
    fn extract_error_returns_none_for_non_json() {
        assert_eq!(try_extract_error_message("<html>bad gateway</html>"), None);
        assert_eq!(try_extract_error_message(""), None);
        assert_eq!(try_extract_error_message("plain text error"), None);
    }

    #[test]
    fn extract_error_returns_none_for_missing_error_field() {
        let body = r#"{"status":"ok","message":"nothing here"}"#;
        assert_eq!(try_extract_error_message(body), None);
    }

    /// 5xx responses carrying an Ollama-style JSON body should render as
    /// the clean string in the user-facing message, and be categorised as
    /// `Temporary` (matches `recoverable: true`) so the status bar treats
    /// them as "come back and retry" rather than "something is broken".
    #[test]
    fn http_500_renders_clean_message_and_temporary_category() {
        let err = ModelError::Backend(BackendError::HttpError {
            status: 500,
            message: r#"{"error":"Internal Server Error (ref: abc-123)"}"#.to_string(),
            debug: Default::default(),
        });
        let ufe = err.to_user_facing();
        assert_eq!(ufe.summary, "Server error");
        assert_eq!(
            ufe.message,
            "HTTP 500: Internal Server Error (ref: abc-123)"
        );
        assert!(ufe.recoverable);
        assert_eq!(ufe.category, ErrorCategory::Temporary);
    }

    /// Unparseable bodies fall back to the raw content so we never lose
    /// information.
    #[test]
    fn http_500_falls_back_to_raw_body_for_html() {
        let err = ModelError::Backend(BackendError::HttpError {
            status: 502,
            message: "<html>Bad Gateway</html>".to_string(),
            debug: Default::default(),
        });
        let ufe = err.to_user_facing();
        assert_eq!(ufe.message, "HTTP 502: <html>Bad Gateway</html>");
    }
}
