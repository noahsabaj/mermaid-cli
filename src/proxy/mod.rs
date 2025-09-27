/// LiteLLM proxy management module - Gateway
mod health;
mod manager;
mod podman;

pub use health::is_proxy_running;
pub use manager::{ensure_proxy, start_proxy, stop_proxy};
pub use podman::{count_mermaid_processes, get_compose_dir, is_container_runtime_available};
