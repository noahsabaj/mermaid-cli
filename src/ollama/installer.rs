use super::detector;
use super::guide;
use anyhow::Result;

/// Install an Ollama model with progress display
pub async fn install_model(model: &str) -> Result<()> {
    use std::process::{Command, Stdio};

    println!("[DOWNLOADING] Pulling {} model...", model);

    let mut child = Command::new("ollama")
        .arg("pull")
        .arg(model)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    let status = child.wait()?;

    if !status.success() {
        anyhow::bail!("Failed to install {} model", model);
    }

    Ok(())
}

/// Ensure Ollama model is available, auto-installing if needed
pub async fn ensure_model(model_name: &str, no_auto_install: bool) -> Result<()> {
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

    // Check if any models exist (use async version)
    let models = detector::list_models_async().await?;

    // Check if the requested model exists
    let model_exists = models.iter().any(|m| m.contains(model));

    // If no models exist and we're using default (tinyllama), auto-install
    if !model_exists && model == "tinyllama" && !no_auto_install {
        println!("[SETUP] First time setup: Installing tinyllama (1.1GB)...");
        println!("   This is a one-time download. Use --no-auto-install to skip.\n");

        install_model("tinyllama").await?;

        println!("\n[OK] tinyllama installed successfully!");
    } else if !model_exists && model != "tinyllama" {
        // For non-default models, prompt to install
        println!("[WARNING] Model '{}' not found locally.", model);
        println!("   Run: ollama pull {}", model);
        println!("   Or use --model ollama/tinyllama for the default model");
        std::process::exit(1);
    }

    Ok(())
}
