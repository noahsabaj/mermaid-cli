use std::io;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize the logging system
pub fn init_logger() {
    // Use RUST_LOG environment variable, default to info level
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(io::stderr) // Write to stderr to not interfere with TUI
                .with_target(false) // Don't show module paths
                .with_thread_ids(false)
                .with_thread_names(false)
                .compact(), // Use compact format
        )
        .init();
}

/// Log an info message with emoji prefix
pub fn log_info(emoji: &str, message: impl std::fmt::Display) {
    info!("{} {}", emoji, message);
}

/// Log a warning message with emoji prefix
pub fn log_warn(emoji: &str, message: impl std::fmt::Display) {
    warn!("{} {}", emoji, message);
}

/// Log an error message with emoji prefix
pub fn log_error(emoji: &str, message: impl std::fmt::Display) {
    error!("{} {}", emoji, message);
}

/// Log a debug message
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
