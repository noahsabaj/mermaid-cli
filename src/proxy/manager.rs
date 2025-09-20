use anyhow::Result;
use tokio::process::Command;

use super::{get_compose_dir, is_container_runtime_available, is_proxy_running};

/// Start the LiteLLM proxy
pub async fn start_proxy() -> Result<()> {
    // Detect container runtime - prefer podman-compose
    let runtime = is_container_runtime_available()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "❌ Neither Podman nor Docker found\n   \
                Install Podman: sudo apt-get install podman podman-compose\n   \
                Or install Docker: https://docs.docker.com/engine/install/"
            )
        })?;

    // Get the directory with docker-compose.yml
    let compose_dir = get_compose_dir()?;

    println!("🚀 Starting LiteLLM proxy with {}...", runtime);

    // Build the command based on runtime type
    let output = if runtime == "podman" || runtime == "docker" {
        // Use compose subcommand for standalone podman/docker
        Command::new(runtime)
            .args(&["compose", "up", "-d", "litellm"])
            .current_dir(&compose_dir)
            .output()
            .await?
    } else {
        // Use podman-compose or docker-compose directly
        Command::new(runtime)
            .args(&["up", "-d", "litellm"])
            .current_dir(&compose_dir)
            .output()
            .await?
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to start LiteLLM proxy: {}", stderr);
    }

    // Wait for proxy to be ready
    println!("⏳ Waiting for LiteLLM proxy to be ready...");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Verify it's running
    for i in 0..10 {
        if is_proxy_running().await {
            println!("✅ LiteLLM proxy started successfully");
            return Ok(());
        }
        if i < 9 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    anyhow::bail!(
        "LiteLLM proxy failed to start properly. Check logs with: {} logs litellm",
        runtime
    )
}

/// Stop the LiteLLM proxy
pub async fn stop_proxy() -> Result<()> {
    let runtime = is_container_runtime_available().ok_or_else(|| {
        anyhow::anyhow!("No container runtime found (Podman or Docker)")
    })?;

    let compose_dir = get_compose_dir()?;

    println!("🛑 Stopping LiteLLM proxy...");

    // Build the command based on runtime type
    let output = if runtime == "podman" || runtime == "docker" {
        Command::new(runtime)
            .args(&["compose", "stop", "litellm"])
            .current_dir(&compose_dir)
            .output()
            .await?
    } else {
        Command::new(runtime)
            .args(&["stop", "litellm"])
            .current_dir(&compose_dir)
            .output()
            .await?
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("⚠️  Failed to stop LiteLLM proxy gracefully: {}", stderr);
    } else {
        println!("✅ LiteLLM proxy stopped");
    }

    Ok(())
}

/// Ensure LiteLLM proxy is running
pub async fn ensure_proxy(no_auto_proxy: bool) -> Result<()> {
    // Check if proxy is already running
    if is_proxy_running().await {
        return Ok(());
    }

    // Proxy not running
    if no_auto_proxy {
        eprintln!("❌ LiteLLM proxy is not running");
        eprintln!("   Start it manually with: ./start_litellm.sh");
        eprintln!("   Or remove the --no-auto-proxy flag");
        std::process::exit(1);
    }

    // Auto-start the proxy
    start_proxy().await
}