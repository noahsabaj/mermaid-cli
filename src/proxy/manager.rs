use anyhow::Result;
use tokio::process::Command;

use super::{get_compose_dir, is_container_runtime_available, is_proxy_running};
use crate::utils::{log_info, log_warn, log_error};
use crate::constants::{PROXY_STARTUP_WAIT_SECS, PROXY_POLL_INTERVAL_MS, PROXY_MAX_STARTUP_ATTEMPTS};

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

    log_info("🚀", format!("Starting LiteLLM proxy with {}...", runtime));

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

    // Smart polling loop - check every 100ms until ready
    log_info("⏳", "Waiting for LiteLLM proxy to be ready...");

    let max_wait_time = std::time::Duration::from_secs(PROXY_STARTUP_WAIT_SECS + (PROXY_MAX_STARTUP_ATTEMPTS as u64));
    let poll_interval = std::time::Duration::from_millis(PROXY_POLL_INTERVAL_MS);
    let start_time = std::time::Instant::now();

    while start_time.elapsed() < max_wait_time {
        if is_proxy_running().await {
            let elapsed = start_time.elapsed();
            log_info("✅", format!("LiteLLM proxy started successfully in {:.1}s", elapsed.as_secs_f64()));
            return Ok(());
        }
        tokio::time::sleep(poll_interval).await;
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

    log_info("🛑", "Stopping LiteLLM proxy...");

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
        log_warn("⚠️", format!("Failed to stop LiteLLM proxy gracefully: {}", stderr));
    } else {
        log_info("✅", "LiteLLM proxy stopped");
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
        log_error("❌", "LiteLLM proxy is not running");
        log_error("", "Start it manually with: ./start_litellm.sh");
        log_error("", "Or remove the --no-auto-proxy flag");
        std::process::exit(1);
    }

    // Auto-start the proxy
    start_proxy().await
}