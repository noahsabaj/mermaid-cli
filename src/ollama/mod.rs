/// Ollama integration module - Gateway
mod cloud_setup;
mod detector;
mod guide;
mod installer;
mod server;

pub use cloud_setup::{
    get_cloud_api_key, is_cloud_configured, is_cloud_model, prompt_cloud_setup_if_needed,
    setup_cloud_interactive,
};
pub use detector::is_installed;
pub use guide::detect_and_guide;
pub use installer::{ensure_model, require_any_model};
pub use server::{AutostartError, ensure_running};
