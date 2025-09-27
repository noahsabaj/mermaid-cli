// Gateway module for app - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod config;
mod state;

// Public re-exports - the ONLY way to access app functionality
pub use config::{init_config, load_config, save_config, Config};
pub use state::AppState;
