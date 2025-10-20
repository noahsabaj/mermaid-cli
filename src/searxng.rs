use std::path::PathBuf;

/// Auto-start Searxng if not already running
pub async fn ensure_searxng_running() {
    // Check if Searxng is already running
    let check_url = "http://localhost:8888/search?q=test&format=json";

    match reqwest::get(check_url).await {
        Ok(_) => {
            tracing::debug!("Searxng is running and accessible");
            return;
        }
        Err(_) => {
            tracing::info!("Searxng not running, attempting to start automatically...");
        }
    }

    // Find docker-compose.yml - check multiple locations
    let compose_dir = find_compose_directory();

    if compose_dir.is_none() {
        tracing::warn!("Could not find docker-compose.yml for Searxng");
        tracing::info!("You can start it manually: podman-compose up -d searxng");
        return;
    }

    let compose_dir = compose_dir.unwrap();
    tracing::debug!("Found docker-compose.yml at: {}", compose_dir.display());

    // Start Searxng via podman-compose
    match tokio::process::Command::new("podman-compose")
        .arg("up")
        .arg("-d")
        .arg("searxng")
        .current_dir(&compose_dir)
        .output()
        .await
    {
        Ok(output) => {
            if !output.status.success() {
                tracing::warn!("Failed to start Searxng via podman-compose");
                tracing::info!("You can start it manually: podman-compose up -d searxng");
                return;
            }
        }
        Err(e) => {
            tracing::warn!("Could not execute podman-compose: {}", e);
            return;
        }
    }

    // Wait for Searxng to be ready (poll up to 10 seconds)
    for attempt in 1..=20 {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if reqwest::get(check_url).await.is_ok() {
            tracing::info!("Searxng started successfully and is responding");
            return;
        }
        if attempt % 4 == 0 {
            tracing::debug!("Waiting for Searxng to be ready... ({}/20 attempts)", attempt);
        }
    }

    tracing::warn!("Searxng started but may not be responding yet. It will be available shortly.");
}

/// Find directory containing docker-compose.yml by checking multiple locations
fn find_compose_directory() -> Option<PathBuf> {
    // 1. Check current directory and ancestors
    if let Ok(current_dir) = std::env::current_dir() {
        if let Some(dir) = current_dir
            .ancestors()
            .find(|p| p.join("docker-compose.yml").exists())
        {
            return Some(dir.to_path_buf());
        }
    }

    // 2. Check mermaid project directory (common installation path)
    let mermaid_project = PathBuf::from("/home/nsabaj/Code/mermaid");
    if mermaid_project.join("docker-compose.yml").exists() {
        return Some(mermaid_project);
    }

    // 3. Check common user directories
    if let Some(home_dir) = directories::BaseDirs::new() {
        let home_path = home_dir.home_dir();

        // Check ~/Code/mermaid
        let code_mermaid = home_path.join("Code/mermaid");
        if code_mermaid.join("docker-compose.yml").exists() {
            return Some(code_mermaid);
        }

        // Check ~/.local/share/mermaid (if installed via package manager)
        let share_mermaid = home_path.join(".local/share/mermaid");
        if share_mermaid.join("docker-compose.yml").exists() {
            return Some(share_mermaid);
        }
    }

    None
}
