use super::detector;
use super::guide;
use anyhow::Result;

/// Validate that a model exists, auto-pull if not found
pub async fn ensure_model(model_name: &str) -> Result<()> {
    // Check if Ollama is installed
    if !detector::is_installed() {
        guide::detect_and_guide();
        std::process::exit(1);
    }

    // Get the model name without provider prefix (all models route through Ollama)
    let model = model_name.strip_prefix("ollama/").unwrap_or(model_name);

    // Check available models
    let models = detector::list_models_async().await?;

    // Check if the requested model exists (exact match or implicit :latest)
    let model_exists = models.iter().any(|m| {
        m == model || (!model.contains(':') && *m == format!("{}:latest", model))
    });

    if !model_exists {
        println!("Model '{}' not found locally. Pulling from Ollama...\n", model);

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
            }
            Ok(_) => {
                println!("\nFailed to pull model '{}'.", model);
                println!("Check if the model name is correct: https://ollama.com/library");
                std::process::exit(1);
            }
            Err(e) => {
                println!("\nFailed to run 'ollama pull': {}", e);
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

/// Check if any Ollama models are available, exit with setup instructions if not
pub async fn require_any_model() -> Result<Vec<String>> {
    // Check if Ollama is installed
    if !detector::is_installed() {
        guide::detect_and_guide();
        std::process::exit(1);
    }

    let models = detector::list_models_async().await?;

    if models.is_empty() {
        println!("No Ollama models found.");
        println!();
        println!("To get started:");
        println!("  1. Browse models at https://ollama.com/library");
        println!("  2. Install one with: ollama pull <model-name>");
        println!("  3. Run mermaid again");
        println!();
        println!("Example: ollama pull qwen3:8b");
        std::process::exit(1);
    }

    Ok(models)
}
