/// Ollama integration module - Gateway
mod cloud_setup;
mod detector;
mod guide;
mod installer;

pub use cloud_setup::{is_cloud_configured, is_cloud_model, prompt_cloud_setup_if_needed, setup_cloud_interactive};
pub use detector::{is_installed, list_models};
pub use guide::detect_and_guide;
pub use installer::{ensure_model, install_model};
