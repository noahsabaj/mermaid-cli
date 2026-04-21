use anyhow::Result;
use clap::Parser;

use mermaid_cli::{
    app::{
        format_result, load_config, persist_last_model, resolve_model_id, run_interactive,
        run_non_interactive,
    },
    cli::{Cli, Commands, OutputFormat},
    ollama::ensure_model as ensure_ollama_model,
    utils::init_logger,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logger(cli.verbose);

    // Handle stand-alone subcommands first (init, list, status, add,
    // remove, mcp, version). Returns Ok(true) when the subcommand
    // handled the invocation and we should exit.
    let config = load_config().unwrap_or_default();
    if let Some(cmd) = &cli.command
        && mermaid_cli::cli::handle_command(cmd, &config).await?
    {
        return Ok(());
    }

    // Otherwise: Commands::Run → headless driver; else interactive.
    if let Some(Commands::Run {
        prompt,
        format,
        max_tokens: _,
        no_execute: _,
    }) = &cli.command
    {
        return dispatch_non_interactive(&cli, config, prompt.clone(), *format).await;
    }

    dispatch_interactive(cli, config).await
}

async fn dispatch_interactive(cli: Cli, config: mermaid_cli::app::Config) -> Result<()> {
    let cli_model_provided = cli.model.is_some();
    let model_id = resolve_model_id(cli.model.as_deref(), &config).await?;

    if is_ollama_model(&model_id) {
        ensure_ollama_model(&model_id, &config).await?;
    }

    if cli_model_provided {
        let _ = persist_last_model(&model_id);
    }

    let cwd = cli.path.clone().unwrap_or(std::env::current_dir()?);
    run_interactive(config, cwd, model_id).await
}

async fn dispatch_non_interactive(
    cli: &Cli,
    config: mermaid_cli::app::Config,
    prompt: String,
    format: OutputFormat,
) -> Result<()> {
    let cli_model_provided = cli.model.is_some();
    let model_id = resolve_model_id(cli.model.as_deref(), &config).await?;

    if is_ollama_model(&model_id) {
        ensure_ollama_model(&model_id, &config).await?;
    }

    if cli_model_provided {
        let _ = persist_last_model(&model_id);
    }

    let cwd = cli.path.clone().unwrap_or(std::env::current_dir()?);
    let result = run_non_interactive(config, cwd, model_id, prompt).await?;
    println!("{}", format_result(&result, format));

    if !result.errors.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// Bare model names default to Ollama; explicit `ollama/…` too.
/// Anything with another provider prefix is remote.
fn is_ollama_model(model_id: &str) -> bool {
    match model_id.split_once('/') {
        Some((provider, _)) => provider.eq_ignore_ascii_case("ollama"),
        None => true,
    }
}
