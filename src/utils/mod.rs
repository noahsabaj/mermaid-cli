// Gateway module for utils - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod auth;
mod bounded;
mod checks;
mod logger;
mod mutex_ext;
mod ndjson;
mod net;
mod open;
mod private_tmp;
mod redact;
mod retry;
mod sse;
mod text;
mod timestamp;
mod tokenizer;

// Public re-exports - the ONLY way to access utils functionality
pub use auth::{resolve_api_key, resolve_api_key_with_fallback};
pub use bounded::{CappedLine, read_capped, read_file_capped, read_line_capped};
pub use checks::{CheckResult, check_ollama_available, check_ollama_model};
pub use logger::{init_logger, log_debug, log_error, log_info, log_progress, log_warn};
pub use mutex_ext::{MutexExt, lock_arc_mutex_safe};
pub use ndjson::drain_complete_lines;
pub use net::{HostClass, classify_host};
pub use open::open_file;
pub use private_tmp::private_temp_dir;
pub use redact::{redact_json, redact_secrets};
pub(crate) use retry::jitter;
pub use retry::{RetryConfig, retry_async, retry_async_if};
pub use sse::drain_sse_events;
pub use text::{format_duration, format_tokens, truncate_content, truncate_web_content};
pub use timestamp::format_relative_timestamp;
pub use tokenizer::Tokenizer;
