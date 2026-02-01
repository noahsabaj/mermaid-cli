use anyhow::Result;
use std::path::PathBuf;

use crate::{
    app::init_config,
    models::ModelFactory,
    ollama::{is_installed as is_ollama_installed, list_models as get_ollama_models},
};

use super::Commands;

/// Handle CLI subcommands
/// Returns Ok(true) if the command was handled and we should exit
/// Returns Ok(false) if we should continue to the main application
pub async fn handle_command(command: &Commands) -> Result<bool> {
    match command {
        Commands::Init => {
            println!("Initializing Mermaid configuration...");
            init_config()?;
            println!("Configuration initialized successfully!");
            Ok(true)
        },
        Commands::List => {
            list_models().await?;
            Ok(true)
        },
        Commands::Version => {
            show_version();
            Ok(true)
        },
        Commands::Status => {
            show_status().await?;
            Ok(true)
        },
        Commands::Chat => Ok(false), // Continue to chat interface
        Commands::Run { .. } => Ok(false), // Handled by main.rs
    }
}

/// List available models across all backends
pub async fn list_models() -> Result<()> {
    let models = ModelFactory::list_all_backend_models().await?;

    if models.is_empty() {
        println!("No models found across any backends");
    } else {
        println!("Available models:");
        for model in models {
            println!("  - {}", model);
        }
    }
    Ok(())
}

/// Show version information
pub fn show_version() {
    println!("Mermaid v{}", env!("CARGO_PKG_VERSION"));
    println!("   An open-source, model-agnostic AI pair programmer");
}

/// Show status of all dependencies
async fn show_status() -> Result<()> {
    println!("Mermaid Status:");
    println!();

    // Check available backends
    let backends = ModelFactory::get_available_backends().await;
    if backends.is_empty() {
        println!("  [WARNING] Backends: None available");
    } else {
        println!("  [OK] Backends: {}", backends.join(", "));
    }

    // Check Ollama
    if is_ollama_installed() {
        let models = get_ollama_models().unwrap_or_default();
        if models.is_empty() {
            println!("  [WARNING] Ollama: Installed (no models)");
        } else {
            println!("  [OK] Ollama: Running ({} models installed)", models.len());
            for model in models.iter().take(3) {
                println!("      - {}", model);
            }
            if models.len() > 3 {
                println!("      ... and {} more", models.len() - 3);
            }
        }
    } else {
        println!("  [ERROR] Ollama: Not installed");
    }

    // Check configuration
    if let Ok(home) = std::env::var("HOME") {
        let config_path = PathBuf::from(home).join(".config/mermaid/config.toml");
        if config_path.exists() {
            println!("  [OK] Configuration: {}", config_path.display());
        } else {
            println!("  [WARNING] Configuration: Not found (using defaults)");
        }
    }

    // Environment variables (for API providers)
    println!("\n  Environment:");
    if std::env::var("OPENROUTER_API_KEY").is_ok() {
        println!("    - OPENROUTER_API_KEY: Set");
    }
    if std::env::var("OLLAMA_API_KEY").is_ok() {
        println!("    - OLLAMA_API_KEY: Set (for Ollama Cloud)");
    }

    println!();
    Ok(())
}
