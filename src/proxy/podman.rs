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
    // Priority 1: MERMAID_PROXY_DIR environment variable (for custom installations)
    if let Ok(proxy_dir) = std::env::var("MERMAID_PROXY_DIR") {
        let path = PathBuf::from(proxy_dir);
        if path.join("docker-compose.yml").exists() {
            return Ok(path);
        }
    }

    // Priority 2: ~/.config/mermaid/proxy/ (standard location)
    if let Ok(config_dir) = crate::app::get_config_dir() {
        let proxy_dir = config_dir.join("proxy");

        // Create directory if it doesn't exist
        if !proxy_dir.exists() {
            std::fs::create_dir_all(&proxy_dir)?;
        }

        let compose_file = proxy_dir.join("docker-compose.yml");
        let config_file = proxy_dir.join("litellm_config.yaml");

        // If compose file doesn't exist, create it from embedded template
        if !compose_file.exists() {
            std::fs::write(&compose_file, include_str!("../../docker-compose.yml"))?;
            eprintln!(
                "[INFO] Created docker-compose.yml at: {}",
                compose_file.display()
            );
        }

        // If litellm config doesn't exist, create it from embedded template
        if !config_file.exists() {
            std::fs::write(&config_file, include_str!("../../litellm_config.yaml"))?;
            eprintln!(
                "[INFO] Created litellm_config.yaml at: {}",
                config_file.display()
            );
        }

        // Also ensure .env.example exists for reference
        let env_example = proxy_dir.join(".env.example");
        if !env_example.exists() {
            std::fs::write(&env_example, include_str!("../../.env.example"))?;
            eprintln!("[INFO] Created .env.example at: {}", env_example.display());
        }

        // Check for .env file, warn if missing
        let env_file = proxy_dir.join(".env");
        if !env_file.exists() {
            eprintln!("[WARNING] No .env file found at: {}", env_file.display());
            eprintln!("[WARNING] Copy .env.example and add your API keys:");
            eprintln!("  cp {} {}", env_example.display(), env_file.display());
        }

        return Ok(proxy_dir);
    }

    // Priority 3: Development directory (current dir) - only for development
    let current_dir = std::env::current_dir()?;
    let compose_file = current_dir.join("docker-compose.yml");
    if compose_file.exists() {
        eprintln!("[DEV] Using docker-compose.yml from current directory");
        return Ok(current_dir);
    }

    anyhow::bail!(
        "Could not find or create docker-compose.yml.\n\
         Set MERMAID_PROXY_DIR to specify a custom location, or ensure ~/.config/mermaid is writable."
    );
}

/// Check if other mermaid processes are running
pub fn count_mermaid_processes() -> usize {
    use std::process::Command;

    let output = Command::new("pgrep").arg("-c").arg("mermaid").output();

    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<usize>()
                .unwrap_or(1) // Default to 1 (ourselves) if parse fails
        },
        _ => 1, // Assume just us if pgrep fails
    }
}
