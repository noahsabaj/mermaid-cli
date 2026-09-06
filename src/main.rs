use anyhow::{Context, Result};
use clap::Parser;

use mermaid_model::utils::init_logger;
use mermaid_runtime::{NewTask, RuntimeStore, TaskStatus};

use mermaid_cli::{
    app::{
        InteractiveOptions, RunOptions, format_result, load_layered_config_or_warn,
        persist_last_model, persist_reasoning_for_model, resolve_model_id, run_interactive_with,
        run_non_interactive_with,
    },
    cli::{Cli, Commands, OutputFormat, resolve_run_prompt},
    ollama::ensure_model as ensure_ollama_model,
    session::{ConversationManager, SessionEntry, select_conversation},
};

fn main() -> Result<()> {
    // Best-effort process hardening (no core dumps, no ptrace attach) before we
    // parse args or touch config — a crash must not write a core file carrying
    // secrets, and the process shouldn't be trivially attachable.
    mermaid_runtime::hardening::harden_process();

    // The `__sandbox-exec` launcher applies OS confinement to this process and
    // execve's the wrapped command. It must run before the async runtime spawns
    // worker threads so the seccomp filter covers a single-threaded image. It
    // returns only on failure (on success execve replaces the process image).
    if let Some(code) = mermaid_cli::app::sandbox_exec::maybe_dispatch(std::env::args_os()) {
        std::process::exit(code);
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build the async runtime")?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
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
    //
    // The config is the layered merge: defaults < user file < project file
    // (`<git-root>/.mermaid/config.toml`, located from cwd — so `-p /repo`
    // adopts that repo's project config, consistent with how `.mermaid/`
    // memory and conversations key off the same root) < session flags
    // (`-c` + the dedicated sandbox/run flags, collected by `session_flags`).
    let cwd = cli.path.clone().unwrap_or(std::env::current_dir()?);
    let mut config = load_layered_config_or_warn(Some(&cwd), &cli.session_flags());
    apply_prompt_flags(&cli, &mut config)?;
    if let Some(cmd) = &cli.command
        && mermaid_cli::cli::handle_command(cmd, &config, &cwd, cli.model.as_deref()).await?
    {
        return Ok(());
    }

    // Otherwise: Commands::Run → headless driver; else interactive.
    // `--max-tokens` / `--allow-untrusted-tools` are already folded into the
    // config via the session-flags layer above.
    if let Some(Commands::Run {
        prompt,
        format,
        no_execute,
        output_schema,
        plan,
        plan_autoaccept,
        ..
    }) = &cli.command
    {
        // Read + validate the schema BEFORE any model work: a bad file must
        // fail fast, not after a full agentic run.
        let output_schema = output_schema
            .as_deref()
            .map(load_output_schema)
            .transpose()?;
        return dispatch_non_interactive(
            &cli,
            config,
            prompt.clone(),
            *format,
            output_schema,
            HeadlessFlags {
                no_execute: *no_execute,
                plan: *plan,
                plan_autoaccept: *plan_autoaccept,
            },
        )
        .await;
    }

    dispatch_interactive(cli, config).await
}

fn apply_prompt_flags(cli: &Cli, config: &mut mermaid_domain::Config) -> Result<()> {
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
    // The prompt flags are the one config surface that deliberately stays
    // OUTSIDE the layered table merge: `config.prompt` is `#[serde(skip)]`
    // (one-off personas must never round-trip through TOML or persist), so
    // they overlay the typed Config here. Everything else config-shaped
    // (`--no-network`, `--confine-fs`, `--sandbox`, `run --max-tokens`,
    // `run --allow-untrusted-tools`) rides the session-flags layer.
    Ok(())
}

async fn dispatch_interactive(cli: Cli, mut config: mermaid_domain::Config) -> Result<()> {
    let cli_model_provided = cli.model.is_some();
    let model_id = resolve_model_id(cli.model.as_deref(), &config).await?;

    if is_ollama_model(&model_id) {
        ensure_ollama_model(&model_id, &config).await?;
    }

    // Remember the model only when its provider actually resolves: a
    // `--model` whose provider cannot be built here (no key, unknown name)
    // would otherwise hijack every later session's startup default.
    if cli_model_provided
        && mermaid_cli::providers::model_provider_resolves(&config, &model_id)
        && let Err(err) = persist_last_model(&model_id)
    {
        tracing::warn!(error = %err, "failed to persist last-used model");
    }

    // F6 `--reasoning <level>`: overlay onto config so `State::new`
    // picks it up via the per-model lookup. Also persist to disk so
    // subsequent sessions without the flag remember the choice.
    if let Some(level) = cli.reasoning {
        config.reasoning_per_model.insert(model_id.clone(), level);
        if let Err(err) = persist_reasoning_for_model(&model_id, level) {
            tracing::warn!(error = %err, "failed to persist reasoning level for model");
        }
    }

    let cwd = cli.path.clone().unwrap_or(std::env::current_dir()?);

    // F6 `--continue` / `--resume [id]`: optionally load a prior
    // conversation and seed the State with its history before the
    // first frame. Mutual exclusion is enforced by clap on Cli.
    let seed_conversation = load_seed_conversation(&cwd, cli.continue_session, &cli.resume, true)?;

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

/// Resolve `--continue` / `--resume [id]` into an optional seeded
/// conversation, shared by the interactive and headless dispatches.
/// Returns `Ok(None)` when neither flag is set or no saved session is
/// available. Bare `--resume` (the searchable picker) is interactive-only;
/// the headless caller passes `interactive = false` and gets a clear error
/// telling it to supply the id.
fn load_seed_conversation(
    cwd: &std::path::Path,
    continue_session: bool,
    resume: &Option<Option<String>>,
    interactive: bool,
) -> Result<Option<mermaid_domain::ConversationHistory>> {
    if continue_session {
        return ConversationManager::new(cwd)?.load_last_conversation();
    }
    match resume {
        None => Ok(None),
        // --resume <id>: direct load, both modes. A missing/invalid id is a
        // hard error — a caller that named a session must not silently start
        // a fresh one.
        Some(Some(id)) => {
            let manager = ConversationManager::new(cwd)?;
            let history = manager.load_conversation(id).map_err(|e| {
                anyhow::anyhow!(
                    "session {id} not found under {}/.mermaid/conversations: {e}",
                    cwd.display()
                )
            })?;
            Ok(Some(history))
        },
        Some(None) if !interactive => anyhow::bail!(
            "--resume without a session id opens an interactive picker; \
             pass --resume <session-id> or use --continue"
        ),
        // Bare --resume: the searchable picker. `select_conversation` owns its
        // own mini-TUI — entering it before the main run loop keeps the two
        // terminal modes from fighting. Pair each conversation with its
        // on-disk size for the meta line; a stat failure just shows 0B rather
        // than dropping the row.
        Some(None) => {
            let manager = ConversationManager::new(cwd)?;
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
            select_conversation(entries, &manager, chrono::Local::now())
        },
    }
}

/// Load and structurally check an `--output-schema` file: bounded size,
/// valid JSON, top-level object. Deeper validity surfaces at validation time.
fn load_output_schema(path: &std::path::Path) -> Result<serde_json::Value> {
    const MAX_SCHEMA_BYTES: u64 = 64 * 1024;
    let meta = std::fs::metadata(path)
        .with_context(|| format!("--output-schema: cannot read {}", path.display()))?;
    anyhow::ensure!(
        meta.len() <= MAX_SCHEMA_BYTES,
        "--output-schema: {} is {} bytes; cap is {} (64 KiB)",
        path.display(),
        meta.len(),
        MAX_SCHEMA_BYTES
    );
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("--output-schema: cannot read {}", path.display()))?;
    let schema: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("--output-schema: {} is not valid JSON", path.display()))?;
    anyhow::ensure!(
        schema.is_object(),
        "--output-schema: {} must contain a JSON object (a JSON Schema)",
        path.display()
    );
    Ok(schema)
}

/// The `mermaid run` boolean switches, grouped so the dispatch signature
/// stays readable as flags accrue.
struct HeadlessFlags {
    no_execute: bool,
    plan: bool,
    plan_autoaccept: bool,
}

async fn dispatch_non_interactive(
    cli: &Cli,
    mut config: mermaid_domain::Config,
    prompt: Option<String>,
    format: OutputFormat,
    output_schema: Option<serde_json::Value>,
    flags: HeadlessFlags,
) -> Result<()> {
    let prompt = resolve_prompt_from_stdin(prompt)?;
    let cli_model_provided = cli.model.is_some();
    let model_id = resolve_model_id(cli.model.as_deref(), &config).await?;

    if is_ollama_model(&model_id) {
        ensure_ollama_model(&model_id, &config).await?;
    }

    // Same gate as the interactive path: never remember a model whose
    // provider cannot be built on this machine.
    if cli_model_provided
        && mermaid_cli::providers::model_provider_resolves(&config, &model_id)
        && let Err(err) = persist_last_model(&model_id)
    {
        tracing::warn!(error = %err, "failed to persist last-used model");
    }

    // F6 `--reasoning <level>`: same overlay as the interactive path.
    if let Some(level) = cli.reasoning {
        config.reasoning_per_model.insert(model_id.clone(), level);
        if let Err(err) = persist_reasoning_for_model(&model_id, level) {
            tracing::warn!(error = %err, "failed to persist reasoning level for model");
        }
    }
    let cwd = cli.path.clone().unwrap_or(std::env::current_dir()?);
    // `--resume <id>` / `--continue` also work headless. A script asking for
    // continuity must not silently start fresh, so `--continue` with no saved
    // session is an error here (interactive mode tolerates it).
    let seed = load_seed_conversation(&cwd, cli.continue_session, &cli.resume, false)?;
    if cli.continue_session && seed.is_none() {
        anyhow::bail!("--continue: no saved session found for {}", cwd.display());
    }
    let runtime_task_id = create_run_task(&cwd, &model_id, &prompt, flags.no_execute);
    let run_result = run_non_interactive_with(
        config,
        cwd,
        model_id,
        prompt,
        RunOptions {
            no_execute: flags.no_execute,
            task_id: runtime_task_id.clone(),
            stream_ndjson: matches!(format, OutputFormat::Ndjson),
            seed,
            output_schema,
            plan: flags.plan,
            plan_autoaccept: flags.plan_autoaccept,
            ..RunOptions::default()
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
    // NDJSON was streamed live during the run; the other formats print the
    // final payload here.
    if !matches!(format, OutputFormat::Ndjson) {
        println!("{}", format_result(&result, format));
    }
    // Surface the session id for the plain-text formats on STDERR — stdout is
    // the response payload scripts pipe (json/ndjson carry the id in-band).
    if matches!(format, OutputFormat::Text | OutputFormat::Markdown) {
        eprintln!("session: {}", result.session_id);
    }

    if !result.errors.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// Resolve the run prompt, reading piped stdin when the CLI arg is `-`/absent.
/// The pure decision lives in `cli::resolve_run_prompt`; this only does the I/O
/// (never reads a TTY, so an interactive `mermaid run` with no prompt errors
/// cleanly instead of blocking).
fn resolve_prompt_from_stdin(prompt: Option<String>) -> Result<String> {
    use std::io::{IsTerminal, Read};
    let stdin_text = if std::io::stdin().is_terminal() {
        None
    } else {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).ok();
        Some(buf)
    };
    resolve_run_prompt(prompt.as_deref(), stdin_text).map_err(anyhow::Error::msg)
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
    // Best-effort by the flaky-drive posture, but never silent: a task row
    // stuck on `queued` because this write failed is a puzzle the log should
    // solve.
    if let Err(error) = store
        .tasks()
        .update_status(&task.id, TaskStatus::Running, None)
    {
        tracing::warn!(task = %task.id, %error, "could not mark the run task running");
    }
    if no_execute
        && let Err(error) =
            store
                .tasks()
                .add_event(&task.id, "run_option", "tools disabled by --no-execute")
    {
        tracing::warn!(task = %task.id, %error, "could not record the run option");
    }
    let _ = mermaid_runtime::run_plugin_hooks(
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
        if let Err(error) = store.tasks().update_status(task_id, status, final_report) {
            tracing::warn!(task = %task_id, %error, "could not record the run's terminal status");
        }
        let _ = mermaid_runtime::run_plugin_hooks(
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
