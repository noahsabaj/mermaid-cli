use anyhow::Result;
use std::sync::Arc;

use crate::{
    app::{Config, get_config_dir, init_config, load_config},
    models::{BackendConfig, Model, PROVIDER_REGISTRY, lookup_provider},
    ollama::is_installed as is_ollama_installed,
    utils::resolve_api_key,
};

use super::Commands;

/// Handle CLI subcommands
/// Returns Ok(true) if the command was handled and we should exit
/// Returns Ok(false) if we should continue to the main application
pub async fn handle_command(command: &Commands, config: &Config) -> Result<bool> {
    match command {
        Commands::Init => {
            println!("Initializing Mermaid configuration...");
            init_config()?;
            println!("Configuration initialized successfully!");
            Ok(true)
        },
        Commands::List => {
            list_models(config).await?;
            Ok(true)
        },
        Commands::Version => {
            show_version();
            Ok(true)
        },
        Commands::Status => {
            show_status(config).await?;
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

/// List available models across all backends (honors user config).
pub async fn list_models(config: &Config) -> Result<()> {
    let ollama_models = list_ollama_models(config).await;
    if ollama_models.is_empty() {
        println!("No Ollama models installed locally.");
    } else {
        println!("Ollama models (local/cloud):");
        for name in &ollama_models {
            println!("  - ollama/{}", name);
        }
    }

    println!("\nConfigured remote providers:");
    let mut any = false;
    for profile in PROVIDER_REGISTRY {
        let env = config
            .providers
            .get(profile.name)
            .and_then(|c| c.api_key_env.as_deref())
            .unwrap_or(profile.api_key_env);
        if resolve_api_key(env, None).is_some() {
            any = true;
            println!("  - {} (via ${})", profile.name, env);
        }
    }
    if !any {
        println!("  (none — set a provider API key env var to enable)");
    }
    println!("\nSwitch models in-session with /model <name>.");
    Ok(())
}

/// Ask the local Ollama daemon for its list of models. Empty on
/// failure — the status widget separately shows whether Ollama is
/// reachable.
async fn list_ollama_models(config: &Config) -> Vec<String> {
    use crate::models::adapters::ollama::OllamaAdapter;
    let backend = BackendConfig {
        ollama_url: format!("http://{}:{}", config.ollama.host, config.ollama.port),
        timeout_secs: 5,
        max_idle_per_host: 2,
    };
    match OllamaAdapter::new("__list__", Arc::new(backend)).await {
        Ok(adapter) => adapter.list_models().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
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
            format!(
                " (env: {})",
                env_keys
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        println!("  {} — {}{}", name, package, env_display);
    }
    println!("\nManage with: mermaid add <name> / mermaid remove <name>");
}

/// Show status of all dependencies
async fn show_status(config: &Config) -> Result<()> {
    println!("Mermaid Status:");
    println!();

    // Check remote providers by API-key env presence (matches the
    // routing ProviderFactory uses when the user picks a model).
    let mut available: Vec<&'static str> = Vec::new();
    for profile in PROVIDER_REGISTRY {
        let env = config
            .providers
            .get(profile.name)
            .and_then(|c| c.api_key_env.as_deref())
            .unwrap_or(profile.api_key_env);
        if resolve_api_key(env, None).is_some() {
            available.push(profile.name);
        }
    }
    if available.is_empty() {
        println!("  [WARNING] Remote providers: none (no API keys in env)");
    } else {
        println!("  [OK] Remote providers: {}", available.join(", "));
    }

    // Check Ollama (via HTTP, so remote deployments are honored).
    if is_ollama_installed() {
        let models = list_ollama_models(config).await;
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
    if config.mcp_servers.is_empty() {
        println!("  [INFO] MCP Servers: None configured (use 'mermaid add <name>')");
    } else {
        println!(
            "  [OK] MCP Servers: {} configured",
            config.mcp_servers.len()
        );
        for (name, server_cfg) in &config.mcp_servers {
            println!(
                "      - {} ({})",
                name,
                server_cfg.args.get(1).unwrap_or(&server_cfg.command)
            );
        }
    }

    // Project instructions (Step 5h). Walks UP from cwd to git root or
    // $HOME to find the nearest MERMAID.md.
    {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        match crate::app::instructions::find_mermaid_md(&cwd) {
            Some(path) => match crate::app::instructions::load_from_path(&path) {
                Some(loaded) => {
                    println!(
                        "  [OK] MERMAID.md: {} ({} bytes{})",
                        loaded.path.display(),
                        loaded.byte_len,
                        if loaded.truncated { ", truncated" } else { "" }
                    );
                },
                None => {
                    println!(
                        "  [WARNING] MERMAID.md: found at {} but unreadable",
                        path.display()
                    );
                },
            },
            None => {
                println!(
                    "  [INFO] MERMAID.md: not found (create one to add persistent project instructions)"
                );
            },
        }
    }

    // OpenAI-compatible providers — list anything from the built-in
    // registry whose API key resolves, plus any user-defined custom
    // providers. No network probe (would slow `mermaid status`).
    show_provider_status(config);

    // Environment variables (for API providers)
    println!("\n  Environment:");
    if std::env::var("OLLAMA_API_KEY").is_ok() {
        println!("    - OLLAMA_API_KEY: Set (for Ollama Cloud)");
    }

    println!();
    Ok(())
}

/// Print the remote-providers status block. Includes Anthropic (bespoke
/// Messages API) and any OpenAI-compatible provider whose API key resolves.
/// Custom providers from `[providers.<name>]` are listed if `base_url`
/// and `api_key_env` are both set and the env var resolves.
fn show_provider_status(config: &Config) {
    let mut configured: Vec<(String, String)> = Vec::new(); // (name, base_url)

    // Anthropic — checked first because it's not in the OpenAI-compat
    // registry but is a top-tier provider users care about.
    let anth_cfg = config.providers.get("anthropic");
    if resolve_api_key(
        "ANTHROPIC_API_KEY",
        anth_cfg.and_then(|c| c.api_key_env.as_deref()),
    )
    .is_some()
    {
        let url = anth_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string());
        configured.push(("anthropic".to_string(), url));
    }

    // Gemini — also bespoke (not in OpenAI-compat registry).
    let gem_cfg = config.providers.get("gemini");
    if resolve_api_key(
        "GOOGLE_API_KEY",
        gem_cfg.and_then(|c| c.api_key_env.as_deref()),
    )
    .is_some()
    {
        let url = gem_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string());
        configured.push(("gemini".to_string(), url));
    }

    for profile in PROVIDER_REGISTRY {
        let user_cfg = config.providers.get(profile.name);
        let api_key_present = resolve_api_key(
            profile.api_key_env,
            user_cfg.and_then(|c| c.api_key_env.as_deref()),
        )
        .is_some();
        if api_key_present {
            let url = user_cfg
                .and_then(|c| c.base_url.clone())
                .unwrap_or_else(|| profile.base_url.to_string());
            configured.push((profile.name.to_string(), url));
        }
    }

    // Custom providers — anything in config.providers not in registry
    // and not "anthropic" / "gemini" (already handled above).
    for (name, cfg) in &config.providers {
        if name == "anthropic" || name == "gemini" || lookup_provider(name).is_some() {
            continue;
        }
        if let (Some(url), Some(env)) = (&cfg.base_url, cfg.api_key_env.as_deref())
            && resolve_api_key(env, None).is_some()
        {
            configured.push((name.clone(), url.clone()));
        }
    }

    if configured.is_empty() {
        println!(
            "  [INFO] Remote providers: None configured (set $ANTHROPIC_API_KEY, \
             $GOOGLE_API_KEY, $OPENAI_API_KEY, $GROQ_API_KEY, $OPENROUTER_API_KEY, etc., or \
             add [providers.<name>] to config.toml)"
        );
    } else {
        println!("  [OK] Remote providers: {} configured", configured.len());
        for (name, url) in configured {
            println!("      - {} ({})", name, url);
        }
    }
}
