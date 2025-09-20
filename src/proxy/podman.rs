use anyhow::Result;
use std::path::PathBuf;

/// Check if a command is available on the system
fn is_command_available(cmd: &str) -> bool {
    which::which(cmd).is_ok()
}

/// Check if container runtime is available and return its name
/// Prefers Podman over Docker
pub fn is_container_runtime_available() -> Option<&'static str> {
    if is_command_available("podman-compose") {
        Some("podman-compose")
    } else if is_command_available("docker-compose") {
        Some("docker-compose")
    } else if is_command_available("podman") {
        // Fallback to standalone podman with compose plugin
        Some("podman")
    } else if is_command_available("docker") {
        // Fallback to standalone docker with compose plugin
        Some("docker")
    } else {
        None
    }
}

/// Get the directory where docker-compose.yml is located
pub fn get_compose_dir() -> Result<PathBuf> {
    // First, check if we're running from the development directory
    let current_dir = std::env::current_dir()?;
    let compose_file = current_dir.join("docker-compose.yml");
    if compose_file.exists() {
        return Ok(current_dir);
    }

    // Check the mermaid source directory
    let mermaid_src = PathBuf::from("/home/nsabaj/Code/mermaid");
    let compose_file = mermaid_src.join("docker-compose.yml");
    if compose_file.exists() {
        return Ok(mermaid_src);
    }

    // Could also check ~/.mermaid/proxy or other locations
    anyhow::bail!("Could not find docker-compose.yml. Please run from the mermaid directory or set MERMAID_PROXY_DIR environment variable.");
}

/// Check if other mermaid processes are running
pub fn count_mermaid_processes() -> usize {
    use std::process::Command;

    let output = Command::new("pgrep")
        .arg("-c")
        .arg("mermaid")
        .output();

    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<usize>()
                .unwrap_or(1) // Default to 1 (ourselves) if parse fails
        }
        _ => 1, // Assume just us if pgrep fails
    }
}