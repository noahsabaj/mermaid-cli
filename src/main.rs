use anyhow::Result;
use clap::Parser;

use mermaid::{
    app::load_config,
    cli::Cli,
    models::ModelFactory,
    ollama::ensure_model as ensure_ollama_model,
    proxy::{ensure_proxy, is_proxy_running},
    runtime::{NonInteractiveRunner, Orchestrator},
    utils::init_logger,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Load .env file from proxy directory (if it exists)
    // This provides LITELLM_MASTER_KEY and other proxy credentials
    if let Some(config_dir) = directories::BaseDirs::new() {
        let env_path = config_dir
            .config_dir()
            .join("mermaid")
            .join("proxy")
            .join(".env");
        if env_path.exists() {
            let _ = dotenvy::from_path(&env_path);
        }
    }

    // Initialize tracing subscriber (always, controlled by RUST_LOG env var)
    init_logger();

    // Auto-start Searxng for web search capability
    let _ = ensure_searxng_running().await;

    // Handle backend discovery commands
    if cli.backends {
        let backends = ModelFactory::get_available_backends().await;
        if backends.is_empty() {
            println!("No backends currently available");
            println!("Ensure at least one of the following is running:");
            println!("  - Ollama: ollama serve");
            println!("  - vLLM: python -m vllm.entrypoints.openai.api_server");
        } else {
            println!("Available backends:");
            for backend in backends {
                println!("  - {}", backend);
            }
        }
        return Ok(());
    }

    if cli.list_all_models {
        match ModelFactory::list_all_backend_models().await {
            Ok(models) => {
                if models.is_empty() {
                    println!("No models found across any backends");
                } else {
                    println!("Available models:");
                    for model in models {
                        println!("  - {}", model);
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to list models: {}", e);
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // Check if running in non-interactive mode
    if let Some(prompt) = cli.prompt.clone() {
        run_non_interactive(cli, prompt).await
    } else {
        // Create and run the orchestrator for interactive mode
        let orchestrator = Orchestrator::new(cli)?;
        orchestrator.run().await
    }
}

/// Auto-start Searxng if not already running
async fn ensure_searxng_running() {
    // Check if Searxng is already running
    let check_url = "http://localhost:8888/search?q=test&format=json";

    match reqwest::get(check_url).await {
        Ok(_) => {
            tracing::debug!("Searxng is running and accessible");
            return;
        }
        Err(_) => {
            tracing::info!("Searxng not running, attempting to start automatically...");
        }
    }

    // Find docker-compose.yml - check multiple locations
    let compose_dir = find_compose_directory();

    if compose_dir.is_none() {
        tracing::warn!("Could not find docker-compose.yml for Searxng");
        tracing::info!("You can start it manually: podman-compose up -d searxng");
        return;
    }

    let compose_dir = compose_dir.unwrap();
    tracing::debug!("Found docker-compose.yml at: {}", compose_dir.display());

    // Start Searxng via podman-compose
    match tokio::process::Command::new("podman-compose")
        .arg("up")
        .arg("-d")
        .arg("searxng")
        .current_dir(&compose_dir)
        .output()
        .await
    {
        Ok(output) => {
            if !output.status.success() {
                tracing::warn!("Failed to start Searxng via podman-compose");
                tracing::info!("You can start it manually: podman-compose up -d searxng");
                return;
            }
        }
        Err(e) => {
            tracing::warn!("Could not execute podman-compose: {}", e);
            return;
        }
    }

    // Wait for Searxng to be ready (poll up to 10 seconds)
    for attempt in 1..=20 {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if reqwest::get(check_url).await.is_ok() {
            tracing::info!("Searxng started successfully and is responding");
            return;
        }
        if attempt % 4 == 0 {
            tracing::debug!("Waiting for Searxng to be ready... ({}/20 attempts)", attempt);
        }
    }

    tracing::warn!("Searxng started but may not be responding yet. It will be available shortly.");
}

/// Find directory containing docker-compose.yml by checking multiple locations
fn find_compose_directory() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    // 1. Check current directory and ancestors
    if let Ok(current_dir) = std::env::current_dir() {
        if let Some(dir) = current_dir
            .ancestors()
            .find(|p| p.join("docker-compose.yml").exists())
        {
            return Some(dir.to_path_buf());
        }
    }

    // 2. Check mermaid project directory (common installation path)
    let mermaid_project = PathBuf::from("/home/nsabaj/Code/mermaid");
    if mermaid_project.join("docker-compose.yml").exists() {
        return Some(mermaid_project);
    }

    // 3. Check common user directories
    if let Some(home_dir) = directories::BaseDirs::new() {
        let home_path = home_dir.home_dir();

        // Check ~/Code/mermaid
        let code_mermaid = home_path.join("Code/mermaid");
        if code_mermaid.join("docker-compose.yml").exists() {
            return Some(code_mermaid);
        }

        // Check ~/.local/share/mermaid (if installed via package manager)
        let share_mermaid = home_path.join(".local/share/mermaid");
        if share_mermaid.join("docker-compose.yml").exists() {
            return Some(share_mermaid);
        }
    }

    None
}

/// Run in non-interactive mode
async fn run_non_interactive(cli: Cli, prompt: String) -> Result<()> {
    // Load configuration
    let config = if let Some(config_path) = &cli.config {
        let toml_str = std::fs::read_to_string(config_path)?;
        toml::from_str(&toml_str)?
    } else {
        load_config().unwrap_or_default()
    };

    // Determine model to use
    let model_id = if let Some(model) = &cli.model {
        model.clone()
    } else {
        format!(
            "{}/{}",
            config.default_model.provider, config.default_model.name
        )
    };

    // Ensure LiteLLM proxy is running
    if !is_proxy_running().await {
        ensure_proxy(cli.no_auto_proxy).await?;
    }

    // Ensure Ollama model is available
    ensure_ollama_model(&model_id, cli.no_auto_install).await?;

    // Determine project path
    let project_path = cli.path.unwrap_or_else(|| std::path::PathBuf::from("."));

    // Create and run the non-interactive runner
    let runner = NonInteractiveRunner::new(
        model_id,
        project_path,
        config,
        cli.no_execute,
        cli.max_tokens,
    )
    .await?;

    // Execute the prompt
    let result = runner.execute(prompt).await?;

    // Format and output the result
    let formatted = runner.format_result(&result, cli.output_format);
    println!("{}", formatted);

    // Exit with appropriate code
    if !result.errors.is_empty() {
        std::process::exit(1);
    }

    Ok(())
}
