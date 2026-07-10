// Gateway module for utils - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod auth;
mod bounded;
mod confirm;
mod host_memory;
mod logger;
mod ndjson;
mod net;
mod open;
mod private_tmp;
mod proc;
mod redact;
mod retry;
pub mod serde_base64;
mod sse;
mod task;
mod text;
mod timestamp;

// Public re-exports - the ONLY way to access utils functionality
pub use auth::{resolve_api_key, resolve_api_key_with_fallback};
pub use bounded::{CappedLine, read_capped, read_file_capped, read_line_capped};
pub use confirm::{confirm_or_refuse, is_affirmative, should_refuse_noninteractive};
pub use host_memory::{gpu_vram_bytes, system_ram_bytes};
pub use logger::{
    TraceRing, init_logger, log_debug, log_error, log_file_path, log_info, log_progress, log_warn,
    trace_ring,
};
pub use ndjson::drain_complete_lines;
pub use net::{HostClass, classify_host};
pub use open::open_file;
pub use private_tmp::private_temp_dir;
#[cfg(target_os = "windows")]
pub use proc::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
pub use proc::{
    Grace, output_with_timeout, terminate_tree, terminate_tree_blocking, write_stdin_with_timeout,
};
pub use redact::{redact_json, redact_secrets};
pub(crate) use retry::jitter;
pub use retry::{RetryConfig, retry_async, retry_async_if};
pub use sse::drain_sse_events;
pub(crate) use task::{AbortOnDrop, join_logged, spawn_guarded};
pub use text::{
    continuation_overlap, format_duration, truncate_content, truncate_middle, truncate_web_content,
};
pub use timestamp::format_relative_timestamp;
