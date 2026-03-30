use super::detector;
use super::guide;
use anyhow::Result;

/// Validate that a model exists, auto-pull if not found
pub async fn ensure_model(model_name: &str) -> Result<()> {
    // Check if Ollama is installed
    if !detector::is_installed() {
        guide::detect_and_guide();
        anyhow::bail!("Ollama is not installed. See instructions above.");
    }

    // Get the model name without provider prefix (all models route through Ollama)
    let model = model_name.strip_prefix("ollama/").unwrap_or(model_name);

    // Check available models
    let models = detector::list_models_async().await?;

    // Check if the requested model exists (exact match or implicit :latest)
    let model_exists = models
        .iter()
        .any(|m| m == model || (!model.contains(':') && *m == format!("{}:latest", model)));

    if !model_exists {
        println!(
            "Model '{}' not found locally. Pulling from Ollama...\n",
            model
        );

        // Auto-pull using subprocess with inherited stdio for native progress display
        let status = std::process::Command::new("ollama")
            .arg("pull")
            .arg(model)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status();

        match status {
            Ok(exit_status) if exit_status.success() => {
                println!("\nModel '{}' pulled successfully.\n", model);
            },
            Ok(_) => {
                anyhow::bail!(
                    "Failed to pull model '{}'. Check if the model name is correct: https://ollama.com/library",
                    model
                );
            },
            Err(e) => {
                anyhow::bail!("Failed to run 'ollama pull': {}", e);
            },
        }
    }

    Ok(())
}

/// Check if any Ollama models are available, return error with setup instructions if not
pub async fn require_any_model() -> Result<Vec<String>> {
    // Check if Ollama is installed
    if !detector::is_installed() {
        guide::detect_and_guide();
        anyhow::bail!("Ollama is not installed. See instructions above.");
    }

    let models = detector::list_models_async().await?;

    if models.is_empty() {
        anyhow::bail!(
            "No Ollama models found.\n\n\
            To get started:\n\
              1. Browse models at https://ollama.com/library\n\
              2. Install one with: ollama pull <model-name>\n\
              3. Run mermaid again\n\n\
            Example: ollama pull qwen3:8b"
        );
    }

    Ok(models)
}
