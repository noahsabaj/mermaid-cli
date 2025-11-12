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
