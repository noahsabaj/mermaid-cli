use anyhow::Result;
use tokio::process::Command as AsyncCommand;

/// Check if Ollama is installed on the system
pub fn is_installed() -> bool {
    which::which("ollama").is_ok()
}

/// Get list of installed Ollama models (async version)
pub async fn list_models_async() -> Result<Vec<String>> {
    let output = AsyncCommand::new("ollama").arg("list").output().await;

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let models: Vec<String> = stdout
                .lines()
                .skip(1) // Skip header line
                .filter_map(|line| {
                    // Parse model name from the output
                    line.split_whitespace().next().map(|s| s.to_string())
                })
                .collect();
            Ok(models)
        },
        _ => Ok(Vec::new()),
    }
}

/// Get list of installed Ollama models (sync version for backward compatibility)
pub fn list_models() -> Result<Vec<String>> {
    use std::process::Command;

    let output = Command::new("ollama").arg("list").output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let models: Vec<String> = stdout
                .lines()
                .skip(1) // Skip header line
                .filter_map(|line| {
                    // Parse model name from the output
                    line.split_whitespace().next().map(|s| s.to_string())
                })
                .collect();
            Ok(models)
        },
        _ => Ok(Vec::new()),
    }
}
