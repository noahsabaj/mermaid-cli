// Gateway module for utils - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod errors;
mod file_watcher;
mod logger;
mod tokenizer;

// Public re-exports - the ONLY way to access utils functionality
pub use errors::MermaidError;
pub use file_watcher::{FileEvent, FileSystemWatcher};
pub use logger::{init_logger, log_debug, log_error, log_info, log_progress, log_status, log_warn};
pub use tokenizer::{count_file_tokens, Tokenizer};
