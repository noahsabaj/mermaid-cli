// Gateway module for utils - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod checks;
mod errors;
mod file_watcher;
mod logger;
mod mutex_ext;
mod retry;
mod timestamp;
mod tokenizer;
pub mod validation;

// Public re-exports - the ONLY way to access utils functionality
pub use checks::{check_git_repo, check_ollama_available, check_ollama_model, CheckResult};
pub use errors::MermaidError;
pub use file_watcher::{FileEvent, FileSystemWatcher};
pub use logger::{init_logger, log_debug, log_error, log_info, log_progress, log_warn};
pub use mutex_ext::{lock_arc_mutex_safe, MutexExt};
pub use retry::{retry_async, retry_sync, RetryConfig};
pub use timestamp::format_relative_timestamp;
pub use tokenizer::{count_file_tokens, Tokenizer};
pub use validation::{validate_command, validate_file_path};
