use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Rotate the log file when it reaches this size. Bounded: at most two
/// log files (`mermaid.log` current + `mermaid.log.old` previous), so
/// worst-case disk use is ~2x this value between restarts.
const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024; // 10 MB

/// Get the log file path (~/.mermaid/mermaid.log)
fn get_log_file_path() -> Option<PathBuf> {
    // Fall back to USERPROFILE on Windows where HOME is not conventionally
    // set; mirrors the pattern used in app::config::get_config_dir.
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(|home| PathBuf::from(home).join(".mermaid").join("mermaid.log"))
}

/// If the log file exceeds MAX_LOG_SIZE, rename it to `.log.old`
/// (overwriting any prior `.log.old`). Best-effort — rotation failures
/// are silent because logging is non-critical. Runs once per startup.
fn rotate_if_large(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() >= MAX_LOG_SIZE {
        let rotated = path.with_extension("log.old");
        let _ = std::fs::rename(path, rotated);
    }
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

        // Rotate at startup if the previous session left a large file.
        rotate_if_large(&log_path);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_small_file_is_noop() {
        let tmp = std::env::temp_dir().join("mermaid_logger_small.log");
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(tmp.with_extension("log.old"));
        std::fs::write(&tmp, b"hello world").unwrap();

        rotate_if_large(&tmp);

        assert!(tmp.exists(), "small file should NOT be rotated");
        assert!(
            !tmp.with_extension("log.old").exists(),
            "no .log.old should be created for small files"
        );

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn rotate_large_file_renames_to_old() {
        let tmp = std::env::temp_dir().join("mermaid_logger_large.log");
        let _ = std::fs::remove_file(&tmp);
        let old = tmp.with_extension("log.old");
        let _ = std::fs::remove_file(&old);

        let file = std::fs::File::create(&tmp).unwrap();
        file.set_len(MAX_LOG_SIZE + 1).unwrap();
        drop(file);

        rotate_if_large(&tmp);

        assert!(!tmp.exists(), "oversized file should be rotated away");
        assert!(old.exists(), ".log.old should now exist");

        let _ = std::fs::remove_file(&old);
    }

    #[test]
    fn rotate_overwrites_prior_old() {
        let tmp = std::env::temp_dir().join("mermaid_logger_overwrite.log");
        let _ = std::fs::remove_file(&tmp);
        let old = tmp.with_extension("log.old");
        std::fs::write(&old, b"stale previous rotation").unwrap();

        let file = std::fs::File::create(&tmp).unwrap();
        file.set_len(MAX_LOG_SIZE + 1).unwrap();
        drop(file);

        rotate_if_large(&tmp);

        // Previous .old should have been replaced by the freshly rotated file.
        let rotated_size = std::fs::metadata(&old).unwrap().len();
        assert!(
            rotated_size >= MAX_LOG_SIZE,
            "the rotated file should be the large one, not the stale old"
        );

        let _ = std::fs::remove_file(&old);
    }
}
