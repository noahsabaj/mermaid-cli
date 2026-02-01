use anyhow::Result;
use clap::Parser;

use mermaid_cli::{
    app::{load_config, persist_last_model},
    cli::{Cli, Commands, OutputFormat},
    ollama::{ensure_model as ensure_ollama_model, require_any_model},
    runtime::{NonInteractiveRunner, Orchestrator},
    utils::init_logger,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Initialize tracing subscriber (controlled by --verbose flag or RUST_LOG env var)
    init_logger(cli.verbose);

    // Check for Run subcommand (non-interactive mode)
    if let Some(Commands::Run {
        prompt,
        format,
        max_tokens,
        no_execute,
    }) = &cli.command
    {
        return run_non_interactive(&cli, prompt.clone(), *format, *max_tokens, *no_execute).await;
    }

    // Create and run the orchestrator for interactive mode
    let orchestrator = Orchestrator::new(cli)?;
    orchestrator.run().await
}

/// Run in non-interactive mode
async fn run_non_interactive(
    cli: &Cli,
    prompt: String,
    format: OutputFormat,
    max_tokens: Option<usize>,
    no_execute: bool,
) -> Result<()> {
    // Load configuration
    let config = load_config().unwrap_or_default();

    // Determine model to use (CLI arg > last_used > default_model)
    let cli_model_provided = cli.model.is_some();
    let model_id = if let Some(model) = &cli.model {
        model.clone()
    } else if let Some(last_model) = &config.last_used_model {
        last_model.clone()
    } else if !config.default_model.provider.is_empty()
        && !config.default_model.name.is_empty()
    {
        format!(
            "{}/{}",
            config.default_model.provider, config.default_model.name
        )
    } else {
        // No model configured - check if any models are available
        let available = require_any_model().await?;
        format!("ollama/{}", available[0])
    };

    // Validate model exists
    ensure_ollama_model(&model_id, true).await?;

    // Persist model if CLI flag was used
    if cli_model_provided {
        let _ = persist_last_model(&model_id);
    }

    // Determine project path
    let project_path = cli
        .path
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    // Use CLI args if provided, otherwise fall back to config
    let effective_max_tokens = max_tokens.or(Some(config.non_interactive.max_tokens));
    let effective_no_execute = no_execute || config.non_interactive.no_execute;

    // Get backend from config (clone to avoid borrow issues)
    let backend_str = config.behavior.backend.clone();
    let backend = if backend_str == "auto" {
        None
    } else {
        Some(backend_str.as_str())
    };

    // Create and run the non-interactive runner
    let runner = NonInteractiveRunner::new(
        model_id,
        project_path,
        config,
        effective_no_execute,
        effective_max_tokens,
        backend,
    )
    .await?;

    // Execute the prompt
    let result = runner.execute(prompt).await?;

    // Format and output the result
    let formatted = runner.format_result(&result, format);
    println!("{}", formatted);

    // Exit with appropriate code
    if !result.errors.is_empty() {
        std::process::exit(1);
    }

    Ok(())
}
