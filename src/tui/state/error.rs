/// Error types for the UI layer
///
/// Structured error logging with severity levels.

use std::time::Instant;

/// Error severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSeverity {
    /// Informational (not really an error)
    Info,
    /// Warning - operation completed but with issues
    Warning,
    /// Error - operation failed
    Error,
    /// Security error - action was rejected
    Security,
}

impl ErrorSeverity {
    pub fn display(&self) -> &str {
        match self {
            ErrorSeverity::Info => "INFO",
            ErrorSeverity::Warning => "WARN",
            ErrorSeverity::Error => "ERROR",
            ErrorSeverity::Security => "SECURITY",
        }
    }

    pub fn color(&self) -> &str {
        match self {
            ErrorSeverity::Info => "cyan",
            ErrorSeverity::Warning => "yellow",
            ErrorSeverity::Error => "red",
            ErrorSeverity::Security => "magenta",
        }
    }
}

/// An error entry in the error log
#[derive(Debug, Clone)]
pub struct ErrorEntry {
    pub timestamp: Instant,
    pub severity: ErrorSeverity,
    pub message: String,
    pub context: Option<String>,
}

impl ErrorEntry {
    pub fn new(severity: ErrorSeverity, message: String) -> Self {
        Self {
            timestamp: Instant::now(),
            severity,
            message,
            context: None,
        }
    }

    pub fn with_context(severity: ErrorSeverity, message: String, context: String) -> Self {
        Self {
            timestamp: Instant::now(),
            severity,
            message,
            context: Some(context),
        }
    }

    pub fn display(&self) -> String {
        match &self.context {
            Some(ctx) => format!(
                "[{}] {} - {}",
                self.severity.display(),
                self.message,
                ctx
            ),
            None => format!("[{}] {}", self.severity.display(), self.message),
        }
    }
}
