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
use super::runtime::{ToolMetadata, ToolStatus};
use crate::models::FinishReason;

/// Wire-format version of the `RunEvent` stream. Bump only on a breaking change
/// to an existing variant's shape; additive variants keep version 1.
pub const RUN_EVENT_PROTOCOL_VERSION: u32 = 1;

/// One line of the `mermaid run --format ndjson` stream. Internally tagged on
/// `type` (snake_case), matching the house style for stable wire unions
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
        /// (defaulted) — protocol_version stays 1.
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

impl RunEvent {
    /// Project a lifecycle [`Msg`] into a public `RunEvent`, or `None` for the
    /// many messages that have no place in the SDK stream.
    ///
    /// Pure and stateless: tool identity is read from the finished outcome's
    /// metadata rather than correlated across messages, so no projector state
    /// is required. The wildcard arm is intentional here (this is a lossy
    /// projection, not the reducer — most `Msg`s are deliberately dropped).
    pub fn from_msg(msg: &Msg) -> Option<RunEvent> {
        Some(match msg {
            Msg::StreamText { chunk, .. } => RunEvent::Text {
                delta: chunk.clone(),
            },
            Msg::StreamReasoning { chunk, .. } => RunEvent::Reasoning {
                delta: chunk.text.clone(),
            },
            Msg::ToolStarted { call_id, .. } => RunEvent::ToolStarted {
                call_id: call_id.to_string(),
            },
            Msg::ToolFinished {
                call_id, outcome, ..
            } => RunEvent::ToolFinished {
                call_id: call_id.to_string(),
                name: tool_name(&outcome.metadata.detail),
                status: status_str(outcome.status).to_string(),
                summary: outcome.summary.clone(),
                error: outcome.error.clone(),
                plan: match &outcome.metadata.detail {
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
                },
            },
            Msg::ApprovalRequested {
                call_id,
                tool,
                risk,
                prompt,
                ..
            } => RunEvent::ApprovalRequired {
                call_id: call_id.to_string(),
                tool: tool.clone(),
                risk: risk.clone(),
                prompt: prompt.clone(),
            },
            Msg::TasksUpdated { store } => {
                let (completed, total) = store.counts();
                RunEvent::TasksUpdated {
                    tasks: store.visible().map(TaskLine::from).collect(),
                    completed: completed as u32,
                    total: total as u32,
                }
            },
            Msg::UpstreamError { error, .. } => RunEvent::Error {
                message: error.message.clone(),
            },
            Msg::StreamDone {
                usage, stop_reason, ..
            } => RunEvent::TurnDone {
                total_tokens: usage.as_ref().map(|u| u.total_tokens() as u64),
                stop_reason: stop_reason.as_ref().map(finish_reason_str),
            },
            _ => return None,
        })
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
    /// Seconds spent in_progress→completed, when both stamps exist.
    #[serde(default)]
    pub elapsed_secs: Option<u64>,
    /// Completion tokens attributed while in progress, when known.
    #[serde(default)]
    pub tokens_spent: Option<u64>,
    /// `model` or `user` (a `/tasks add` entry).
    #[serde(default)]
    pub origin: Option<String>,
}

impl From<&crate::domain::TaskItem> for TaskLine {
    fn from(task: &crate::domain::TaskItem) -> Self {
        Self {
            id: task.id,
            subject: task.subject.clone(),
            status: task.status.as_str().to_string(),
            active_form: task.active_form.clone(),
            elapsed_secs: task.elapsed_secs(),
            tokens_spent: task.tokens_spent,
            origin: Some(
                match task.origin {
                    crate::domain::TaskOrigin::Model => "model",
                    crate::domain::TaskOrigin::User => "user",
                }
                .to_string(),
            ),
        }
    }
}

/// Snake_case name of a finished tool, from its run-metadata tag. The exhaustive
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
    use crate::domain::ids::{ToolCallId, TurnId};
    use crate::domain::runtime::ToolRunMetadata;
    use crate::domain::state::ToolOutcome;
    use crate::models::TokenUsage;

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
            })
        );
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
        })
        .unwrap();
        assert!(!json.contains("\"plan\""));
    }
}
