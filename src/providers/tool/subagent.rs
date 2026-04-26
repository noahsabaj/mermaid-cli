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
//!   Depth tracking uses `tokio::task_local!` so nested subagents
//!   (a subagent calling `agent`) can see their own depth without
//!   threading state through `ExecContext`.
//! - `SubagentTool::execute` builds a fresh child `State`, a
//!   filtered `ToolRegistry` (no self-recursion, no GUI tools), and
//!   a child `EffectRunner` + msg channel. It drives the child
//!   reducer to `Idle`, streaming progress back to the parent via
//!   `ProgressEvent::Subagent*`, and returns the last assistant
//!   message as the tool's `output`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc};
use tokio::time::timeout;
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

/// Maximum nesting depth. `agent` calling `agent` calling `agent`
/// works up to this cap; the fourth-level spawn errors cleanly.
pub const MAX_DEPTH: usize = 3;

/// Maximum subagents running simultaneously across the whole process.
/// Covers the pathological "parent emits 30 agent calls in one turn"
/// case. Hit this cap → later calls block on the semaphore until
/// some earlier subagent finishes or cancels.
pub const MAX_INFLIGHT: usize = 10;

/// Hard ceiling on a subagent's wall-clock runtime. Above this the
/// subagent is cancelled and reports `Error`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 20 * 60;

tokio::task_local! {
    /// Current subagent depth. Unset (=0) at the root; incremented
    /// once per `SubagentTool::execute` nesting. Read via
    /// `SUBAGENT_DEPTH.try_with(|d| *d).unwrap_or(0)` (so unset ==
    /// root).
    static SUBAGENT_DEPTH: usize;
}

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
                 Depth-capped at {max_depth}; breadth-capped at {max_breadth} concurrent. \
                 Subagents don't get GUI (screenshot/click/…) access because coordinate \
                 metadata can't be shared cleanly.",
                max_depth = MAX_DEPTH,
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

        // Depth gate: if we're already at the cap, return immediately.
        let current_depth = SUBAGENT_DEPTH.try_with(|d| *d).unwrap_or(0);
        if current_depth >= MAX_DEPTH {
            return ToolOutcome::error(format!("subagent depth limit {} reached", MAX_DEPTH), 0.0);
        }

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
        let child_state = State::new(config.clone(), cwd.clone(), model_id);

        let child_tools = build_child_registry(self.spawner.providers.clone());

        // Child runner rooted at parent's scope child token. When
        // parent cancels, `child_token.cancelled()` fires and the
        // child's subprocess + model streams abort.
        let child_token = ctx.token.child_token();
        let (child_tx, child_rx) = mpsc::channel(MSG_CHANNEL_CAPACITY);
        let child_runner =
            EffectRunner::new_child(child_tx, cwd, self.spawner.providers.clone(), child_tools);

        // Depth-scoped drive: nested `agent` calls inside this child
        // see `current_depth + 1` via `SUBAGENT_DEPTH.try_with`.
        let drive = drive_child(
            child_state,
            child_runner,
            child_rx,
            ctx.progress.clone(),
            prompt,
            description.clone(),
            child_token,
        );
        let depth_scoped = SUBAGENT_DEPTH.scope(current_depth + 1, drive);

        let result = timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS), depth_scoped).await;
        drop(permit);

        let elapsed = started.elapsed().as_secs_f64();
        match result {
            Ok(Ok(summary)) => ToolOutcome::success(summary, "subagent completed", elapsed)
                .with_metadata(subagent_metadata(child_model_id)),
            Ok(Err(DriveError::Cancelled)) => ToolOutcome::cancelled(),
            Ok(Err(DriveError::Errored(e))) => {
                ToolOutcome::error(format!("subagent ({}): {}", description, e), elapsed)
                    .with_metadata(subagent_metadata(child_model_id))
            },
            Err(_) => ToolOutcome::error(
                format!(
                    "subagent ({}) exceeded {}s timeout",
                    description, DEFAULT_TIMEOUT_SECS
                ),
                elapsed,
            )
            .with_metadata(subagent_metadata(child_model_id)),
        }
    }
}

fn subagent_metadata(model_id: String) -> ToolRunMetadata {
    ToolRunMetadata {
        detail: ToolMetadata::Subagent { model_id },
        ..ToolRunMetadata::default()
    }
}

enum DriveError {
    Cancelled,
    Errored(String),
}

/// Drive the child's reducer loop to `Idle`. Forwards child
/// `ToolStarted` / `ToolFinished` / `StreamText` events to the
/// parent's progress channel as `ProgressEvent::Subagent*`.
async fn drive_child(
    mut state: State,
    mut runner: EffectRunner,
    mut msg_rx: mpsc::Receiver<Msg>,
    parent_progress: mpsc::Sender<ProgressEvent>,
    prompt: String,
    description: String,
    token: CancellationToken,
) -> Result<String, DriveError> {
    // Signal start to parent.
    let _ = parent_progress
        .send(ProgressEvent::SubagentText(format!(
            "▶ {} — {}",
            description,
            prompt.chars().take(80).collect::<String>()
        )))
        .await;

    // MERMAID.md instructions — same as the root interactive path.
    runner.dispatch(crate::domain::Cmd::RefreshInstructions);

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

    // Loop until the child reducer reaches Idle with no queued work.
    loop {
        if token.is_cancelled() {
            runner.shutdown().await;
            return Err(DriveError::Cancelled);
        }
        if matches!(state.turn, TurnState::Idle) && state.ui.queued_messages.is_empty() {
            break;
        }

        let msg = tokio::select! {
            biased;
            _ = token.cancelled() => {
                runner.shutdown().await;
                return Err(DriveError::Cancelled);
            },
            recv = msg_rx.recv() => match recv {
                Some(m) => m,
                None => {
                    // Channel closed — child runner shut down.
                    break;
                },
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

    runner.shutdown().await;

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
        return Err(DriveError::Errored(
            "subagent produced no assistant output".to_string(),
        ));
    }
    Ok(summary)
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

/// Construct the child `ToolRegistry` — a subset of what the parent
/// offers. Explicitly excludes:
///
///   - `agent` itself — depth cap would catch it but excluding up
///     front saves a wasted call.
///   - All seven GUI / computer-use tools — the parent's
///     `ComputerUseDriver` owns the screenshot coord registry; a
///     subagent clicking would corrupt the parent's latest-capture
///     pointer.
///
/// Filesystem + exec + web + MCP tools come along unchanged. That
/// lets subagents read/write files, run commands, and call MCP tools
/// for their work.
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
    // NO subagent::SubagentTool — depth cap would catch it.
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
    async fn depth_cap_rejects_when_at_max() {
        let spawner = Arc::new(SubagentSpawner::new(Arc::new(ProviderFactory::new(
            crate::app::Config::default(),
        ))));
        let tool = SubagentTool::new(spawner);
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));

        let outcome = SUBAGENT_DEPTH
            .scope(
                MAX_DEPTH,
                tool.execute(serde_json::json!({"prompt": "hi"}), ctx),
            )
            .await;
        let error = outcome.error_message().expect("expected error");
        assert!(
            error.contains("depth limit"),
            "expected depth-limit error, got: {}",
            error
        );
    }

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
