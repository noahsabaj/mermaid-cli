use std::fs::OpenOptions;
use std::path::PathBuf;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Get the log file path (~/.mermaid/mermaid.log)
fn get_log_file_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".mermaid").join("mermaid.log"))
}

/// Initialize the logging system with tracing
pub fn init_logger(verbose: bool) {
    // If --verbose flag is set, override to debug level
    // Otherwise use RUST_LOG environment variable, default to warn level (quieter)
    let filter = if verbose {
        EnvFilter::new("debug,mermaid=debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,mermaid=info"))
    };

    // Try to write logs to a file to avoid corrupting the TUI
    // Falls back to no logging if file creation fails (TUI takes priority)
    if let Some(log_path) = get_log_file_path() {
        // Ensure parent directory exists
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Open log file for appending
        if let Ok(file) = OpenOptions::new().create(true).append(true).open(&log_path) {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_writer(file)
                .with_target(false)
                .with_thread_ids(false)
                .with_thread_names(false)
                .with_ansi(false) // No ANSI colors in file
                .compact();

            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .init();
            return;
        }
    }

    // Fallback: no logging if file creation fails (don't corrupt TUI)
    tracing_subscriber::registry().with(filter).init();
}

/// Log an info message with category prefix (backward compatible)
pub fn log_info(category: &str, message: impl std::fmt::Display) {
    info!(category = %category, "{}", message);
}

/// Log a warning message with category prefix (backward compatible)
pub fn log_warn(category: &str, message: impl std::fmt::Display) {
    warn!(category = %category, "{}", message);
}

/// Log an error message with category prefix (backward compatible)
pub fn log_error(category: &str, message: impl std::fmt::Display) {
    error!(category = %category, "{}", message);
}

/// Log a debug message (backward compatible)
pub fn log_debug(message: impl std::fmt::Display) {
    debug!("{}", message);
}

/// Progress indicator for startup sequence
pub fn log_progress(step: usize, total: usize, message: impl std::fmt::Display) {
    info!(step = step, total = total, "{}", message);
}
