use anyhow::Result;

use crate::{
    app::{get_config_dir, init_config, load_config},
    models::ModelFactory,
    ollama::{is_installed as is_ollama_installed, list_models_async as get_ollama_models},
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
        Commands::Add { name } => {
            crate::mcp::add_server(name).await?;
            Ok(true)
        },
        Commands::Remove { name } => {
            crate::mcp::remove_server(name).await?;
            Ok(true)
        },
        Commands::Mcp => {
            show_mcp_servers();
            Ok(true)
        },
        Commands::Chat => Ok(false),       // Continue to chat interface
        Commands::Run { .. } => Ok(false), // Handled by main.rs
    }
}

/// List available models across all backends
pub async fn list_models() -> Result<()> {
    let models = ModelFactory::list_all_models().await?;

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

/// Show configured MCP servers
fn show_mcp_servers() {
    let config = load_config().unwrap_or_default();

    if config.mcp_servers.is_empty() {
        println!("No MCP servers configured.\n");
        println!("Add one with: mermaid add <name>");
        println!("Examples:");
        println!("  mermaid add context7     # Library documentation");
        println!("  mermaid add github       # GitHub API integration");
        println!("  mermaid add playwright   # Browser automation");
        println!("  mermaid add memory       # Persistent knowledge graph");
        return;
    }

    println!("Configured MCP servers:\n");
    for (name, server_cfg) in &config.mcp_servers {
        let package = server_cfg
            .args
            .iter()
            .find(|a| !a.starts_with('-'))
            .unwrap_or(&server_cfg.command);
        let env_keys: Vec<&String> = server_cfg.env.keys().collect();
        let env_display = if env_keys.is_empty() {
            String::new()
        } else {
            format!(" (env: {})", env_keys.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", "))
        };
        println!("  {} — {}{}", name, package, env_display);
    }
    println!("\nManage with: mermaid add <name> / mermaid remove <name>");
}

/// Show status of all dependencies
async fn show_status() -> Result<()> {
    println!("Mermaid Status:");
    println!();

    // Check available backends (use user's config for host/port)
    let status_config = load_config().unwrap_or_default();
    let factory = ModelFactory::from_config(&status_config);
    let backends = factory.available_providers_pub().await;
    if backends.is_empty() {
        println!("  [WARNING] Backends: None available");
    } else {
        println!("  [OK] Backends: {}", backends.join(", "));
    }

    // Check Ollama
    if is_ollama_installed() {
        let models = get_ollama_models().await.unwrap_or_default();
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

    // Check configuration (uses platform-specific path via ProjectDirs)
    if let Ok(config_dir) = get_config_dir() {
        let config_path = config_dir.join("config.toml");
        if config_path.exists() {
            println!("  [OK] Configuration: {}", config_path.display());
        } else {
            println!("  [WARNING] Configuration: Not found (using defaults)");
        }
    }

    // MCP Servers
    if let Ok(cfg) = load_config() {
        if cfg.mcp_servers.is_empty() {
            println!("  [INFO] MCP Servers: None configured (use 'mermaid add <name>')");
        } else {
            println!(
                "  [OK] MCP Servers: {} configured",
                cfg.mcp_servers.len()
            );
            for (name, server_cfg) in &cfg.mcp_servers {
                println!(
                    "      - {} ({})",
                    name,
                    server_cfg.args.get(1).unwrap_or(&server_cfg.command)
                );
            }
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
