// Gateway module for app - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod config;

// Public re-exports - the ONLY way to access app functionality
pub use config::{get_config_dir, init_config, load_config, persist_last_model, save_config, Config};
