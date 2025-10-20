use std::path::PathBuf;

/// Auto-start Searxng if not already running
///
/// This runs silently in the background to avoid logging leaks into the TUI.
/// If diagnostics are needed, run with RUST_LOG=debug to see tracing output.
pub async fn ensure_searxng_running() {
    // Check if Searxng is already running
    let check_url = "http://localhost:8888/search?q=test&format=json";

    match reqwest::get(check_url).await {
        Ok(_) => {
            // Already running, nothing to do
            return;
        }
        Err(_) => {
            // Not running, attempt to start
        }
    }

    // Find docker-compose.yml - check multiple locations
    let compose_dir = find_compose_directory();

    if compose_dir.is_none() {
        // Could not find compose file, silent failure
        return;
    }

    let compose_dir = compose_dir.unwrap();

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
                // Start failed, silent failure
                return;
            }
        }
        Err(_e) => {
            // Could not execute command, silent failure
            return;
        }
    }

    // Wait for Searxng to be ready (poll up to 10 seconds)
    for _attempt in 1..=20 {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if reqwest::get(check_url).await.is_ok() {
            // Searxng is responding, we're done
            return;
        }
    }

    // Searxng may not be fully ready yet, but we tried our best
    // First web search will just take a bit longer
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
