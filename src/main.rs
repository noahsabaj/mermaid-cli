use anyhow::{Context, Result};
use clap::Parser;

use mermaid_cli::{
    app::{
        InteractiveOptions, RunOptions, format_result, load_config_or_warn, persist_last_model,
        persist_reasoning_for_model, resolve_model_id, run_interactive_with,
        run_non_interactive_with,
    },
    cli::{Cli, Commands, OutputFormat},
    ollama::ensure_model as ensure_ollama_model,
    runtime::{NewTask, RuntimeStore, TaskStatus},
    session::{ConversationManager, SessionEntry, select_conversation},
    utils::init_logger,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logger(cli.verbose);

    // `--replay` is fully self-contained (the recording embeds its config
    // snapshot), so it dispatches before any config/model resolution. A
    // non-deterministic fold is a reducer purity bug — exit non-zero so CI
    // and scripts can catch it.
    if let Some(path) = cli.replay.as_ref() {
        let deterministic = mermaid_cli::app::run_replay(path)?;
        if !deterministic {
            std::process::exit(1);
        }
        return Ok(());
    }

    // Handle stand-alone subcommands first (init, list, status, add,
    // remove, mcp, version). Returns Ok(true) when the subcommand
    // handled the invocation and we should exit.
    let mut config = load_config_or_warn();
    apply_prompt_flags(&cli, &mut config)?;
    let cwd = cli.path.clone().unwrap_or(std::env::current_dir()?);
    if let Some(cmd) = &cli.command
        && mermaid_cli::cli::handle_command(cmd, &config, &cwd, cli.model.as_deref()).await?
    {
        return Ok(());
    }

    // Otherwise: Commands::Run → headless driver; else interactive.
    if let Some(Commands::Run {
        prompt,
        format,
        max_tokens,
        no_execute,
        allow_untrusted_tools,
    }) = &cli.command
    {
        return dispatch_non_interactive(
            &cli,
            config,
            prompt.clone(),
            *format,
            *max_tokens,
            *no_execute,
            *allow_untrusted_tools,
        )
        .await;
    }

    dispatch_interactive(cli, config).await
}

fn apply_prompt_flags(cli: &Cli, config: &mut mermaid_cli::app::Config) -> Result<()> {
    if let Some(prompt) = cli.system_prompt.as_ref() {
        config.prompt.system_prompt = Some(prompt.clone());
    }
    if let Some(path) = cli.system_prompt_file.as_ref() {
        config.prompt.system_prompt =
            Some(std::fs::read_to_string(path).with_context(|| {
                format!("failed to read --system-prompt-file {}", path.display())
            })?);
    }
    if let Some(prompt) = cli.append_system_prompt.as_ref() {
        config.prompt.append_system_prompt.push(prompt.clone());
    }
    if let Some(path) = cli.append_system_prompt_file.as_ref() {
        config
            .prompt
            .append_system_prompt
            .push(std::fs::read_to_string(path).with_context(|| {
                format!(
                    "failed to read --append-system-prompt-file {}",
                    path.display()
                )
            })?);
    }
    Ok(())
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
    let seed_conversation = load_seed_conversation(&cwd, cli.continue_session, cli.resume)?;

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
    resume_picker: bool,
) -> Result<Option<mermaid_cli::session::ConversationHistory>> {
    if !continue_session && !resume_picker {
        return Ok(None);
    }
    let manager = ConversationManager::new(cwd)?;
    if continue_session {
        return manager.load_last_conversation();
    }
    // --resume: the searchable picker. `select_conversation` owns its own
    // mini-TUI — entering it before the main run loop keeps the two terminal
    // modes from fighting. Pair each conversation with its on-disk size for
    // the meta line; a stat failure just shows 0B rather than dropping the row.
    let entries: Vec<SessionEntry> = manager
        .list_conversations()?
        .into_iter()
        .map(|history| {
            let path = manager
                .conversations_dir()
                .join(format!("{}.json", history.id));
            let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            SessionEntry {
                history,
                size_bytes,
            }
        })
        .collect();
    select_conversation(entries, chrono::Local::now())
}

async fn dispatch_non_interactive(
    cli: &Cli,
    mut config: mermaid_cli::app::Config,
    prompt: String,
    format: OutputFormat,
    max_tokens: Option<usize>,
    no_execute: bool,
    allow_untrusted_tools: bool,
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
    // `run --allow-untrusted-tools`: headless opt-in for non-replayable tools
    // on an Ask decision (otherwise blocked when there's no approval UI).
    if allow_untrusted_tools {
        config.safety.allow_untrusted_headless_tools = true;
    }

    let cwd = cli.path.clone().unwrap_or(std::env::current_dir()?);
    let runtime_task_id = create_run_task(&cwd, &model_id, &prompt, no_execute);
    let run_result = run_non_interactive_with(
        config,
        cwd,
        model_id,
        prompt,
        RunOptions {
            no_execute,
            task_id: runtime_task_id.clone(),
        },
    )
    .await;
    match &run_result {
        Ok(result) if result.errors.is_empty() => {
            finish_run_task(
                runtime_task_id.as_deref(),
                TaskStatus::Completed,
                Some(&result.response),
            );
        },
        Ok(result) => {
            finish_run_task(
                runtime_task_id.as_deref(),
                TaskStatus::Failed,
                Some(&result.errors.join("\n")),
            );
        },
        Err(err) => {
            finish_run_task(
                runtime_task_id.as_deref(),
                TaskStatus::Failed,
                Some(&err.to_string()),
            );
        },
    }
    let result = run_result?;
    println!("{}", format_result(&result, format));

    if !result.errors.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

fn create_run_task(
    cwd: &std::path::Path,
    model_id: &str,
    prompt: &str,
    no_execute: bool,
) -> Option<String> {
    let store = RuntimeStore::open_default().ok()?;
    let task = store
        .tasks()
        .create(NewTask::new(
            task_title_from_prompt(prompt),
            cwd.display().to_string(),
            model_id.to_string(),
        ))
        .ok()?;
    let _ = store
        .tasks()
        .update_status(&task.id, TaskStatus::Running, None);
    if no_execute {
        let _ = store
            .tasks()
            .add_event(&task.id, "run_option", "tools disabled by --no-execute");
    }
    let _ = mermaid_cli::runtime::run_plugin_hooks(
        "task_start",
        &serde_json::json!({
            "id": task.id.clone(),
            "title": task.title.clone(),
            "project_path": task.project_path.clone(),
            "model_id": task.model_id.clone(),
        }),
    );
    Some(task.id)
}

fn finish_run_task(task_id: Option<&str>, status: TaskStatus, final_report: Option<&str>) {
    let Some(task_id) = task_id else {
        return;
    };
    if let Ok(store) = RuntimeStore::open_default() {
        let _ = store.tasks().update_status(task_id, status, final_report);
        let _ = mermaid_cli::runtime::run_plugin_hooks(
            "task_stop",
            &serde_json::json!({
                "id": task_id,
                "status": status.as_str(),
                "final_report": final_report,
            }),
        );
    }
}

fn task_title_from_prompt(prompt: &str) -> String {
    let one_line = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return "mermaid run".to_string();
    }
    if one_line.len() <= 80 {
        return one_line;
    }
    let end = one_line.floor_char_boundary(80);
    format!("{}...", &one_line[..end])
}

/// Bare model names default to Ollama; explicit `ollama/…` too.
/// Anything with another provider prefix is remote.
fn is_ollama_model(model_id: &str) -> bool {
    match model_id.split_once('/') {
        Some((provider, _)) => provider.eq_ignore_ascii_case("ollama"),
        None => true,
    }
}
