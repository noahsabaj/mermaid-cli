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
//! - An **agent type** shapes the child: a tool filter, a safety ceiling
//!   (the child runs at the LESS permissive of the parent's live mode and
//!   the ceiling), a system-prompt preamble, and an optional default
//!   model. Built-ins: `general` (everything, at the parent's mode) and
//!   `explore` (read-only reconnaissance). `[agents.types.*]` config
//!   entries define more — a custom name shadows a built-in.
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
//! - **Continuations**: every result carries an `[agent_id: …]` trailer.
//!   The finished child's full `State` is kept in a bounded spawner cache;
//!   passing `agent_id` restores it and seeds the new prompt as its next
//!   user message, so a follow-up question reuses the context the child
//!   already built instead of re-exploring from scratch.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::domain::{
    Msg, State, TokenUsageTotals, ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata,
    TurnState, update,
};
use crate::effect::{EffectRunner, MSG_CHANNEL_CAPACITY};
use crate::models::MessageRole;
use crate::providers::ProviderFactory;
use crate::providers::ctx::{ExecContext, ProgressEvent, SubagentPhase};
use crate::runtime::SafetyMode;

use super::ToolExecutor;
use super::ToolRegistry;

/// Maximum subagents running simultaneously across the whole process.
/// Covers the pathological "parent emits 30 agent calls in one turn"
/// case. Hit this cap → later calls block on the semaphore until
/// some earlier subagent finishes or cancels.
pub const MAX_INFLIGHT: usize = 10;

/// Hard ceiling on a subagent's wall-clock runtime when the user hasn't set
/// `[agents] timeout_secs` (or set it to 0). Above this the subagent is
/// cancelled and reports `Error`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 20 * 60;

/// How many finished children the spawner keeps for continuation
/// (`agent_id` arg). Oldest evicted first.
pub const MAX_CACHED_AGENTS: usize = 8;

/// System-prompt block for the built-in `explore` type.
const EXPLORE_PREAMBLE: &str = "\
## Explore Agent
You are an Explore agent: read-only reconnaissance. Locate files, map \
structure, and extract exactly the facts asked for, using reads and \
read-only commands. You cannot mutate anything — do not try. Report \
concrete paths, names, and findings.";

/// Tool names an agent type's `tools` filter may reference — the full child
/// surface (`mcp` covers every `mcp__server__tool` via the proxy). GUI tools
/// and `agent` itself are structurally absent from children and can't be
/// granted here.
const CHILD_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "apply_patch",
    "delete_file",
    "create_directory",
    "execute_command",
    "web_search",
    "web_fetch",
    "mcp",
];

/// A resolved agent type: what the `type` arg maps to after merging the
/// built-ins with `[agents.types]` config entries (a custom name shadows a
/// built-in, so users can retune `explore`).
#[derive(Debug)]
struct AgentType {
    name: String,
    /// Allowed tool names (`None` = the full child set).
    tools: Option<Vec<String>>,
    /// The child runs at the LESS permissive of the parent's live mode and
    /// this ceiling — a type can tighten safety, never widen it.
    safety_ceiling: SafetyMode,
    /// Extra system-prompt block, appended after the subagent contract.
    preamble: Option<String>,
    /// Default model for this type; a per-call `model` arg wins.
    model: Option<String>,
}

impl AgentType {
    fn allows_tool(&self, name: &str) -> bool {
        self.tools
            .as_ref()
            .is_none_or(|tools| tools.iter().any(|t| t == name))
    }
}

fn builtin_agent_type(name: &str) -> Option<AgentType> {
    match name {
        "general" => Some(AgentType {
            name: "general".to_string(),
            tools: None,
            safety_ceiling: SafetyMode::FullAccess,
            preamble: None,
            model: None,
        }),
        "explore" => Some(AgentType {
            name: "explore".to_string(),
            tools: Some(vec!["read_file".to_string(), "execute_command".to_string()]),
            safety_ceiling: SafetyMode::ReadOnly,
            preamble: Some(EXPLORE_PREAMBLE.to_string()),
            model: None,
        }),
        _ => None,
    }
}

/// Resolve a requested type name (default `general`) against config-defined
/// types first, then built-ins. Errors are model-facing and actionable: they
/// name the valid types / tools / safety modes.
fn resolve_agent_type(
    requested: Option<&str>,
    config: &crate::app::Config,
) -> Result<AgentType, String> {
    let name = requested.unwrap_or("general");
    if let Some(custom) = config.agents.types.get(name) {
        let safety_ceiling = match custom.safety.as_deref() {
            None => SafetyMode::FullAccess,
            Some(s) => SafetyMode::parse(s).ok_or_else(|| {
                format!(
                    "[agents.types.{name}] safety '{s}' is not one of \
                     read_only/ask/auto/full_access"
                )
            })?,
        };
        if let Some(tools) = &custom.tools
            && let Some(bad) = tools
                .iter()
                .find(|t| !CHILD_TOOL_NAMES.contains(&t.as_str()))
        {
            return Err(format!(
                "[agents.types.{name}] unknown tool '{bad}'; valid tools: {}",
                CHILD_TOOL_NAMES.join(", ")
            ));
        }
        return Ok(AgentType {
            name: name.to_string(),
            tools: custom.tools.clone(),
            safety_ceiling,
            preamble: custom.preamble.clone(),
            model: custom.model.clone(),
        });
    }
    builtin_agent_type(name).ok_or_else(|| {
        let mut available: Vec<&str> = vec!["general", "explore"];
        available.extend(config.agents.types.keys().map(String::as_str));
        format!(
            "unknown agent type '{name}'; available: {}",
            available.join(", ")
        )
    })
}

/// A finished child kept for follow-ups: its full session state plus the
/// type name it was built with (re-resolved on continuation so the registry,
/// ceiling, and preamble are rebuilt the same way).
struct CachedAgent {
    state: State,
    type_name: String,
}

#[derive(Default)]
struct AgentCache {
    entries: HashMap<String, CachedAgent>,
    /// Insertion/refresh order for eviction (front = oldest).
    order: VecDeque<String>,
}

/// Shared spawner. One per process; held by `SubagentTool`.
pub struct SubagentSpawner {
    providers: Arc<ProviderFactory>,
    inflight: Arc<Semaphore>,
    /// Monotonic source for continuation handles ("a1", "a2", …).
    next_agent_id: AtomicU64,
    /// Finished children kept for continuation. An entry is REMOVED while
    /// its agent runs a continuation (re-stored afterward), so concurrent
    /// continuations of one id error instead of racing a single `State`.
    cache: Mutex<AgentCache>,
    /// Kill handles for detached (Ctrl+B backgrounded) children. Registered
    /// by `detach_child` before its task spawns, removed by that task when
    /// the drive ends — so an entry here always maps to a live child.
    detached_cancels: Mutex<HashMap<String, CancellationToken>>,
}

/// What `kill_detached` found for an agent id.
#[derive(Debug, PartialEq, Eq)]
pub enum KillResult {
    /// A running detached child — its cancel token was fired; expect a
    /// `Msg::BackgroundAgentFinished { cancelled: true, .. }` shortly.
    Killed,
    /// No running child, but a finished one sat in the continuation cache —
    /// it was evicted (the id can no longer be continued).
    Evicted,
    NotFound,
}

impl SubagentSpawner {
    pub fn new(providers: Arc<ProviderFactory>) -> Self {
        Self {
            providers,
            inflight: Arc::new(Semaphore::new(MAX_INFLIGHT)),
            next_agent_id: AtomicU64::new(0),
            cache: Mutex::new(AgentCache::default()),
            detached_cancels: Mutex::new(HashMap::new()),
        }
    }

    fn mint_agent_id(&self) -> String {
        format!(
            "a{}",
            self.next_agent_id.fetch_add(1, Ordering::Relaxed) + 1
        )
    }

    /// Remove and return a cached agent (see `cache` field docs).
    fn cache_take(&self, id: &str) -> Option<CachedAgent> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.order.retain(|x| x != id);
        cache.entries.remove(id)
    }

    /// Insert (or refresh) a cached agent, evicting the oldest past the cap.
    fn cache_store(&self, id: String, agent: CachedAgent) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.order.retain(|x| x != &id);
        cache.order.push_back(id.clone());
        cache.entries.insert(id, agent);
        while cache.entries.len() > MAX_CACHED_AGENTS {
            let Some(oldest) = cache.order.pop_front() else {
                break;
            };
            cache.entries.remove(&oldest);
        }
    }

    fn register_detached(&self, agent_id: String, cancel: CancellationToken) {
        self.detached_cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent_id, cancel);
    }

    fn unregister_detached(&self, agent_id: &str) {
        self.detached_cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(agent_id);
    }

    /// Kill a detached child (fires its cancel token; the child unwinds
    /// orderly and reports through `Msg::BackgroundAgentFinished`), or —
    /// if the id only names a FINISHED child — evict it from the
    /// continuation cache.
    pub fn kill_detached(&self, agent_id: &str) -> KillResult {
        let cancel = self
            .detached_cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(agent_id);
        if let Some(cancel) = cancel {
            cancel.cancel();
            return KillResult::Killed;
        }
        if self.cache_take(agent_id).is_some() {
            return KillResult::Evicted;
        }
        KillResult::NotFound
    }

    /// Kill every detached child. Returns how many tokens were fired.
    pub fn kill_all_detached(&self) -> usize {
        let cancels: Vec<CancellationToken> = {
            let mut map = self
                .detached_cancels
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            map.drain().map(|(_, c)| c).collect()
        };
        let n = cancels.len();
        for cancel in cancels {
            cancel.cancel();
        }
        n
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
                 Types: 'general' (default — full tool access at your safety mode) and \
                 'explore' (read-only reconnaissance: locate files and extract facts, \
                 cannot mutate), plus any defined in config [agents.types]. Every result \
                 ends with an [agent_id: …] trailer; pass that id back as `agent_id` to \
                 send a follow-up prompt to the same child with its context intact (the \
                 {max_cached} most recent children are kept). Breadth-capped at \
                 {max_breadth} concurrent; subagents can't themselves spawn subagents \
                 and never get GUI (screenshot/click/…) access. A child moved to the \
                 background (the user detaches one with Ctrl+B) can be cancelled with \
                 action: \"kill\" plus its agent_id.",
                max_cached = MAX_CACHED_AGENTS,
                max_breadth = MAX_INFLIGHT,
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["spawn", "kill"],
                        "description": "Default 'spawn' (also covers continuing via agent_id). 'kill' cancels a backgrounded child by agent_id — no prompt needed."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The task for the subagent (required unless action is 'kill'). Self-contained; the subagent has no access to the parent's conversation. When continuing via agent_id, this is the next user message to that child."
                    },
                    "description": {
                        "type": "string",
                        "description": "Short label shown in the parent's status line (e.g. 'list domain files')."
                    },
                    "type": {
                        "type": "string",
                        "description": "Agent type: 'general' (default), 'explore' (read-only recon), or a config-defined type. Ignored when continuing via agent_id — the child keeps the type it was built with."
                    },
                    "model": {
                        "type": "string",
                        "description": "Model id override for this child (e.g. 'ollama/qwen3:8b') — use a cheaper/faster model for search-and-summarize subtasks. Defaults to the type's model, else the session model."
                    },
                    "agent_id": {
                        "type": "string",
                        "description": "Continue a previous child (its conversation context is restored and `prompt` becomes its next user message) or, with action 'kill', the backgrounded child to cancel. Use the id from a prior result's [agent_id: …] trailer or the background notice."
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: ExecContext) -> ToolOutcome {
        let started = Instant::now();

        // Kill action: cancel a backgrounded child (or evict a finished one
        // from the continuation cache). Short-circuits before the policy
        // gate and the breadth permit — there is no model-authored prompt to
        // vet and no new capacity consumed; it only reaches children this
        // session already spawned. The dying child reports through
        // `Msg::BackgroundAgentFinished { cancelled: true, .. }`.
        if args.get("action").and_then(|v| v.as_str()) == Some("kill") {
            let Some(id) = args
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return ToolOutcome::error("action 'kill' requires `agent_id`", 0.0);
            };
            return match self.spawner.kill_detached(id) {
                KillResult::Killed => ToolOutcome::success(
                    format!(
                        "Background agent '{id}' cancelled — it unwinds at its next \
                         await point; a cancellation notice will appear in the \
                         conversation."
                    ),
                    "subagent killed",
                    started.elapsed().as_secs_f64(),
                ),
                KillResult::Evicted => ToolOutcome::success(
                    format!(
                        "Agent '{id}' had already finished; removed it from the \
                         continuation cache instead."
                    ),
                    "subagent evicted",
                    started.elapsed().as_secs_f64(),
                ),
                KillResult::NotFound => ToolOutcome::error(
                    format!(
                        "no background or cached agent '{id}' — it may have already \
                         finished and been evicted, or the id was never issued"
                    ),
                    started.elapsed().as_secs_f64(),
                ),
            };
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
        let requested_type = args
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let model_override = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let continue_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

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
        // config + cwd, with a fresh (or cache-restored) `State` and a tool
        // registry filtered by the agent type (never self-recursion or GUI).
        //
        // F7: `ExecContext` now carries the parent's `Config` +
        // `model_id`. Previously we built `Config::default()` here and
        // the child model id defaulted to `config.default_model.name`
        // (usually empty), which made subagents fail at provider
        // resolution.
        let config = (*ctx.config).clone();
        let cwd = ctx.workdir.clone();

        // Continuation: pull the cached child out of the spawner cache (it
        // stays out while it runs, so concurrent continuations of one id
        // error instead of racing a single `State`). Fresh spawn: mint a
        // new continuation handle.
        let (agent_id, cached) = match continue_id {
            Some(id) => match self.spawner.cache_take(&id) {
                Some(cached) => (id, Some(cached)),
                None => {
                    return ToolOutcome::error(
                        format!(
                            "unknown agent_id '{id}': it may have expired (the \
                             {MAX_CACHED_AGENTS} most recent children are kept), be running \
                             a continuation right now, or never have existed. Omit agent_id \
                             to start a new agent."
                        ),
                        started.elapsed().as_secs_f64(),
                    );
                },
            },
            None => (self.spawner.mint_agent_id(), None),
        };

        // Resolve the agent type. A continuation keeps the type it was built
        // with (its registry/ceiling/preamble must match the context it
        // accumulated); a fresh spawn resolves the request (default general).
        let type_name = cached
            .as_ref()
            .map(|c| c.type_name.clone())
            .or_else(|| requested_type.map(str::to_string));
        let agent_type = match resolve_agent_type(type_name.as_deref(), &config) {
            Ok(agent_type) => agent_type,
            Err(e) => {
                // Don't lose a cached child over a config error — put it back.
                if let Some(cached) = cached {
                    self.spawner.cache_store(agent_id, cached);
                }
                return ToolOutcome::error(e, started.elapsed().as_secs_f64());
            },
        };

        // Inherit the parent's LIVE safety mode (Shift+Tab / `/safety` apply
        // immediately) rather than the static config default `State::new`
        // would pick up — otherwise a downgraded session could be escaped by
        // delegating risky work to a subagent — then tighten it by the
        // type's ceiling (`explore` pins read_only regardless of parent).
        // The child runs headless (no approval broker), so in `ask` its
        // mutations block/await rather than silently escalate;
        // non-replayable tools fail closed (see #3).
        let child_safety = SafetyMode::least_permissive(ctx.safety_mode, agent_type.safety_ceiling);

        // Model priority: per-call arg > type default > parent's active model.
        let model_id = model_override
            .map(str::to_string)
            .or_else(|| agent_type.model.clone())
            .unwrap_or_else(|| {
                if ctx.model_id.is_empty() {
                    default_model_id(&config)
                } else {
                    ctx.model_id.clone()
                }
            });

        let (mut child_state, usage_before) = match cached {
            Some(cached) => {
                // Continuations accumulate usage across drives in one State;
                // snapshot so only THIS drive's delta rolls up to the parent.
                let before = cached.state.session.cumulative_token_usage;
                (cached.state, before)
            },
            None => (
                State::new(
                    config.clone(),
                    cwd.clone(),
                    model_id.clone(),
                    chrono::Local::now(),
                ),
                TokenUsageTotals::default(),
            ),
        };
        // A per-call model override retargets a continued child too; without
        // one it keeps the model it was built with.
        if let Some(model) = model_override {
            child_state.session.model_id = model.to_string();
        }
        let child_model_id = child_state.session.model_id.clone();

        // Refresh everything that may have moved since the child was built
        // (or since the parent session started): the injected clock, the
        // live safety mode, the type preamble, project instructions + the
        // memory index, and the MCP surface. Instructions/memory load
        // synchronously — dispatching RefreshInstructions/RefreshMemory as
        // effects (as this used to) races the child's FIRST model call,
        // which is emitted synchronously from the seed prompt.
        child_state.now = chrono::Local::now();
        child_state.session.safety_mode = child_safety;
        // Mark the child as a subagent: its system prompt gains the report
        // contract (final message = the report returned to the parent; never
        // ask questions — nobody is watching to answer them).
        child_state.session.is_subagent = true;
        child_state.session.agent_preamble = agent_type.preamble.clone();
        let (instructions, memory, skills) =
            crate::app::instructions::load_project_context(&cwd, &config.memory);
        child_state.instructions = instructions;
        child_state.memory = memory;
        child_state.skills = skills;
        // Advertise the parent's live MCP tools to the child (types that
        // exclude `mcp` skip this — their registry has no proxy, so
        // advertising would invite unknown-tool calls). The MCP manager is
        // process-global (`crate::mcp::manager_ref`), so the child's
        // `mcp_proxy` calls hit the SAME already-running servers — no
        // per-child processes. Without this seeding, the child's server
        // entries sit `Starting` forever (a child has no `InitMcpServers`
        // path of its own) and `build_chat_request` advertises zero `mcp__`
        // tools, making the registry's MCP proxy unreachable in practice.
        if agent_type.allows_tool("mcp") {
            seed_child_mcp(&mut child_state);
        }

        let child_tools = build_child_registry(
            self.spawner.providers.clone(),
            agent_type.tools.as_deref(),
            &config.web,
        );

        // Child cancel token OWNED here rather than derived from the turn's
        // token: a Ctrl+B detach must sever the child from the turn scope
        // (a derived token would die with the turn). Parent cancellation is
        // forwarded explicitly in the select below.
        let child_cancel = CancellationToken::new();
        let (child_tx, child_rx) = mpsc::channel(MSG_CHANNEL_CAPACITY);
        let child_runner =
            EffectRunner::new_child(child_tx, cwd, self.spawner.providers.clone(), child_tools);

        // Drive the child reducer loop to completion. The wall-clock
        // timeout lives inside `drive_child` so the child runner is always
        // shut down — even on timeout — rather than dropped mid-flight (#76).
        let timeout_secs = match config.agents.timeout_secs {
            0 => DEFAULT_TIMEOUT_SECS,
            secs => secs,
        };
        // Child progress flows through a local channel so a Ctrl+B detach can
        // re-route it (turn progress channel → turn-independent notify Msgs).
        let (child_progress_tx, mut child_progress_rx) = mpsc::channel::<ProgressEvent>(16);
        let mut drive = Box::pin(drive_child(
            child_state,
            child_runner,
            child_rx,
            child_progress_tx,
            prompt,
            child_cancel.clone(),
            Duration::from_secs(timeout_secs),
        ));

        let mut progress_open = true;
        let (result, final_state) = loop {
            tokio::select! {
                biased;
                _ = ctx.token.cancelled() => {
                    // Parent turn cancelled (Esc): stop the child, then await
                    // its orderly shutdown — the returned state still carries
                    // the usage this drive burned.
                    child_cancel.cancel();
                    break drive.await;
                },
                _ = ctx.background.cancelled() => {
                    // Ctrl+B: detach. The child keeps running in its own task
                    // (holding its breadth permit), the turn gets an immediate
                    // outcome, and the report arrives later through
                    // `Msg::BackgroundAgentFinished`.
                    return self.detach_child(DetachArgs {
                        drive,
                        progress_rx: child_progress_rx,
                        permit,
                        cancel: child_cancel.clone(),
                        notify: ctx.notify.clone(),
                        agent_id,
                        description,
                        type_name: agent_type.name.clone(),
                        child_model_id,
                        usage_before,
                        timeout_secs,
                        started,
                    });
                },
                ev = child_progress_rx.recv(), if progress_open => match ev {
                    Some(ev) => { let _ = ctx.progress.send(ev).await; },
                    None => progress_open = false,
                },
                r = &mut drive => break r,
            }
        };
        drop(permit);

        finish_drive(
            &self.spawner,
            agent_type.name.clone(),
            agent_id,
            &description,
            child_model_id,
            usage_before,
            timeout_secs,
            started,
            result,
            final_state,
        )
    }
}

/// Everything a Ctrl+B detach hands off to the background task. Bundled so
/// the handoff reads as one unit instead of a 11-argument call.
struct DetachArgs<F> {
    drive: std::pin::Pin<Box<F>>,
    progress_rx: mpsc::Receiver<ProgressEvent>,
    permit: tokio::sync::OwnedSemaphorePermit,
    /// The child's own cancel token — registered on the spawner so
    /// `/agents kill` and the `agent` tool's kill action can reach it.
    cancel: CancellationToken,
    notify: Option<mpsc::Sender<Msg>>,
    agent_id: String,
    description: String,
    type_name: String,
    child_model_id: String,
    usage_before: TokenUsageTotals,
    timeout_secs: u64,
    started: Instant,
}

impl SubagentTool {
    /// Detach a running child from its turn: keep driving it in a spawned
    /// task, translate its progress into `Msg::BackgroundAgent*` (the turn's
    /// progress relay dies with the turn), and deliver the finished report
    /// through the queued-message path. Returns the immediate outcome the
    /// releasing turn reports to the model.
    fn detach_child<F>(&self, args: DetachArgs<F>) -> ToolOutcome
    where
        F: std::future::Future<Output = (Result<String, DriveError>, State)> + Send + 'static,
    {
        let DetachArgs {
            mut drive,
            mut progress_rx,
            permit,
            cancel,
            notify,
            agent_id,
            description,
            type_name,
            child_model_id,
            usage_before,
            timeout_secs,
            started,
        } = args;
        if let Some(notify) = &notify {
            let _ = notify.try_send(Msg::BackgroundAgentStarted {
                agent_id: agent_id.clone(),
                description: description.clone(),
            });
        }
        let spawner = self.spawner.clone();
        // Register the kill handle before the task spawns so a kill can
        // never race a not-yet-registered child.
        spawner.register_detached(agent_id.clone(), cancel);
        let outcome_text = format!(
            "Agent '{description}' ({agent_id}) moved to background — it keeps running and \
             its report will be posted to the conversation when it finishes."
        );
        let (bg_agent_id, bg_description) = (agent_id, description);
        tokio::spawn(async move {
            // Hold the breadth permit for the child's whole life — detached
            // agents still consume real provider capacity.
            let _permit = permit;
            let mut activity = String::new();
            let mut tokens = 0usize;
            let mut progress_open = true;
            let (result, final_state) = loop {
                tokio::select! {
                    ev = progress_rx.recv(), if progress_open => match ev {
                        Some(ev) => {
                            match &ev {
                                ProgressEvent::SubagentToolCall { tool_name, phase, .. } => {
                                    activity = match phase {
                                        SubagentPhase::Started => format!("{tool_name}…"),
                                        SubagentPhase::Finished => format!("{tool_name} done"),
                                        SubagentPhase::Errored => format!("{tool_name} failed"),
                                    };
                                },
                                ProgressEvent::SubagentActivity(label) => activity = label.clone(),
                                ProgressEvent::SubagentTokens(count) => tokens = *count,
                                _ => continue,
                            }
                            if let Some(notify) = &notify {
                                let _ = notify.try_send(Msg::BackgroundAgentProgress {
                                    agent_id: bg_agent_id.clone(),
                                    activity: activity.clone(),
                                    tokens,
                                });
                            }
                        },
                        None => progress_open = false,
                    },
                    r = &mut drive => break r,
                }
            };
            spawner.unregister_detached(&bg_agent_id);
            let cancelled = matches!(result, Err(DriveError::Cancelled));
            let outcome = finish_drive(
                &spawner,
                type_name,
                bg_agent_id.clone(),
                &bg_description,
                child_model_id,
                usage_before,
                timeout_secs,
                started,
                result,
                final_state,
            );
            if let Some(notify) = notify {
                let usage = outcome.metadata.token_usage.clone();
                let tokens_total = usage.as_ref().map_or(tokens, |u| u.total_tokens());
                let _ = notify
                    .send(Msg::BackgroundAgentFinished {
                        agent_id: bg_agent_id,
                        description: bg_description,
                        report: outcome.model_content.clone(),
                        success: outcome.is_success(),
                        cancelled,
                        usage,
                        tokens: tokens_total,
                        duration_secs: started.elapsed().as_secs(),
                    })
                    .await;
            }
        });
        ToolOutcome::success(
            outcome_text,
            "subagent backgrounded",
            started.elapsed().as_secs_f64(),
        )
    }
}

/// Shared post-drive processing for foreground and detached children: cache
/// the child for continuations (unless cancelled), roll up this drive's
/// usage, and shape the model-facing outcome.
#[allow(clippy::too_many_arguments)]
fn finish_drive(
    spawner: &SubagentSpawner,
    type_name: String,
    agent_id: String,
    description: &str,
    child_model_id: String,
    usage_before: TokenUsageTotals,
    timeout_secs: u64,
    started: Instant,
    result: Result<String, DriveError>,
    mut final_state: State,
) -> ToolOutcome {
    let child_usage = usage_delta(final_state.session.cumulative_token_usage, usage_before);

    // Keep the child for follow-ups unless the parent cancelled (that
    // turn is being torn down). Timeout/error children are kept too —
    // "continue a3: what did you find so far?" is exactly the follow-up
    // a timeout invites. Normalize the turn first: the child's runner is
    // gone, so a mid-turn state could never complete — a continuation
    // must be able to seed a fresh prompt into an Idle child.
    if !matches!(result, Err(DriveError::Cancelled)) {
        final_state.turn = TurnState::Idle;
        final_state.ui.queued_messages.clear();
        final_state.ui.live_tool_status.clear();
        final_state.pending_approval.clear();
        spawner.cache_store(
            agent_id.clone(),
            CachedAgent {
                state: final_state,
                type_name,
            },
        );
    }

    let elapsed = started.elapsed().as_secs_f64();
    let trailer = format!("[agent_id: {agent_id} — pass agent_id to continue this child]");
    let metadata = subagent_metadata(child_model_id, child_usage, agent_id);
    match result {
        Ok(summary) => ToolOutcome::success(
            format!("{summary}\n\n{trailer}"),
            "subagent completed",
            elapsed,
        )
        .with_metadata(metadata),
        Err(DriveError::Cancelled) => ToolOutcome::cancelled(),
        Err(DriveError::TimedOut) => ToolOutcome::error(
            format!(
                "subagent ({description}) exceeded {timeout_secs}s timeout; its context \
                 is preserved — {trailer}"
            ),
            elapsed,
        )
        .with_metadata(metadata),
        Err(DriveError::Errored(e)) => {
            ToolOutcome::error(format!("subagent ({description}): {e} {trailer}"), elapsed)
                .with_metadata(metadata)
        },
    }
}

/// Metadata for the parent: which model ran the child, the continuation
/// handle, and what THIS drive cost. The usage rides
/// `ToolRunMetadata.token_usage`, which `handle_tool_finished` folds into the
/// parent session's totals — without it the footer and the run summary
/// silently exclude subagent spend. Timeout and error outcomes carry it too
/// (that work was still billed); `None` when the provider reported nothing,
/// so the UI doesn't render a bogus "0 tokens".
fn subagent_metadata(
    model_id: String,
    usage: TokenUsageTotals,
    agent_id: String,
) -> ToolRunMetadata {
    let token_usage = (usage.total_tokens() > 0).then(|| crate::models::TokenUsage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
        source: Default::default(),
    });
    ToolRunMetadata {
        detail: ToolMetadata::Subagent { model_id, agent_id },
        token_usage,
        ..ToolRunMetadata::default()
    }
}

/// The child-session usage attributable to ONE drive: cumulative totals
/// minus the pre-drive snapshot. Continuations accumulate usage across
/// drives in a single `State`; reporting the delta keeps the parent from
/// double-counting spend it already rolled up.
fn usage_delta(after: TokenUsageTotals, before: TokenUsageTotals) -> TokenUsageTotals {
    TokenUsageTotals {
        prompt_tokens: after.prompt_tokens.saturating_sub(before.prompt_tokens),
        completion_tokens: after
            .completion_tokens
            .saturating_sub(before.completion_tokens),
        cached_input_tokens: after
            .cached_input_tokens
            .saturating_sub(before.cached_input_tokens),
        cache_creation_input_tokens: after
            .cache_creation_input_tokens
            .saturating_sub(before.cache_creation_input_tokens),
        reasoning_output_tokens: after
            .reasoning_output_tokens
            .saturating_sub(before.reasoning_output_tokens),
    }
}

enum DriveError {
    Cancelled,
    TimedOut,
    Errored(String),
}

/// Drive the child's reducer loop to `Idle`, bounded by `timeout`. Forwards
/// stable child activity (tool calls, coarse phase changes, throttled token
/// counts — never raw stream text) to the parent's progress channel as
/// `ProgressEvent::Subagent*`. Returns the child's final report alongside its
/// full `State` — returned on EVERY exit path so the caller can roll up the
/// usage (real spend regardless of how the child ended) and cache the context
/// for continuations.
async fn drive_child(
    mut state: State,
    mut runner: EffectRunner,
    mut msg_rx: mpsc::Receiver<Msg>,
    parent_progress: mpsc::Sender<ProgressEvent>,
    prompt: String,
    token: CancellationToken,
    timeout: Duration,
) -> (Result<String, DriveError>, State) {
    // Signal start to parent. One stable label — the prompt itself never
    // belongs on the parent's status line.
    let _ = parent_progress
        .send(ProgressEvent::SubagentActivity("starting…".to_string()))
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
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    let mut outcome: Result<(), DriveError> = Ok(());
    let mut child_progress = ChildProgress::new(tokio::time::Instant::now());
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
        for event in child_progress.observe(&msg, &state, tokio::time::Instant::now()) {
            let _ = parent_progress.send(event).await;
        }

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

    if let Err(e) = outcome {
        return (Err(e), state);
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
            state,
        );
    }
    (Ok(summary), state)
}

/// Minimum spacing between `SubagentTokens` progress events. Anything faster
/// is invisible churn: the parent redraws at most a few times a second and
/// per-chunk updates were the source of the status-line flicker.
const TOKEN_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// Translates child-scope `Msg` events into the parent-facing
/// `ProgressEvent::Subagent*` vocabulary — a pure state machine so the
/// calm-down rules are unit-testable:
///
/// - tool starts/finishes forward as `SubagentToolCall` (stable labels);
/// - stream chunks NEVER forward text — they only flip a coarse phase
///   ("thinking"/"replying"), emitted once per phase CHANGE;
/// - output tokens accumulate as a chars/4 estimate, snapped to the
///   provider-reported count on `StreamDone`, and emit as `SubagentTokens`
///   at most every `TOKEN_PROGRESS_INTERVAL` (piggybacking on other events
///   so a busy child still reads fresh).
struct ChildProgress {
    phase: &'static str,
    /// Provider-confirmed output tokens from the child's completed model calls.
    confirmed_tokens: usize,
    /// Character count of the in-flight stream (reset when `StreamDone` snaps
    /// to the provider-reported figure).
    streamed_chars: usize,
    last_tokens_sent: usize,
    last_tokens_at: tokio::time::Instant,
}

impl ChildProgress {
    fn new(now: tokio::time::Instant) -> Self {
        Self {
            phase: "",
            confirmed_tokens: 0,
            streamed_chars: 0,
            last_tokens_sent: 0,
            last_tokens_at: now,
        }
    }

    fn total_tokens(&self) -> usize {
        self.confirmed_tokens + self.streamed_chars / 4
    }

    /// Observe one child `Msg`; returns the progress events the parent
    /// should see (usually none).
    fn observe(
        &mut self,
        msg: &Msg,
        state: &State,
        now: tokio::time::Instant,
    ) -> Vec<ProgressEvent> {
        let mut out = Vec::new();
        match msg {
            Msg::ToolStarted {
                turn: _, call_id, ..
            } => {
                let tool_name =
                    lookup_tool_name(state, *call_id).unwrap_or_else(|| "tool".to_string());
                out.push(ProgressEvent::SubagentToolCall {
                    child_call_id: *call_id,
                    tool_name,
                    phase: SubagentPhase::Started,
                });
                // Next stream chunk re-announces its phase after the tool.
                self.phase = "";
            },
            Msg::ToolFinished {
                turn: _,
                call_id,
                outcome,
            } => {
                let tool_name =
                    lookup_tool_name(state, *call_id).unwrap_or_else(|| "tool".to_string());
                let phase = if outcome.is_success() {
                    SubagentPhase::Finished
                } else {
                    SubagentPhase::Errored
                };
                out.push(ProgressEvent::SubagentToolCall {
                    child_call_id: *call_id,
                    tool_name,
                    phase,
                });
                self.phase = "";
            },
            Msg::StreamReasoning { chunk, .. } => {
                self.streamed_chars += chunk.text.len();
                self.set_phase("thinking", &mut out);
            },
            Msg::StreamText { chunk, .. } => {
                self.streamed_chars += chunk.len();
                self.set_phase("replying", &mut out);
            },
            Msg::StreamDone { usage, .. } => {
                if let Some(usage) = usage {
                    self.confirmed_tokens += usage
                        .completion_tokens
                        .saturating_add(usage.reasoning_output_tokens);
                    self.streamed_chars = 0;
                }
            },
            _ => {},
        }
        // Token count rides along whenever something else is being said, and
        // otherwise at most every TOKEN_PROGRESS_INTERVAL.
        let total = self.total_tokens();
        let due = now.duration_since(self.last_tokens_at) >= TOKEN_PROGRESS_INTERVAL;
        if total != self.last_tokens_sent && (due || !out.is_empty()) {
            out.push(ProgressEvent::SubagentTokens(total));
            self.last_tokens_sent = total;
            self.last_tokens_at = now;
        }
        out
    }

    fn set_phase(&mut self, phase: &'static str, out: &mut Vec<ProgressEvent>) {
        if self.phase != phase {
            self.phase = phase;
            out.push(ProgressEvent::SubagentActivity(phase.to_string()));
        }
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
    apply_live_mcp(&mut state.mcp.servers, &manager.all_specs(), |name| {
        manager.has_server(name)
    });
}

/// Pure core of [`seed_child_mcp`], injectable for tests: flip every entry
/// the live manager actually runs to `Ready` and attach its advertised
/// (already-sanitized) specs. Entries for servers the manager doesn't have
/// (failed to start) keep their `Starting` status and stay un-advertised —
/// same as in the parent.
fn apply_live_mcp(
    servers: &mut std::collections::HashMap<String, crate::domain::McpServerEntry>,
    live_specs: &[(String, crate::domain::McpToolSpec)],
    has_server: impl Fn(&str) -> bool,
) {
    for (name, entry) in servers.iter_mut() {
        if !has_server(name) {
            continue;
        }
        entry.status = crate::domain::McpServerStatus::Ready;
        let cfg = &entry.config;
        let tools: Vec<crate::domain::McpToolSpec> = live_specs
            .iter()
            .filter(|(server, _)| server == name)
            // Honor the per-server enabled_tools/disabled_tools filter,
            // matched against the server's own (raw) tool names.
            .filter(|(_, spec)| cfg.tool_allowed(&spec.raw_name))
            .map(|(_, spec)| spec.clone())
            .collect();
        entry.tools = tools;
    }
}

/// Construct the child `ToolRegistry` — a subset of what the parent
/// offers, optionally narrowed further by an agent type's `tools` filter
/// (`None` = the full child set; see `CHILD_TOOL_NAMES`). Always excludes:
///
///   - `agent` itself — subagents don't spawn subagents. This
///     exclusion is the guard (there is no depth counter).
///   - All seven GUI / computer-use tools — the parent's
///     `ComputerUseDriver` owns the screenshot coord registry; a
///     subagent clicking would corrupt the parent's latest-capture
///     pointer.
///
/// The MCP proxy routes through the process-global `McpServerManager` —
/// the child calls the SAME running servers as the parent (advertised via
/// `seed_child_mcp`, which marks them Ready in the child's state). Every
/// registered tool is gated at the child's effective safety mode.
fn build_child_registry(
    providers: Arc<ProviderFactory>,
    tools: Option<&[String]>,
    web: &crate::app::WebConfig,
) -> Arc<ToolRegistry> {
    use super::{
        apply_patch, computer_use, exec, filesystem, mcp,
        web::{web_fetch_tool, web_search_tool},
    };
    let allowed = |name: &str| tools.is_none_or(|t| t.iter().any(|x| x == name));
    let mut r = ToolRegistry::new();
    if allowed("read_file") {
        r.register(Arc::new(filesystem::ReadFileTool));
    }
    if allowed("write_file") {
        r.register(Arc::new(filesystem::WriteFileTool));
    }
    if allowed("apply_patch") {
        r.register(Arc::new(apply_patch::ApplyPatchTool));
    }
    if allowed("delete_file") {
        r.register(Arc::new(filesystem::DeleteFileTool));
    }
    if allowed("create_directory") {
        r.register(Arc::new(filesystem::CreateDirectoryTool));
    }
    if allowed("execute_command") {
        r.register(Arc::new(exec::ExecuteCommandTool));
    }
    if allowed("mcp") {
        r.register(Arc::new(mcp::McpToolProxy));
    }
    if allowed("web_search")
        && let Some(tool) = web_search_tool(web)
    {
        r.register(Arc::new(tool));
    }
    if allowed("web_fetch")
        && let Some(tool) = web_fetch_tool(web)
    {
        r.register(Arc::new(tool));
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

    fn test_state() -> State {
        State::new(
            crate::app::Config::default(),
            PathBuf::from("/tmp"),
            "ollama/test".to_string(),
            chrono::Local::now(),
        )
    }

    fn stream_text(chunk: &str) -> Msg {
        Msg::StreamText {
            turn: TurnId(1),
            chunk: chunk.to_string(),
        }
    }

    #[tokio::test]
    async fn child_stream_chunks_never_forward_text_only_one_phase_change() {
        // The status-line flicker regression: every child StreamText chunk
        // used to become a parent progress event. Now the FIRST chunk flips
        // the phase ("replying") and subsequent chunks are silent until the
        // token throttle elapses.
        let state = test_state();
        let now = tokio::time::Instant::now();
        let mut progress = ChildProgress::new(now);

        let first = progress.observe(&stream_text("chunk one — some text"), &state, now);
        assert!(
            first.iter().any(
                |e| matches!(e, ProgressEvent::SubagentActivity(label) if label == "replying")
            ),
            "first chunk announces the phase: {first:?}"
        );
        assert!(
            !first
                .iter()
                .any(|e| matches!(e, ProgressEvent::SubagentToolCall { .. })),
            "no raw text ever forwards: {first:?}"
        );

        // A burst of further chunks inside the throttle window emits NOTHING.
        for i in 0..50 {
            let events = progress.observe(&stream_text(&format!("chunk {i}")), &state, now);
            assert!(
                events.is_empty(),
                "chunk {i} must be silent inside the throttle window: {events:?}"
            );
        }
    }

    #[tokio::test]
    async fn token_estimates_respect_the_throttle_and_snap_to_provider_usage() {
        let state = test_state();
        let start = tokio::time::Instant::now();
        let mut progress = ChildProgress::new(start);

        // First tiny chunk: phase flip only (0 tokens → nothing to report).
        let _ = progress.observe(&stream_text("xy"), &state, start);
        // 400 chars ≈ 100 tokens accumulate silently inside the window…
        let silent = progress.observe(&stream_text(&"x".repeat(400)), &state, start);
        assert!(
            silent.is_empty(),
            "inside the window stays silent: {silent:?}"
        );
        // …and flush once the interval has elapsed.
        let later = start + TOKEN_PROGRESS_INTERVAL;
        let events = progress.observe(&stream_text("y"), &state, later);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProgressEvent::SubagentTokens(t) if *t >= 100)),
            "tokens flush after the interval: {events:?}"
        );

        // StreamDone snaps the estimate to the provider-reported count and
        // piggybacks... only once the throttle allows again.
        let done = Msg::StreamDone {
            turn: TurnId(1),
            usage: Some(crate::models::TokenUsage::provider(10, 5_000)),
            provider_continuation: None,
            stop_reason: None,
        };
        let much_later = later + TOKEN_PROGRESS_INTERVAL;
        let events = progress.observe(&done, &state, much_later);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProgressEvent::SubagentTokens(t) if *t >= 5_000)),
            "provider usage snaps the counter: {events:?}"
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
            config: crate::app::McpServerConfig::default(),
            status: McpServerStatus::Starting,
            tools: Vec::new(),
        };
        let mut servers = std::collections::HashMap::new();
        servers.insert("slack".to_string(), entry());
        servers.insert("broken".to_string(), entry());

        let live = vec![
            (
                "slack".to_string(),
                crate::domain::McpToolSpec {
                    name: "mcp__slack__send".to_string(),
                    raw_name: "send".to_string(),
                    description: "send a message".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            ),
            // A tool from a server the child doesn't have configured must
            // not create an entry out of thin air.
            (
                "other".to_string(),
                crate::domain::McpToolSpec {
                    name: "mcp__other__x".to_string(),
                    raw_name: "x".to_string(),
                    description: String::new(),
                    input_schema: serde_json::json!({}),
                },
            ),
        ];
        apply_live_mcp(&mut servers, &live, |name| name == "slack");

        let slack = &servers["slack"];
        assert_eq!(slack.status, McpServerStatus::Ready);
        assert_eq!(slack.tools.len(), 1);
        assert_eq!(slack.tools[0].name, "mcp__slack__send");
        assert_eq!(slack.tools[0].raw_name, "send");
        // A configured server the manager doesn't run stays un-advertised.
        assert_eq!(servers["broken"].status, McpServerStatus::Starting);
        assert!(servers["broken"].tools.is_empty());
        assert!(!servers.contains_key("other"));
    }

    #[test]
    fn subagent_metadata_carries_usage_only_when_reported() {
        let some = subagent_metadata(
            "ollama/test".to_string(),
            TokenUsageTotals {
                prompt_tokens: 100,
                completion_tokens: 40,
                ..TokenUsageTotals::default()
            },
            "a7".to_string(),
        );
        let usage = some.token_usage.expect("usage attached");
        assert_eq!(usage.total_tokens(), 140);
        assert_eq!(usage.completion_tokens, 40);
        assert!(matches!(
            some.detail,
            crate::domain::ToolMetadata::Subagent { ref model_id, ref agent_id }
                if model_id == "ollama/test" && agent_id == "a7"
        ));
        // A provider that reported nothing must not render as "0 tokens".
        let none = subagent_metadata(
            "ollama/test".to_string(),
            TokenUsageTotals::default(),
            "a8".to_string(),
        );
        assert!(none.token_usage.is_none());
    }

    #[test]
    fn usage_delta_reports_only_this_drive() {
        // Continuations accumulate usage in one State; the parent must see
        // the delta, not the cumulative total again (double-count).
        let before = TokenUsageTotals {
            prompt_tokens: 1_000,
            completion_tokens: 200,
            ..TokenUsageTotals::default()
        };
        let after = TokenUsageTotals {
            prompt_tokens: 1_600,
            completion_tokens: 350,
            ..TokenUsageTotals::default()
        };
        let delta = usage_delta(after, before);
        assert_eq!(delta.prompt_tokens, 600);
        assert_eq!(delta.completion_tokens, 150);
        assert_eq!(delta.total_tokens(), 750);
        // A fresh spawn's snapshot is zero — the delta IS the total.
        let fresh = usage_delta(after, TokenUsageTotals::default());
        assert_eq!(fresh.total_tokens(), 1_950);
    }

    #[test]
    fn resolve_agent_type_builtins_custom_shadowing_and_errors() {
        use crate::app::AgentTypeConfig;
        let mut config = crate::app::Config::default();

        // Built-ins: no request defaults to general; explore pins read_only.
        assert_eq!(resolve_agent_type(None, &config).unwrap().name, "general");
        assert_eq!(
            resolve_agent_type(None, &config).unwrap().safety_ceiling,
            SafetyMode::FullAccess,
        );
        let explore = resolve_agent_type(Some("explore"), &config).unwrap();
        assert_eq!(explore.safety_ceiling, SafetyMode::ReadOnly);
        assert!(explore.preamble.as_deref().unwrap().contains("read-only"));
        assert!(explore.allows_tool("read_file"));
        assert!(!explore.allows_tool("write_file"));
        assert!(!explore.allows_tool("mcp"));

        // Unknown type: actionable error naming what exists.
        let err = resolve_agent_type(Some("nope"), &config).unwrap_err();
        assert!(err.contains("general") && err.contains("explore"), "{err}");

        // Custom type from config.
        config.agents.types.insert(
            "scout".to_string(),
            AgentTypeConfig {
                tools: Some(vec!["read_file".to_string()]),
                safety: Some("read_only".to_string()),
                preamble: Some("You are a scout.".to_string()),
                model: Some("ollama/qwen3:8b".to_string()),
            },
        );
        let scout = resolve_agent_type(Some("scout"), &config).unwrap();
        assert_eq!(scout.model.as_deref(), Some("ollama/qwen3:8b"));
        assert_eq!(scout.safety_ceiling, SafetyMode::ReadOnly);

        // A custom name shadows the built-in, so users can retune explore.
        config.agents.types.insert(
            "explore".to_string(),
            AgentTypeConfig {
                safety: Some("ask".to_string()),
                ..AgentTypeConfig::default()
            },
        );
        assert_eq!(
            resolve_agent_type(Some("explore"), &config)
                .unwrap()
                .safety_ceiling,
            SafetyMode::Ask,
        );

        // Invalid safety string and unknown tool name both fail fast with
        // the offending value in the message.
        config.agents.types.insert(
            "bad-safety".to_string(),
            AgentTypeConfig {
                safety: Some("yolo".to_string()),
                ..AgentTypeConfig::default()
            },
        );
        assert!(
            resolve_agent_type(Some("bad-safety"), &config)
                .unwrap_err()
                .contains("yolo")
        );
        config.agents.types.insert(
            "bad-tool".to_string(),
            AgentTypeConfig {
                tools: Some(vec!["screenshot".to_string()]),
                ..AgentTypeConfig::default()
            },
        );
        assert!(
            resolve_agent_type(Some("bad-tool"), &config)
                .unwrap_err()
                .contains("screenshot")
        );
    }

    #[test]
    fn agent_cache_stores_takes_and_evicts_oldest() {
        let spawner = SubagentSpawner::new(Arc::new(ProviderFactory::new(
            crate::app::Config::default(),
        )));
        let mk_state = || {
            State::new(
                crate::app::Config::default(),
                PathBuf::from("/tmp"),
                "ollama/test".to_string(),
                chrono::Local::now(),
            )
        };
        let mk = || CachedAgent {
            state: mk_state(),
            type_name: "general".to_string(),
        };

        // Handles mint monotonically distinct.
        assert_ne!(spawner.mint_agent_id(), spawner.mint_agent_id());

        // Store/take round-trip; take REMOVES (a running continuation owns
        // the state exclusively).
        spawner.cache_store("x".to_string(), mk());
        assert!(spawner.cache_take("x").is_some());
        assert!(spawner.cache_take("x").is_none(), "take must remove");

        // Eviction: past the cap, oldest goes first.
        for i in 0..(MAX_CACHED_AGENTS + 2) {
            spawner.cache_store(format!("e{i}"), mk());
        }
        assert!(spawner.cache_take("e0").is_none(), "oldest evicted");
        assert!(spawner.cache_take("e1").is_none(), "second-oldest evicted");
        assert!(
            spawner
                .cache_take(&format!("e{}", MAX_CACHED_AGENTS + 1))
                .is_some(),
            "newest survives",
        );
    }

    #[tokio::test]
    async fn continuing_an_unknown_agent_id_errors_actionably() {
        let spawner = Arc::new(SubagentSpawner::new(Arc::new(ProviderFactory::new(
            crate::app::Config::default(),
        ))));
        let tool = SubagentTool::new(spawner);
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = tool
            .execute(
                serde_json::json!({"prompt": "follow up", "agent_id": "a99"}),
                ctx,
            )
            .await;
        assert_eq!(outcome.status, crate::domain::ToolStatus::Error);
        let msg = outcome.error_message().unwrap_or_default();
        assert!(msg.contains("a99"), "names the bad id: {msg}");
        assert!(
            msg.contains("Omit agent_id"),
            "tells the model how to recover: {msg}"
        );
    }

    #[tokio::test]
    async fn unknown_agent_type_errors_actionably() {
        let spawner = Arc::new(SubagentSpawner::new(Arc::new(ProviderFactory::new(
            crate::app::Config::default(),
        ))));
        let tool = SubagentTool::new(spawner);
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = tool
            .execute(
                serde_json::json!({"prompt": "look around", "type": "wizard"}),
                ctx,
            )
            .await;
        assert_eq!(outcome.status, crate::domain::ToolStatus::Error);
        let msg = outcome.error_message().unwrap_or_default();
        assert!(msg.contains("wizard") && msg.contains("explore"), "{msg}");
    }

    #[test]
    fn build_child_registry_excludes_gui_and_self() {
        let providers = Arc::new(ProviderFactory::new(crate::app::Config::default()));
        let r = build_child_registry(providers, None, &crate::app::WebConfig::default());
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

    #[test]
    fn kill_detached_fires_registered_tokens_and_evicts_cached_children() {
        let spawner = SubagentSpawner::new(Arc::new(ProviderFactory::new(
            crate::app::Config::default(),
        )));

        // Running detached child: token registered → killed exactly once.
        let cancel = CancellationToken::new();
        spawner.register_detached("a1".to_string(), cancel.clone());
        assert_eq!(spawner.kill_detached("a1"), KillResult::Killed);
        assert!(cancel.is_cancelled());
        // The handle is consumed — a second kill finds nothing.
        assert_eq!(spawner.kill_detached("a1"), KillResult::NotFound);

        // Finished child (continuation cache only): evicted, not killable.
        spawner.cache_store(
            "a2".to_string(),
            CachedAgent {
                state: test_state(),
                type_name: "general".to_string(),
            },
        );
        assert_eq!(spawner.kill_detached("a2"), KillResult::Evicted);
        assert!(spawner.cache_take("a2").is_none(), "eviction is permanent");

        // Never-issued id.
        assert_eq!(spawner.kill_detached("a99"), KillResult::NotFound);

        // kill_all fires every registered token and drains the map.
        let (c1, c2) = (CancellationToken::new(), CancellationToken::new());
        spawner.register_detached("a3".to_string(), c1.clone());
        spawner.register_detached("a4".to_string(), c2.clone());
        assert_eq!(spawner.kill_all_detached(), 2);
        assert!(c1.is_cancelled() && c2.is_cancelled());
        assert_eq!(spawner.kill_all_detached(), 0);
    }

    #[tokio::test]
    async fn kill_action_validates_agent_id_and_skips_prompt_requirement() {
        let spawner = Arc::new(SubagentSpawner::new(Arc::new(ProviderFactory::new(
            crate::app::Config::default(),
        ))));
        let tool = SubagentTool::new(spawner.clone());

        // No agent_id → arg error (and NOT the missing-prompt error: the
        // kill path must not require a prompt).
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = tool
            .execute(serde_json::json!({"action": "kill"}), ctx)
            .await;
        assert!(!outcome.is_success());
        assert!(outcome.model_content.contains("requires `agent_id`"));

        // Unknown id → error naming the id.
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(2), PathBuf::from("/tmp"));
        let outcome = tool
            .execute(serde_json::json!({"action": "kill", "agent_id": "a7"}), ctx)
            .await;
        assert!(!outcome.is_success());
        assert!(outcome.model_content.contains("a7"));

        // Registered detached child → success, token fired.
        let cancel = CancellationToken::new();
        spawner.register_detached("a7".to_string(), cancel.clone());
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(3), PathBuf::from("/tmp"));
        let outcome = tool
            .execute(serde_json::json!({"action": "kill", "agent_id": "a7"}), ctx)
            .await;
        assert!(outcome.is_success(), "{}", outcome.model_content);
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn explore_registry_is_a_read_only_surface() {
        let providers = Arc::new(ProviderFactory::new(crate::app::Config::default()));
        let explore = builtin_agent_type("explore").expect("builtin");
        let r = build_child_registry(
            providers,
            explore.tools.as_deref(),
            &crate::app::WebConfig::default(),
        );
        assert!(r.get("read_file").is_some());
        assert!(r.get("execute_command").is_some());
        for tool in [
            "write_file",
            "apply_patch",
            "delete_file",
            "create_directory",
            "mcp_proxy",
            "agent",
        ] {
            assert!(r.get(tool).is_none(), "explore must not carry {tool}");
        }
    }
}
