use anyhow::Result;
use std::io::{self, Write};

use crate::app::{get_config_dir, load_config, save_config};

/// Check if Ollama cloud is configured (API key exists)
pub fn is_cloud_configured() -> bool {
    // Check environment variable first
    if std::env::var("OLLAMA_API_KEY").is_ok() {
        return true;
    }

    // Check config file
    if let Ok(config) = load_config() {
        return config.ollama.cloud_api_key.is_some();
    }

    false
}

/// Interactive setup flow for Ollama cloud API key
/// Returns true if setup was completed successfully
pub fn setup_cloud_interactive() -> Result<bool> {
    println!("\n=== Ollama Cloud Setup ===\n");
    println!("Ollama Cloud allows you to run large models on datacenter-grade hardware.");
    println!("Cloud models use the :cloud suffix (e.g., kimi-k2-thinking:cloud)\n");
    println!("To get started:");
    println!("  1. Visit https://ollama.com/cloud");
    println!("  2. Sign in or create an account");
    println!("  3. Generate an API key\n");

    print!("Would you like to set up Ollama Cloud now? [Y/n]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();

    if input == "n" || input == "no" {
        println!("\nSkipping cloud setup. You can set it up later with:");
        println!("  export OLLAMA_API_KEY=your_key_here");
        println!("  Or add it to ~/.config/mermaid/config.toml");
        return Ok(false);
    }

    // Prompt for API key
    print!("\nEnter your Ollama Cloud API key: ");
    io::stdout().flush()?;

    let mut api_key = String::new();
    io::stdin().read_line(&mut api_key)?;
    let api_key = api_key.trim();

    if api_key.is_empty() {
        println!("\nNo API key provided. Setup cancelled.");
        return Ok(false);
    }

    // Validate API key format (basic check)
    if api_key.len() < 10 {
        println!("\nAPI key seems too short. Please check and try again.");
        return Ok(false);
    }

    // Load or create config
    let mut config = load_config().unwrap_or_default();

    // Save API key to config
    config.ollama.cloud_api_key = Some(api_key.to_string());

    // Save config file
    let config_path = get_config_dir()?.join("config.toml");
    save_config(&config, Some(config_path.clone()))?;

    println!("\n✓ Ollama Cloud API key saved to: {}", config_path.display());
    println!("\nYou can now use cloud models with the :cloud suffix:");
    println!("  :model kimi-k2-thinking:cloud");
    println!("  :model qwen3-coder:480b-cloud");
    println!("  :model deepseek-v3.1:671b-cloud\n");

    Ok(true)
}

/// Get the Ollama Cloud API key from environment or config
///
/// Priority: OLLAMA_API_KEY env var > config file cloud_api_key
pub fn get_cloud_api_key() -> Option<String> {
    // Check environment variable first
    if let Ok(key) = std::env::var("OLLAMA_API_KEY")
        && !key.is_empty() {
            return Some(key);
        }

    // Check config file
    if let Ok(config) = load_config() {
        return config.ollama.cloud_api_key;
    }

    None
}

/// Check if a model name requires cloud access
pub fn is_cloud_model(model_name: &str) -> bool {
    model_name.ends_with(":cloud")
}

/// Prompt user to set up cloud if trying to use a cloud model without API key
pub fn prompt_cloud_setup_if_needed(model_name: &str) -> Result<bool> {
    if !is_cloud_model(model_name) {
        return Ok(true); // Not a cloud model, proceed
    }

    if is_cloud_configured() {
        return Ok(true); // Already configured, proceed
    }

    // Cloud model requested but not configured
    println!("\n⚠ Cloud model requested but Ollama Cloud is not configured.");
    println!("   Model: {}\n", model_name);

    setup_cloud_interactive()
}
