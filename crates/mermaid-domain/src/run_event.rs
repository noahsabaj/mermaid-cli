//! Public, versioned event stream for `mermaid run --format ndjson`.
//!
//! [`RunEvent`] is the stable SDK surface: a lossy but frozen projection of the
//! internal turn/tool/approval lifecycle ([`Msg`]) into one JSON object per
//! line. Unlike `Msg` — which is deliberately loose so `--record`/`--replay`
//! can grow variants freely — this wire format is a contract. The golden test
//! in this module pins every variant's serialization so it cannot drift
//! silently.
//!
//! Purity: this module is serde-only (no I/O, no wall clock), so it lives in
//! `domain` and stays inside the purity guard. The impure emission (writing the
//! lines to stdout) lives in the headless driver, `app::run_non_interactive`.

use serde::{Deserialize, Serialize};

use super::msg::Msg;
use super::session_event::SessionEvent;
use mermaid_model::action::{ActionDisplay, ActionResult};
use mermaid_model::models::{ChatMessage, FinishReason, MessageRole};
use mermaid_model::tool_run::{ToolMetadata, ToolStatus};

/// Wire-format version of the `RunEvent` stream. Bump only on a breaking change
/// to an existing variant's shape; additive variants keep version 1.
pub const RUN_EVENT_PROTOCOL_VERSION: u32 = 1;

/// One line of the `mermaid run --format ndjson` stream. Internally tagged on
/// `type` (`snake_case`), matching the house style for stable wire unions
/// (`ToolMetadata`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    /// First line of every stream: protocol version + run identity.
    SessionStarted {
        /// [`RUN_EVENT_PROTOCOL_VERSION`] at the time of emission.
        protocol_version: u32,
        /// Mermaid version that produced the stream.
        cli_version: String,
        /// Resolved model id driving the run.
        model: String,
        /// Durable runtime task id, when the run is task-backed.
        #[serde(default)]
        task_id: Option<String>,
        /// Conversation/session id owning this run — pass it to
        /// `mermaid run --resume <id>` to continue the session. Additive
        /// (defaulted) so pre-existing recordings still deserialize.
        #[serde(default)]
        session_id: String,
    },
    /// A chunk of assistant answer text.
    Text {
        /// The appended text.
        delta: String,
    },
    /// A chunk of model reasoning / thinking.
    Reasoning {
        /// The appended reasoning text.
        delta: String,
    },
    /// A tool began executing (bracketed by a later [`RunEvent::ToolFinished`]
    /// with the same `call_id`).
    ToolStarted {
        /// Stable per-run tool-call id.
        call_id: String,
    },
    /// A tool finished. `name` is derived from the run metadata; `status` is
    /// `success` / `error` / `cancelled`.
    ToolFinished {
        /// Stable per-run tool-call id (matches the `tool_started` line).
        call_id: String,
        /// Tool name, e.g. `execute_command`, `read_file`, `<server>/<tool>`.
        name: String,
        /// `success`, `error`, or `cancelled`.
        status: String,
        /// One-line human summary of the outcome.
        summary: String,
        /// Error detail, when the tool failed.
        #[serde(default)]
        error: Option<String>,
        /// Present when this call is `exit_plan_mode` resolving with an
        /// APPROVED plan — first-class plan visibility for SDK/daemon
        /// subscribers without breaking the started/finished pairing.
        /// Additive: absent for every other tool, and omitted from the wire
        /// when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<PlanApproved>,
        /// Structured, secret-safe web transport details for web tools.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        web: Option<Box<WebEventDetails>>,
    },
    /// A gated tool is waiting for approval. Headless runs surface this so a
    /// supervising process can decide.
    ApprovalRequired {
        /// Stable per-run tool-call id.
        call_id: String,
        /// Tool name being gated.
        tool: String,
        /// Risk classification (e.g. `network`, `mutation`).
        risk: String,
        /// Human-readable approval prompt.
        prompt: String,
    },
    /// The session task checklist changed (`task_create` / `task_update` /
    /// a user `/tasks` edit). Full snapshot of the visible list, so consumers
    /// never need to correlate diffs. Additive — protocol stays v1.
    TasksUpdated {
        /// Every non-deleted task, in creation order.
        tasks: Vec<TaskLine>,
        /// Count of completed tasks (numerator of "Tasks m/n").
        completed: u32,
        /// Count of visible tasks (denominator of "Tasks m/n").
        total: u32,
    },
    /// The turn hit a recoverable or terminal upstream error.
    Error {
        /// Human-readable error message.
        message: String,
    },
    /// A model turn completed (token usage + why it stopped, when known).
    TurnDone {
        /// Total tokens for the turn, when the provider reported them.
        #[serde(default)]
        total_tokens: Option<u64>,
        /// Why the turn stopped (`stop`, `length`, `tool_use`, …), when known.
        #[serde(default)]
        stop_reason: Option<String>,
    },
    /// Terminal line of the stream: the aggregated run result.
    Result {
        /// Final assistant response text.
        response: String,
        /// Final reasoning text, when the model exposed any.
        #[serde(default)]
        reasoning: Option<String>,
        /// Cumulative token usage for the whole run.
        total_tokens: u64,
        /// Errors encountered during the run (empty on success).
        errors: Vec<String>,
        /// Conversation/session id owning this run (same as the
        /// `session_started` line; repeated here so a consumer that only
        /// reads the terminal line still gets it). Additive (defaulted).
        #[serde(default)]
        session_id: String,
        /// `--output-schema` runs: the response parsed as JSON, present only
        /// when it parsed AND validated against the schema. Additive
        /// (defaulted) — `protocol_version` stays 1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured_output: Option<serde_json::Value>,
    },
}

/// Plan payload on a [`RunEvent::ToolFinished`] for `exit_plan_mode`: the
/// approved plan's location and the execution disposition the user chose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanApproved {
    /// Plan-file path, project-relative.
    pub path: String,
    /// Implementation starts immediately.
    #[serde(default)]
    pub start: bool,
    /// Execution continues in a fresh conversation.
    #[serde(default)]
    pub fresh: bool,
    /// Execution continues in a forked conversation.
    #[serde(default)]
    pub fork: bool,
}

/// Stable, bounded web facts exposed to NDJSON consumers. Query text and page
/// content deliberately stay out of this surface; URLs are sanitized before
/// entering `ToolMetadata`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebEventDetails {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_byte_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_byte_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered_byte_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_lines: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub succeeded_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_count: Option<usize>,
    #[serde(default)]
    pub failed_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<super::WebSearchFailure>,
    #[serde(default)]
    pub partial: bool,
    #[serde(default)]
    pub truncated: bool,
}

impl RunEvent {
    /// Project a lifecycle [`Msg`] into a public `RunEvent`, or `None` for the
    /// many messages that have no place in the SDK stream.
    ///
    /// Pure and stateless: tool identity is read from the finished outcome's
    /// metadata rather than correlated across messages, so no projector state
    /// is required. The wildcard arm is intentional here (this is a lossy
    /// projection, not the reducer — most `Msg`s are deliberately dropped).
    pub fn from_msg(msg: &Msg) -> Option<Self> {
        Some(match msg {
            Msg::StreamText { chunk, .. } => Self::Text {
                delta: chunk.clone(),
            },
            Msg::StreamReasoning { chunk, .. } => Self::Reasoning {
                delta: chunk.text.clone(),
            },
            Msg::ToolStarted { call_id, .. } => Self::ToolStarted {
                call_id: call_id.to_string(),
            },
            Msg::ToolFinished {
                call_id, outcome, ..
            } => Self::ToolFinished {
                call_id: call_id.to_string(),
                name: tool_name(&outcome.metadata.detail),
                status: status_str(outcome.status).to_string(),
                summary: outcome.summary.clone(),
                error: outcome.error.clone(),
                plan: plan_approved(&outcome.metadata.detail),
                web: web_event_details(&outcome.metadata.detail).map(Box::new),
            },
            Msg::ApprovalRequested {
                call_id,
                tool,
                risk,
                prompt,
                ..
            } => Self::ApprovalRequired {
                call_id: call_id.to_string(),
                tool: tool.clone(),
                risk: risk.clone(),
                prompt: prompt.clone(),
            },
            Msg::TasksUpdated { store } => {
                let (completed, total) = store.counts();
                Self::TasksUpdated {
                    tasks: store.visible().map(TaskLine::from).collect(),
                    completed: completed as u32,
                    total: total as u32,
                }
            },
            Msg::UpstreamError { error, .. } => Self::Error {
                message: error.message.clone(),
            },
            Msg::StreamDone {
                usage, stop_reason, ..
            } => Self::TurnDone {
                total_tokens: usage.as_ref().map(|u| u.total_tokens() as u64),
                stop_reason: stop_reason.as_ref().map(finish_reason_str),
            },
            _ => return None,
        })
    }

    /// Project committed session-log events onto the public stream: the
    /// CATCH-UP a `subscribe_task` attach replays before it joins the live
    /// broadcast.
    ///
    /// The log is coarse where the live stream is fine — one committed
    /// message against the dozens of deltas that produced it — so this is a
    /// projection, not a recording. Everything a consumer accumulates
    /// (answer text, reasoning, tool activity, the checklist) arrives in
    /// order at message granularity, which is what a subscriber that missed
    /// the deltas actually needs.
    ///
    /// The ASSIGNING events (`compaction`, `reset`) clear what came before,
    /// exactly as [`fold_session`](crate::fold_session) does: the catch-up
    /// describes the transcript as it now stands, not every message that
    /// ever existed. That rule also keeps tool calls single — a replacement
    /// message carries its actions inline, and the clear drops the `action`
    /// events that already replayed them.
    ///
    /// `tasks` is a whole snapshot, so only the last one is emitted, after
    /// the transcript: the checklist is session state, not transcript, and
    /// must survive a clear.
    ///
    /// Identity is deliberately absent. The log's `started` event describes
    /// the SESSION, which for a resumed one predates this run, and only the
    /// caller knows which task the stream belongs to — so the daemon states
    /// `session_started` itself.
    #[must_use]
    pub fn catch_up(events: &[SessionEvent]) -> Vec<Self> {
        let mut out = Vec::new();
        // Replayed pairs need call ids the live stream never issued. The
        // prefix is what keeps them from colliding with a provider's, and
        // it stays monotonic across a clear so no two lines in one
        // catch-up can share an id.
        let mut calls = 0usize;
        let mut tasks = None;
        for event in events {
            match event {
                SessionEvent::Message { message }
                | SessionEvent::InsertedBeforeLast { message } => {
                    push_message(&mut out, &mut calls, message);
                },
                SessionEvent::Action { action } => push_action(&mut out, &mut calls, action),
                SessionEvent::Compaction { replacement, .. } => {
                    out.clear();
                    for message in replacement {
                        push_message(&mut out, &mut calls, message);
                    }
                },
                SessionEvent::Reset { messages, .. } => {
                    out.clear();
                    for message in messages {
                        push_message(&mut out, &mut calls, message);
                    }
                },
                SessionEvent::Tasks { store } => tasks = Some(store),
                // Identity, scalars, prompt history, and screenshot bytes
                // have no projection on this wire. Exhaustive on purpose: a
                // new event type has to decide whether a catch-up shows it.
                SessionEvent::Started { .. }
                | SessionEvent::State(_)
                | SessionEvent::Input { .. }
                | SessionEvent::Image { .. } => {},
            }
        }
        // An EMPTY checklist is not news to a subscriber that has never seen
        // one — and every log carries one from the backfill, so sending it
        // would put a `tasks_updated { total: 0 }` on the front of every
        // catch-up.
        if let Some(store) = tasks.filter(|store| !store.is_empty()) {
            let (completed, total) = store.counts();
            out.push(Self::TasksUpdated {
                tasks: store.visible().map(TaskLine::from).collect(),
                completed: u32::try_from(completed).unwrap_or(u32::MAX),
                total: u32::try_from(total).unwrap_or(u32::MAX),
            });
        }
        out
    }
}

/// Prefix on the `call_id` of a tool pair replayed from the log. The
/// committed transcript keeps no provider call ids, so a catch-up mints its
/// own — and says so, rather than inventing something that could be mistaken
/// for the live stream's.
const CATCH_UP_CALL_PREFIX: &str = "catchup-";

/// Cap on a replayed tool summary. A `target` can be a whole shell command,
/// and this line is a one-liner by contract.
const MAX_CATCH_UP_SUMMARY_CHARS: usize = 200;

/// One committed message's contribution to a catch-up: the assistant's
/// reasoning and answer text, then the tool calls attached to it.
///
/// Only an assistant turn yields text. The live stream has no variant for a
/// user prompt, a system note, or a tool result — replaying them would
/// invent content a from-the-start subscriber never saw.
fn push_message(out: &mut Vec<RunEvent>, calls: &mut usize, message: &ChatMessage) {
    if matches!(message.role, MessageRole::Assistant) {
        if let Some(thinking) = message.thinking.as_ref().filter(|text| !text.is_empty()) {
            out.push(RunEvent::Reasoning {
                delta: thinking.clone(),
            });
        }
        if !message.content.is_empty() {
            out.push(RunEvent::Text {
                delta: message.content.clone(),
            });
        }
    }
    for action in &message.actions {
        push_action(out, calls, action);
    }
}

/// A committed tool run, replayed as the started/finished pair the live
/// stream emitted at the time. `name`, `plan`, and `web` come off the same
/// run metadata [`RunEvent::from_msg`] reads, so a replayed line matches the
/// live one wherever that metadata survived to disk.
fn push_action(out: &mut Vec<RunEvent>, calls: &mut usize, action: &ActionDisplay) {
    *calls += 1;
    let call_id = format!("{CATCH_UP_CALL_PREFIX}{calls}");
    out.push(RunEvent::ToolStarted {
        call_id: call_id.clone(),
    });
    // `Running` is synthesized by the renderer and never committed. If one
    // reaches disk anyway it describes a call still in flight, so the pair
    // stays open — exactly as it would live, where the `tool_finished` is
    // still to come on the broadcast.
    let (status, error) = match &action.result {
        ActionResult::Success { .. } => (ToolStatus::Success, None),
        ActionResult::Error { error } => (ToolStatus::Error, Some(error.clone())),
        ActionResult::Running => return,
    };
    let detail = action.metadata.as_ref().map(|meta| &meta.detail);
    out.push(RunEvent::ToolFinished {
        call_id,
        name: detail.map_or_else(|| fallback_tool_name(&action.action_type), tool_name),
        status: status_str(status).to_string(),
        summary: action_summary(action),
        error,
        plan: detail.and_then(plan_approved),
        web: detail.and_then(web_event_details).map(Box::new),
    });
}

/// The wire name for an action with no run metadata — a simple action, or
/// one saved before metadata existed. Its display kind is all there is, so
/// normalize `"Web Search"` into the wire's `snake_case` shape rather than
/// answer the useless generic `tool`.
fn fallback_tool_name(action_type: &str) -> String {
    let name = action_type.trim().to_lowercase().replace([' ', '-'], "_");
    if name.is_empty() {
        "tool".to_string()
    } else {
        name
    }
}

/// The one-line summary on a replayed `tool_finished`. The transcript keeps
/// the action's display header, not the live `ToolOutcome::summary`, so this
/// is the closest honest equivalent.
fn action_summary(action: &ActionDisplay) -> String {
    let target = action.target.trim();
    let summary = if target.is_empty() {
        action.action_type.clone()
    } else {
        format!("{} {}", action.action_type, target)
    };
    if summary.chars().count() <= MAX_CATCH_UP_SUMMARY_CHARS {
        return summary;
    }
    let mut capped: String = summary.chars().take(MAX_CATCH_UP_SUMMARY_CHARS).collect();
    capped.push('…');
    capped
}

/// The approved-plan payload a `tool_finished` carries, when the call was
/// `exit_plan_mode` resolving with a plan. Shared by the live projection and
/// the catch-up so the two cannot drift.
fn plan_approved(detail: &ToolMetadata) -> Option<PlanApproved> {
    match detail {
        ToolMetadata::Plan {
            path,
            start,
            fresh,
            fork,
            ..
        } => Some(PlanApproved {
            path: path.clone(),
            start: *start,
            fresh: *fresh,
            fork: *fork,
        }),
        _ => None,
    }
}

/// One checklist row on the `tasks_updated` line. Flat and stringly-statused
/// (wire contract — the internal enum can grow without breaking consumers).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskLine {
    pub id: u32,
    pub subject: String,
    /// `pending` / `in_progress` / `completed`.
    pub status: String,
    pub active_form: String,
    /// Seconds spent `in_progress→completed`, when both stamps exist.
    #[serde(default)]
    pub elapsed_secs: Option<u64>,
    /// Completion tokens attributed while in progress, when known.
    #[serde(default)]
    pub tokens_spent: Option<u64>,
    /// `model` or `user` (a `/tasks add` entry).
    #[serde(default)]
    pub origin: Option<String>,
}

impl From<&crate::ChecklistItem> for TaskLine {
    fn from(task: &crate::ChecklistItem) -> Self {
        Self {
            id: task.id,
            subject: task.subject.clone(),
            status: task.status.as_str().to_string(),
            active_form: task.active_form.clone(),
            elapsed_secs: task.elapsed_secs(),
            tokens_spent: task.tokens_spent,
            origin: Some(
                match task.origin {
                    crate::ChecklistOrigin::Model => "model",
                    crate::ChecklistOrigin::User => "user",
                }
                .to_string(),
            ),
        }
    }
}

/// `Snake_case` name of a finished tool, from its run-metadata tag. The exhaustive
/// match doubles as a drift guard: a new `ToolMetadata` variant forces a name
/// mapping here.
fn tool_name(detail: &ToolMetadata) -> String {
    match detail {
        ToolMetadata::None => "tool".to_string(),
        ToolMetadata::ReadFile { .. } => "read_file".to_string(),
        ToolMetadata::WriteFile { .. } => "write_file".to_string(),
        ToolMetadata::ApplyPatch { .. } => "apply_patch".to_string(),
        ToolMetadata::DeleteFile { .. } => "delete_file".to_string(),
        ToolMetadata::CreateDirectory { .. } => "create_directory".to_string(),
        ToolMetadata::WebSearch { .. } => "web_search".to_string(),
        ToolMetadata::WebFetch { .. } => "web_fetch".to_string(),
        ToolMetadata::ExecuteCommand { .. } => "execute_command".to_string(),
        ToolMetadata::ComputerUse { .. } => "computer_use".to_string(),
        ToolMetadata::Mcp { server, tool } => format!("{server}/{tool}"),
        ToolMetadata::Subagent { .. } => "agent".to_string(),
        ToolMetadata::Tasks { action, .. } => format!("task_{action}"),
        ToolMetadata::Questions { .. } => "ask_user_question".to_string(),
        ToolMetadata::Plan { .. } => "exit_plan_mode".to_string(),
        ToolMetadata::Custom { name, .. } => name.clone(),
    }
}

fn web_event_details(detail: &ToolMetadata) -> Option<WebEventDetails> {
    match detail {
        ToolMetadata::WebSearch {
            queries,
            requested_count,
            result_count,
            backend,
            succeeded_queries,
            failed_queries,
            partial,
            truncated,
            failures,
            ..
        } => Some(WebEventDetails {
            backend: backend.clone(),
            requested_url: None,
            final_url: None,
            status: None,
            error_kind: None,
            media_type: None,
            charset: None,
            extraction: None,
            snapshot_id: None,
            source_byte_count: None,
            output_byte_count: None,
            rendered_byte_count: None,
            line_count: None,
            pattern: None,
            context_lines: None,
            match_count: None,
            query_count: Some(queries.len()),
            succeeded_count: Some(*succeeded_queries),
            requested_count: Some(*requested_count),
            result_count: Some(*result_count),
            failed_count: *failed_queries,
            failures: failures.clone(),
            partial: *partial,
            truncated: *truncated,
        }),
        ToolMetadata::WebFetch {
            url,
            final_url,
            status,
            error_kind,
            media_type,
            charset,
            backend,
            extraction,
            snapshot_id,
            source_byte_count,
            output_byte_count,
            byte_count,
            line_count,
            pattern,
            context_lines,
            match_count,
            truncated,
            ..
        } => Some(WebEventDetails {
            backend: backend.clone(),
            requested_url: Some(url.clone()),
            final_url: final_url.clone(),
            status: *status,
            error_kind: error_kind.clone(),
            media_type: media_type.clone(),
            charset: charset.clone(),
            extraction: (!extraction.is_empty()).then(|| extraction.clone()),
            snapshot_id: snapshot_id.clone(),
            source_byte_count: Some(*source_byte_count),
            output_byte_count: Some(*output_byte_count),
            rendered_byte_count: Some(*byte_count),
            line_count: Some(*line_count),
            pattern: pattern.as_deref().map(mermaid_model::utils::redact_secrets),
            context_lines: *context_lines,
            match_count: *match_count,
            query_count: None,
            succeeded_count: None,
            requested_count: None,
            result_count: None,
            failed_count: 0,
            failures: Vec::new(),
            partial: false,
            truncated: *truncated,
        }),
        _ => None,
    }
}

/// Stable string form of a tool status.
fn status_str(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Success => "success",
        ToolStatus::Error => "error",
        ToolStatus::Cancelled => "cancelled",
    }
}

/// Stable string form of a finish reason (mirrors the `FinishReason` serde tags).
fn finish_reason_str(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop".to_string(),
        FinishReason::ToolUse => "tool_use".to_string(),
        FinishReason::Length => "length".to_string(),
        FinishReason::ContentFilter => "content_filter".to_string(),
        FinishReason::Other(other) => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ToolOutcome;
    use mermaid_model::ids::{ToolCallId, TurnId};
    use mermaid_model::models::TokenUsage;
    use mermaid_model::tool_run::ToolRunMetadata;

    /// One canonical value per variant, in declaration order.
    fn samples() -> Vec<RunEvent> {
        vec![
            RunEvent::SessionStarted {
                protocol_version: RUN_EVENT_PROTOCOL_VERSION,
                cli_version: "9.9.9".to_string(),
                model: "anthropic/claude-x".to_string(),
                task_id: None,
                session_id: "20260709_120000_000".to_string(),
            },
            RunEvent::Text {
                delta: "hello".to_string(),
            },
            RunEvent::Reasoning {
                delta: "thinking".to_string(),
            },
            RunEvent::ToolStarted {
                call_id: "tool#3".to_string(),
            },
            RunEvent::ToolFinished {
                call_id: "tool#3".to_string(),
                name: "execute_command".to_string(),
                status: "success".to_string(),
                summary: "command completed".to_string(),
                error: None,
                plan: None,
                web: None,
            },
            RunEvent::ApprovalRequired {
                call_id: "tool#4".to_string(),
                tool: "execute_command".to_string(),
                risk: "network".to_string(),
                prompt: "Run curl?".to_string(),
            },
            RunEvent::TasksUpdated {
                tasks: vec![TaskLine {
                    id: 1,
                    subject: "wire the broker".to_string(),
                    status: "completed".to_string(),
                    active_form: "wiring the broker".to_string(),
                    elapsed_secs: Some(130),
                    tokens_spent: Some(8400),
                    origin: Some("model".to_string()),
                }],
                completed: 1,
                total: 1,
            },
            RunEvent::Error {
                message: "connection failed".to_string(),
            },
            RunEvent::TurnDone {
                total_tokens: Some(1234),
                stop_reason: Some("stop".to_string()),
            },
            RunEvent::Result {
                response: "Hi there".to_string(),
                reasoning: None,
                total_tokens: 1234,
                errors: vec![],
                session_id: "20260709_120000_000".to_string(),
                structured_output: None,
            },
        ]
    }

    /// The frozen wire form of each variant. The exhaustive match (no `_ =>`) is
    /// the compile-time drift guard: a new variant forces a pinned string here,
    /// exactly mirroring `app::recorder`'s per-`MsgKind` sample guard.
    fn golden(ev: &RunEvent) -> &'static str {
        match ev {
            RunEvent::SessionStarted { .. } => {
                r#"{"type":"session_started","protocol_version":1,"cli_version":"9.9.9","model":"anthropic/claude-x","task_id":null,"session_id":"20260709_120000_000"}"#
            },
            RunEvent::Text { .. } => r#"{"type":"text","delta":"hello"}"#,
            RunEvent::Reasoning { .. } => r#"{"type":"reasoning","delta":"thinking"}"#,
            RunEvent::ToolStarted { .. } => r#"{"type":"tool_started","call_id":"tool#3"}"#,
            RunEvent::ToolFinished { .. } => {
                r#"{"type":"tool_finished","call_id":"tool#3","name":"execute_command","status":"success","summary":"command completed","error":null}"#
            },
            RunEvent::ApprovalRequired { .. } => {
                r#"{"type":"approval_required","call_id":"tool#4","tool":"execute_command","risk":"network","prompt":"Run curl?"}"#
            },
            RunEvent::TasksUpdated { .. } => {
                r#"{"type":"tasks_updated","tasks":[{"id":1,"subject":"wire the broker","status":"completed","active_form":"wiring the broker","elapsed_secs":130,"tokens_spent":8400,"origin":"model"}],"completed":1,"total":1}"#
            },
            RunEvent::Error { .. } => r#"{"type":"error","message":"connection failed"}"#,
            RunEvent::TurnDone { .. } => {
                r#"{"type":"turn_done","total_tokens":1234,"stop_reason":"stop"}"#
            },
            RunEvent::Result { .. } => {
                r#"{"type":"result","response":"Hi there","reasoning":null,"total_tokens":1234,"errors":[],"session_id":"20260709_120000_000"}"#
            },
        }
    }

    #[test]
    fn wire_format_is_frozen() {
        for ev in samples() {
            assert_eq!(
                serde_json::to_string(&ev).unwrap(),
                golden(&ev),
                "RunEvent wire format drifted for {ev:?}"
            );
        }
    }

    #[test]
    fn every_variant_round_trips() {
        for ev in samples() {
            let wire = serde_json::to_string(&ev).unwrap();
            let back: RunEvent = serde_json::from_str(&wire).unwrap();
            assert_eq!(back, ev);
        }
    }

    #[test]
    fn every_variant_has_a_sample() {
        // Backstop for `golden`'s compile-time guard: keep one sample per
        // variant. Bump the count when a variant lands (and add its golden
        // line above, which won't compile otherwise).
        assert_eq!(samples().len(), 10);
    }

    #[test]
    fn result_structured_output_is_additive() {
        // Present -> serialized; absent -> omitted entirely (golden above
        // stays frozen); old wire lines without the field still parse.
        let ev = RunEvent::Result {
            response: "{\"answer\":42}".to_string(),
            reasoning: None,
            total_tokens: 10,
            errors: vec![],
            session_id: "s".to_string(),
            structured_output: Some(serde_json::json!({"answer": 42})),
        };
        let wire = serde_json::to_string(&ev).unwrap();
        assert!(
            wire.contains("\"structured_output\":{\"answer\":42}"),
            "{wire}"
        );
        let back: RunEvent = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, ev);
        let old = r#"{"type":"result","response":"x","reasoning":null,"total_tokens":1,"errors":[],"session_id":"s"}"#;
        let parsed: RunEvent = serde_json::from_str(old).unwrap();
        assert!(matches!(
            parsed,
            RunEvent::Result {
                structured_output: None,
                ..
            }
        ));
    }

    #[test]
    fn protocol_version_is_pinned() {
        assert_eq!(RUN_EVENT_PROTOCOL_VERSION, 1);
    }

    #[test]
    fn tool_name_maps_each_metadata_kind() {
        assert_eq!(tool_name(&ToolMetadata::None), "tool");
        assert_eq!(
            tool_name(&ToolMetadata::Mcp {
                server: "srv".to_string(),
                tool: "do".to_string(),
            }),
            "srv/do"
        );
        assert_eq!(
            tool_name(&ToolMetadata::Custom {
                name: "weird".to_string(),
                data: serde_json::Value::Null,
            }),
            "weird"
        );
    }

    #[test]
    fn from_msg_projects_streamed_and_drops_the_rest() {
        let text = Msg::StreamText {
            turn: TurnId(1),
            chunk: "hi".to_string(),
        };
        assert_eq!(
            RunEvent::from_msg(&text),
            Some(RunEvent::Text {
                delta: "hi".to_string()
            })
        );

        // A message with no SDK projection returns None.
        assert_eq!(RunEvent::from_msg(&Msg::Tick), None);

        let done = Msg::StreamDone {
            turn: TurnId(1),
            usage: Some(TokenUsage::provider(10, 20)),
            provider_continuation: None,
            stop_reason: Some(FinishReason::Stop),
        };
        assert_eq!(
            RunEvent::from_msg(&done),
            Some(RunEvent::TurnDone {
                total_tokens: Some(30),
                stop_reason: Some("stop".to_string()),
            })
        );
    }

    #[test]
    fn from_msg_projects_tool_finished_with_name_from_metadata() {
        let outcome = ToolOutcome {
            status: ToolStatus::Success,
            summary: "command completed".to_string(),
            model_content: "out".to_string(),
            error: None,
            metadata: Box::new(ToolRunMetadata {
                detail: ToolMetadata::ExecuteCommand {
                    command: "ls".to_string(),
                    working_dir: None,
                    exit_code: Some(0),
                    timed_out: false,
                    background: false,
                    stdout_lines: 1,
                    stderr_lines: 0,
                    detected_urls: vec![],
                    pid: None,
                    log_path: None,
                    denied_by_sandbox: false,
                },
                ..ToolRunMetadata::default()
            }),
            artifacts: vec![],
            duration_secs: Some(0.0),
        };
        let finished = Msg::ToolFinished {
            turn: TurnId(1),
            call_id: ToolCallId(3),
            outcome,
        };
        assert_eq!(
            RunEvent::from_msg(&finished),
            Some(RunEvent::ToolFinished {
                call_id: "tool#3".to_string(),
                name: "execute_command".to_string(),
                status: "success".to_string(),
                summary: "command completed".to_string(),
                error: None,
                plan: None,
                web: None,
            })
        );
    }

    #[test]
    fn web_fetch_event_exposes_bounded_structured_provenance() {
        let outcome = ToolOutcome::success("page", "fetched", 0.1).with_metadata(ToolRunMetadata {
            detail: ToolMetadata::WebFetch {
                url: "https://example.test/start".to_string(),
                final_url: Some("https://example.test/final".to_string()),
                status: Some(200),
                error_kind: None,
                media_type: Some("text/html".to_string()),
                charset: Some("utf-8".to_string()),
                backend: "native".to_string(),
                extraction: "readability".to_string(),
                title: Some("Example".to_string()),
                line_count: 3,
                byte_count: 100,
                source_byte_count: 400,
                output_byte_count: 240,
                truncated: true,
                pattern: Some("needle".to_string()),
                context_lines: Some(2),
                match_count: Some(3),
                snapshot_id: Some("web-1".to_string()),
            },
            ..ToolRunMetadata::default()
        });
        let event = RunEvent::from_msg(&Msg::ToolFinished {
            turn: TurnId(1),
            call_id: ToolCallId(9),
            outcome,
        })
        .expect("mapped");
        let RunEvent::ToolFinished {
            name,
            web: Some(web),
            ..
        } = event
        else {
            panic!("expected structured web event");
        };
        assert_eq!(name, "web_fetch");
        assert_eq!(web.backend, "native");
        assert_eq!(web.status, Some(200));
        assert!(web.error_kind.is_none());
        assert_eq!(web.final_url.as_deref(), Some("https://example.test/final"));
        assert_eq!(web.source_byte_count, Some(400));
        assert_eq!(web.output_byte_count, Some(240));
        assert_eq!(web.rendered_byte_count, Some(100));
        assert_eq!(web.pattern.as_deref(), Some("needle"));
        assert_eq!(web.match_count, Some(3));
        assert!(web.truncated);
    }

    #[test]
    fn web_fetch_error_event_keeps_typed_failure_context() {
        let outcome =
            ToolOutcome::error("backend unavailable", 0.1).with_metadata(ToolRunMetadata {
                detail: ToolMetadata::WebFetch {
                    url: "https://example.test/start".to_string(),
                    final_url: None,
                    status: Some(503),
                    error_kind: Some("http_status".to_string()),
                    media_type: None,
                    charset: None,
                    backend: "native".to_string(),
                    extraction: String::new(),
                    title: None,
                    line_count: 0,
                    byte_count: 0,
                    source_byte_count: 0,
                    output_byte_count: 0,
                    truncated: false,
                    pattern: None,
                    context_lines: None,
                    match_count: None,
                    snapshot_id: None,
                },
                ..ToolRunMetadata::default()
            });
        let event = RunEvent::from_msg(&Msg::ToolFinished {
            turn: TurnId(1),
            call_id: ToolCallId(10),
            outcome,
        })
        .expect("mapped");
        let RunEvent::ToolFinished { web: Some(web), .. } = event else {
            panic!("expected structured web event");
        };
        assert_eq!(web.status, Some(503));
        assert_eq!(web.error_kind.as_deref(), Some("http_status"));
        assert_eq!(web.backend, "native");
        assert!(web.final_url.is_none());
    }

    #[test]
    fn web_search_event_keeps_partial_failure_and_backend_context() {
        let outcome = ToolOutcome::success("results", "3 results returned", 0.1).with_metadata(
            ToolRunMetadata {
                detail: ToolMetadata::WebSearch {
                    queries: vec!["first".to_string(), "second".to_string()],
                    requested_count: 10,
                    result_count: 3,
                    sources: vec!["https://example.test/result".to_string()],
                    backend: "managed_searxng".to_string(),
                    succeeded_queries: 1,
                    failed_queries: 1,
                    partial: true,
                    truncated: true,
                    failures: vec![crate::WebSearchFailure {
                        query_index: 1,
                        error: "upstream timed out".to_string(),
                    }],
                },
                ..ToolRunMetadata::default()
            },
        );
        let event = RunEvent::from_msg(&Msg::ToolFinished {
            turn: TurnId(1),
            call_id: ToolCallId(11),
            outcome,
        })
        .expect("mapped");
        let RunEvent::ToolFinished {
            name,
            web: Some(web),
            ..
        } = event
        else {
            panic!("expected structured web event");
        };
        assert_eq!(name, "web_search");
        assert_eq!(web.backend, "managed_searxng");
        assert_eq!(web.query_count, Some(2));
        assert_eq!(web.succeeded_count, Some(1));
        assert_eq!(web.failed_count, 1);
        assert_eq!(web.result_count, Some(3));
        assert!(web.partial);
        assert!(web.truncated);
        assert_eq!(web.failures.len(), 1);
        assert_eq!(web.failures[0].query_index, 1);
    }

    #[test]
    fn approved_plan_rides_tool_finished_as_an_additive_payload() {
        let outcome =
            ToolOutcome::success("approved", "plan approved", 0.1).with_metadata(ToolRunMetadata {
                detail: ToolMetadata::Plan {
                    path: ".mermaid/plans/x.md".to_string(),
                    body: "## Summary".to_string(),
                    start: true,
                    fresh: true,
                    fork: false,
                    model: None,
                },
                ..ToolRunMetadata::default()
            });
        let event = RunEvent::from_msg(&Msg::ToolFinished {
            turn: TurnId(1),
            call_id: ToolCallId(7),
            outcome,
        })
        .expect("mapped");
        let RunEvent::ToolFinished { name, plan, .. } = &event else {
            panic!("expected ToolFinished, got {event:?}");
        };
        assert_eq!(name, "exit_plan_mode");
        let plan = plan.as_ref().expect("plan payload");
        assert_eq!(plan.path, ".mermaid/plans/x.md");
        assert!(plan.start && plan.fresh && !plan.fork);
        // The wire stays clean for every other tool: `plan` is omitted, not
        // null, so existing consumers see byte-identical lines.
        let json = serde_json::to_string(&RunEvent::ToolFinished {
            call_id: "tool#1".to_string(),
            name: "read_file".to_string(),
            status: "success".to_string(),
            summary: "read".to_string(),
            error: None,
            plan: None,
            web: None,
        })
        .unwrap();
        assert!(!json.contains("\"plan\""));
    }

    fn action(action_type: &str, target: &str, result: ActionResult) -> ActionDisplay {
        ActionDisplay {
            action_type: action_type.to_string(),
            target: target.to_string(),
            result,
            details: mermaid_model::action::ActionDetails::Simple,
            duration_seconds: Some(0.1),
            metadata: Some(ToolRunMetadata {
                detail: ToolMetadata::ReadFile {
                    paths: vec![target.to_string()],
                    line_count: 3,
                    byte_count: 30,
                    truncated: false,
                },
                ..ToolRunMetadata::default()
            }),
        }
    }

    fn ok(output: &str) -> ActionResult {
        ActionResult::Success {
            output: output.to_string(),
            images: None,
        }
    }

    /// The whole point of the catch-up: what a from-now attach missed comes
    /// back as coarse lines of the same wire — reasoning, answer text, and
    /// the tool pairs — in the order the session committed them.
    #[test]
    fn catch_up_replays_the_committed_transcript_as_wire_lines() {
        let mut assistant = ChatMessage::assistant("the answer");
        assistant.thinking = Some("first, think".to_string());
        let events = vec![
            SessionEvent::Message {
                message: ChatMessage::user("do the thing"),
            },
            SessionEvent::Message { message: assistant },
            SessionEvent::Action {
                action: Box::new(action("Read", "src/main.rs", ok("contents"))),
            },
        ];

        let replay = RunEvent::catch_up(&events);

        // The user turn has no projection — the live stream never carried
        // one, so replaying it would invent content.
        assert_eq!(
            replay,
            vec![
                RunEvent::Reasoning {
                    delta: "first, think".to_string()
                },
                RunEvent::Text {
                    delta: "the answer".to_string()
                },
                RunEvent::ToolStarted {
                    call_id: "catchup-1".to_string()
                },
                RunEvent::ToolFinished {
                    call_id: "catchup-1".to_string(),
                    // Off the same run metadata the live projection reads.
                    name: "read_file".to_string(),
                    status: "success".to_string(),
                    summary: "Read src/main.rs".to_string(),
                    error: None,
                    plan: None,
                    web: None,
                },
            ]
        );
    }

    /// A failed call replays as an error line, not a silent success.
    #[test]
    fn catch_up_carries_a_failed_tool_run() {
        let events = vec![SessionEvent::Action {
            action: Box::new(action(
                "Read",
                "nope.rs",
                ActionResult::Error {
                    error: "no such file".to_string(),
                },
            )),
        }];

        assert_eq!(
            finished(&RunEvent::catch_up(&events)[1]),
            Some(("read_file", "error", Some("no such file")))
        );
    }

    /// `(name, status, error)` of a `tool_finished` line, or `None` for
    /// anything else — so an assertion can name what it expects instead of
    /// destructuring.
    fn finished(event: &RunEvent) -> Option<(&str, &str, Option<&str>)> {
        if let RunEvent::ToolFinished {
            name,
            status,
            error,
            ..
        } = event
        {
            Some((name, status, error.as_deref()))
        } else {
            None
        }
    }

    /// The `delta` of every `text` line, in order.
    fn texts(events: &[RunEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| {
                if let RunEvent::Text { delta } = event {
                    Some(delta.as_str())
                } else {
                    None
                }
            })
            .collect()
    }

    /// An assigning event replaces the transcript, so the catch-up describes
    /// what the session HOLDS, not everything it ever held — the same rule
    /// `fold_session` applies. It is also what keeps a replacement message's
    /// inline actions from replaying a second time.
    #[test]
    fn catch_up_clears_at_a_compaction_boundary() {
        let mut summary = ChatMessage::assistant("summary of the above");
        summary.actions = vec![action("Read", "src/main.rs", ok("contents"))];
        let events = vec![
            SessionEvent::Message {
                message: ChatMessage::assistant("a long first turn"),
            },
            SessionEvent::Action {
                action: Box::new(action("Read", "src/main.rs", ok("contents"))),
            },
            SessionEvent::Compaction {
                at: chrono::Local::now(),
                record: compaction_record(),
                replacement: vec![summary],
            },
            SessionEvent::Message {
                message: ChatMessage::assistant("and on we go"),
            },
        ];

        let replay = RunEvent::catch_up(&events);

        assert_eq!(texts(&replay), vec!["summary of the above", "and on we go"]);
        // Exactly one tool pair survives: the replacement's inline action.
        // The pre-compaction `action` event went with the messages it
        // belonged to.
        let started: Vec<&str> = replay
            .iter()
            .filter_map(|event| {
                if let RunEvent::ToolStarted { call_id } = event {
                    Some(call_id.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            started,
            vec!["catchup-2"],
            "call ids stay unique per attach"
        );
    }

    /// Every log carries a checklist event from its backfill, so an empty
    /// one must not put a `tasks_updated { total: 0 }` on the front of every
    /// catch-up. A subscriber that has never seen a list cannot tell an
    /// empty one from none.
    #[test]
    fn catch_up_stays_quiet_about_an_empty_checklist() {
        let events = vec![SessionEvent::Tasks {
            store: crate::checklist::ChecklistStore::default(),
        }];
        assert!(RunEvent::catch_up(&events).is_empty());
    }

    /// The checklist is session state, not transcript: it survives a clear,
    /// and only the newest snapshot is worth sending.
    #[test]
    fn catch_up_ends_with_the_latest_checklist_only() {
        let first = checklist(&["write the test"]);
        let second = checklist(&["write the test", "ship it"]);
        let events = vec![
            SessionEvent::Tasks { store: first },
            SessionEvent::Reset {
                at: chrono::Local::now(),
                messages: vec![ChatMessage::assistant("fresh start")],
            },
            SessionEvent::Tasks { store: second },
        ];

        let replay = RunEvent::catch_up(&events);

        assert_eq!(replay.len(), 2, "one text line, one checklist: {replay:?}");
        assert_eq!(texts(&replay), vec!["fresh start"]);
        let counted = replay.iter().find_map(|event| {
            if let RunEvent::TasksUpdated {
                completed, total, ..
            } = event
            {
                Some((*completed, *total))
            } else {
                None
            }
        });
        assert_eq!(
            counted,
            Some((0, 2)),
            "the checklist lands last: {replay:?}"
        );
    }

    /// Identity, scalars, prompt history, and screenshots are not stream
    /// content — a log of nothing else replays as nothing.
    #[test]
    fn catch_up_projects_nothing_for_the_non_transcript_events() {
        let events = vec![
            SessionEvent::Started {
                session_id: "20260709_120000_000".to_string(),
                project_path: "/tmp/proj".to_string(),
                model_id: "ollama/test".to_string(),
                created_at: chrono::Local::now(),
                forked_from: None,
                parent_session: None,
            },
            SessionEvent::Input {
                text: "do the thing".to_string(),
            },
            SessionEvent::Image {
                data: "AAAA".to_string(),
            },
        ];
        assert!(RunEvent::catch_up(&events).is_empty());
    }

    fn checklist(subjects: &[&str]) -> crate::checklist::ChecklistStore {
        let mut store = crate::checklist::ChecklistStore::default();
        store.create(
            subjects
                .iter()
                .map(|subject| crate::checklist::ChecklistSpec {
                    subject: (*subject).to_string(),
                    active_form: format!("{subject}…"),
                    description: None,
                    in_progress: false,
                })
                .collect(),
            crate::checklist::ChecklistOrigin::Model,
            crate::checklist::Stamp {
                now_epoch: 1_700_000_000,
                run_tokens: 0,
            },
        );
        store
    }

    fn compaction_record() -> crate::compaction::CompactionEvent {
        crate::compaction::CompactionEvent {
            id: "c1".to_string(),
            trigger: crate::compaction::CompactionTrigger::Manual,
            created_at: chrono::Local::now(),
            before_tokens: 1000,
            after_tokens: 100,
            archived_message_count: 2,
            preserved_message_count: 1,
            preserved_turn_count: 1,
            summary_tokens: 90,
            duration_secs: 1.5,
            review_status: crate::compaction::CompactionReviewStatus::Reviewed,
            review_error: None,
            focus: None,
            archive_path: None,
        }
    }

    /// A pre-metadata action still names itself on the wire.
    #[test]
    fn catch_up_falls_back_to_the_display_kind_when_metadata_is_gone() {
        let mut legacy = action("Web Search", "rust lifetimes", ok("3 results"));
        legacy.metadata = None;
        let replay = RunEvent::catch_up(&[SessionEvent::Action {
            action: Box::new(legacy),
        }]);
        assert_eq!(
            finished(&replay[1]),
            Some(("web_search", "success", None)),
            "the display kind normalizes onto the wire: {replay:?}"
        );
    }
}
