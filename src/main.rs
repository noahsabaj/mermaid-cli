use anyhow::Result;
use clap::Parser;

use mermaid_cli::{
    app::{
        InteractiveOptions, RunOptions, format_result, load_config, persist_last_model,
        persist_reasoning_for_model, resolve_model_id, run_interactive_with,
        run_non_interactive_with,
    },
    cli::{Cli, Commands, OutputFormat},
    ollama::ensure_model as ensure_ollama_model,
    session::{ConversationManager, select_conversation},
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
        max_tokens,
        no_execute,
    }) = &cli.command
    {
        return dispatch_non_interactive(
            &cli,
            config,
            prompt.clone(),
            *format,
            *max_tokens,
            *no_execute,
        )
        .await;
    }

    dispatch_interactive(cli, config).await
}

async fn dispatch_interactive(cli: Cli, mut config: mermaid_cli::app::Config) -> Result<()> {
    let cli_model_provided = cli.model.is_some();
    let model_id = resolve_model_id(cli.model.as_deref(), &config).await?;

    if is_ollama_model(&model_id) {
        ensure_ollama_model(&model_id, &config).await?;
    }

    if cli_model_provided {
        let _ = persist_last_model(&model_id);
    }

    // F6 `--reasoning <level>`: overlay onto config so `State::new`
    // picks it up via the per-model lookup. Also persist to disk so
    // subsequent sessions without the flag remember the choice.
    if let Some(level) = cli.reasoning {
        config.reasoning_per_model.insert(model_id.clone(), level);
        let _ = persist_reasoning_for_model(&model_id, level);
    }

    let cwd = cli.path.clone().unwrap_or(std::env::current_dir()?);

    // F6 `--continue` / `--sessions`: optionally load a prior
    // conversation and seed the State with its history before the
    // first frame. Mutual exclusion is enforced by clap on Cli.
    let seed_conversation = load_seed_conversation(&cwd, cli.continue_session, cli.sessions)?;

    let recorder = match cli.record.as_ref() {
        Some(path) => Some(mermaid_cli::app::Recorder::open(path.clone())?),
        None => None,
    };
    run_interactive_with(
        config,
        cwd,
        model_id,
        InteractiveOptions {
            recorder,
            seed_conversation,
        },
    )
    .await
}

/// Resolve `--continue` / `--sessions` into an optional seeded
/// conversation. Returns `Ok(None)` when neither flag is set or no
/// saved session is available.
fn load_seed_conversation(
    cwd: &std::path::Path,
    continue_session: bool,
    sessions_picker: bool,
) -> Result<Option<mermaid_cli::session::ConversationHistory>> {
    if !continue_session && !sessions_picker {
        return Ok(None);
    }
    let manager = ConversationManager::new(cwd)?;
    if continue_session {
        return manager.load_last_conversation();
    }
    // --sessions: show the legacy picker. `select_conversation` owns
    // its own mini-TUI — entering it before the main run loop keeps
    // the two terminal modes from fighting.
    let candidates = manager.list_conversations()?;
    select_conversation(candidates)
}

async fn dispatch_non_interactive(
    cli: &Cli,
    mut config: mermaid_cli::app::Config,
    prompt: String,
    format: OutputFormat,
    max_tokens: Option<usize>,
    no_execute: bool,
) -> Result<()> {
    let cli_model_provided = cli.model.is_some();
    let model_id = resolve_model_id(cli.model.as_deref(), &config).await?;

    if is_ollama_model(&model_id) {
        ensure_ollama_model(&model_id, &config).await?;
    }

    if cli_model_provided {
        let _ = persist_last_model(&model_id);
    }

    // F6 `--reasoning <level>`: same overlay as the interactive path.
    if let Some(level) = cli.reasoning {
        config.reasoning_per_model.insert(model_id.clone(), level);
        let _ = persist_reasoning_for_model(&model_id, level);
    }
    // F6 `run --max-tokens <n>`: overlay the config's per-model cap.
    if let Some(n) = max_tokens {
        config.default_model.max_tokens = n;
    }

    let cwd = cli.path.clone().unwrap_or(std::env::current_dir()?);
    let result =
        run_non_interactive_with(config, cwd, model_id, prompt, RunOptions { no_execute }).await?;
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
