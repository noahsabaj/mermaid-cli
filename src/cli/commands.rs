use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;
use std::sync::Arc;

use crate::{
    app::{Config, get_config_dir, init_config, load_config_or_warn},
    domain::{
        ChatRequest, Cmd, CompactionRecord, CompactionResult, CompactionTrigger, Msg, SlashCmd,
        State, build_replacement_messages, estimate_context_usage_for_request, prepare_compaction,
        update,
    },
    models::{BackendConfig, ChatMessage, Model, PROVIDER_REGISTRY, lookup_provider},
    ollama::is_installed as is_ollama_installed,
    providers::discovery::{configured_remote_provider_names, configured_remote_providers},
    runtime::{NewProviderProbe, RuntimeClient, RuntimeStore, TaskRecord},
    session::ConversationManager,
};

use super::{Commands, GitHost, OutputFormat, PairCommand, PluginCommand, PrCommand, QaCommand};

/// Handle CLI subcommands
/// Returns Ok(true) if the command was handled and we should exit
/// Returns Ok(false) if we should continue to the main application
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub async fn handle_command(
    command: &Commands,
    config: &Config,
    cwd: &Path,
    cli_model: Option<&str>,
) -> Result<bool> {
    match command {
        Commands::Init => {
            println!("Initializing Mermaid configuration...");
            init_config()?;
            println!("Configuration initialized successfully!");
            Ok(true)
        },
        Commands::List => {
            list_models(config).await?;
            Ok(true)
        },
        Commands::Models => {
            show_models(config).await?;
            Ok(true)
        },
        Commands::ModelInfo { model } => {
            show_model_info(model, config).await?;
            Ok(true)
        },
        Commands::Version => {
            show_version();
            Ok(true)
        },
        Commands::Update { check, force } => {
            run_update(*check, *force).await?;
            Ok(true)
        },
        Commands::Status => {
            show_status(config).await?;
            Ok(true)
        },
        Commands::Doctor { format } => {
            show_doctor(config, cwd, cli_model, *format).await?;
            Ok(true)
        },
        Commands::Feedback { stdout, format } => {
            super::feedback::run_feedback(config, cwd, cli_model, *stdout, *format).await?;
            Ok(true)
        },
        Commands::SelfTest {
            format,
            keep_workspace,
        } => {
            run_self_test(config, *format, *keep_workspace)?;
            Ok(true)
        },
        Commands::Tasks { limit } => {
            show_tasks(*limit)?;
            Ok(true)
        },
        Commands::Task { id, follow } => {
            if *follow {
                follow_task(id)?;
            } else {
                show_task(id)?;
            }
            Ok(true)
        },
        Commands::Processes { limit } => {
            show_processes(*limit)?;
            Ok(true)
        },
        Commands::Logs { id } => {
            show_logs(id)?;
            Ok(true)
        },
        Commands::Stop { id } => {
            stop_process(id)?;
            Ok(true)
        },
        Commands::Restart { id } => {
            restart_process(id)?;
            Ok(true)
        },
        Commands::Open { target } => {
            open_target(target)?;
            Ok(true)
        },
        Commands::Ports => {
            show_ports()?;
            Ok(true)
        },
        Commands::Approvals => {
            show_approvals()?;
            Ok(true)
        },
        Commands::Approve { id } => {
            approve(id)?;
            Ok(true)
        },
        Commands::Deny { id } => {
            deny(id)?;
            Ok(true)
        },
        Commands::Cancel { id } => {
            cancel_task(id)?;
            Ok(true)
        },
        Commands::ToolRuns { limit } => {
            show_tool_runs(*limit)?;
            Ok(true)
        },
        Commands::Checkpoints { limit } => {
            show_checkpoints(*limit)?;
            Ok(true)
        },
        Commands::Restore { id, force } => {
            restore_checkpoint(id, *force)?;
            Ok(true)
        },
        Commands::Plugin { command } => {
            handle_plugin(command)?;
            Ok(true)
        },
        Commands::Daemon { command } => {
            super::daemon::handle_daemon_command(command)?;
            Ok(true)
        },
        Commands::Pair { command } => {
            handle_pair(command)?;
            Ok(true)
        },
        Commands::Qa { command } => {
            handle_qa(command, config, cwd)?;
            Ok(true)
        },
        Commands::Add {
            name,
            yes,
            command,
            arg,
            env,
            url,
            header,
            env_header,
        } => {
            // --url conflicts with --command/--arg/--env at the clap level, so
            // exactly one registration path runs.
            match url {
                Some(url) => {
                    crate::mcp::add_http_server(
                        name,
                        url.clone(),
                        header.clone(),
                        env_header.clone(),
                    )
                    .await?;
                },
                None => {
                    crate::mcp::add_server(name, *yes, command.clone(), arg.clone(), env.clone())
                        .await?;
                },
            }
            Ok(true)
        },
        Commands::Remove { name } => {
            crate::mcp::remove_server(name).await?;
            Ok(true)
        },
        Commands::Pr { command } => {
            handle_pr(command)?;
            Ok(true)
        },
        Commands::Mcp => {
            show_mcp_servers();
            Ok(true)
        },
        Commands::Login { provider } => {
            login(provider.as_deref(), config)?;
            Ok(true)
        },
        Commands::Logout { provider } => {
            logout(provider, config)?;
            Ok(true)
        },
        Commands::CloudSetup => {
            // Interactive stdin prompt — runs before the TUI enters
            // raw mode so rpassword works. The in-TUI slash command
            // `/cloud-setup` just points users here.
            let _ = crate::ollama::setup_cloud_interactive();
            Ok(true)
        },
        Commands::Chat => Ok(false),       // Continue to chat interface
        Commands::Run { .. } => Ok(false), // Handled by main.rs
    }
}

fn handle_qa(command: &QaCommand, config: &Config, cwd: &Path) -> Result<()> {
    match command {
        QaCommand::CompactSmoke { turns, format } => {
            let report = match run_qa_compact_smoke(config, cwd, *turns) {
                Ok(report) => report,
                Err(err) => QaCompactSmokeReport::failed(cwd, *turns, err.to_string()),
            };
            print_qa_compact_report(&report, *format)?;
            anyhow::ensure!(report.ok, "qa compact smoke failed");
            Ok(())
        },
    }
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct DoctorReport {
    pub(crate) ok: bool,
    pub(crate) cwd: String,
    /// The `--profile` overlay active for this invocation, if any.
    pub(crate) active_profile: Option<String>,
    pub(crate) active_model: Option<String>,
    pub(crate) model_error: Option<String>,
    pub(crate) model_capabilities: Option<DoctorModelCapabilities>,
    pub(crate) safety_mode: String,
    pub(crate) checkpoint_on_mutation: bool,
    pub(crate) prompt_customized: bool,
    pub(crate) ollama: DoctorCheck,
    pub(crate) remote_providers: Vec<String>,
    /// Providers the user configured that still cannot be built, with the
    /// factory's own reason. Empty on a clean machine.
    pub(crate) provider_problems: Vec<DoctorProviderProblem>,
    pub(crate) project_instructions: DoctorCheck,
    pub(crate) tools: Vec<String>,
    pub(crate) runtime: DoctorRuntime,
    pub(crate) next_steps: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct DoctorModelCapabilities {
    pub(crate) provider: String,
    pub(crate) name: String,
    pub(crate) supports_tools: bool,
    pub(crate) supports_vision: bool,
    pub(crate) reasoning: String,
    pub(crate) max_context_tokens: Option<usize>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct DoctorCheck {
    pub(crate) status: &'static str,
    pub(crate) message: String,
}

/// A provider that is configured but unusable. `reason` is `ProviderFactory`'s
/// own error, so `doctor` reports exactly what a real request would have said.
#[derive(Debug, serde::Serialize)]
pub(crate) struct DoctorProviderProblem {
    pub(crate) name: String,
    pub(crate) reason: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct DoctorRuntime {
    pub(crate) daemon: DoctorCheck,
    pub(crate) local_store: DoctorCheck,
}

fn web_doctor_entries(config: &Config) -> (Vec<String>, Vec<String>) {
    let capabilities = crate::providers::tool::web::WebCapabilities::resolve(&config.web);
    let mut tools = Vec::new();
    let mut next_steps = Vec::new();
    for (name, status) in [
        ("web_fetch", capabilities.fetch),
        ("web_search", capabilities.search),
    ] {
        if config.safety.network == crate::app::NetworkPolicy::Deny {
            next_steps.push(format!(
                "{name} is disabled by safety.network = \"deny\" (selected backend '{}'; {}).",
                status.backend, status.trust_destination
            ));
        } else if status.available {
            tools.push(format!(
                "{name} ({}; {})",
                status.backend, status.trust_destination
            ));
        } else {
            next_steps.push(format!(
                "{name} is unavailable with backend '{}': {}.",
                status.backend,
                status
                    .reason
                    .as_deref()
                    .unwrap_or("the selected backend could not be initialized")
            ));
        }
    }
    (tools, next_steps)
}

async fn show_doctor(
    config: &Config,
    cwd: &Path,
    cli_model: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let report = build_doctor_report(config, cwd, cli_model).await;
    print_doctor_report(&report, format)
}

/// Assemble the full readiness report without printing — shared by
/// `mermaid doctor` and the `mermaid feedback` diagnostic bundle.
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub(crate) async fn build_doctor_report(
    config: &Config,
    cwd: &Path,
    cli_model: Option<&str>,
) -> DoctorReport {
    let active_model_result = crate::app::resolve_model_id(cli_model, config).await;
    let (active_model, model_error, model_capabilities) = match active_model_result {
        Ok(model) => {
            let snapshot = crate::domain::ProviderCapabilitySnapshot::from_model_id(&model);
            (
                Some(model),
                None,
                Some(DoctorModelCapabilities {
                    provider: snapshot.provider,
                    name: snapshot.model,
                    supports_tools: snapshot.supports_tools,
                    supports_vision: snapshot.supports_vision,
                    reasoning: snapshot.reasoning,
                    max_context_tokens: snapshot.max_context_tokens,
                }),
            )
        },
        Err(err) => (None, Some(err.to_string()), None),
    };

    // Diagnostics observe, they don't heal: autostart=false so `doctor` can
    // actually report a dead server instead of reviving it mid-check. `None`
    // (unreachable) vs `Some(vec![])` (running, nothing pulled) get distinct
    // messages; the "next steps" nudge below only needs "no usable models".
    let ollama_models = if is_ollama_installed() {
        list_ollama_models(config).await
    } else {
        None
    };
    let ollama = if !is_ollama_installed() {
        DoctorCheck {
            status: "warning",
            message: "Ollama is not installed; remote providers can still work if configured."
                .to_string(),
        }
    } else {
        match &ollama_models {
            None => DoctorCheck {
                status: "warning",
                message: "Ollama is installed but not running; mermaid starts it \
                          automatically when an Ollama model is used."
                    .to_string(),
            },
            Some(models) if models.is_empty() => DoctorCheck {
                status: "warning",
                message: "Ollama is running but no local/cloud models were listed.".to_string(),
            },
            Some(models) => DoctorCheck {
                status: "ok",
                message: format!("Ollama reachable with {} models.", models.len()),
            },
        }
    };

    let remote_providers = configured_remote_provider_names(config);
    let provider_problems = crate::providers::provider_problems(config)
        .into_iter()
        .map(|problem| DoctorProviderProblem {
            name: problem.name,
            reason: problem.reason,
        })
        .collect::<Vec<_>>();
    let instruction_paths = crate::app::instructions::find_instruction_files(cwd);
    let project_instructions = if instruction_paths.is_empty() {
        DoctorCheck {
            status: "info",
            message: "No AGENTS.md or MERMAID.md found.".to_string(),
        }
    } else if let Some(loaded) = crate::app::instructions::load_from_paths(&instruction_paths) {
        DoctorCheck {
            status: "ok",
            message: format!(
                "{} bytes loaded from {} source(s){}.",
                loaded.byte_len,
                loaded.sources.len(),
                if loaded.truncated { " (truncated)" } else { "" }
            ),
        }
    } else {
        DoctorCheck {
            status: "warning",
            message: "Instruction files were found but could not be loaded.".to_string(),
        }
    };

    let daemon = match RuntimeClient::daemon().health() {
        Ok(read) => DoctorCheck {
            status: "ok",
            message: format!("daemon attached; database {}", read.value.database),
        },
        Err(err) => DoctorCheck {
            status: "info",
            message: format!("daemon not attached; CLI will use local runtime store ({err})"),
        },
    };
    let local_store = match RuntimeClient::local().health() {
        Ok(read) => DoctorCheck {
            status: "ok",
            message: format!("local runtime store ready at {}", read.value.database),
        },
        Err(err) => DoctorCheck {
            status: "warning",
            message: format!("local runtime store unavailable: {err}"),
        },
    };

    let mut tools = vec![
        "read/edit/write files".to_string(),
        "run shell commands".to_string(),
        "create checkpoints before risky mutations".to_string(),
    ];
    let (web_tools, web_next_steps) = web_doctor_entries(config);
    tools.extend(web_tools);
    if !config.mcp_servers.is_empty() {
        tools.push(format!(
            "{} configured MCP server(s)",
            config.mcp_servers.len()
        ));
    }
    if let Some(skills) = crate::app::skills::load(cwd) {
        tools.push(format!(
            "{} skill(s) discovered (SKILL.md playbooks)",
            skills.entries.len()
        ));
    }

    let mut next_steps = web_next_steps;
    if active_model.is_none() {
        next_steps.push(
            "Pick a model with `mermaid --model <provider/model>` or run `mermaid list`."
                .to_string(),
        );
    }
    if remote_providers.is_empty() && ollama_models.as_deref().unwrap_or_default().is_empty() {
        next_steps.push(
            "Install or start Ollama, pull a model, or set a remote provider API key.".to_string(),
        );
    }
    if instruction_paths.is_empty() {
        next_steps.push("Optional: add MERMAID.md or AGENTS.md with project-specific run commands and conventions.".to_string());
    }
    if next_steps.is_empty() {
        next_steps.push(
            "Start Mermaid with `mermaid` or run one prompt with `mermaid run \"...\"`."
                .to_string(),
        );
    }

    let ok = active_model.is_some()
        && local_store.status != "warning"
        && (ollama.status == "ok" || !remote_providers.is_empty());
    DoctorReport {
        ok,
        cwd: cwd.display().to_string(),
        active_profile: config.active_profile.clone(),
        active_model,
        model_error,
        model_capabilities,
        safety_mode: safety_mode_name(config.safety.mode).to_string(),
        checkpoint_on_mutation: config.safety.checkpoint_on_mutation,
        prompt_customized: config.prompt.is_customized(),
        ollama,
        remote_providers,
        provider_problems,
        project_instructions,
        tools,
        runtime: DoctorRuntime {
            daemon,
            local_store,
        },
        next_steps,
    }
}

fn print_doctor_report(report: &DoctorReport, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(report)?),
        OutputFormat::Ndjson => println!("{}", serde_json::to_string(report)?),
        OutputFormat::Markdown => {
            println!("# Mermaid Doctor\n");
            print_doctor_text(report);
        },
        OutputFormat::Text => print_doctor_text(report),
    }
    Ok(())
}

fn print_doctor_text(report: &DoctorReport) {
    println!(
        "Mermaid Doctor: {}",
        if report.ok {
            "ready"
        } else {
            "needs attention"
        }
    );
    println!("Project: {}", report.cwd);
    match (&report.active_model, &report.model_error) {
        (Some(model), _) => println!("  [OK] Active model: {model}"),
        (None, Some(error)) => println!("  [WARNING] Active model: {error}"),
        _ => println!("  [WARNING] Active model: unresolved"),
    }
    if let Some(caps) = &report.model_capabilities {
        println!(
            "       provider={} tools={} vision={} reasoning={} context={}",
            caps.provider,
            caps.supports_tools,
            caps.supports_vision,
            caps.reasoning,
            caps.max_context_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );
    }
    println!(
        "  [{}] Ollama: {}",
        label(report.ollama.status),
        report.ollama.message
    );
    println!(
        "  [INFO] Remote providers: {}",
        if report.remote_providers.is_empty() {
            "none configured".to_string()
        } else {
            report.remote_providers.join(", ")
        }
    );
    for problem in &report.provider_problems {
        println!(
            "  [WARNING] Provider {} is configured but unusable: {}",
            problem.name, problem.reason
        );
    }
    println!(
        "  [{}] Project instructions: {}",
        label(report.project_instructions.status),
        report.project_instructions.message
    );
    println!(
        "  [INFO] Safety: mode={}, checkpoint_on_mutation={}",
        report.safety_mode, report.checkpoint_on_mutation
    );
    if let Some(profile) = &report.active_profile {
        println!("  [INFO] Config profile: {}", profile);
    }
    println!(
        "  [INFO] Prompt customization: {}",
        if report.prompt_customized {
            "active"
        } else {
            "default"
        }
    );
    println!(
        "  [{}] Runtime daemon: {}",
        label(report.runtime.daemon.status),
        report.runtime.daemon.message
    );
    println!(
        "  [{}] Runtime store: {}",
        label(report.runtime.local_store.status),
        report.runtime.local_store.message
    );
    println!("  [OK] Tool surface:");
    for tool in &report.tools {
        println!("       - {tool}");
    }
    println!("\nNext steps:");
    for step in &report.next_steps {
        println!("  - {step}");
    }
}

#[derive(Debug, serde::Serialize)]
struct SelfTestReport {
    ok: bool,
    workspace: String,
    checks: Vec<String>,
    compact_smoke: QaCompactSmokeReport,
    runtime_store: DoctorCheck,
    kept_workspace: bool,
}

fn run_self_test(config: &Config, format: OutputFormat, keep_workspace: bool) -> Result<()> {
    let workspace = std::env::temp_dir().join(format!("mermaid-self-test-{}", fresh_qa_id()));
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("failed to create {}", workspace.display()))?;

    let compact_smoke = match run_qa_compact_smoke(config, &workspace, 6) {
        Ok(report) => report,
        Err(err) => QaCompactSmokeReport::failed(&workspace, 6, err.to_string()),
    };
    let runtime_store = match RuntimeClient::local().health() {
        Ok(read) => DoctorCheck {
            status: "ok",
            message: format!("local runtime store ready at {}", read.value.database),
        },
        Err(err) => DoctorCheck {
            status: "warning",
            message: err.to_string(),
        },
    };

    // Real per-platform probes (Linux: the seccomp filter / Landlock ruleset
    // assemble; macOS: /usr/bin/sandbox-exec exists; elsewhere: no backend
    // yet, truthfully "no" instead of the old hardcoded "yes").
    let sandbox_available = crate::runtime::network_killswitch_available();
    let fs_sandbox_available = crate::runtime::fs_confinement_available();
    let (network_check, fs_check) = if cfg!(target_os = "linux") {
        (
            "network kill-switch (seccomp) builds on this platform",
            "filesystem confinement (Landlock) ruleset builds on this platform",
        )
    } else if cfg!(target_os = "macos") {
        (
            "network sandbox (Seatbelt via sandbox-exec) available on this platform",
            "filesystem confinement (Seatbelt via sandbox-exec) available on this platform",
        )
    } else {
        (
            "network sandbox backend available on this platform",
            "filesystem confinement backend available on this platform",
        )
    };
    let checks = vec![
        "compact smoke exercises reducer compaction path".to_string(),
        "compact smoke persists conversation and archive artifacts".to_string(),
        "local runtime store opens without daemon".to_string(),
        format!(
            "{network_check}: {}",
            if sandbox_available { "yes" } else { "no" }
        ),
        format!(
            "{fs_check}: {}",
            if fs_sandbox_available { "yes" } else { "no" }
        ),
    ];
    // Platforms with a sandbox backend must have it working; platforms
    // without one (Windows until the AppContainer port) truthfully report
    // "no" above without failing the whole self-test.
    let sandbox_expected = cfg!(any(target_os = "linux", target_os = "macos"));
    let ok = compact_smoke.ok
        && runtime_store.status == "ok"
        && (!sandbox_expected || (sandbox_available && fs_sandbox_available));
    let report = SelfTestReport {
        ok,
        workspace: workspace.display().to_string(),
        checks,
        compact_smoke,
        runtime_store,
        kept_workspace: keep_workspace,
    };

    print_self_test_report(&report, format)?;
    if !keep_workspace {
        let _ = std::fs::remove_dir_all(&workspace);
    }
    anyhow::ensure!(report.ok, "mermaid self-test failed");
    Ok(())
}

fn print_self_test_report(report: &SelfTestReport, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(report)?),
        OutputFormat::Ndjson => println!("{}", serde_json::to_string(report)?),
        OutputFormat::Markdown => {
            println!("# Mermaid Self-Test\n");
            print_self_test_text(report);
        },
        OutputFormat::Text => print_self_test_text(report),
    }
    Ok(())
}

fn print_self_test_text(report: &SelfTestReport) {
    println!(
        "Mermaid self-test: {}",
        if report.ok { "ok" } else { "failed" }
    );
    println!("workspace: {}", report.workspace);
    println!(
        "compact smoke: {}",
        if report.compact_smoke.ok {
            "ok"
        } else {
            "failed"
        }
    );
    println!("runtime store: {}", report.runtime_store.message);
    println!("checks:");
    for check in &report.checks {
        println!("  - {check}");
    }
    if !report.ok
        && let Some(failure) = &report.compact_smoke.failure
    {
        println!("failure: {failure}");
    }
}

fn label(status: &str) -> &'static str {
    match status {
        "ok" => "OK",
        "warning" => "WARNING",
        "error" => "ERROR",
        _ => "INFO",
    }
}

fn safety_mode_name(mode: crate::runtime::SafetyMode) -> &'static str {
    mode.as_str()
}

/// Every provider `mermaid login` can store a key for: the bespoke
/// providers, the OpenAI-compat registry, and user-defined `[providers.*]`
/// entries. Yields `(name, default_env, override_env)`.
fn login_providers(config: &Config) -> Vec<(String, String, Option<String>)> {
    let over = |name: &str| {
        config
            .providers
            .get(name)
            .and_then(|c| c.api_key_env.clone())
    };
    let mut rows: Vec<(String, String, Option<String>)> = vec![
        (
            "anthropic".to_string(),
            "ANTHROPIC_API_KEY".to_string(),
            over("anthropic"),
        ),
        (
            "gemini".to_string(),
            "GOOGLE_API_KEY".to_string(),
            over("gemini"),
        ),
        (
            "meta".to_string(),
            crate::providers::model::meta::DEFAULT_API_KEY_ENV.to_string(),
            over("meta"),
        ),
        (
            "ollama".to_string(),
            "OLLAMA_API_KEY".to_string(),
            over("ollama"),
        ),
    ];
    for profile in PROVIDER_REGISTRY {
        rows.push((
            profile.name.to_string(),
            profile.api_key_env.to_string(),
            over(profile.name),
        ));
    }
    for (name, cfg) in &config.providers {
        if rows.iter().any(|(n, _, _)| n == name) {
            continue;
        }
        // Custom providers: their api_key_env IS the default env.
        if let Some(env) = &cfg.api_key_env {
            rows.push((name.clone(), env.clone(), None));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// `mermaid login [provider]`: no arg lists key status; with a provider,
/// prompt (hidden input) and store the key in the OS keyring. Env vars keep
/// absolute precedence over stored keys.
fn login(provider: Option<&str>, config: &Config) -> Result<()> {
    let rows = login_providers(config);
    let Some(provider) = provider else {
        println!(
            "Provider API-key status (env beats keyring; `mermaid login <provider>` stores a key):\n"
        );
        for (name, default_env, override_env) in &rows {
            let source =
                crate::utils::provider_key_source(name, default_env, override_env.as_deref());
            let env_name = override_env.as_deref().unwrap_or(default_env);
            println!("  {:<14} {:<8} (${})", name, source, env_name);
        }
        return Ok(());
    };
    let provider = provider.to_lowercase();
    let Some((name, default_env, override_env)) = rows.into_iter().find(|(n, _, _)| n == &provider)
    else {
        let names: Vec<String> = login_providers(config)
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        anyhow::bail!(
            "unknown provider '{}'; known: {}",
            provider,
            names.join(", ")
        );
    };
    let key = rpassword::prompt_password(format!("API key for {} (input hidden): ", name))
        .context("read API key")?;
    let key = key.trim();
    anyhow::ensure!(!key.is_empty(), "no key entered; nothing stored");
    let store = crate::utils::default_store();
    store
        .set(&name, key)
        .with_context(|| format!("store key for {}", name))?;
    println!(
        "Stored key for {} in {} (service \"mermaid\").",
        name,
        store.label()
    );
    // The env var, when set, silently wins — say so now, not at 2am.
    if crate::utils::resolve_api_key(&default_env, override_env.as_deref()).is_some() {
        let env_name = override_env.as_deref().unwrap_or(&default_env);
        println!(
            "Note: ${} is currently set and takes precedence over the stored key.",
            env_name
        );
    }
    Ok(())
}

/// `mermaid logout <provider>`: delete the stored key (reports whether
/// anything was stored).
fn logout(provider: &str, config: &Config) -> Result<()> {
    let provider = provider.to_lowercase();
    // Unknown names are allowed here — a key may be stored for a provider
    // that was since removed from config; deleting it must stay possible.
    let _ = config;
    let store = crate::utils::default_store();
    if store
        .delete(&provider)
        .with_context(|| format!("delete key for {}", provider))?
    {
        println!(
            "Removed stored key for {} from {}.",
            provider,
            store.label()
        );
    } else {
        println!("No stored key for {}.", provider);
    }
    Ok(())
}

fn meta_api_key(config: &Config) -> Option<String> {
    crate::utils::resolve_provider_key(
        "meta",
        crate::providers::model::meta::DEFAULT_API_KEY_ENV,
        config
            .providers
            .get("meta")
            .and_then(|provider| provider.api_key_env.as_deref()),
    )
}

fn meta_base_url(config: &Config) -> String {
    config
        .providers
        .get("meta")
        .and_then(|provider| provider.base_url.clone())
        .unwrap_or_else(|| crate::providers::model::meta::DEFAULT_BASE_URL.to_string())
}

#[derive(Debug, serde::Serialize)]
struct QaCompactSmokeReport {
    ok: bool,
    turns: usize,
    archived_messages: usize,
    preserved_messages: usize,
    replacement_messages: usize,
    conversation_path: Option<String>,
    archive_path: Option<String>,
    checks: Vec<String>,
    failure: Option<String>,
}

impl QaCompactSmokeReport {
    fn failed(cwd: &Path, turns: usize, failure: String) -> Self {
        Self {
            ok: false,
            turns,
            archived_messages: 0,
            preserved_messages: 0,
            replacement_messages: 0,
            conversation_path: Some(
                cwd.join(".mermaid")
                    .join("conversations")
                    .display()
                    .to_string(),
            ),
            archive_path: None,
            checks: Vec::new(),
            failure: Some(failure),
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
fn run_qa_compact_smoke(
    config: &Config,
    cwd: &Path,
    requested_turns: usize,
) -> Result<QaCompactSmokeReport> {
    let turns = requested_turns.max(3);
    let mut state = State::new(
        config.clone(),
        cwd.to_path_buf(),
        qa_model_id(config),
        chrono::Local::now(),
    );
    for message in synthetic_compaction_messages(turns) {
        state.session.append(message, state.now);
    }

    let (state_after_slash, compact_cmds) = update(
        state,
        Msg::Slash(SlashCmd::Compact(Some("qa compact smoke".to_string()))),
    );
    let turn = state_after_slash
        .turn
        .id()
        .context("manual compaction did not enter a compaction turn")?;
    let request = compact_cmds
        .iter()
        .find_map(|cmd| match cmd {
            Cmd::CompactConversation { request, .. } => Some(request.clone()),
            _ => None,
        })
        .context("manual compaction did not emit a CompactConversation command")?;

    let before_snapshot = estimate_context_usage_for_request(&request.chat, Some(100_000));
    let prepared = prepare_compaction(&request, Some(100_000))
        .map_err(|reason| anyhow::anyhow!("prepare_compaction skipped: {reason}"))?;
    anyhow::ensure!(
        !prepared.archived_messages.is_empty(),
        "compaction archived no messages"
    );
    anyhow::ensure!(
        !prepared.preserved_messages.is_empty(),
        "compaction preserved no messages"
    );

    let summary = deterministic_compaction_summary(&prepared, turns);
    let mut record = CompactionRecord {
        id: format!("qa_compact_{}", fresh_qa_id()),
        trigger: CompactionTrigger::Manual,
        created_at: chrono::Local::now(),
        before_tokens: before_snapshot.used_tokens,
        after_tokens: 0,
        archived_message_count: prepared.archived_messages.len(),
        preserved_message_count: prepared.preserved_messages.len(),
        preserved_turn_count: prepared
            .preserved_messages
            .iter()
            .filter(|message| message.role == crate::models::MessageRole::User)
            .count(),
        summary_tokens: summary.len().div_ceil(4),
        duration_secs: 0.0,
        review_status: crate::domain::CompactionReviewStatus::DraftValidated,
        review_error: None,
        focus: Some("qa compact smoke".to_string()),
        archive_path: None,
    };
    let mut replacement = build_replacement_messages(&summary, &prepared, &record);
    let mut after_chat: ChatRequest = request.chat.clone();
    after_chat.messages = replacement.clone();
    let mut after_snapshot = estimate_context_usage_for_request(&after_chat, Some(100_000));
    record.after_tokens = after_snapshot.used_tokens;
    replacement = build_replacement_messages(&summary, &prepared, &record);
    after_chat.messages = replacement.clone();
    after_snapshot = estimate_context_usage_for_request(&after_chat, Some(100_000));

    let result = CompactionResult {
        record,
        replacement_messages: replacement,
        archived_messages: prepared.archived_messages,
        before_snapshot,
        after_snapshot,
        usage: None,
        source_boundaries: Vec::new(),
    };
    let (final_state, save_cmds) =
        update(state_after_slash, Msg::CompactionFinished { turn, result });

    let manager = ConversationManager::new(cwd)?;
    let mut conversation_path = None;
    let mut archive_path = None;
    for cmd in save_cmds {
        match cmd {
            Cmd::SaveConversation(conversation) => {
                manager.save_conversation(&conversation)?;
                conversation_path = Some(
                    manager
                        .conversations_dir()
                        .join(format!("{}.json", conversation.id))
                        .display()
                        .to_string(),
                );
            },
            Cmd::SaveCompactionArchive {
                archive,
                conversation,
                ..
            } => {
                // Archive first, then the stripped conversation (same order
                // as the live effect path), with `?` so a failed archive
                // aborts before the conversation is overwritten.
                archive_path = Some(
                    manager
                        .save_compaction_archive(&archive)?
                        .display()
                        .to_string(),
                );
                manager.save_conversation(&conversation)?;
                conversation_path = Some(
                    manager
                        .conversations_dir()
                        .join(format!("{}.json", conversation.id))
                        .display()
                        .to_string(),
                );
            },
            _ => {},
        }
    }

    let conversation_path = conversation_path.context("compaction did not save conversation")?;
    let archive_path = archive_path.context("compaction did not save archive")?;
    let messages = final_state.session.messages();
    let compactions = &final_state.session.conversation.compactions;

    let mut checks = Vec::new();
    anyhow::ensure!(
        !compactions.is_empty(),
        "conversation did not record compaction metadata"
    );
    checks.push("conversation records compaction metadata".to_string());
    anyhow::ensure!(
        messages
            .first()
            .is_some_and(|msg| msg.kind == crate::models::ChatMessageKind::ContextCheckpoint),
        "replacement does not start with a context checkpoint"
    );
    checks.push("replacement starts with context checkpoint".to_string());
    anyhow::ensure!(
        std::path::Path::new(&conversation_path).exists(),
        "conversation file missing after save"
    );
    checks.push("conversation file saved".to_string());
    anyhow::ensure!(
        std::path::Path::new(&archive_path).exists(),
        "compaction archive file missing after save"
    );
    checks.push("archive file saved".to_string());
    anyhow::ensure!(
        compactions[0].archived_message_count > 0 && compactions[0].preserved_message_count > 0,
        "compaction did not archive and preserve messages"
    );
    checks.push("archived and preserved message counts are non-zero".to_string());

    Ok(QaCompactSmokeReport {
        ok: true,
        turns,
        archived_messages: compactions[0].archived_message_count,
        preserved_messages: compactions[0].preserved_message_count,
        replacement_messages: messages.len(),
        conversation_path: Some(conversation_path),
        archive_path: Some(archive_path),
        checks,
        failure: None,
    })
}

fn print_qa_compact_report(report: &QaCompactSmokeReport, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(report)?);
        },
        OutputFormat::Ndjson => {
            println!("{}", serde_json::to_string(report)?);
        },
        OutputFormat::Text => {
            println!(
                "qa compact smoke: {}",
                if report.ok { "ok" } else { "failed" }
            );
            println!("turns: {}", report.turns);
            println!("archived messages: {}", report.archived_messages);
            println!("preserved messages: {}", report.preserved_messages);
            println!("replacement messages: {}", report.replacement_messages);
            if let Some(path) = &report.conversation_path {
                println!("conversation: {path}");
            }
            if let Some(path) = &report.archive_path {
                println!("archive: {path}");
            }
            if let Some(failure) = &report.failure {
                println!("failure: {failure}");
            }
        },
        OutputFormat::Markdown => {
            println!(
                "# QA Compact Smoke\n\n- Status: {}\n- Turns: {}\n- Archived messages: {}\n- Preserved messages: {}\n- Replacement messages: {}",
                if report.ok { "ok" } else { "failed" },
                report.turns,
                report.archived_messages,
                report.preserved_messages,
                report.replacement_messages
            );
            if let Some(path) = &report.conversation_path {
                println!("- Conversation: `{path}`");
            }
            if let Some(path) = &report.archive_path {
                println!("- Archive: `{path}`");
            }
            if let Some(failure) = &report.failure {
                println!("\nFailure: `{failure}`");
            }
        },
    }
    Ok(())
}

fn qa_model_id(config: &Config) -> String {
    if let Some(model) = config
        .last_used_model
        .as_ref()
        .filter(|value| !value.is_empty())
    {
        return model.clone();
    }
    if !config.default_model.name.is_empty() {
        if config.default_model.provider.is_empty() {
            return config.default_model.name.clone();
        }
        return format!(
            "{}/{}",
            config.default_model.provider, config.default_model.name
        );
    }
    "qa/deterministic".to_string()
}

fn synthetic_compaction_messages(turns: usize) -> Vec<ChatMessage> {
    let mut messages = Vec::with_capacity(turns.saturating_mul(2));
    for idx in 1..=turns {
        messages.push(ChatMessage::user(format!(
            "User turn {idx}: investigate Mermaid compaction behavior in src/domain/compaction.rs and keep exact file paths in the summary."
        )));
        messages.push(ChatMessage::assistant(format!(
            "Assistant turn {idx}: inspected src/domain/compaction.rs, tests/reducer_flows.rs, and scripts/qa_mermaid.py; noted command `cargo test --all-targets` result placeholder {idx}."
        )));
    }
    messages
}

fn deterministic_compaction_summary(
    prepared: &crate::domain::PreparedCompaction,
    turns: usize,
) -> String {
    format!(
        "## Goal\n- Verify Mermaid can compact a multi-turn conversation through the reducer path.\n\n## User Preferences And Constraints\n- Headless QA must not require a human to open the TUI.\n\n## Project State\n- Synthetic QA conversation seeded with {turns} user/assistant turns.\n\n## Completed Work\n- Prepared compaction archived {} messages and preserved {} messages.\n\n## Current Work\n- Running deterministic compact smoke from the hidden QA command.\n\n## Key Decisions\n- Use deterministic summary text so fast QA does not call a real model.\n\n## Critical Files And Symbols\n- src/domain/compaction.rs: compaction preparation and replacement shape.\n- src/domain/reducer.rs: manual compaction completion handling.\n- scripts/qa_mermaid.py: headless QA harness.\n\n## Commands Tests And Results\n- mermaid qa compact-smoke --format json: running inside this smoke.\n\n## Open Questions Or Risks\n- Full TUI automation remains intentionally deferred.\n\n## Next Steps\n- Keep using the real-model QA tier for end-to-end dogfood checks.",
        prepared.archived_messages.len(),
        prepared.preserved_messages.len()
    )
}

fn fresh_qa_id() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn show_tasks(limit: usize) -> Result<()> {
    let read = RuntimeClient::auto().list_tasks(limit)?;
    let mut tasks = read.value;
    tasks.truncate(limit);
    println!("Mermaid runtime tasks");
    println!("Source: {}", read.source.as_str());
    println!();
    if tasks.is_empty() {
        println!("No tasks recorded yet.");
        return Ok(());
    }
    for task in tasks {
        println!(
            "{}  [{}] {}  {}  {}",
            task.id, task.status, task.priority, task.updated_at, task.title
        );
        println!("    project: {}", task.project_path);
        println!("    model: {}", task.model_id);
    }
    Ok(())
}

fn show_task(id: &str) -> Result<()> {
    let detail = RuntimeClient::auto().task_detail(id)?.value;
    print_task_detail(&detail.task);
    let events = detail.events;
    if !events.is_empty() {
        println!();
        println!("Timeline:");
        for event in events {
            println!("  {}  {}  {}", event.created_at, event.kind, event.message);
        }
    }
    Ok(())
}

fn print_task_detail(task: &TaskRecord) {
    println!("Task: {}", task.id);
    println!("Title: {}", task.title);
    println!("Status: {}", task.status);
    println!("Priority: {}", task.priority);
    println!("Project: {}", task.project_path);
    println!("Model: {}", task.model_id);
    if let Some(conversation_id) = &task.conversation_id {
        println!("Conversation: {}", conversation_id);
    }
    println!("Created: {}", task.created_at);
    println!("Updated: {}", task.updated_at);
    if let Some(report) = &task.final_report {
        println!();
        println!("Final report:");
        println!("{}", sanitize_terminal_text(report));
    }
}

fn show_processes(limit: usize) -> Result<()> {
    let read = RuntimeClient::auto().list_processes(limit)?;
    let mut processes = read.value;
    processes.truncate(limit);
    println!("Mermaid runtime processes");
    println!("Source: {}", read.source.as_str());
    println!();
    if processes.is_empty() {
        println!("No processes recorded yet.");
        return Ok(());
    }
    for process in processes {
        println!(
            "{}  pid={}  status={}  {}",
            process.id,
            process.pid,
            process.status.as_str(),
            process.command
        );
        if let Some(task_id) = process.task_id {
            println!("    task: {}", task_id);
        }
        if let Some(cwd) = process.cwd {
            println!("    cwd: {}", cwd);
        }
        if let Some(log_path) = process.log_path {
            println!("    log: {}", log_path);
        }
        if let Some(url) = process.detected_url {
            println!("    url: {}", url);
        }
    }
    Ok(())
}

async fn show_models(config: &Config) -> Result<()> {
    list_models(config).await?;
    probe_configured_provider_models(config).await?;
    let store = RuntimeStore::open_default()?;
    let probes = store.provider_probes().list(None, None)?;
    if !probes.is_empty() {
        println!("\nCached capability probes:");
        for probe in probes {
            println!(
                "  - {}/{} {}={} ({})",
                probe.provider,
                probe.model_id,
                probe.capability_key,
                probe.capability_value,
                probe.confidence
            );
        }
    }
    Ok(())
}

async fn show_model_info(model: &str, config: &Config) -> Result<()> {
    let snapshot = crate::domain::ProviderCapabilitySnapshot::from_model_id(model);
    let store = RuntimeStore::open_default()?;
    let provider = snapshot.provider.clone();

    // The static snapshot has no limits for providers that discover them live.
    // Resolve through the same provider path a real turn uses — cache-first
    // via `provider_probes`, one live fetch on a miss (Ollama `/api/show`,
    // Anthropic/Gemini models endpoints, OpenAI-compat `/models` metadata) —
    // so this reports real numbers, not "unknown". Falls back to the static
    // snapshot when the provider can't be built (e.g. no API key configured).
    let mut context_tokens = snapshot.max_context_tokens;
    let mut context_confidence = "static";
    let mut output_tokens = snapshot.max_output_tokens;
    let mut output_confidence = "static";
    let factory = crate::providers::ProviderFactory::new(config.clone());
    if let Ok(live) = factory.resolve(model).await {
        let probe_request = ChatRequest {
            model_id: model.to_string(),
            messages: vec![],
            system_prompt: String::new(),
            instructions: None,
            reasoning: crate::models::ReasoningLevel::None,
            temperature: 0.0,
            max_tokens: 0,
            tools: vec![],
            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
            output_schema: None,
            suppress_auto_compact: false,
            suppressed_builtin_tools: Vec::new(),
        };
        let sizing = live.resolve_context_window(&probe_request).await;
        if let Some(window) = sizing.model_max.or(sizing.effective) {
            context_tokens = Some(window);
            context_confidence = "probed";
        }
        if let Some(output) = sizing.max_output {
            output_tokens = Some(output);
            output_confidence = "probed";
        }
    }

    for (key, value) in [
        ("supports_tools", snapshot.supports_tools.to_string()),
        ("supports_vision", snapshot.supports_vision.to_string()),
        ("reasoning", snapshot.reasoning.clone()),
    ] {
        let _ = store.provider_probes().upsert(NewProviderProbe {
            provider: provider.clone(),
            model_id: snapshot.model.clone(),
            capability_key: key.to_string(),
            capability_value: value,
            confidence: "static".to_string(),
            error: None,
        });
    }
    // Context window separately — probed (Ollama) or static.
    let _ = store.provider_probes().upsert(NewProviderProbe {
        provider: provider.clone(),
        model_id: snapshot.model.clone(),
        capability_key: "max_context_tokens".to_string(),
        capability_value: context_tokens
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        confidence: context_confidence.to_string(),
        error: None,
    });
    println!("Model: {}", model);
    println!("Provider: {}", snapshot.provider);
    println!("Name: {}", snapshot.model);
    println!("Supports tools: {}", snapshot.supports_tools);
    println!("Supports vision: {}", snapshot.supports_vision);
    println!("Reasoning: {}", snapshot.reasoning);
    println!(
        "Context: {}",
        context_tokens
            .map(|n| format!("{n} ({context_confidence})"))
            .unwrap_or_else(|| "unknown".to_string())
    );
    println!(
        "Output limit: {}",
        output_tokens
            .map(|n| format!("{n} ({output_confidence})"))
            .unwrap_or_else(|| {
                "unknown (discovered live from the provider's models endpoint when exposed)"
                    .to_string()
            })
    );
    if let Some(profile) = lookup_provider(&snapshot.provider) {
        crate::runtime::record_static_provider_probes(&store, profile, &provider, &snapshot.model);
        println!("Token budget field: {:?}", profile.max_tokens_param);
        println!(
            "Single-tool-call models: {}",
            if profile.disable_parallel_tool_calls_for.is_empty() {
                "(none)".to_string()
            } else {
                profile.disable_parallel_tool_calls_for.join(", ")
            }
        );
    }
    Ok(())
}

async fn probe_configured_provider_models(config: &Config) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    for profile in PROVIDER_REGISTRY {
        let user_cfg = config.providers.get(profile.name);
        let Some(api_key) = crate::utils::resolve_provider_key(
            profile.name,
            profile.api_key_env,
            user_cfg.and_then(|c| c.api_key_env.as_deref()),
        ) else {
            continue;
        };
        let Some(base_url) = crate::providers::factory::discovery_base_url(
            profile,
            user_cfg.and_then(|c| c.base_url.clone()),
        ) else {
            // cloudflare with a token but no CLOUDFLARE_ACCOUNT_ID: there is no
            // real endpoint to probe — record the misconfiguration instead of a
            // guaranteed 404 against the registry placeholder.
            record_provider_probe(
                profile.name,
                "*",
                "models_availability",
                "failed",
                "failed",
                Some("CLOUDFLARE_ACCOUNT_ID not set".to_string()),
            );
            continue;
        };
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let mut request = client.get(&url).bearer_auth(api_key);
        for (name, value) in profile.extra_headers {
            request = request.header(*name, *value);
        }
        if let Some(user_cfg) = user_cfg {
            for (name, value) in &user_cfg.extra_headers {
                request = request.header(name, value);
            }
        }

        let result = request.send().await;
        match result {
            Ok(response) if response.status().is_success() => {
                let status = response.status();
                let body: serde_json::Value = response.json().await.unwrap_or_default();
                let ids = body
                    .get("data")
                    .and_then(|v| v.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                record_provider_probe(
                    profile.name,
                    "*",
                    "models_availability",
                    &format!("available:{}:{}", status.as_u16(), ids.len()),
                    "probed",
                    None,
                );
                for model_id in ids.into_iter().take(200) {
                    record_provider_probe(
                        profile.name,
                        &model_id,
                        "model_listed",
                        "true",
                        "listed",
                        None,
                    );
                }
            },
            Ok(response) => {
                record_provider_probe(
                    profile.name,
                    "*",
                    "models_availability",
                    "failed",
                    "failed",
                    Some(format!("HTTP {}", response.status().as_u16())),
                );
            },
            Err(error) => {
                record_provider_probe(
                    profile.name,
                    "*",
                    "models_availability",
                    "failed",
                    "failed",
                    Some(error.to_string()),
                );
            },
        }
    }
    probe_meta_models(&client, config).await;
    Ok(())
}

async fn probe_meta_models(client: &reqwest::Client, config: &Config) {
    let Some(api_key) = meta_api_key(config) else {
        return;
    };
    let url = format!("{}/models", meta_base_url(config).trim_end_matches('/'));
    let mut request = client.get(&url).bearer_auth(api_key);
    if let Some(provider) = config.providers.get("meta") {
        for (name, value) in &provider.extra_headers {
            request = request.header(name, value);
        }
        for (name, env_var) in &provider.env_headers {
            if let Ok(value) = std::env::var(env_var) {
                request = request.header(name, value);
            }
        }
    }
    match request.send().await {
        Ok(response) if response.status().is_success() => {
            let status = response.status();
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            let ids = body
                .get("data")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|item| item.get("id").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect::<Vec<_>>();
            record_provider_probe(
                "meta",
                "*",
                "models_availability",
                &format!("available:{}:{}", status.as_u16(), ids.len()),
                "probed",
                None,
            );
            for model_id in ids.into_iter().take(200) {
                record_provider_probe("meta", &model_id, "model_listed", "true", "listed", None);
            }
        },
        Ok(response) => record_provider_probe(
            "meta",
            "*",
            "models_availability",
            "failed",
            "failed",
            Some(format!("HTTP {}", response.status().as_u16())),
        ),
        Err(error) => record_provider_probe(
            "meta",
            "*",
            "models_availability",
            "failed",
            "failed",
            Some(error.to_string()),
        ),
    }
}

fn record_provider_probe(
    provider: &str,
    model_id: &str,
    key: &str,
    value: &str,
    confidence: &str,
    error: Option<String>,
) {
    if let Ok(store) = RuntimeStore::open_default() {
        let _ = store.provider_probes().upsert(NewProviderProbe {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            capability_key: key.to_string(),
            capability_value: value.to_string(),
            confidence: confidence.to_string(),
            error,
        });
    }
}

fn show_approvals() -> Result<()> {
    let approvals = RuntimeClient::auto().list_approvals()?.value;
    if approvals.is_empty() {
        println!("No pending approvals.");
        return Ok(());
    }
    for approval in approvals {
        println!(
            "{} [{} -> {}] {}",
            approval.id,
            approval.risk_classification,
            approval.policy_decision,
            approval.proposed_action
        );
        if let Some(args) = approval.args_summary {
            println!("    args: {}", args);
        }
        if let Some(checkpoint_id) = approval.checkpoint_id {
            println!("    checkpoint: {}", checkpoint_id);
        }
        if approval.pending_action_json.is_some() {
            println!("    pending action: recorded");
        }
    }
    Ok(())
}

fn approve(id: &str) -> Result<()> {
    let result = RuntimeClient::auto().approve(id)?;
    println!("Approved {}", id);
    if result.replayed {
        println!("{}", result.summary);
    }
    Ok(())
}

fn deny(id: &str) -> Result<()> {
    let _ = RuntimeClient::auto().deny(id)?;
    println!("Denied {}", id);
    Ok(())
}

/// `mermaid task <id> --follow`: attach to the daemon's live `RunEvent`
/// stream for a task and print it as NDJSON until the terminal `result`.
/// Daemon-only — there is no local fallback (the events only exist while the
/// daemon executes the run).
fn follow_task(id: &str) -> Result<()> {
    let lines = crate::runtime::subscribe_daemon_lines(
        crate::runtime::DaemonRequest::SubscribeTask {
            task_id: id.to_string(),
        }
        .to_wire(),
    )
    .context("mermaid task --follow needs a running daemon (`mermaid daemon start`)")?;
    let mut saw_any = false;
    for line in lines {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // The ack line carries ok:false on unknown task / auth failure.
        if !saw_any {
            saw_any = true;
            let ack: serde_json::Value =
                serde_json::from_str(line.trim()).context("daemon returned invalid JSON")?;
            if ack.get("ok").and_then(|v| v.as_bool()) == Some(false) {
                anyhow::bail!(
                    "{}",
                    ack.get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("subscribe failed")
                );
            }
            println!("{}", line.trim());
            continue;
        }
        println!("{}", line.trim());
        if serde_json::from_str::<serde_json::Value>(line.trim())
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
            .as_deref()
            == Some("result")
        {
            return Ok(());
        }
    }
    if saw_any {
        anyhow::bail!("stream ended without a result (daemon restarted mid-run?)");
    }
    anyhow::bail!("daemon closed the connection without responding");
}

/// Cancel a daemon task. Cancelling a *running* task must reach the daemon —
/// it holds the in-flight cancellation tokens. A *queued* task can be
/// cancelled straight in the local store when no daemon is reachable, since
/// queued tasks only ever execute via the daemon's claim query.
fn cancel_task(id: &str) -> Result<()> {
    match crate::runtime::request_daemon_json(
        crate::runtime::DaemonRequest::CancelTask { id: id.to_string() }.to_wire(),
    ) {
        Ok(response) => {
            if response.get("cancelling").and_then(|v| v.as_bool()) == Some(true) {
                println!("Cancelling {} (running; the agent unwinds gracefully)", id);
            } else {
                println!("Cancelled {}", id);
            }
            Ok(())
        },
        Err(daemon_err) => {
            let store = crate::runtime::RuntimeStore::open_default()?;
            match store.tasks().get(id)? {
                Some(task) if task.status == crate::runtime::TaskStatus::Queued => {
                    store.tasks().update_status(
                        id,
                        crate::runtime::TaskStatus::Cancelled,
                        Some("cancelled before start"),
                    )?;
                    println!("Cancelled {} (was queued; daemon unreachable)", id);
                    Ok(())
                },
                Some(task) => anyhow::bail!(
                    "task {} is {} and the daemon request failed: {}",
                    id,
                    task.status,
                    daemon_err
                ),
                None => anyhow::bail!("task not found: {}", id),
            }
        },
    }
}

fn show_tool_runs(limit: usize) -> Result<()> {
    let mut runs = RuntimeClient::auto().list_tool_runs(limit)?.value;
    runs.truncate(limit);
    if runs.is_empty() {
        println!("No tool runs recorded yet.");
        return Ok(());
    }
    for run in runs {
        println!(
            "{} [{}] {} started {}",
            run.id, run.status, run.tool_name, run.started_at
        );
        if let Some(turn_id) = run.turn_id {
            println!("    turn: {}", turn_id);
        }
        if let Some(call_id) = run.call_id {
            println!("    call: {}", call_id);
        }
        if let Some(finished_at) = run.finished_at {
            println!("    finished: {}", finished_at);
        }
    }
    Ok(())
}

fn show_checkpoints(limit: usize) -> Result<()> {
    let mut checkpoints = RuntimeClient::auto().list_checkpoints(limit)?.value;
    checkpoints.truncate(limit);
    if checkpoints.is_empty() {
        println!("No checkpoints recorded yet.");
        return Ok(());
    }
    for checkpoint in checkpoints {
        println!(
            "{}  {}  {}",
            checkpoint.id, checkpoint.created_at, checkpoint.project_path
        );
        println!("    snapshot: {}", checkpoint.snapshot_path);
        println!("    files: {}", checkpoint.changed_files_json);
        if let Some(approval_id) = checkpoint.approval_id {
            println!("    approval: {}", approval_id);
        }
    }
    Ok(())
}

fn restore_checkpoint(id: &str, force: bool) -> Result<()> {
    // Restoring overwrites the working tree from the checkpoint. Confirm first
    // (default NO); `--force` is the scripted-use bypass, and a non-interactive
    // session without it refuses rather than clobbering the tree unprompted (#113).
    if !crate::utils::confirm_or_refuse(
        &format!("Restore checkpoint {id}? This overwrites the current working tree."),
        force,
    )? {
        println!("Restore cancelled.");
        return Ok(());
    }
    let manifest = RuntimeClient::auto().restore_checkpoint(id)?.checkpoint;
    println!("Restored {} ({} files)", manifest.id, manifest.files.len());
    if let Some(repo) = manifest.shadow_git_repo {
        println!("Shadow repo: {}", repo);
    }
    if let Some(commit) = manifest.shadow_git_commit {
        println!("Shadow commit: {}", commit);
    }
    if let Some(action) = manifest.pending_action {
        println!("Pending action: {}", serde_json::to_string_pretty(&action)?);
    }
    Ok(())
}

fn handle_plugin(command: &PluginCommand) -> Result<()> {
    match command {
        PluginCommand::Install { path } => {
            let preview = crate::runtime::plugin_capability_preview(path)?;
            print_plugin_capability_preview(&preview);
            let record = crate::runtime::install_plugin_from_path(path)?;
            println!(
                "Installed plugin {} ({}) — DISABLED.",
                record.name, record.id
            );
            println!(
                "Run `mermaid plugin enable {}` to activate it (this runs the plugin's hook code).",
                record.id
            );
        },
        PluginCommand::List => {
            let plugins = RuntimeClient::auto().list_plugins()?.value;
            if plugins.is_empty() {
                println!("No plugins installed.");
            } else {
                for plugin in plugins {
                    println!(
                        "{} [{}] {} ({})",
                        plugin.id,
                        if plugin.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        plugin.name,
                        plugin.source
                    );
                }
            }
        },
        PluginCommand::Enable { id } => {
            // Surface what the plugin declares before activating its native code.
            let client = RuntimeClient::auto();
            if let Some(plugin) = client
                .list_plugins()?
                .value
                .into_iter()
                .find(|p| p.id == *id || p.name == *id)
                && let Ok(preview) =
                    crate::runtime::plugin_capability_preview(Path::new(&plugin.source))
            {
                print_plugin_capability_preview(&preview);
            }
            client.set_plugin_enabled(id, true)?;
            println!("Enabled plugin {} — its hooks will now run.", id);
        },
        PluginCommand::Disable { id } => {
            RuntimeClient::auto().set_plugin_enabled(id, false)?;
            println!("Disabled plugin {}", id);
        },
        PluginCommand::Audit { path } => {
            let manifest_path = if path.is_dir() {
                path.join("plugin.toml")
            } else {
                path.clone()
            };
            let raw = std::fs::read_to_string(&manifest_path)?;
            let manifest: crate::runtime::PluginManifest = toml::from_str(&raw)?;
            let root = manifest_path.parent().unwrap_or_else(|| Path::new("."));
            crate::runtime::validate_plugin_manifest(&manifest, root)?;
            let preview = crate::runtime::plugin_capability_preview(path)?;
            println!("Plugin manifest is valid: {}", manifest.name);
            print_plugin_capability_preview(&preview);
        },
    }
    Ok(())
}

fn print_plugin_capability_preview(preview: &crate::runtime::PluginCapabilityPreview) {
    println!(
        "Capabilities declared by plugin {} (advisory, not sandbox-enforced):",
        preview.name
    );
    if preview.declared_capabilities.is_empty() && preview.capabilities_toml.is_none() {
        println!("  capabilities: (none declared)");
    } else {
        if !preview.declared_capabilities.is_empty() {
            println!("  declared: {}", preview.declared_capabilities.join(", "));
        }
        if let Some(value) = &preview.capabilities_toml {
            println!(
                "  capabilities.toml: {}",
                serde_json::to_string(value).unwrap_or_else(|_| "<unprintable>".to_string())
            );
        }
    }
    if !preview.hooks.is_empty() {
        println!("  hooks: {}", preview.hooks.join(", "));
    }
    if !preview.mcp.is_empty() {
        println!("  mcp: {}", preview.mcp.join(", "));
    }
    if !preview.bin.is_empty() {
        println!("  bin: {}", preview.bin.join(", "));
    }
}

fn handle_pair(command: &PairCommand) -> Result<()> {
    let store = RuntimeStore::open_default()?;
    match command {
        PairCommand::Create { label, ttl_days } => {
            let ttl = ttl_days.unwrap_or(crate::runtime::DEFAULT_PAIRING_TTL_DAYS);
            let expires_at = crate::runtime::pairing_expiry_from_now(ttl);
            let (token, hash) = crate::runtime::generate_pairing_token()?;
            let record =
                store
                    .pairing_tokens()
                    .create(&hash, label.as_deref(), expires_at.as_deref())?;
            println!("Pairing token id: {}", record.id);
            println!("Pairing token: {}", token);
            println!(
                "Expires: {}",
                record.expires_at.as_deref().unwrap_or("never")
            );
            println!(
                "Use with daemon JSON by setting {}.",
                crate::runtime::daemon::DAEMON_TOKEN_ENV
            );
            println!("Store this now; Mermaid will not print it again.");
        },
        PairCommand::List => {
            let tokens = store.pairing_tokens().list()?;
            if tokens.is_empty() {
                println!("No pairing tokens.");
            } else {
                // Never print token_hash — only the non-secret metadata.
                for t in tokens {
                    println!(
                        "{} [{}] label={} created={} expires={} last_used={}",
                        t.id,
                        if t.enabled { "active" } else { "revoked" },
                        t.label.as_deref().unwrap_or("-"),
                        t.created_at,
                        t.expires_at.as_deref().unwrap_or("never"),
                        t.last_used_at.as_deref().unwrap_or("never"),
                    );
                }
            }
        },
        PairCommand::Revoke { id } => {
            if store.pairing_tokens().revoke(id)? {
                println!("Revoked pairing token {id}");
            } else {
                println!("No active pairing token with id {id}");
            }
        },
    }
    Ok(())
}

/// Strip terminal control sequences from untrusted subprocess output before
/// printing it to a cooked terminal. Managed-process logs / reports / port
/// listings are attacker-influenceable (a dev server can emit anything), so a
/// raw `print!` would let escape sequences execute — OSC-52 clipboard writes,
/// window-title/prompt rewrites, cursor moves used for spoofing. Keeps `\n` and
/// `\t`; drops every ESC-introduced sequence (CSI / OSC / DCS / PM / APC / SOS
/// and simple two-/three-byte forms) and all other C0/C1 control characters
/// (incl. `\r` and DEL). See F49.
fn sanitize_terminal_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' => out.push(c),
            '\u{1b}' => match chars.next() {
                // CSI: ESC '[' params/intermediates then a final byte
                // (0x40-0x7e), which is also dropped.
                Some('[') => {
                    for p in chars.by_ref() {
                        if ('@'..='~').contains(&p) {
                            break;
                        }
                    }
                },
                // String sequences (OSC ']', DCS 'P', PM '^', APC '_', SOS 'X'):
                // arbitrary body terminated by BEL or ST (ESC '\').
                Some(']') | Some('P') | Some('^') | Some('_') | Some('X') => {
                    while let Some(p) = chars.next() {
                        if p == '\u{07}' {
                            break;
                        }
                        if p == '\u{1b}' {
                            // ESC here starts ST (ESC '\'); drop the trailing '\'.
                            let mut peek = chars.clone();
                            if peek.next() == Some('\\') {
                                chars = peek;
                            }
                            break;
                        }
                    }
                },
                // Other ESC forms: optional intermediates (0x20-0x2f) then a
                // final byte; drop them all.
                Some(mut b) => {
                    while ('\u{20}'..='\u{2f}').contains(&b) {
                        match chars.next() {
                            Some(next) => b = next,
                            None => break,
                        }
                    }
                },
                None => {},
            },
            // Drop DEL, all other C0 controls (incl. `\r`), and C1 controls.
            c if (c as u32) < 0x20 || matches!(c as u32, 0x7f..=0x9f) => {},
            c => out.push(c),
        }
    }
    out
}

fn show_logs(id: &str) -> Result<()> {
    let content = RuntimeClient::auto().process_log(id, None)?.content;
    print!("{}", sanitize_terminal_text(&content));
    Ok(())
}

fn stop_process(id: &str) -> Result<()> {
    let process = RuntimeClient::auto().stop_process(id)?.item;
    println!("Stopped process {} (pid {})", id, process.pid);
    Ok(())
}

fn restart_process(id: &str) -> Result<()> {
    let process = RuntimeClient::auto().restart_process(id)?.item;
    println!("Restarted process {} (pid {})", id, process.pid);
    Ok(())
}

fn open_target(target: &str) -> Result<()> {
    if RuntimeClient::auto().open_process(target).is_err() {
        crate::utils::open_file(target);
    }
    Ok(())
}

fn show_ports() -> Result<()> {
    let ports = RuntimeClient::auto().ports()?.ports;
    print!("{}", sanitize_terminal_text(&ports));
    Ok(())
}

/// List available models across all backends (honors user config).
/// Read-only: a dead local server is reported, never resurrected — a
/// cloud-model user who stopped Ollama on purpose must be able to
/// enumerate without a surprise VRAM grab.
pub async fn list_models(config: &Config) -> Result<()> {
    match list_ollama_models(config).await {
        None if is_ollama_installed() => {
            println!("Ollama is installed but not running — local models can't be listed.");
            println!("(It starts automatically when you use an Ollama model.)");
        },
        None => println!("Ollama is not installed; no local models."),
        Some(models) if models.is_empty() => println!("No Ollama models installed locally."),
        Some(models) => {
            println!("Ollama models (local/cloud):");
            for name in &models {
                println!("  - ollama/{}", name);
            }
        },
    }

    println!("\nConfigured remote providers:");
    let catalogs = crate::providers::discovery::provider_catalogs(config).await;
    if catalogs.is_empty() {
        println!("  (none — set a provider API key env var to enable)");
    }
    for catalog in &catalogs {
        println!(
            "  - {} ({}) {}",
            catalog.provider.name,
            catalog.provider.source_label(),
            catalog.provider.endpoint
        );
        match &catalog.models {
            // The key resolves but the catalog didn't answer. Say so instead of
            // printing an empty list that reads as "this provider has nothing".
            None => {
                println!("      (model list unavailable — the provider's /models did not answer)")
            },
            Some(models) if models.is_empty() => println!("      (provider lists no models)"),
            Some(models) => {
                for id in models {
                    println!("      {}/{}", catalog.provider.name, id);
                }
            },
        }
    }

    // A provider the user started configuring that still cannot be built. It
    // belongs on the "what can I use" surface precisely because the answer is
    // "not this, and here is the one thing missing".
    let problems = crate::providers::provider_problems(config);
    if !problems.is_empty() {
        println!("\nConfigured but not usable:");
        for problem in &problems {
            println!("  - {}: {}", problem.name, problem.reason);
        }
    }

    println!("\nSwitch models in-session with /model <name>.");
    Ok(())
}

/// Ask the local Ollama daemon for its list of models — strictly read-only.
/// `None` when the server couldn't be reached (distinct from `Some(vec![])`:
/// running with nothing pulled) so callers can report a dead server
/// truthfully. Every caller is an enumeration/diagnostic verb (`list` /
/// `models` / `status` / `doctor`), and observing state must never mutate
/// it — a cloud-only user who deliberately stopped Ollama to free VRAM must
/// not get it resurrected by a listing — so autostart is hard-off here. The
/// intent paths (chat's `send_chat`, the startup preflight in
/// `ollama::installer`) keep autostart; that's where a dead server heals.
async fn list_ollama_models(config: &Config) -> Option<Vec<String>> {
    use crate::models::adapters::ollama::OllamaAdapter;
    let backend = BackendConfig {
        ollama_url: format!("{}:{}", config.ollama.host, config.ollama.port),
        timeout_secs: 5,
        max_idle_per_host: 2,
        ollama_autostart: false,
    };
    match OllamaAdapter::new("__list__", Arc::new(backend)).await {
        Ok(adapter) => adapter.list_models().await.ok(),
        Err(_) => None,
    }
}

/// Show version information
pub fn show_version() {
    println!("Mermaid v{}", env!("CARGO_PKG_VERSION"));
    println!("   An open-source, model-agnostic AI pair programmer");
}

const RELEASE_LATEST_API: &str =
    "https://api.github.com/repos/noahsabaj/mermaid-cli/releases/latest";
const INSTALL_SH_URL: &str = "https://noahsabaj.github.io/mermaid-cli/install.sh";
const INSTALL_PS1_URL: &str = "https://noahsabaj.github.io/mermaid-cli/install.ps1";

/// `mermaid update` — check GitHub Releases for a newer version and, unless
/// `--check`, re-run the platform install script to replace this binary in
/// place. The install script is the single source of truth for the
/// download + checksum + replace (incl. the running-exe rename on Windows), so
/// there's no archive-handling logic (or extra dependency) here.
async fn run_update(check: bool, force: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("Installed: v{current}");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let resp = client
        .get(RELEASE_LATEST_API)
        .header("User-Agent", "mermaid-cli")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| anyhow!("could not reach GitHub Releases: {e}"))?;
    if !resp.status().is_success() {
        bail!("GitHub Releases API returned HTTP {}", resp.status());
    }
    let release: serde_json::Value = resp.json().await?;
    let tag = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("release response had no tag_name"))?;
    println!("Latest:    {tag}");

    let up_to_date = version_at_least(current, tag.trim_start_matches('v'));
    if check {
        if up_to_date {
            println!("You're on the latest version.");
        } else {
            println!("Update available: v{current} -> {tag}. Run `mermaid update` to install it.");
        }
        return Ok(());
    }
    if up_to_date && !force {
        println!("Already up to date.");
        return Ok(());
    }

    // Replace the binary in the directory it's running from, in place.
    let exe =
        std::env::current_exe().map_err(|e| anyhow!("could not locate current executable: {e}"))?;
    let install_dir = exe
        .parent()
        .ok_or_else(|| anyhow!("current executable has no parent directory"))?;

    // Confirm before fetching + running the install script — it executes
    // downloaded shell/PowerShell and replaces the running binary. `--force` is
    // the scripted-use bypass; a non-interactive session without it refuses
    // rather than running fetched code unprompted (#110).
    let script_url = if cfg!(target_os = "windows") {
        INSTALL_PS1_URL
    } else {
        INSTALL_SH_URL
    };
    if !crate::utils::confirm_or_refuse(
        &format!(
            "About to download and run {script_url} to replace {}.",
            install_dir.display()
        ),
        force,
    )? {
        println!("Update cancelled.");
        return Ok(());
    }

    println!("Updating {} …", install_dir.display());
    run_install_script(&client, install_dir).await?;
    println!("Updated. New version takes effect on the next run.");
    Ok(())
}

/// Fetch the platform install script from the Pages site and run it, pointed at
/// `install_dir` so it updates in place without touching PATH.
async fn run_install_script(client: &reqwest::Client, install_dir: &Path) -> Result<()> {
    let windows = cfg!(target_os = "windows");
    let url = if windows {
        INSTALL_PS1_URL
    } else {
        INSTALL_SH_URL
    };
    let script = client
        .get(url)
        .header("User-Agent", "mermaid-cli")
        .send()
        .await
        .map_err(|e| anyhow!("could not fetch install script: {e}"))?
        .error_for_status()?
        .text()
        .await?;

    let ext = if windows { "ps1" } else { "sh" };
    // Stage the fetched script in the per-user 0700 private temp dir, created
    // exclusively (O_EXCL → never follows/opens a pre-planted symlink) so a local
    // attacker can neither redirect the write nor swap the file between write and
    // exec (#F50). The previous world-readable, predictable
    // `temp_dir()/mermaid-update-<pid>.<ext>` allowed both a symlink redirect and
    // a write→exec TOCTOU.
    let dir = crate::utils::private_temp_dir()
        .map_err(|e| anyhow!("could not create private temp dir for install script: {e}"))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let script_path = dir.join(format!(
        "mermaid-update-{}-{nanos}.{ext}",
        std::process::id()
    ));
    stage_install_script(&script_path, script.as_bytes())
        .map_err(|e| anyhow!("could not stage install script: {e}"))?;

    let mut cmd = if windows {
        let mut c = tokio::process::Command::new("powershell");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
        c.arg(&script_path);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg(&script_path);
        c
    };
    cmd.env("MERMAID_INSTALL_DIR", install_dir)
        .env("MERMAID_NO_MODIFY_PATH", "1");

    let status = cmd
        .status()
        .await
        .map_err(|e| anyhow!("could not run install script: {e}"))?;
    let _ = std::fs::remove_file(&script_path);
    if !status.success() {
        bail!("install script exited with {:?}", status.code());
    }
    Ok(())
}

/// Write the fetched install script to `path`, creating it **exclusively** so a
/// symlink pre-planted at the path is refused (`O_EXCL` never follows) and the
/// staged code is owner-only (`0600` file inside the `0700` private dir). This
/// closes the symlink-redirect and write→exec TOCTOU that the old predictable,
/// world-readable temp path left open (#F50).
fn stage_install_script(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)
}

/// Parse a `[v]MAJOR.MINOR.PATCH[-pre][+build]` string into a comparable tuple.
fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// True iff `current` is at least `latest` (no update needed). Unparseable
/// versions fall back to string equality, so we never falsely report
/// up-to-date on garbage — at worst we re-run the (idempotent) installer.
fn version_at_least(current: &str, latest: &str) -> bool {
    match (parse_semver(current), parse_semver(latest)) {
        (Some(c), Some(l)) => c >= l,
        _ => current == latest,
    }
}

/// Show configured MCP servers
fn show_mcp_servers() {
    let config = load_config_or_warn();

    if config.mcp_servers.is_empty() {
        println!("No MCP servers configured.\n");
        println!("Add one with: mermaid add <name>");
        println!("Examples:");
        println!("  mermaid add context7     # Library documentation");
        println!("  mermaid add playwright   # Browser automation");
        println!("  mermaid add memory       # Persistent knowledge graph");
        return;
    }

    println!("Configured MCP servers:\n");
    for (name, server_cfg) in &config.mcp_servers {
        // Remote servers show their endpoint; stdio servers their package.
        let package: &str = match &server_cfg.url {
            Some(url) => url,
            None => server_cfg
                .args
                .iter()
                .find(|a| !a.starts_with('-'))
                .map(String::as_str)
                .unwrap_or(server_cfg.command.as_str()),
        };
        let env_keys: Vec<&String> = server_cfg.env.keys().collect();
        let env_display = if env_keys.is_empty() {
            String::new()
        } else {
            format!(
                " (env: {})",
                env_keys
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        println!("  {} — {}{}", name, package, env_display);
    }
    println!("\nManage with: mermaid add <name> / mermaid remove <name>");
}

/// Show status of all dependencies
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
async fn show_status(config: &Config) -> Result<()> {
    println!("Mermaid Status:");
    println!();

    // Remote providers: one block, listing exactly what `ProviderFactory`
    // could build right now — name, where the key came from, and the endpoint
    // requests would go to. This used to be printed twice, by two walks that
    // disagreed with each other and with the factory; see `providers::discovery`.
    let available = configured_remote_providers(config);
    if available.is_empty() {
        println!(
            "  [WARNING] Remote providers: none (no API keys in env or keyring; `mermaid login <provider>`)"
        );
    } else {
        println!("  [OK] Remote providers: {} configured", available.len());
        for provider in &available {
            println!(
                "      - {} ({}) {}",
                provider.name,
                provider.source_label(),
                provider.endpoint
            );
        }
    }
    // Half-configured providers are the ones worth a warning: the user set
    // something up and it still cannot be used. The reason is the factory's own
    // error, so it says exactly what a real request would have said.
    let problems = crate::providers::provider_problems(config);
    if !problems.is_empty() {
        println!(
            "  [WARNING] Providers configured but not usable: {}",
            problems.len()
        );
        for problem in &problems {
            println!("      - {}: {}", problem.name, problem.reason);
        }
    }

    // Check Ollama (via HTTP, so remote deployments are honored).
    // Diagnostics observe, they don't heal: autostart=false, otherwise a
    // status check would start the server and then report "Running" —
    // never able to observe the dead state it exists to surface.
    if is_ollama_installed() {
        match list_ollama_models(config).await {
            None => println!(
                "  [WARNING] Ollama: Installed but not running (started automatically \
                 when an Ollama model is used)"
            ),
            Some(models) if models.is_empty() => {
                println!("  [WARNING] Ollama: Running (no models installed)");
            },
            Some(models) => {
                println!("  [OK] Ollama: Running ({} models installed)", models.len());
                for model in models.iter().take(3) {
                    println!("      - {}", model);
                }
                if models.len() > 3 {
                    println!("      ... and {} more", models.len() - 3);
                }
            },
        }
    } else if available.is_empty() {
        println!("  [WARNING] Ollama: Not installed (and no remote provider configured)");
    } else {
        // Not a failure: the configured remote providers cover every model
        // this machine needs. Ollama is only required for local models.
        println!("  [INFO] Ollama: Not installed (only needed for local models)");
    }

    // Check configuration (uses platform-specific path via ProjectDirs)
    if let Ok(config_dir) = get_config_dir() {
        let config_path = config_dir.join("config.toml");
        if config_path.exists() {
            println!("  [OK] Configuration: {}", config_path.display());
        } else {
            println!("  [WARNING] Configuration: Not found (using defaults)");
        }
    }

    // MCP Servers
    if config.mcp_servers.is_empty() {
        println!("  [INFO] MCP Servers: None configured (use 'mermaid add <name>')");
    } else {
        println!(
            "  [OK] MCP Servers: {} configured",
            config.mcp_servers.len()
        );
        for (name, server_cfg) in &config.mcp_servers {
            let target: &str = match &server_cfg.url {
                Some(url) => url,
                None => server_cfg
                    .args
                    .get(1)
                    .map(String::as_str)
                    .unwrap_or(server_cfg.command.as_str()),
            };
            println!("      - {} ({})", name, target);
        }
    }

    // Project instructions (Step 5h). Walks UP from cwd to git root or
    // $HOME to find the nearest supported instruction files.
    {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let paths = crate::app::instructions::find_instruction_files(&cwd);
        if paths.is_empty() {
            println!("  [INFO] Project instructions: not found (AGENTS.md, MERMAID.md)");
        } else {
            match crate::app::instructions::load_from_paths(&paths) {
                Some(loaded) => {
                    let files = loaded
                        .sources
                        .iter()
                        .map(|source| {
                            source
                                .path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("instructions")
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "  [OK] Project instructions: {} at {} ({} bytes{})",
                        files,
                        loaded.path.display(),
                        loaded.byte_len,
                        if loaded.truncated { ", truncated" } else { "" }
                    );
                },
                None => {
                    println!(
                        "  [WARNING] Project instructions: found but unreadable ({})",
                        paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                },
            }
        }
    }

    // Environment variables (for API providers)
    println!("\n  Environment:");
    if std::env::var("OLLAMA_API_KEY").is_ok() {
        println!("    - OLLAMA_API_KEY: Set (for Ollama Cloud)");
    }

    println!();
    Ok(())
}

/// Dispatch `mermaid pr` subcommands.
fn handle_pr(command: &PrCommand) -> Result<()> {
    match command {
        PrCommand::Create {
            title,
            body,
            summary,
            base,
            draft,
            web,
            provider,
        } => create_pr(CreatePrArgs {
            title: title.as_deref(),
            body: body.as_deref(),
            summary: summary.as_deref(),
            base: base.as_deref(),
            draft: *draft,
            web: *web,
            provider: *provider,
        }),
    }
}

struct CreatePrArgs<'a> {
    title: Option<&'a str>,
    body: Option<&'a str>,
    summary: Option<&'a Path>,
    base: Option<&'a str>,
    draft: bool,
    web: bool,
    provider: Option<GitHost>,
}

/// Create a PR/MR by driving the host's official CLI (`gh`/`glab`), reusing
/// its authentication. We wrap the platform CLI rather than reimplementing
/// per-provider REST clients (issue #2): it reuses existing `gh auth` /
/// `glab auth`, handles each host's quirks, and keeps the surface tiny.
fn create_pr(args: CreatePrArgs) -> Result<()> {
    // Body precedence: --summary <file> wins over inline --body.
    let body = match args.summary {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("failed to read summary file {}", path.display()))?,
        ),
        None => args.body.map(str::to_string),
    };

    let host = match args.provider {
        Some(host) => host,
        None => detect_git_host()?,
    };

    let (cli, install_hint) = match host {
        GitHost::Github => (
            "gh",
            "Install the GitHub CLI (https://cli.github.com) and run `gh auth login`.",
        ),
        GitHost::Gitlab => (
            "glab",
            "Install the GitLab CLI (https://gitlab.com/gitlab-org/cli) and run `glab auth login`.",
        ),
    };
    if which::which(cli).is_err() {
        anyhow::bail!("`{cli}` was not found on your PATH. {install_hint}");
    }

    let argv = build_pr_argv(
        host,
        args.title,
        body.as_deref(),
        args.base,
        args.draft,
        args.web,
    );

    println!("Creating pull/merge request via `{cli}`…");
    let status = std::process::Command::new(cli)
        .args(&argv)
        .status()
        .with_context(|| format!("failed to run `{cli}`"))?;
    anyhow::ensure!(status.success(), "`{cli}` exited unsuccessfully ({status})");
    Ok(())
}

/// Auto-detect the host: prefer the `origin` remote URL, else fall back to
/// whichever provider CLI is installed.
fn detect_git_host() -> Result<GitHost> {
    if let Some(host) = git_origin_host() {
        return Ok(host);
    }
    if which::which("gh").is_ok() {
        return Ok(GitHost::Github);
    }
    if which::which("glab").is_ok() {
        return Ok(GitHost::Gitlab);
    }
    anyhow::bail!(
        "could not detect a Git host from the `origin` remote. Pass `--provider github|gitlab` and install the matching CLI (`gh`/`glab`)."
    )
}

fn git_origin_host() -> Option<GitHost> {
    let output = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    host_from_remote_url(String::from_utf8_lossy(&output.stdout).trim())
}

fn host_from_remote_url(url: &str) -> Option<GitHost> {
    let lower = url.to_ascii_lowercase();
    if lower.contains("github.com") {
        Some(GitHost::Github)
    } else if lower.contains("gitlab") {
        Some(GitHost::Gitlab)
    } else {
        None
    }
}

/// Build the argv passed to the host CLI. Pure (no I/O), so it's unit-tested.
fn build_pr_argv(
    host: GitHost,
    title: Option<&str>,
    body: Option<&str>,
    base: Option<&str>,
    draft: bool,
    web: bool,
) -> Vec<String> {
    let s = |v: &str| v.to_string();
    let has_content = title.is_some() || body.is_some();
    let mut argv = Vec::new();
    match host {
        GitHost::Github => {
            argv.push(s("pr"));
            argv.push(s("create"));
            if web {
                argv.push(s("--web"));
            }
            if draft {
                argv.push(s("--draft"));
            }
            if let Some(title) = title {
                argv.push(s("--title"));
                argv.push(s(title));
            }
            if let Some(body) = body {
                argv.push(s("--body"));
                argv.push(s(body));
            }
            // No explicit content (and not the web form) → let gh fill the
            // title/body from the branch's commits rather than blocking on an
            // interactive prompt.
            if !has_content && !web {
                argv.push(s("--fill"));
            }
            if let Some(base) = base {
                argv.push(s("--base"));
                argv.push(s(base));
            }
        },
        GitHost::Gitlab => {
            argv.push(s("mr"));
            argv.push(s("create"));
            if web {
                argv.push(s("--web"));
            }
            if draft {
                argv.push(s("--draft"));
            }
            if let Some(title) = title {
                argv.push(s("--title"));
                argv.push(s(title));
            }
            if let Some(body) = body {
                argv.push(s("--description"));
                argv.push(s(body));
            }
            if !has_content && !web {
                argv.push(s("--fill"));
            }
            if let Some(base) = base {
                argv.push(s("--target-branch"));
                argv.push(s(base));
            }
        },
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_uses_resolved_keyless_web_capabilities() {
        let config = Config {
            web: crate::app::WebConfig {
                fetch_backend: crate::app::FetchBackend::Native,
                search_backend: crate::app::SearchBackend::Searxng,
                searxng_url: "http://127.0.0.1:8080".to_string(),
            },
            ..Config::default()
        };
        let (tools, next_steps) = web_doctor_entries(&config);
        assert!(
            tools
                .iter()
                .any(|entry| entry.contains("web_fetch (native"))
        );
        assert!(
            tools
                .iter()
                .any(|entry| entry.contains("web_search (searxng"))
        );
        assert!(next_steps.is_empty(), "unexpected warnings: {next_steps:?}");
        assert!(
            tools.iter().all(|entry| !entry.contains("container")),
            "doctor must describe the selected capability, not stale container setup"
        );
    }

    #[test]
    fn doctor_reports_global_network_deny_instead_of_advertising_web() {
        let mut config = Config::default();
        config.web.search_backend = crate::app::SearchBackend::Searxng;
        config.web.searxng_url = "http://127.0.0.1:8080".to_string();
        config.safety.network = crate::app::NetworkPolicy::Deny;

        let (tools, next_steps) = web_doctor_entries(&config);
        assert!(
            tools
                .iter()
                .all(|entry| !entry.starts_with("web_fetch") && !entry.starts_with("web_search")),
            "network-denied tools were advertised: {tools:?}"
        );
        for name in ["web_fetch", "web_search"] {
            assert!(
                next_steps.iter().any(|entry| {
                    entry.contains(name) && entry.contains("safety.network = \"deny\"")
                }),
                "missing network-deny explanation for {name}: {next_steps:?}"
            );
        }
    }

    #[test]
    fn sanitize_terminal_text_strips_control_sequences() {
        // Plain text and the allowed whitespace pass through unchanged.
        assert_eq!(
            sanitize_terminal_text("hello\tworld\nline two"),
            "hello\tworld\nline two"
        );
        // CSI color sequence is removed, surrounding text kept.
        assert_eq!(
            sanitize_terminal_text("\u{1b}[31mRED\u{1b}[0m text"),
            "RED text"
        );
        // OSC-52 clipboard write (BEL-terminated) is removed whole.
        assert_eq!(
            sanitize_terminal_text("before\u{1b}]52;c;cGF5bG9hZA==\u{07}after"),
            "beforeafter"
        );
        // OSC window-title rewrite terminated by ST (ESC '\').
        assert_eq!(sanitize_terminal_text("a\u{1b}]0;pwned\u{1b}\\b"), "ab");
        // Charset-designation (ESC '(' 'B') drops its final byte too.
        assert_eq!(sanitize_terminal_text("x\u{1b}(By"), "xy");
        // Bare CR and a C1 control are dropped; \n is preserved.
        assert_eq!(sanitize_terminal_text("a\rb\u{9b}c\n"), "abc\n");
    }

    #[test]
    fn version_compare_handles_update_logic() {
        // Up to date / newer than latest ⇒ no update.
        assert!(version_at_least("0.10.2", "0.10.2"));
        assert!(version_at_least("0.11.0", "0.10.2"));
        assert!(version_at_least("1.0.0", "0.99.99"));
        // Older ⇒ update available.
        assert!(!version_at_least("0.10.1", "0.10.2"));
        assert!(!version_at_least("0.9.0", "0.10.0"));
        assert!(!version_at_least("0.10.2", "0.11.0"));
        // Pre-release/build suffixes and `v` prefixes are tolerated.
        assert!(version_at_least("0.10.2", "v0.10.2"));
        assert_eq!(parse_semver("v0.11.0-rc1+build"), Some((0, 11, 0)));
        assert_eq!(parse_semver("0.10"), Some((0, 10, 0)));
        // Garbage never falsely reports up-to-date unless identical.
        assert!(!version_at_least("0.10.2", "not-a-version"));
    }

    #[test]
    fn host_from_remote_url_detects_provider() {
        assert_eq!(
            host_from_remote_url("https://github.com/foo/bar.git"),
            Some(GitHost::Github)
        );
        assert_eq!(
            host_from_remote_url("git@github.com:foo/bar.git"),
            Some(GitHost::Github)
        );
        assert_eq!(
            host_from_remote_url("https://gitlab.com/foo/bar.git"),
            Some(GitHost::Gitlab)
        );
        assert_eq!(
            host_from_remote_url("git@gitlab.example.com:foo/bar.git"),
            Some(GitHost::Gitlab)
        );
        assert_eq!(host_from_remote_url("https://bitbucket.org/foo/bar"), None);
    }

    #[test]
    fn build_pr_argv_github_with_content() {
        let argv = build_pr_argv(
            GitHost::Github,
            Some("T"),
            Some("B"),
            Some("main"),
            true,
            false,
        );
        assert_eq!(
            argv,
            vec![
                "pr", "create", "--draft", "--title", "T", "--body", "B", "--base", "main"
            ]
        );
    }

    #[test]
    fn build_pr_argv_github_fills_without_content() {
        let argv = build_pr_argv(GitHost::Github, None, None, None, false, false);
        assert!(argv.contains(&"--fill".to_string()));
        assert!(!argv.contains(&"--title".to_string()));
    }

    #[test]
    fn build_pr_argv_web_skips_fill() {
        let argv = build_pr_argv(GitHost::Github, None, None, None, false, true);
        assert!(argv.contains(&"--web".to_string()));
        assert!(!argv.contains(&"--fill".to_string()));
    }

    #[test]
    fn build_pr_argv_gitlab_uses_mr_and_target_branch() {
        let argv = build_pr_argv(GitHost::Gitlab, Some("T"), None, Some("main"), false, false);
        assert_eq!(&argv[0..2], &["mr", "create"]);
        assert!(argv.windows(2).any(|w| w == ["--target-branch", "main"]));
        assert!(argv.contains(&"--title".to_string()));
    }

    #[test]
    fn qa_compact_smoke_persists_conversation_and_archive() {
        let dir = unique_temp_dir("mermaid-qa-compact-smoke");
        std::fs::create_dir_all(&dir).unwrap();

        let report = run_qa_compact_smoke(&Config::default(), &dir, 6).unwrap();

        assert!(report.ok);
        assert!(report.archived_messages > 0);
        assert!(report.preserved_messages > 0);
        assert!(report.replacement_messages >= 3);
        assert!(
            std::path::Path::new(report.conversation_path.as_ref().unwrap()).exists(),
            "conversation path should exist"
        );
        assert!(
            std::path::Path::new(report.archive_path.as_ref().unwrap()).exists(),
            "archive path should exist"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn qa_model_id_falls_back_to_deterministic() {
        assert_eq!(qa_model_id(&Config::default()), "qa/deterministic");
    }

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("{name}-{nanos}"))
    }
}
