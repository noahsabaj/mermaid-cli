use anyhow::Result;
use clap::Parser;

use mermaid_cli::{
    app::{load_config, persist_last_model, resolve_model_id},
    cli::{Cli, Commands, OutputFormat},
    ollama::ensure_model as ensure_ollama_model,
    runtime::{NonInteractiveRunner, Orchestrator, actionable_init_error, format_result},
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

    // Validate model exists — only for Ollama. Remote providers
    // (anthropic, openai, groq, etc.) don't have a local pull step;
    // the adapter will surface 404 if the model name is wrong.
    if is_ollama_model(&model_id) {
        ensure_ollama_model(&model_id, &config).await?;
    }

    // Use CLI args if provided, otherwise fall back to config
    let effective_max_tokens = max_tokens.or(Some(config.non_interactive.max_tokens));
    let effective_no_execute = no_execute || config.non_interactive.no_execute;

    // Create and run the non-interactive runner. Clone model_id so we
    // can still reference it for the persist below (the runner moves it).
    let runner = NonInteractiveRunner::new(
        model_id.clone(),
        config,
        effective_no_execute,
        effective_max_tokens,
        cli.reasoning,
    )
    .await
    .map_err(|e| anyhow::anyhow!(actionable_init_error(&model_id, e)))?;

    // Persist model AFTER successful creation — guarantees we never
    // persist a broken model to last_used_model. (Step 5d fix.)
    if cli_model_provided {
        let _ = persist_last_model(&model_id);
    }

    // Execute the prompt
    let result = runner.execute(prompt).await?;

    // Format and output the result
    let formatted = format_result(&result, format);
    println!("{}", formatted);

    // Exit with appropriate code
    if !result.errors.is_empty() {
        std::process::exit(1);
    }

    Ok(())
}

/// Whether the model_id resolves to the Ollama backend. Bare names
/// (no `/`) default to Ollama; explicit `ollama/...` is also Ollama;
/// anything with a non-ollama provider prefix is remote.
fn is_ollama_model(model_id: &str) -> bool {
    match model_id.split_once('/') {
        Some((provider, _)) => provider.eq_ignore_ascii_case("ollama"),
        None => true,
    }
}
