/// Ollama integration module - Gateway
mod detector;
mod guide;
mod installer;

pub use detector::{is_installed, list_models};
pub use guide::detect_and_guide;
pub use installer::{ensure_model, install_model};
