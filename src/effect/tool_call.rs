//! Tool dispatch, plus the runtime tool-run bookkeeping that brackets it.
use mermaid_model::utils::{join_logged, spawn_guarded};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::*;

/// Dispatch an `ExecuteTool` command.
#[expect(clippy::too_many_arguments)]
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub(super) async fn dispatch_execute_tool(
    msg_tx: MsgSender,
    tools: Option<Arc<ToolRegistry>>,
    workdir: PathBuf,
    turn: TurnId,
    call_id: mermaid_domain::ToolCallId,
    source: mermaid_model::models::tool_call::ToolCall,
    token: tokio_util::sync::CancellationToken,
    background: tokio_util::sync::CancellationToken,
    web_bytes: Arc<std::sync::atomic::AtomicUsize>,
    config: Arc<mermaid_domain::Config>,
    model_id: String,
    task_id: Option<String>,
    session_id: String,
    message_index: usize,
    scratchpad: Option<PathBuf>,
    safety_mode: mermaid_runtime::SafetyMode,
    plan_file: Option<PathBuf>,
    plan_permissions: mermaid_domain::PlanPermissions,
    context_percent: Option<u8>,
    intent: Option<String>,
    classifier: Option<Arc<dyn crate::providers::AutoClassifier>>,
    approval: Option<crate::providers::ApprovalBroker>,
    questions: Option<crate::providers::QuestionBroker>,
    tasks: crate::providers::TaskBroker,
) {
    let _ = msg_tx.send(Msg::ToolStarted { turn, call_id }).await;

    let Some(registry) = tools else {
        let _ = msg_tx
            .send(Msg::ToolFinished {
                turn,
                call_id,
                outcome: mermaid_domain::ToolOutcome::error(
                    "EffectRunner has no ToolRegistry bound",
                    0.0,
                ),
            })
            .await;
        return;
    };

    // Route MCP-prefixed calls to the mcp proxy, which takes
    // {server_name, tool_name, arguments}. The raw model call has
    // those embedded in the function name and arguments respectively.
    let (tool_key, args) = if source.function.name.starts_with("mcp__") {
        let rest = &source.function.name[5..];
        if let Some((server, tool)) = rest.split_once("__") {
            (
                "mcp_proxy",
                serde_json::json!({
                    "server_name": server,
                    "tool_name": tool,
                    "arguments": source.function.arguments.clone(),
                }),
            )
        } else {
            let _ = msg_tx
                .send(Msg::ToolFinished {
                    turn,
                    call_id,
                    outcome: mermaid_domain::ToolOutcome::error(
                        format!("invalid MCP tool name: {}", source.function.name),
                        0.0,
                    ),
                })
                .await;
            return;
        }
    } else {
        (
            source.function.name.as_str(),
            source.function.arguments.clone(),
        )
    };
    let tool_run_id =
        start_runtime_tool_run(task_id.as_deref(), turn, call_id, tool_key, &args).await;

    let Some(tool) = registry.get(tool_key) else {
        let outcome = mermaid_domain::ToolOutcome::error(format!("unknown tool: {tool_key}"), 0.0);
        finish_runtime_tool_run(tool_run_id.as_deref(), &outcome);
        let _ = msg_tx
            .send(Msg::ToolFinished {
                turn,
                call_id,
                outcome,
            })
            .await;
        return;
    };

    // Bridge the tool's progress channel to `Msg::ToolProgress`.
    // A sibling task drains progress events while the tool runs.
    // The channel closes when `progress_tx` drops (when `ctx`
    // drops at the end of `tool.execute`), which terminates the
    // relay loop cleanly.
    let (progress_tx, mut progress_rx) = mpsc::channel(16);
    let relay_tx = msg_tx.clone();
    let relay_token = token.clone();
    let progress_relay = spawn_guarded(async move {
        loop {
            let event = tokio::select! {
                biased;
                _ = relay_token.cancelled() => break,
                ev = progress_rx.recv() => match ev {
                    Some(ev) => ev,
                    None => break,
                },
            };
            if relay_tx
                .send(Msg::ToolProgress {
                    turn,
                    call_id,
                    event,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let mut ctx = ExecContext::new(
        token,
        progress_tx,
        call_id,
        turn,
        workdir,
        config,
        model_id,
        task_id,
        Some(session_id),
        Some(message_index as i64),
        safety_mode,
        intent,
        classifier,
        approval,
        questions,
        Some(tasks.clone()),
    );
    ctx.background = background;
    ctx.web_bytes = web_bytes;
    ctx.plan_file = plan_file;
    ctx.plan_permissions = plan_permissions;
    ctx.context_percent = context_percent;
    // Detached work (backgrounded subagents) reports back through the main
    // msg channel after this turn's progress relay is gone.
    ctx.notify = Some(msg_tx.clone());
    // Per-session scratch dir, when the session has one materialized.
    ctx.scratchpad = scratchpad;
    // `before_tool_use` is the one DECISION event: an enabled plugin hook may
    // deny the call, rewrite its arguments, or inject context for the next
    // model request. Every other event stays fire-and-forget.
    let before_payload = serde_json::json!({
        "turn_id": turn.0,
        "call_id": call_id.0,
        "tool": tool_key,
        "arguments": args,
    });
    let gate = run_plugin_hooks_gated("before_tool_use", before_payload).await;
    if !gate.context.is_empty() {
        // Injected context flows into transcripts/model input — scrub
        // credential-shaped content on the way in.
        let texts = gate
            .context
            .iter()
            .map(|t| mermaid_model::utils::redact_secrets(t))
            .collect();
        let _ = msg_tx.send(Msg::HookContext { turn, texts }).await;
    }
    if let Some((plugin, reason)) = gate.deny {
        // Mirror the unknown-tool arm: synthesize an error outcome and unwind.
        // Dropping `ctx` closes the progress channel so the relay terminates
        // before the join below.
        drop(ctx);
        let reason = mermaid_model::utils::redact_secrets(&reason);
        let outcome = mermaid_domain::ToolOutcome::error(
            format!("Denied by plugin hook ({plugin}): {reason}"),
            0.0,
        );
        finish_runtime_tool_run(tool_run_id.as_deref(), &outcome);
        join_logged(progress_relay.take(), "tool_progress_relay").await;
        let _ = msg_tx
            .send(Msg::ToolFinished {
                turn,
                call_id,
                outcome,
            })
            .await;
        return;
    }
    // A rewritten input is deliberately NOT redacted (it becomes executable
    // args — corrupting them would be worse), and it cannot launder a blocked
    // action: the policy gate runs inside `tool.execute` and vets the
    // rewritten call exactly like an original one.
    let args = gate.updated_input.unwrap_or(args);
    let outcome = tool.execute(args, ctx).await;
    // Evidence trail: attribute this call to the in-progress checklist task
    // (no-op when none). The task tools themselves are skipped — a checklist
    // edit is not evidence of work on the task. `display_info_for` gives the
    // same human target the transcript row shows (path, command head, query).
    if !source.function.name.starts_with("task_") {
        let (action, target) = mermaid_domain::display_info_for(&mermaid_domain::PendingToolCall {
            call_id,
            source: source.clone(),
        });
        tasks
            .record_evidence(mermaid_domain::EvidenceEntry {
                tool: action,
                target,
                status: tool_status_label(outcome.status).to_string(),
            })
            .await;
    }
    let after_payload = serde_json::json!({
        "turn_id": turn.0,
        "call_id": call_id.0,
        "tool": tool_key,
        "status": tool_status_label(outcome.status),
        "summary": &outcome.summary,
    });
    fire_plugin_hooks("after_tool_use", after_payload).await;
    finish_runtime_tool_run(tool_run_id.as_deref(), &outcome);
    join_logged(progress_relay.take(), "tool_progress_relay").await;
    let _ = msg_tx
        .send(Msg::ToolFinished {
            turn,
            call_id,
            outcome,
        })
        .await;
}

pub(super) async fn start_runtime_tool_run(
    task_id: Option<&str>,
    turn: TurnId,
    call_id: mermaid_domain::ToolCallId,
    tool_name: &str,
    args: &serde_json::Value,
) -> Option<String> {
    // Synchronous rusqlite write on the hot tool-execution path — offload it to
    // the blocking pool. The id is needed by `finish`, so we await the result
    // (unlike `finish`, which is fire-and-forget) (#39).
    let task_id = task_id.map(str::to_string);
    let tool_name = tool_name.to_string();
    let args_json = redacted_json_string(args);
    tokio::task::spawn_blocking(move || {
        mermaid_runtime::RuntimeStore::open_default()
            .and_then(|store| {
                store.tool_runs().start(mermaid_runtime::NewToolRun {
                    id: None,
                    task_id,
                    turn_id: Some(turn.0.to_string()),
                    call_id: Some(call_id.0.to_string()),
                    tool_name,
                    args_json,
                })
            })
            .map(|record| record.id)
            .ok()
    })
    .await
    .ok()
    .flatten()
}

pub(super) fn finish_runtime_tool_run(
    tool_run_id: Option<&str>,
    outcome: &mermaid_domain::ToolOutcome,
) {
    let Some(tool_run_id) = tool_run_id else {
        return;
    };
    let tool_run_id = tool_run_id.to_string();
    let status = tool_status_label(outcome.status).to_string();
    let output_json = redacted_json_string(&serde_json::json!({
        "status": tool_status_label(outcome.status),
        "summary": &outcome.summary,
        "model_content": &outcome.model_content,
        "error": &outcome.error,
        "metadata": &outcome.metadata,
        "artifacts": &outcome.artifacts,
        "duration_secs": outcome.duration_secs,
    }));
    // Fire-and-forget telemetry write on the blocking pool — don't stall the
    // tool-finish path waiting on rusqlite (#39).
    tokio::task::spawn_blocking(move || {
        if let Ok(store) = mermaid_runtime::RuntimeStore::open_default() {
            let _ = store
                .tool_runs()
                .finish(&tool_run_id, &status, output_json.as_deref());
        }
    });
}

/// Serialize a durable runtime payload only after applying the same mandatory
/// credential redaction used by recordings and conversation archives. Keep
/// executable values unmodified in memory; this helper is exclusively for
/// persistence sinks.
pub(super) fn redacted_json_string(value: &serde_json::Value) -> Option<String> {
    let mut redacted = value.clone();
    mermaid_model::utils::redact_json(&mut redacted);
    serde_json::to_string(&redacted).ok()
}

pub(super) fn tool_status_label(status: mermaid_domain::ToolStatus) -> &'static str {
    match status {
        mermaid_domain::ToolStatus::Success => "success",
        mermaid_domain::ToolStatus::Error => "error",
        mermaid_domain::ToolStatus::Cancelled => "cancelled",
    }
}

pub(super) fn runtime_model_info_text(model: &str) -> String {
    let snapshot = mermaid_domain::runtime::ProviderCapabilitySnapshot::from_model_id(model);
    let mut lines = vec![
        format!("Model info: {}", model),
        format!("- provider: {}", snapshot.provider),
        format!("- model: {}", snapshot.model),
        format!("- supports tools: {}", snapshot.supports_tools),
        format!("- supports vision: {}", snapshot.supports_vision),
        format!("- reasoning: {}", snapshot.reasoning),
        format!(
            "- context limit: {}",
            snapshot
                .max_context_tokens
                .map(|value: usize| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ),
    ];
    if let Ok(store) = mermaid_runtime::RuntimeStore::open_default()
        && let Ok(probes) = store
            .provider_probes()
            .list(Some(&snapshot.provider), Some(&snapshot.model))
        && !probes.is_empty()
    {
        lines.push(String::new());
        lines.push("Cached provider reality records:".to_string());
        for probe in probes {
            lines.push(format!(
                "- {} = {} ({})",
                probe.capability_key, probe.capability_value, probe.confidence
            ));
        }
    }
    lines.join("\n")
}

/// Spawn `ollama pull <model>` and stream its stdout lines as
/// `Msg::ModelPullProgress` status updates. Emits a final
/// `Msg::ModelPullFinished` on successful exit; on failure, emits a
/// single `ModelPullProgress` with the error text.
pub(super) async fn dispatch_pull_ollama_model(tx: MsgSender, model: String) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let mut cmd = Command::new("ollama");
    cmd.arg("pull")
        .arg(&model)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx
                .send(Msg::ModelPullProgress(format!(
                    "ollama pull failed to start: {e}"
                )))
                .await;
            return;
        },
    };

    // Capture the reader's handle instead of orphaning it: the child's stdout
    // closes when it exits, so this task finishes right after `child.wait`
    // below — we join it there so a panic is logged, not silently lost (#60).
    let reader_handle = child.stdout.take().map(|stdout| {
        let tx_inner = tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = tx_inner.send(Msg::ModelPullProgress(line)).await;
            }
        })
    });

    match child.wait().await {
        Ok(status) if status.success() => {
            let _ = tx.send(Msg::ModelPullFinished { model }).await;
        },
        Ok(status) => {
            let _ = tx
                .send(Msg::ModelPullProgress(format!(
                    "ollama pull exited with status {}",
                    status.code().unwrap_or(-1)
                )))
                .await;
        },
        Err(e) => {
            let _ = tx
                .send(Msg::ModelPullProgress(format!(
                    "ollama pull wait error: {e}"
                )))
                .await;
        },
    }

    // The child has exited; its stdout is closed, so the reader is finishing.
    // Join it (logging a panic) so it isn't left orphaned (#60).
    if let Some(handle) = reader_handle {
        join_logged(handle, "ollama_pull_reader").await;
    }
}

/// Start every configured MCP server CONCURRENTLY, each bounded by
/// `MCP_STARTUP_TIMEOUT`, emitting one `Msg::McpServerReady`/`McpServerErrored`
/// per server AS IT RESOLVES — a slow server never delays the rest. The
/// (initially empty) manager is installed BEFORE the tasks spawn so shutdown
/// always finds it; init is "complete" once every server has resolved
/// (`McpToolProxy::wait_ready` semantics unchanged — a first-message
/// `mcp__` call waits, bounded, for the full fleet). A zero-tool server that
/// started successfully is still Ready with an empty tool list.
pub(super) async fn dispatch_init_mcp_servers(
    configs: std::collections::HashMap<String, mermaid_domain::McpServerConfig>,
    tx: tokio::sync::mpsc::Sender<Msg>,
) {
    if configs.is_empty() {
        return;
    }
    crate::mcp::manager_ref::mark_init_started();
    let manager = std::sync::Arc::new(crate::mcp::McpServerManager::new(&configs));
    crate::mcp::manager_ref::set_manager(manager.clone());
    let mut join = tokio::task::JoinSet::new();
    for (name, config) in configs {
        let manager = manager.clone();
        let tx = tx.clone();
        join.spawn(async move {
            let msg = match manager.start_server(&name, &config).await {
                Ok(tools) => Msg::McpServerReady { name, tools },
                Err(e) => Msg::McpServerErrored {
                    name,
                    reason: e.to_string(),
                },
            };
            let _ = tx.send(msg).await;
        });
    }
    while join.join_next().await.is_some() {}
    crate::mcp::manager_ref::mark_init_complete();
}

/// Read the system clipboard on a blocking thread and emit a `Msg`
/// back into the main loop. Image content wins when present; falls
/// back to text; empty or error surface as `Msg::TransientStatus` so
/// the user gets visible feedback (a silent no-op on Ctrl+V would be
/// confusing, especially on macOS where `osascript` can take ~300ms).
///
/// `tokio::task::spawn_blocking` is the right primitive: `clipboard::
/// has_image` / `read_image_bytes` / `read_text` shell out to xclip /
/// wl-paste / pngpaste / PowerShell, all of which block synchronously —
/// bounded, since every clipboard subprocess runs under a kill-on-timeout
/// deadline, so a hung helper returns an error here instead of pinning
/// this blocking thread forever.
pub(super) async fn dispatch_read_clipboard(tx: MsgSender) {
    use mermaid_domain::ClipboardRead;

    enum Outcome {
        Image { bytes: Vec<u8>, format: String },
        Text(String),
        Empty,
        Error(String),
    }

    let outcome = tokio::task::spawn_blocking(|| {
        if crate::clipboard::has_image() {
            match crate::clipboard::read_image_bytes() {
                Ok((bytes, format)) => Outcome::Image { bytes, format },
                Err(e) => Outcome::Error(format!("Clipboard image read failed: {e}")),
            }
        } else {
            match crate::clipboard::read_text() {
                Ok(t) if !t.is_empty() => Outcome::Text(t),
                Ok(_) => Outcome::Empty,
                Err(e) => Outcome::Error(format!("Clipboard empty / read failed: {e}")),
            }
        }
    })
    .await
    .unwrap_or_else(|e| Outcome::Error(format!("clipboard spawn_blocking: {e}")));

    // Route ALL four outcomes through `Msg::ClipboardRead` (not `Msg::Paste` /
    // `Msg::TransientStatus`): the reducer decrements `clipboard_reads_pending`
    // on exactly these messages, so an empty/failed read must still land here to
    // release a submit that was held waiting on it.
    let msg = match outcome {
        Outcome::Image { bytes, format } => {
            Msg::ClipboardRead(ClipboardRead::Image { bytes, format })
        },
        Outcome::Text(text) => Msg::ClipboardRead(ClipboardRead::Text(text)),
        Outcome::Empty => Msg::ClipboardRead(ClipboardRead::Empty),
        Outcome::Error(text) => Msg::ClipboardRead(ClipboardRead::Error(text)),
    };
    let _ = tx.send(msg).await;
}

/// Probe whether `model_id` can see images and report it via
/// `Msg::ProviderVisionResolved`. Best-effort: an unresolvable provider or a
/// provider that doesn't probe (non-Ollama) reports `None` ("unknown"), which
/// the reducer treats as "don't warn". `warn` rides through unchanged so the
/// reducer knows whether an image is actually in play.
pub(super) async fn dispatch_probe_vision(
    model_id: String,
    warn: bool,
    providers: Option<Arc<ProviderFactory>>,
    tx: MsgSender,
) {
    let supports_vision = match providers {
        Some(factory) => match factory.resolve(&model_id).await {
            Ok(provider) => provider.supports_vision().await,
            Err(_) => None,
        },
        None => None,
    };
    let _ = tx
        .send(Msg::ProviderVisionResolved {
            model_id,
            supports_vision,
            warn,
        })
        .await;
}

/// Everything the user could switch to, for the `/model` picker.
///
/// Two sources, both best-effort — a failure anywhere yields fewer rows, never
/// an error: the local Ollama daemon (queried WITHOUT autostart, so opening a
/// list can't resurrect a server the user deliberately stopped) and the model
/// catalog of every remote provider that has a resolvable key. The catalogs
/// come from `providers::discovery`, which covers the bespoke providers
/// (Anthropic, Gemini, Meta) as well as the OpenAI-compatible registry — this
/// list was registry-only once, and every `meta/muse-spark-*` was missing.
pub(super) async fn discover_available_models(
    providers: Option<Arc<ProviderFactory>>,
) -> Vec<mermaid_domain::state::ModelChoice> {
    use mermaid_domain::state::ModelChoice;
    let Some(factory) = providers else {
        return Vec::new();
    };
    let config = factory.config();
    let mut out: Vec<ModelChoice> = Vec::new();

    // ── Local models ────────────────────────────────────────────────
    if let Some(names) = list_ollama_models_readonly(config).await {
        for name in names {
            out.push(ModelChoice {
                id: format!("ollama/{name}"),
                group: "Local (Ollama)".to_string(),
                detail: "runs on this machine".to_string(),
                ready: true,
            });
        }
    }

    // ── Remote catalogs ─────────────────────────────────────────────
    for catalog in crate::providers::discovery::provider_catalogs(config).await {
        // Every row here is selectable, so an unreadable catalog contributes
        // nothing rather than a placeholder that would switch to a broken id.
        // `mermaid list` is the surface that says the catalog failed.
        let Some(models) = catalog.models else {
            continue;
        };
        for id in models {
            out.push(ModelChoice {
                id: format!("{}/{}", catalog.provider.name, id),
                group: catalog.provider.name.clone(),
                detail: String::new(),
                ready: true,
            });
        }
    }

    // Stable order: local first (it is the sovereign default), then providers
    // alphabetically, then model id. The picker's filter does the rest.
    out.sort_by(|a, b| {
        let local = |c: &ModelChoice| u8::from(c.group != "Local (Ollama)");
        local(a)
            .cmp(&local(b))
            .then_with(|| a.group.cmp(&b.group))
            .then_with(|| a.id.cmp(&b.id))
    });
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// Ask the local Ollama daemon for its models. `None` when it could not be
/// reached, distinct from `Some(vec![])` (running with nothing pulled).
///
/// Autostart is hard-off: enumerating must never mutate. Same contract as the
/// CLI's `list_ollama_models`.
pub(super) async fn list_ollama_models_readonly(
    config: &mermaid_domain::Config,
) -> Option<Vec<String>> {
    use mermaid_model::models::adapters::ollama::OllamaAdapter;
    use mermaid_model::models::{BackendConfig, Model};
    let backend = BackendConfig {
        ollama_url: format!("{}:{}", config.ollama.host, config.ollama.port),
        timeout_secs: 5,
        max_idle_per_host: 2,
        // Enumerating must never mutate — see the doc comment.
        ollama_autostart: false,
    };
    match OllamaAdapter::new("__list__", Arc::new(backend)).await {
        Ok(adapter) => adapter.list_models().await.ok(),
        Err(_) => None,
    }
}

/// Write text to the system clipboard on a blocking thread (the platform
/// tools shell out and block), then report the result via a transient status.
pub(super) async fn dispatch_copy_to_clipboard(text: String, tx: MsgSender) {
    let char_count = text.chars().count();
    let result = tokio::task::spawn_blocking(move || crate::clipboard::write_text(&text))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("clipboard spawn_blocking: {e}")));

    let msg = match result {
        // A successful copy is feedback on a keystroke, not conversation: it
        // toasts above the input and expires. Failures still land in the
        // transcript, where they can be read after the fact.
        Ok(()) => Msg::Toast {
            text: format!("copied {char_count} chars to clipboard"),
        },
        Err(e) => Msg::TransientStatus {
            text: format!("Copy failed: {e}"),
        },
    };
    let _ = tx.send(msg).await;
}

pub(super) fn classify_error_for_ui(
    e: &mermaid_model::models::ModelError,
) -> mermaid_model::models::UserFacingError {
    use mermaid_model::models::{ErrorCategory, ModelError, UserFacingError};
    match e {
        ModelError::Backend(b) => UserFacingError {
            summary: "Backend error".to_string(),
            message: b.to_string(),
            suggestion: "Check the provider endpoint / API key.".to_string(),
            category: ErrorCategory::Connection,
            recoverable: true,
        },
        ModelError::Authentication(msg) => UserFacingError {
            summary: "Auth error".to_string(),
            message: msg.clone(),
            suggestion: "Set the env var the provider expects.".to_string(),
            category: ErrorCategory::Auth,
            recoverable: false,
        },
        ModelError::RateLimit {
            retry_after,
            message,
        } => UserFacingError {
            summary: "Rate limited".to_string(),
            // The provider's own reason distinguishes "slow down" from
            // "daily quota exhausted" — show it when the 429 body had one.
            message: message.clone().unwrap_or_else(|| {
                "The provider rejected the request with 429 (too many requests).".to_string()
            }),
            suggestion: match retry_after {
                Some(secs) => format!("The provider asked to retry after {secs}s."),
                None => "Retry shortly; if it persists, check your plan's quota.".to_string(),
            },
            category: ErrorCategory::Temporary,
            recoverable: true,
        },
        ModelError::StreamError(msg) => UserFacingError {
            summary: "Stream error".to_string(),
            message: msg.clone(),
            suggestion: "Retry the request.".to_string(),
            category: ErrorCategory::Connection,
            recoverable: true,
        },
        other => UserFacingError {
            summary: "Model error".to_string(),
            message: other.to_string(),
            suggestion: String::new(),
            category: ErrorCategory::Internal,
            recoverable: false,
        },
    }
}
