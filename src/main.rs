use anyhow::Result;
use clap::Parser;

use mermaid::{
    cli::Cli,
    runtime::{Orchestrator, NonInteractiveRunner},
    app::load_config,
    proxy::{ensure_proxy, is_proxy_running},
    ollama::ensure_model as ensure_ollama_model,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Set up logging if verbose
    if cli.verbose {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .init();
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
            config.default_model.provider,
            config.default_model.name
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
    ).await?;

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