use anyhow::Result;
use colored::Colorize;
use std::path::PathBuf;

use crate::{
    app::init_config,
    models::ModelFactory,
    ollama::{is_installed as is_ollama_installed, list_models as get_ollama_models},
    proxy::is_proxy_running,
};

use super::Commands;

/// Handle CLI subcommands
pub async fn handle_command(command: &Commands) -> Result<bool> {
    match command {
        Commands::Init => {
            println!("Initializing Mermaid configuration...");
            init_config()?;
            println!("Configuration initialized successfully!");
            Ok(true)
        }
        Commands::List => {
            list_models().await?;
            Ok(true)
        }
        Commands::Version => {
            show_version();
            Ok(true)
        }
        Commands::Status => {
            show_status().await?;
            Ok(true)
        }
        Commands::Chat => Ok(false), // Continue to chat interface
    }
}

/// List available models
pub async fn list_models() -> Result<()> {
    println!("Available models:");
    let models = ModelFactory::list_available().await?;
    for model in models {
        println!("  • {}", model.green());
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

    // Check Ollama
    if is_ollama_installed() {
        let models = get_ollama_models().unwrap_or_default();
        if models.is_empty() {
            println!("  [WARNING] Ollama: Installed (no models)");
        } else {
            println!("  [OK] Ollama: Running ({} models installed)", models.len());
            for model in models.iter().take(3) {
                println!("      • {}", model);
            }
            if models.len() > 3 {
                println!("      ... and {} more", models.len() - 3);
            }
        }
    } else {
        println!("  [ERROR] Ollama: Not installed");
    }

    // Check LiteLLM Proxy
    if is_proxy_running().await {
        println!("  [OK] LiteLLM Proxy: Running at http://localhost:4000");
    } else {
        println!("  [ERROR] LiteLLM Proxy: Not running");
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

    // Check container runtime
    if which::which("podman-compose").is_ok() {
        println!("  [OK] Container Runtime: Podman Compose");
    } else if which::which("podman").is_ok() {
        println!("  [OK] Container Runtime: Podman");
    } else if which::which("docker-compose").is_ok() {
        println!("  [OK] Container Runtime: Docker Compose");
    } else if which::which("docker").is_ok() {
        println!("  [OK] Container Runtime: Docker");
    } else {
        println!("  [ERROR] Container Runtime: Not found (Podman or Docker required)");
    }

    // Environment variables
    println!("\n  Environment:");
    if std::env::var("OPENAI_API_KEY").is_ok() {
        println!("    • OPENAI_API_KEY: Set");
    }
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        println!("    • ANTHROPIC_API_KEY: Set");
    }
    if std::env::var("GROQ_API_KEY").is_ok() {
        println!("    • GROQ_API_KEY: Set");
    }
    if std::env::var("LITELLM_MASTER_KEY").is_ok() {
        println!("    • LITELLM_MASTER_KEY: Set");
    }

    println!();
    Ok(())
}