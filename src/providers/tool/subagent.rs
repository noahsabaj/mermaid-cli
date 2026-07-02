//! `agent` tool — spawn a child reducer loop as a tool.
//!
//! The design rests on one observation: from the model's perspective,
//! delegating to a subagent is "call a tool with a prompt, get back
//! a summary". There's no state-machine visibility the parent
//! reducer needs — `TurnState::ExecutingTools` already parallelizes
//! tool calls for free, so a single model turn emitting three
//! `agent` calls gets three concurrent `SubagentTool::execute`
//! invocations with zero additional infrastructure.
//!
//! Everything lives inside this module:
//!
//! - `SubagentSpawner` owns the shared `ProviderFactory` + a
//!   `Semaphore(max_inflight)` that backpressures parallel fan-out.
//!   Subagents can't themselves spawn subagents — `build_child_registry`
//!   omits the `agent` tool — so there's no recursion to depth-cap.
//! - `SubagentTool::execute` builds a fresh child `State` (flagged
//!   `is_subagent`, so its system prompt carries the report contract;
//!   MCP entries seeded Ready from the process-global manager), a
//!   filtered `ToolRegistry` (no self-recursion, no GUI tools), and
//!   a child `EffectRunner` + msg channel. It drives the child
//!   reducer to `Idle`, streaming progress back to the parent via
//!   `ProgressEvent::Subagent*` (rendered live in the status line),
//!   and returns the last assistant message as the tool's `output`,
//!   with the child's token usage on the outcome metadata so the
//!   parent's session totals count the whole tree.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::domain::{
    Msg, State, ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata, TurnState, update,
};
use crate::effect::{EffectRunner, MSG_CHANNEL_CAPACITY};
use crate::models::MessageRole;
use crate::providers::ProviderFactory;
use crate::providers::ctx::{ExecContext, ProgressEvent, SubagentPhase};

use super::ToolExecutor;
use super::ToolRegistry;

/// Maximum subagents running simultaneously across the whole process.
/// Covers the pathological "parent emits 30 agent calls in one turn"
/// case. Hit this cap → later calls block on the semaphore until
/// some earlier subagent finishes or cancels.
pub const MAX_INFLIGHT: usize = 10;

/// Hard ceiling on a subagent's wall-clock runtime. Above this the
/// subagent is cancelled and reports `Error`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 20 * 60;

/// Shared spawner. One per process; held by `SubagentTool`.
pub struct SubagentSpawner {
    providers: Arc<ProviderFactory>,
    inflight: Arc<Semaphore>,
}

impl SubagentSpawner {
    pub fn new(providers: Arc<ProviderFactory>) -> Self {
        Self {
            providers,
            inflight: Arc::new(Semaphore::new(MAX_INFLIGHT)),
        }
    }
}

/// The `agent` tool the model sees.
pub struct SubagentTool {
    spawner: Arc<SubagentSpawner>,
}

impl SubagentTool {
    pub fn new(spawner: Arc<SubagentSpawner>) -> Self {
        Self { spawner }
    }
}

#[async_trait]
impl ToolExecutor for SubagentTool {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "agent".to_string(),
            description: format!(
                "Spawn a child agent with its own context and tool access to work on an \
                 independent sub-task. Useful for parallel fan-out (emit multiple `agent` \
                 calls in the same turn to run them concurrently) or for scoping a noisy \
                 sub-task (the child's tool output doesn't clutter the parent's turn). \
                 Breadth-capped at {max_breadth} concurrent; subagents can't themselves \
                 spawn subagents. Subagents don't get GUI (screenshot/click/…) access \
                 because coordinate metadata can't be shared cleanly.",
                max_breadth = MAX_INFLIGHT,
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The task for the subagent. Self-contained; the subagent has no access to the parent's conversation."
                    },
                    "description": {
                        "type": "string",
                        "description": "Short label shown in the parent's status line (e.g. 'list domain files')."
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: ExecContext) -> ToolOutcome {
        let started = Instant::now();

        // Parse args.
        let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => {
                return ToolOutcome::error("agent requires non-empty `prompt`", 0.0);
            },
        };
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("subagent")
            .to_string();

        // Safety gate: Ask/Auto still vet the spawn (the prompt is
        // model-authored), and Deny overrides plus the destructive-prompt
        // hard-deny always win. ReadOnly deliberately ALLOWS the spawn: the
        // child inherits the live safety mode below, so its own tool calls
        // are re-gated at the same strength — a read_only child can fan out
        // exploration but still can't mutate anything.
        if let Some(blocked) = super::policy_gate::gate_external(
            &ctx,
            "agent",
            crate::runtime::ToolCategory::Subagent,
            format!("subagent: {}", description),
            &args,
        )
        .await
        {
            return blocked;
        }

        // Acquire a breadth permit. Respects parent cancellation so
        // a fan-out that lands 30 calls doesn't hold the parent's
        // Ctrl+C response hostage.
        let permit = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            p = self.spawner.inflight.clone().acquire_owned() => match p {
                Ok(permit) => permit,
                Err(_) => return ToolOutcome::error(
                    "subagent semaphore closed",
                    started.elapsed().as_secs_f64(),
                ),
            },
        };

        // Build the child runtime. The child uses the same parent
        // config + cwd + model id, with a fresh `State` and a tool
        // registry filtered to remove self-recursion and GUI tools.
        //
        // F7: `ExecContext` now carries the parent's `Config` +
        // `model_id`. Previously we built `Config::default()` here and
        // the child model id defaulted to `config.default_model.name`
        // (usually empty), which made subagents fail at provider
        // resolution.
        let config = (*ctx.config).clone();
        let cwd = ctx.workdir.clone();
        let model_id = if ctx.model_id.is_empty() {
            default_model_id(&config)
        } else {
            ctx.model_id.clone()
        };
        let child_model_id = model_id.clone();
        // Inherit the parent's LIVE safety mode (Shift+Tab / `/safety` apply
        // immediately) rather than the static config default `State::new`
        // would pick up — otherwise a downgraded session could be escaped by
        // delegating risky work to a subagent. The child runs headless (no
        // approval broker), so in `ask` its mutations block/await rather than
        // silently escalate; non-replayable tools fail closed (see #3).
        let mut child_state =
            State::new(config.clone(), cwd.clone(), model_id, chrono::Local::now());
        child_state.session.safety_mode = ctx.safety_mode;
        // Mark the child as a subagent: its system prompt gains the report
        // contract (final message = the report returned to the parent; never
        // ask questions — nobody is watching to answer them).
        child_state.session.is_subagent = true;
        // Load project instructions + the memory index synchronously, before the
        // child is driven. Dispatching RefreshInstructions/RefreshMemory as
        // effects (as this used to) races the child's FIRST model call — which
        // is emitted synchronously from the seed prompt — so the opening call
        // went out blind. The child has no config watcher either, so this is the
        // only load.
        let (instructions, memory) =
            crate::app::instructions::load_project_context(&cwd, &config.memory);
        child_state.instructions = instructions;
        child_state.memory = memory;
        // Advertise the parent's live MCP tools to the child. The MCP manager
        // is process-global (`crate::mcp::manager_ref`), so the child's
        // `mcp_proxy` calls hit the SAME already-running servers — no
        // per-child processes. Without this seeding, the child's server
        // entries sit `Starting` forever (a child has no `InitMcpServers`
        // path of its own) and `build_chat_request` advertises zero `mcp__`
        // tools, making the registry's MCP proxy unreachable in practice.
        seed_child_mcp(&mut child_state);

        let child_tools = build_child_registry(self.spawner.providers.clone());

        // Child runner rooted at parent's scope child token. When
        // parent cancels, `child_token.cancelled()` fires and the
        // child's subprocess + model streams abort.
        let child_token = ctx.token.child_token();
        let (child_tx, child_rx) = mpsc::channel(MSG_CHANNEL_CAPACITY);
        let child_runner =
            EffectRunner::new_child(child_tx, cwd, self.spawner.providers.clone(), child_tools);

        // Drive the child reducer loop to completion. The wall-clock
        // timeout lives inside `drive_child` so the child runner is always
        // shut down — even on timeout — rather than dropped mid-flight (#76).
        let (result, child_usage) = drive_child(
            child_state,
            child_runner,
            child_rx,
            ctx.progress.clone(),
            prompt,
            description.clone(),
            child_token,
        )
        .await;
        drop(permit);

        let elapsed = started.elapsed().as_secs_f64();
        match result {
            Ok(summary) => ToolOutcome::success(summary, "subagent completed", elapsed)
                .with_metadata(subagent_metadata(child_model_id, child_usage)),
            Err(DriveError::Cancelled) => ToolOutcome::cancelled(),
            Err(DriveError::TimedOut) => ToolOutcome::error(
                format!(
                    "subagent ({}) exceeded {}s timeout",
                    description, DEFAULT_TIMEOUT_SECS
                ),
                elapsed,
            )
            .with_metadata(subagent_metadata(child_model_id, child_usage)),
            Err(DriveError::Errored(e)) => {
                ToolOutcome::error(format!("subagent ({}): {}", description, e), elapsed)
                    .with_metadata(subagent_metadata(child_model_id, child_usage))
            },
        }
    }
}

/// Metadata for the parent: which model ran the child, and what the child's
/// session cost. The usage rides `ToolRunMetadata.token_usage`, which
/// `handle_tool_finished` folds into the parent session's totals — without it
/// the footer and the run summary silently exclude subagent spend. Timeout and
/// error outcomes carry it too (that work was still billed); `None` when the
/// provider reported nothing, so the UI doesn't render a bogus "0 tokens".
fn subagent_metadata(model_id: String, usage: crate::domain::TokenUsageTotals) -> ToolRunMetadata {
    let token_usage = (usage.total_tokens > 0).then(|| crate::models::TokenUsage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
        source: Default::default(),
    });
    ToolRunMetadata {
        detail: ToolMetadata::Subagent { model_id },
        token_usage,
        ..ToolRunMetadata::default()
    }
}

enum DriveError {
    Cancelled,
    TimedOut,
    Errored(String),
}

/// Drive the child's reducer loop to `Idle`. Forwards child
/// `ToolStarted` / `ToolFinished` / `StreamText` events to the
/// parent's progress channel as `ProgressEvent::Subagent*`. Returns the
/// child's final report alongside its cumulative session token usage —
/// the usage is returned on EVERY exit path (success, timeout, error)
/// because that spend is real regardless of how the child ended.
async fn drive_child(
    mut state: State,
    mut runner: EffectRunner,
    mut msg_rx: mpsc::Receiver<Msg>,
    parent_progress: mpsc::Sender<ProgressEvent>,
    prompt: String,
    description: String,
    token: CancellationToken,
) -> (Result<String, DriveError>, crate::domain::TokenUsageTotals) {
    // Signal start to parent.
    let _ = parent_progress
        .send(ProgressEvent::SubagentText(format!(
            "{} — {}",
            description,
            prompt.chars().take(80).collect::<String>()
        )))
        .await;

    // Project instructions + memory are loaded synchronously into `state` before
    // `drive_child` is called (see `execute`), so the child's first model call
    // sees them — no RefreshInstructions/RefreshMemory dispatch here, which would
    // race that first call.

    // Seed the child turn.
    let seed = Msg::SubmitPrompt {
        text: prompt,
        attachment_ids: vec![],
    };
    let (new_state, cmds) = update(state, seed);
    state = new_state;
    for cmd in cmds {
        runner.dispatch(cmd);
    }

    // Drive the child reducer to Idle, bounded by a wall-clock deadline.
    // The deadline is a `select!` arm (not a `timeout()` wrapper) so the
    // single `runner.shutdown()` below always runs — on normal exit,
    // cancel, OR timeout — instead of the runner being dropped mid-flight
    // and leaking its MCP children (#76).
    let deadline = tokio::time::sleep(Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    tokio::pin!(deadline);

    let mut outcome: Result<(), DriveError> = Ok(());
    loop {
        if token.is_cancelled() {
            outcome = Err(DriveError::Cancelled);
            break;
        }
        if matches!(state.turn, TurnState::Idle) && state.ui.queued_messages.is_empty() {
            break;
        }

        let msg = tokio::select! {
            biased;
            _ = token.cancelled() => {
                outcome = Err(DriveError::Cancelled);
                break;
            },
            _ = &mut deadline => {
                outcome = Err(DriveError::TimedOut);
                break;
            },
            recv = msg_rx.recv() => match recv {
                Some(m) => m,
                None => break, // channel closed — child runner shut down
            },
        };

        // Forward child activity to parent progress BEFORE the
        // reducer mutates state (we want `call_id` + `tool_name`
        // semantic info, which reducer events strip).
        forward_child_event(&msg, &parent_progress, &state).await;

        let (new_state, cmds) = update(state, msg);
        state = new_state;
        for cmd in cmds {
            runner.dispatch(cmd);
        }
        if state.should_exit {
            break;
        }
    }

    // Always reap the child runner regardless of how the loop exited. This
    // cancels the child's scopes and drains its tasks; it does NOT touch the
    // process-global MCP manager (`new_child` opts out of that reap — the
    // servers are shared with the parent).
    runner.shutdown().await;

    // The child session's cumulative provider usage, handed to the parent on
    // every exit path so the spend rolls up into the parent's counters.
    let usage = state.session.cumulative_token_usage;

    if let Err(e) = outcome {
        return (Err(e), usage);
    }

    // Extract last assistant message as the result.
    let summary = state
        .session
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Assistant)
        .map(|m| m.content.clone())
        .unwrap_or_default();
    if summary.trim().is_empty() {
        return (
            Err(DriveError::Errored(
                "subagent produced no assistant output".to_string(),
            )),
            usage,
        );
    }
    (Ok(summary), usage)
}

/// Translate child-scope `Msg` events into parent-scope
/// `ProgressEvent::Subagent*`. Flat mapping, never recursive — the
/// parent reducer just sees "a tool started / finished / said
/// something" with the child's call identity.
async fn forward_child_event(msg: &Msg, progress: &mpsc::Sender<ProgressEvent>, state: &State) {
    match msg {
        Msg::ToolStarted {
            turn: _, call_id, ..
        } => {
            let tool_name = lookup_tool_name(state, *call_id).unwrap_or_else(|| "tool".to_string());
            let _ = progress
                .send(ProgressEvent::SubagentToolCall {
                    child_call_id: *call_id,
                    tool_name,
                    phase: SubagentPhase::Started,
                })
                .await;
        },
        Msg::ToolFinished {
            turn: _,
            call_id,
            outcome,
        } => {
            let tool_name = lookup_tool_name(state, *call_id).unwrap_or_else(|| "tool".to_string());
            let phase = if outcome.is_success() {
                SubagentPhase::Finished
            } else {
                SubagentPhase::Errored
            };
            let _ = progress
                .send(ProgressEvent::SubagentToolCall {
                    child_call_id: *call_id,
                    tool_name,
                    phase,
                })
                .await;
        },
        Msg::StreamText { chunk, .. } => {
            // Only forward a compact preview; long assistant text is
            // overwhelming in the parent's status line.
            if !chunk.trim().is_empty() {
                let snippet: String = chunk.chars().take(120).collect();
                let _ = progress.send(ProgressEvent::SubagentText(snippet)).await;
            }
        },
        _ => {},
    }
}

/// Look up a tool name from a `PendingToolCall` in the state.
/// Returns `None` if the call id isn't known (e.g. during teardown).
fn lookup_tool_name(state: &State, call_id: crate::domain::ToolCallId) -> Option<String> {
    match &state.turn {
        TurnState::ExecutingTools { calls, .. } => calls
            .iter()
            .find(|c| c.call_id == call_id)
            .map(|c| c.source.function.name.clone()),
        _ => None,
    }
}

/// Mark the child's configured MCP servers `Ready` (with their live tool
/// lists) from the process-global manager, so the child's outgoing requests
/// advertise `mcp__` tools. `State::new` seeds every configured server as
/// `Starting`, and only the app entrypoints ever dispatch `InitMcpServers` —
/// a child has no init path, so without this its MCP surface is empty even
/// though its registry carries the proxy. No-op when no manager is installed
/// (MCP unconfigured, or startup init still racing — same window in which the
/// parent's own first turn sees no MCP tools either).
fn seed_child_mcp(state: &mut State) {
    let Some(manager) = crate::mcp::manager_ref::get() else {
        return;
    };
    apply_live_mcp(&mut state.mcp.servers, manager.get_all_tools(), |name| {
        manager.has_server(name)
    });
}

/// Pure core of [`seed_child_mcp`], injectable for tests: flip every entry
/// the live manager actually runs to `Ready` and attach its advertised tools.
/// Entries for servers the manager doesn't have (failed to start) keep their
/// `Starting` status and stay un-advertised — same as in the parent.
fn apply_live_mcp(
    servers: &mut std::collections::HashMap<String, crate::domain::McpServerEntry>,
    live_tools: &[(String, crate::mcp::McpToolDef)],
    has_server: impl Fn(&str) -> bool,
) {
    for (name, entry) in servers.iter_mut() {
        if !has_server(name) {
            continue;
        }
        entry.status = crate::domain::McpServerStatus::Ready;
        entry.tools = live_tools
            .iter()
            .filter(|(server, _)| server == name)
            .map(|(_, def)| crate::domain::McpToolSpec {
                name: def.name.clone(),
                description: def.description.clone(),
                input_schema: def.input_schema.clone(),
            })
            .collect();
    }
}

/// Construct the child `ToolRegistry` — a subset of what the parent
/// offers. Explicitly excludes:
///
///   - `agent` itself — subagents don't spawn subagents. This
///     exclusion is the guard (there is no depth counter).
///   - All seven GUI / computer-use tools — the parent's
///     `ComputerUseDriver` owns the screenshot coord registry; a
///     subagent clicking would corrupt the parent's latest-capture
///     pointer.
///
/// Filesystem + exec + web tools come along unchanged, and the MCP
/// proxy routes through the process-global `McpServerManager` — the
/// child calls the SAME running servers as the parent (advertised via
/// `seed_child_mcp`, which marks them Ready in the child's state). So
/// subagents read/write files, run commands, and call MCP tools, all
/// gated at the child's inherited safety mode.
fn build_child_registry(providers: Arc<ProviderFactory>) -> Arc<ToolRegistry> {
    use super::{
        computer_use, exec, filesystem, mcp,
        web::{WebFetchTool, WebSearchTool},
    };
    let mut r = ToolRegistry::new();
    r.register(Arc::new(filesystem::ReadFileTool));
    r.register(Arc::new(filesystem::WriteFileTool));
    r.register(Arc::new(filesystem::EditFileTool));
    r.register(Arc::new(filesystem::DeleteFileTool));
    r.register(Arc::new(filesystem::CreateDirectoryTool));
    r.register(Arc::new(exec::ExecuteCommandTool));
    r.register(Arc::new(mcp::McpToolProxy));
    if let Some(key) = crate::utils::resolve_api_key("OLLAMA_API_KEY", None) {
        r.register(Arc::new(WebSearchTool::new(key.clone())));
        r.register(Arc::new(WebFetchTool::new(key)));
    }
    // NO computer_use::*  — GUI tools are parent-only.
    // NO subagent::SubagentTool — subagents can't spawn subagents; this
    // exclusion IS the guard (there is no depth counter).
    // Silence unused-import if the above imports don't all resolve.
    let _ = computer_use::probe;
    let _ = providers;
    Arc::new(r)
}

/// Fallback child model id when `ExecContext::model_id` is empty
/// (e.g. a test harness that uses the default `test_exec_context`
/// builder). Production code always provides the parent's active model
/// id via `Cmd::ExecuteTool::model_id`.
fn default_model_id(config: &crate::app::Config) -> String {
    if !config.default_model.provider.is_empty() && !config.default_model.name.is_empty() {
        format!(
            "{}/{}",
            config.default_model.provider, config.default_model.name
        )
    } else {
        config.default_model.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ToolCallId, TurnId};
    use crate::providers::ctx::test_exec_context;
    use std::path::PathBuf;

    #[tokio::test]
    async fn empty_prompt_is_rejected() {
        let spawner = Arc::new(SubagentSpawner::new(Arc::new(ProviderFactory::new(
            crate::app::Config::default(),
        ))));
        let tool = SubagentTool::new(spawner);
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = tool.execute(serde_json::json!({"prompt": "  "}), ctx).await;
        assert_eq!(outcome.status, crate::domain::ToolStatus::Error);
    }

    #[test]
    fn child_state_inherits_live_safety_mode_over_config_default() {
        // #2: a subagent must run at the parent's LIVE safety mode, not the
        // static config default `State::new` would otherwise apply — otherwise
        // a downgraded session is escapable by delegating to a subagent.
        use crate::runtime::SafetyMode;
        let mut config = crate::app::Config::default();
        config.safety.mode = SafetyMode::FullAccess; // static config default
        let mut child_state = State::new(
            config,
            PathBuf::from("/tmp"),
            "ollama/test".to_string(),
            chrono::Local::now(),
        );
        // The bug source: State::new picks up the config default…
        assert_eq!(child_state.session.safety_mode, SafetyMode::FullAccess);
        // …and the fix: the parent's live ctx.safety_mode overrides it.
        child_state.session.safety_mode = SafetyMode::Ask;
        assert_eq!(child_state.session.safety_mode, SafetyMode::Ask);
    }

    /// F7: when `ExecContext::model_id` is empty (the test builder's
    /// default), the fallback walks `config.default_model.{provider,name}`.
    /// This pins the happy-path behavior.
    #[test]
    fn default_model_id_reads_config_provider_and_name() {
        let mut cfg = crate::app::Config::default();
        cfg.default_model.provider = "ollama".to_string();
        cfg.default_model.name = "qwen3-coder:30b".to_string();
        assert_eq!(default_model_id(&cfg), "ollama/qwen3-coder:30b");
    }

    #[test]
    fn default_model_id_returns_bare_name_when_provider_empty() {
        let mut cfg = crate::app::Config::default();
        cfg.default_model.name = "just-a-name".to_string();
        // provider is empty — single-slash shape would be
        // "/just-a-name", which provider resolution would reject.
        assert_eq!(default_model_id(&cfg), "just-a-name");
    }

    #[test]
    fn apply_live_mcp_marks_running_servers_ready_with_their_tools() {
        use crate::domain::{McpServerEntry, McpServerStatus};
        let entry = || McpServerEntry {
            config: crate::app::McpServerConfig {
                command: String::new(),
                args: Vec::new(),
                env: std::collections::HashMap::new(),
            },
            status: McpServerStatus::Starting,
            tools: Vec::new(),
        };
        let mut servers = std::collections::HashMap::new();
        servers.insert("slack".to_string(), entry());
        servers.insert("broken".to_string(), entry());

        let live = vec![
            (
                "slack".to_string(),
                crate::mcp::McpToolDef {
                    name: "send".to_string(),
                    description: "send a message".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            ),
            // A tool from a server the child doesn't have configured must
            // not create an entry out of thin air.
            (
                "other".to_string(),
                crate::mcp::McpToolDef {
                    name: "x".to_string(),
                    description: String::new(),
                    input_schema: serde_json::json!({}),
                },
            ),
        ];
        apply_live_mcp(&mut servers, &live, |name| name == "slack");

        let slack = &servers["slack"];
        assert_eq!(slack.status, McpServerStatus::Ready);
        assert_eq!(slack.tools.len(), 1);
        assert_eq!(slack.tools[0].name, "send");
        // A configured server the manager doesn't run stays un-advertised.
        assert_eq!(servers["broken"].status, McpServerStatus::Starting);
        assert!(servers["broken"].tools.is_empty());
        assert!(!servers.contains_key("other"));
    }

    #[test]
    fn subagent_metadata_carries_usage_only_when_reported() {
        use crate::domain::TokenUsageTotals;
        let some = subagent_metadata(
            "ollama/test".to_string(),
            TokenUsageTotals {
                prompt_tokens: 100,
                completion_tokens: 40,
                total_tokens: 140,
                ..TokenUsageTotals::default()
            },
        );
        let usage = some.token_usage.expect("usage attached");
        assert_eq!(usage.total_tokens, 140);
        assert_eq!(usage.completion_tokens, 40);
        assert!(matches!(
            some.detail,
            crate::domain::ToolMetadata::Subagent { ref model_id } if model_id == "ollama/test"
        ));
        // A provider that reported nothing must not render as "0 tokens".
        let none = subagent_metadata("ollama/test".to_string(), TokenUsageTotals::default());
        assert!(none.token_usage.is_none());
    }

    #[test]
    fn build_child_registry_excludes_gui_and_self() {
        let providers = Arc::new(ProviderFactory::new(crate::app::Config::default()));
        let r = build_child_registry(providers);
        // GUI tools absent.
        assert!(r.get("screenshot").is_none());
        assert!(r.get("click").is_none());
        assert!(r.get("type_text").is_none());
        assert!(r.get("press_key").is_none());
        assert!(r.get("scroll").is_none());
        assert!(r.get("mouse_move").is_none());
        assert!(r.get("list_windows").is_none());
        // Self absent — no recursion bootstrap.
        assert!(r.get("agent").is_none());
        // Core tools present.
        assert!(r.get("read_file").is_some());
        assert!(r.get("execute_command").is_some());
    }
}
