// Gateway module for utils - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod checks;
mod logger;
mod mutex_ext;
mod open;
mod retry;
mod text;
mod timestamp;
mod tokenizer;

// Public re-exports - the ONLY way to access utils functionality
pub use checks::{CheckResult, check_ollama_available, check_ollama_model};
pub use logger::{init_logger, log_debug, log_error, log_info, log_progress, log_warn};
pub use mutex_ext::{MutexExt, lock_arc_mutex_safe};
pub use open::open_file;
pub use retry::{RetryConfig, retry_async};
pub use text::{format_duration, format_tokens, truncate_content, truncate_web_content};
pub use timestamp::format_relative_timestamp;
pub use tokenizer::Tokenizer;
