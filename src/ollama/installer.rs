use super::detector;
use super::guide;
use anyhow::Result;

/// Validate that a model exists, exit with helpful message if not
pub async fn ensure_model(model_name: &str, _no_auto_install: bool) -> Result<()> {
    // Skip if not using Ollama
    if !model_name.starts_with("ollama/") {
        return Ok(());
    }

    // Check if Ollama is installed
    if !detector::is_installed() {
        guide::detect_and_guide();
        std::process::exit(1);
    }

    // Get the model name without provider prefix
    let model = &model_name[7..]; // Remove "ollama/" prefix

    // Check available models
    let models = detector::list_models_async().await?;

    // Check if the requested model exists
    let model_exists = models.iter().any(|m| m.contains(model));

    if !model_exists {
        // Format available models for display
        let available = if models.is_empty() {
            String::new()
        } else {
            format!("\nAvailable models: {}", models.join(", "))
        };

        println!("Model '{}' not found.", model);
        println!();
        println!("To fix:");
        println!("  - Install it: ollama pull {}", model);
        println!("  - Or use a different model: mermaid --model <model-name>");
        if !available.is_empty() {
            println!("{}", available);
        }
        std::process::exit(1);
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
