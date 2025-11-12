use std::io;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialize the logging system with tracing
pub fn init_logger(verbose: bool) {
    // If --verbose flag is set, override to debug level
    // Otherwise use RUST_LOG environment variable, default to warn level (quieter)
    let filter = if verbose {
        EnvFilter::new("debug,mermaid=debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,mermaid=info"))
    };

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr) // Write to stderr to not interfere with TUI
        .with_target(false) // Don't show module paths in compact mode
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_file(false) // Don't show file locations
        .with_line_number(false) // Don't show line numbers
        .compact(); // Use compact format for cleaner output

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
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

/// Status messages for the TUI (special handling)
pub fn log_status(message: impl std::fmt::Display) {
    // For now, still use eprintln for TUI status messages
    // These will be handled differently when TUI is active
    eprintln!("{}", message);
}

/// Progress indicator for startup sequence
pub fn log_progress(step: usize, total: usize, message: impl std::fmt::Display) {
    let progress = format!("[{}/{}]", step, total);
    eprintln!("{} {} {}", progress, "->".to_string(), message);
}
