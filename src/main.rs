use anyhow::Result;
use clap::Parser;

use mermaid_cli::{
    app::{load_config, persist_last_model, resolve_model_id},
    cli::{Cli, Commands, OutputFormat},
    ollama::ensure_model as ensure_ollama_model,
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
    let model_id = resolve_model_id(cli.model.as_deref(), &config).await?;

    // Validate model exists
    ensure_ollama_model(&model_id).await?;

    // Persist model if CLI flag was used
    if cli_model_provided {
        let _ = persist_last_model(&model_id);
    }

    // Use CLI args if provided, otherwise fall back to config
    let effective_max_tokens = max_tokens.or(Some(config.non_interactive.max_tokens));
    let effective_no_execute = no_execute || config.non_interactive.no_execute;

    // Create and run the non-interactive runner
    let runner =
        NonInteractiveRunner::new(model_id, config, effective_no_execute, effective_max_tokens)
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
