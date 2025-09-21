// Gateway module for utils - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod errors;
mod file_watcher;
mod logger;
mod tokenizer;

// Public re-exports - the ONLY way to access utils functionality
pub use errors::MermaidError;
pub use file_watcher::{FileSystemWatcher, FileEvent};
pub use logger::{init_logger, log_info, log_warn, log_error, log_debug, log_status, log_progress};
pub use tokenizer::{Tokenizer, count_file_tokens};