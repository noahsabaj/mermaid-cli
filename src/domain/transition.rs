//! Helpers that enforce invariants during turn-state transitions.
//!
//! The reducer calls these so the type system — not a comment or a
//! convention — guarantees that you can't transition to the
//! follow-up model call with missing tool outcomes, or commit a
//! partial assistant message that's still streaming, or drop a
//! thinking signature that the next request needs.
//!
//! Everything here is pure and sync.

use std::time::SystemTime;

use crate::models::tool_call::ToolCall as ModelToolCall;
use crate::models::{ChatMessage, MessageRole};

use super::action::{ActionDetails, ActionDisplay, ActionResult};
use super::ids::{ToolCallId, TurnId};
use super::runtime::ToolMetadata;
use super::state::{GenPhase, PendingToolCall, ToolOutcome, TurnState};

/// Flatten `Vec<Option<ToolOutcome>>` into `Vec<ToolOutcome>` iff
/// every slot is populated. `None` means "still waiting on at least
/// one tool" — the reducer stays in `ExecutingTools` and drops the
/// event without state change.
///
/// This is the single gate between `ExecutingTools` and the follow-up
/// `Generating`. It's impossible to bypass: there is no public
/// constructor for `Vec<ToolOutcome>` elsewhere in the codebase, and
/// the follow-up transition's builder function takes `Vec<ToolOutcome>`
/// by value.
pub fn try_complete_outcomes(outcomes: &[Option<ToolOutcome>]) -> Option<Vec<ToolOutcome>> {
    let mut out = Vec::with_capacity(outcomes.len());
    for slot in outcomes {
        match slot {
            Some(o) => out.push(o.clone()),
            None => return None,
        }
    }
    Some(out)
}

/// Write the outcome for a specific tool call ID into the slot
/// carrying that call. Returns `true` if the slot was found and empty;
/// `false` if the call isn't pending (stale event) or was already
/// filled (duplicate event — first write wins).
pub fn fill_outcome(
    calls: &[PendingToolCall],
    outcomes: &mut [Option<ToolOutcome>],
    call_id: ToolCallId,
    outcome: ToolOutcome,
) -> bool {
    debug_assert_eq!(
        calls.len(),
        outcomes.len(),
        "calls and outcomes must be aligned"
    );
    let Some(idx) = calls.iter().position(|c| c.call_id == call_id) else {
        return false;
    };
    if outcomes[idx].is_some() {
        return false;
    }
    outcomes[idx] = Some(outcome);
    true
}

/// Transition `Idle → Generating`. Always pure: the caller builds a
/// `ChatRequest` separately and returns it to the reducer as a `Cmd`.
/// `now` is the reducer step's injected clock (`state.now`), so the
/// `started` stamp is deterministic on replay rather than read live (Cause 3).
pub fn start_generating(id: TurnId, now: SystemTime) -> TurnState {
    TurnState::Generating {
        id,
        started: now,
        partial_text: String::new(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Sending,
        thinking_signature: None,
        pending_tool_calls: Vec::new(),
    }
}

/// Transition `Generating → ExecutingTools`. Allocates `None` slots
/// for every call so the invariant ("outcomes.len() == calls.len()")
/// is upheld by construction. `now` is the reducer step's injected clock
/// (`state.now`) so `started` is deterministic on replay (Cause 3).
pub fn start_executing_tools(
    id: TurnId,
    calls: Vec<PendingToolCall>,
    now: SystemTime,
) -> TurnState {
    let outcomes = vec![None; calls.len()];
    TurnState::ExecutingTools {
        id,
        started: now,
        calls,
        outcomes,
    }
}

/// Build the committed assistant message from a `Generating` state's
/// accumulated content. Safe to call with empty text (the model might
/// have responded with only tool calls). Returns the message plus the
/// thinking signature so the reducer can record it separately for
/// Anthropic round-trip.
pub fn commit_assistant_message(
    partial_text: String,
    partial_reasoning: String,
    tool_calls: Vec<ModelToolCall>,
    thinking_signature: Option<String>,
    now: chrono::DateTime<chrono::Local>,
) -> ChatMessage {
    let thinking = if partial_reasoning.is_empty() {
        None
    } else {
        Some(partial_reasoning)
    };
    let mut msg = ChatMessage {
        role: MessageRole::Assistant,
        content: partial_text,
        timestamp: now,
        kind: crate::models::ChatMessageKind::Normal,
        metadata: None,
        actions: Vec::new(),
        thinking,
        images: None,
        image_numbers: None,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
        tool_name: None,
        thinking_signature: None,
    };
    if let Some(sig) = thinking_signature {
        msg = msg.with_thinking_signature(sig);
    }
    msg
}

/// Build the follow-up `tool` role messages from completed outcomes.
/// The OpenAI-compatible wire format requires (tool_call_id, tool_name,
/// content) — we pull name from the original call.
pub fn tool_result_messages(
    calls: &[PendingToolCall],
    outcomes: Vec<ToolOutcome>,
) -> Vec<ChatMessage> {
    debug_assert_eq!(calls.len(), outcomes.len());
    calls
        .iter()
        .zip(outcomes)
        .map(|(call, outcome)| {
            let tool_call_id = call
                .source
                .id
                .clone()
                .unwrap_or_else(|| format!("call_{}", call.call_id.0));
            ChatMessage::tool(
                tool_call_id,
                call.source.function.name.clone(),
                outcome.as_tool_message_content(),
            )
        })
        .collect()
}

/// Convert a completed tool outcome into an `ActionDisplay` entry
/// attached to the assistant message that triggered the call. Used so
/// the chat renderer can show "Read main.rs → 1,234 bytes" etc.
pub fn action_display_for(call: &PendingToolCall, outcome: &ToolOutcome) -> ActionDisplay {
    let (action_type, target) = display_info_for(call);
    let duration = outcome.duration_secs;
    let result = if outcome.is_success() {
        ActionResult::Success {
            output: outcome.output().to_string(),
            images: outcome.images(),
        }
    } else {
        ActionResult::Error {
            error: outcome.error_message().unwrap_or("[cancelled]").to_string(),
        }
    };
    let details = action_details_for(call, outcome, duration);
    ActionDisplay {
        action_type,
        target,
        result,
        details,
        duration_seconds: duration,
        metadata: Some((*outcome.metadata).clone()),
    }
}

fn action_details_for(
    call: &PendingToolCall,
    outcome: &ToolOutcome,
    duration: Option<f64>,
) -> ActionDetails {
    if !outcome.is_success() {
        return ActionDetails::Simple;
    }

    let name = call.source.function.name.as_str();
    let args = &call.source.function.arguments;
    match name {
        "read_file" => {
            let line_count = outcome
                .metadata
                .line_count
                .or_else(|| metadata_line_count(&outcome.metadata.detail))
                .unwrap_or_else(|| outcome.output().lines().count());
            ActionDetails::Preview {
                text: success_summary(
                    format!("{} {} read", line_count, pluralize("line", line_count)),
                    duration,
                ),
                line_count: Some(line_count),
            }
        },
        "write_file" => {
            let line_count = outcome
                .metadata
                .line_count
                .or_else(|| metadata_line_count(&outcome.metadata.detail))
                .or_else(|| {
                    args.get("content")
                        .and_then(|v| v.as_str())
                        .map(|content| content.lines().count())
                })
                .unwrap_or(0);
            if let Some(diff) = outcome.metadata.display_diff.clone() {
                ActionDetails::Diff {
                    summary: diff_success_summary(&diff, duration),
                    diff,
                }
            } else {
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                ActionDetails::FileContent {
                    line_count,
                    content,
                }
            }
        },
        "web_search" => {
            let result_count = outcome
                .metadata
                .result_count
                .or_else(|| metadata_result_count(&outcome.metadata.detail))
                .unwrap_or_else(|| count_search_results(outcome.output()));
            ActionDetails::Preview {
                text: success_summary(
                    format!(
                        "{} {} returned",
                        result_count,
                        pluralize("result", result_count)
                    ),
                    duration,
                ),
                line_count: None,
            }
        },
        "web_fetch" => {
            let line_count = outcome
                .metadata
                .line_count
                .or_else(|| metadata_line_count(&outcome.metadata.detail))
                .unwrap_or_else(|| outcome.output().lines().count());
            ActionDetails::Preview {
                text: success_summary(
                    format!("{} {} fetched", line_count, pluralize("line", line_count)),
                    duration,
                ),
                line_count: Some(line_count),
            }
        },
        "execute_command" => ActionDetails::Preview {
            text: command_success_summary(outcome, duration),
            line_count: outcome
                .metadata
                .line_count
                .or_else(|| metadata_line_count(&outcome.metadata.detail))
                .or_else(|| Some(outcome.output().lines().count())),
        },
        "edit_file" => {
            if let Some(diff) = outcome.metadata.display_diff.clone() {
                ActionDetails::Diff {
                    summary: diff_success_summary(&diff, duration),
                    diff,
                }
            } else {
                ActionDetails::Preview {
                    text: success_summary(outcome.summary.clone(), duration),
                    line_count: None,
                }
            }
        },
        "agent" => {
            // Surface what the child actually cost and which model ran it —
            // the child's usage is real spend the parent's own counters would
            // otherwise hide.
            let mut bits = Vec::new();
            if let Some(usage) = outcome.metadata.token_usage.as_ref()
                && usage.total_tokens > 0
            {
                bits.push(format!(
                    "{} tokens",
                    super::compaction::format_compact_count(usage.total_tokens)
                ));
            }
            if let ToolMetadata::Subagent { model_id, agent_id } = &outcome.metadata.detail {
                if !model_id.is_empty() {
                    bits.push(model_id.clone());
                }
                if !agent_id.is_empty() {
                    bits.push(agent_id.clone());
                }
            }
            let detail = if bits.is_empty() {
                "subagent finished".to_string()
            } else {
                bits.join(" · ")
            };
            ActionDetails::Preview {
                text: success_summary(detail, duration),
                line_count: None,
            }
        },
        _ => ActionDetails::Simple,
    }
}

fn diff_success_summary(diff: &str, duration: Option<f64>) -> String {
    let (added, removed) = diff_counts(diff);
    success_summary(format!("+{} -{}", added, removed), duration)
}

fn diff_counts(diff: &str) -> (usize, usize) {
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in diff.lines() {
        match crate::render::diff::parse_diff_line(line) {
            crate::render::diff::DiffLineKind::Added => added += 1,
            crate::render::diff::DiffLineKind::Removed => removed += 1,
            crate::render::diff::DiffLineKind::Context => {},
        }
    }
    (added, removed)
}

fn metadata_line_count(metadata: &ToolMetadata) -> Option<usize> {
    match metadata {
        ToolMetadata::ReadFile { line_count, .. }
        | ToolMetadata::WriteFile { line_count, .. }
        | ToolMetadata::WebFetch { line_count, .. } => Some(*line_count),
        ToolMetadata::ExecuteCommand {
            stdout_lines,
            stderr_lines,
            ..
        } => Some(stdout_lines + stderr_lines),
        _ => None,
    }
}

fn metadata_result_count(metadata: &ToolMetadata) -> Option<usize> {
    match metadata {
        ToolMetadata::WebSearch { result_count, .. } => Some(*result_count),
        _ => None,
    }
}

fn success_summary(detail: String, duration: Option<f64>) -> String {
    match duration {
        Some(seconds) => format!("Success, {}, took {}", detail, format_duration(seconds)),
        None => format!("Success, {}", detail),
    }
}

fn command_success_summary(outcome: &ToolOutcome, duration: Option<f64>) -> String {
    if outcome.metadata.process.is_none() {
        return success_summary("command completed".to_string(), duration);
    }

    let mut lines = vec![success_summary(
        "background process started".to_string(),
        duration,
    )];
    for line in outcome.output().lines().skip(1) {
        if line.starts_with("--- startup output ---") {
            break;
        }
        if !line.trim().is_empty() {
            lines.push(line.to_string());
        }
    }
    lines.join("\n")
}

fn format_duration(seconds: f64) -> String {
    if seconds < 1.0 {
        format!("{}ms", (seconds * 1000.0).round().max(1.0) as u64)
    } else if seconds < 10.0 {
        format!("{:.1}s", seconds)
    } else {
        format!("{}s", seconds.round() as u64)
    }
}

/// Human-readable duration for a finished run: "Ns", "Mm Ss", or "Hh Mm".
pub fn format_run_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

fn pluralize(word: &str, count: usize) -> String {
    if count == 1 {
        word.to_string()
    } else {
        format!("{}s", word)
    }
}

fn count_search_results(output: &str) -> usize {
    output
        .lines()
        .filter(|line| line.starts_with('[') && line.contains("] Title:"))
        .count()
}

/// Best-effort name + target extraction from a tool call, for chat
/// display ("Read src/main.rs", "Bash cargo test", etc). Matches on
/// the wire-format tool name + arguments; unknown tools fall through
/// to the raw function name.
pub fn display_info_for(call: &PendingToolCall) -> (String, String) {
    let name = call.source.function.name.as_str();
    let args = &call.source.function.arguments;
    let string_arg =
        |k: &str| -> Option<String> { args.get(k).and_then(|v| v.as_str()).map(str::to_string) };
    match name {
        "read_file" => {
            let target = string_arg("path")
                .or_else(|| {
                    args.get("paths")
                        .and_then(|v| v.as_array())
                        .map(|a| match a.len() {
                            0 => "(no paths)".to_string(),
                            1 => a[0].as_str().unwrap_or("").to_string(),
                            n => format!("{} files", n),
                        })
                })
                .unwrap_or_default();
            ("Read".to_string(), target)
        },
        "write_file" => ("Write".to_string(), string_arg("path").unwrap_or_default()),
        "edit_file" => ("Edit".to_string(), string_arg("path").unwrap_or_default()),
        "delete_file" => ("Delete".to_string(), string_arg("path").unwrap_or_default()),
        "create_directory" => (
            "Bash".to_string(),
            format!("mkdir -p {}", string_arg("path").unwrap_or_default()),
        ),
        "execute_command" => (
            "Bash".to_string(),
            string_arg("command").unwrap_or_default(),
        ),
        "web_search" => {
            let target = string_arg("query")
                .or_else(|| {
                    args.get("queries")
                        .and_then(|v| v.as_array())
                        .map(|a| match a.len() {
                            0 => "(no queries)".to_string(),
                            1 => a[0]
                                .get("query")
                                .and_then(|q| q.as_str())
                                .unwrap_or("")
                                .to_string(),
                            n => format!("{} queries", n),
                        })
                })
                .unwrap_or_default();
            ("Web Search".to_string(), target)
        },
        "web_fetch" => (
            "Web Fetch".to_string(),
            string_arg("url").unwrap_or_default(),
        ),
        "memory" => {
            let action = string_arg("action").unwrap_or_default();
            let target = string_arg("id")
                .or_else(|| string_arg("name"))
                .unwrap_or_default();
            let scope = if args
                .get("global")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                " [global]"
            } else if args
                .get("shared")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                " [shared]"
            } else {
                ""
            };
            (
                "Memory".to_string(),
                format!("{action} {target}{scope}").trim().to_string(),
            )
        },
        "agent" => (
            "Agent".to_string(),
            string_arg("description").unwrap_or_default(),
        ),
        n if n.starts_with("mcp__") => {
            let rest = &n[5..];
            let target = rest.replacen("__", ":", 1);
            ("MCP".to_string(), target)
        },
        _ => (name.to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ManagedProcess, ManagedProcessStatus, ToolMetadata, ToolRunMetadata};
    use crate::models::tool_call::{FunctionCall, ToolCall as ModelToolCall};

    fn sample_call(id: u64, name: &str) -> PendingToolCall {
        sample_call_args(id, name, serde_json::json!({}))
    }

    fn sample_call_args(id: u64, name: &str, arguments: serde_json::Value) -> PendingToolCall {
        PendingToolCall {
            call_id: ToolCallId(id),
            source: ModelToolCall {
                id: Some(format!("c{}", id)),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments,
                },
            },
        }
    }

    #[test]
    fn agent_action_reports_child_model_and_tokens() {
        let call = sample_call_args(1, "agent", serde_json::json!({"description": "explore"}));
        let usage = crate::models::TokenUsage {
            prompt_tokens: 9_000,
            completion_tokens: 3_300,
            total_tokens: 12_300,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            source: Default::default(),
        };
        let outcome = ToolOutcome::success("the report", "subagent completed", 62.0).with_metadata(
            ToolRunMetadata {
                detail: ToolMetadata::Subagent {
                    model_id: "ollama/minimax-m3".to_string(),
                    agent_id: "a3".to_string(),
                },
                token_usage: Some(usage),
                ..Default::default()
            },
        );
        let action = action_display_for(&call, &outcome);
        match &action.details {
            ActionDetails::Preview { text, .. } => {
                assert!(text.starts_with("Success"), "got {text}");
                assert!(text.contains("12.3k tokens"), "got {text}");
                assert!(text.contains("ollama/minimax-m3"), "got {text}");
                assert!(text.contains("a3"), "the continuation handle shows: {text}");
            },
            other => panic!("expected Preview details, got {other:?}"),
        }
    }

    #[test]
    fn agent_action_without_usage_metadata_stays_plain() {
        // Old recordings / providers that report no usage: no "0 tokens".
        let call = sample_call_args(1, "agent", serde_json::json!({}));
        let outcome = ToolOutcome::success("report", "subagent completed", 2.0);
        let action = action_display_for(&call, &outcome);
        match &action.details {
            ActionDetails::Preview { text, .. } => {
                assert!(!text.contains("tokens"), "got {text}");
                assert!(text.contains("subagent finished"), "got {text}");
            },
            other => panic!("expected Preview details, got {other:?}"),
        }
    }

    #[test]
    fn action_display_read_reports_line_count_and_duration() {
        let call = sample_call_args(1, "read_file", serde_json::json!({"path": "src/main.rs"}));
        let action = action_display_for(
            &call,
            &ToolOutcome::success("one\ntwo\nthree\n", "3 lines read", 1.25),
        );

        assert_eq!(action.action_type, "Read");
        match action.details {
            ActionDetails::Preview { text, line_count } => {
                assert_eq!(line_count, Some(3));
                assert!(text.contains("Success, 3 lines read"));
                assert!(text.contains("took 1.2s"));
            },
            other => panic!("expected preview details, got {:?}", other),
        }
    }

    #[test]
    fn action_display_write_carries_display_diff_when_available() {
        let call = sample_call_args(
            1,
            "write_file",
            serde_json::json!({"path": "petal/index.html", "content": "a\nb\n"}),
        );
        let action = action_display_for(
            &call,
            &ToolOutcome::success("Wrote petal/index.html (2 lines)", "2 lines written", 0.05)
                .with_metadata(ToolRunMetadata {
                    detail: ToolMetadata::WriteFile {
                        path: "petal/index.html".to_string(),
                        line_count: 2,
                        byte_count: 4,
                        created: Some(true),
                    },
                    display_diff: Some("   1 + a\n   2 + b".to_string()),
                    ..ToolRunMetadata::default()
                }),
        );

        match action.details {
            ActionDetails::Diff { summary, diff } => {
                assert!(summary.contains("+2 -0"));
                assert!(diff.contains("+ a"));
                assert!(!diff.contains("+++"), "no diff header clutter");
            },
            other => panic!("expected diff details, got {:?}", other),
        }
    }

    #[test]
    fn action_display_edit_carries_display_diff_when_available() {
        let call = sample_call_args(
            1,
            "edit_file",
            serde_json::json!({"path": "src/main.rs", "old_string": "old", "new_string": "new"}),
        );
        let action = action_display_for(
            &call,
            &ToolOutcome::success("Edited src/main.rs", "+1 -1", 0.05).with_metadata(
                ToolRunMetadata {
                    detail: ToolMetadata::EditFile {
                        path: "src/main.rs".to_string(),
                        replacements: 1,
                    },
                    display_diff: Some("   1 - old\n   1 + new".to_string()),
                    ..ToolRunMetadata::default()
                },
            ),
        );

        match action.details {
            ActionDetails::Diff { summary, diff } => {
                assert!(summary.contains("+1 -1"));
                assert!(diff.contains("- old"));
                assert!(diff.contains("+ new"));
            },
            other => panic!("expected diff details, got {:?}", other),
        }
    }

    #[test]
    fn action_display_web_search_reports_result_count() {
        let call = sample_call_args(1, "web_search", serde_json::json!({"query": "rust"}));
        let output = "[SEARCH_RESULTS]\n[1] Title: A\nURL: https://a.test\nContent:\nA\n---\n[2] Title: B\nURL: https://b.test\nContent:\nB\n---\n";
        let outcome = ToolOutcome::success(output, "2 results returned", 15.2).with_metadata(
            ToolRunMetadata {
                detail: ToolMetadata::WebSearch {
                    queries: vec!["rust".to_string()],
                    requested_count: 5,
                    result_count: 2,
                    sources: vec!["https://a.test".to_string(), "https://b.test".to_string()],
                },
                result_count: Some(2),
                ..ToolRunMetadata::default()
            },
        );
        let action = action_display_for(&call, &outcome);

        match action.details {
            ActionDetails::Preview { text, .. } => {
                assert!(text.contains("Success, 2 results returned"));
                assert!(text.contains("took 15s"));
            },
            other => panic!("expected preview details, got {:?}", other),
        }
        let metadata = action.metadata.expect("metadata");
        assert_eq!(metadata.result_count, Some(2));
        assert_eq!(metadata.duration_secs, Some(15.2));
    }

    #[test]
    fn action_display_background_command_surfaces_pid_and_log() {
        let call = sample_call_args(
            1,
            "execute_command",
            serde_json::json!({"command": "npm run dev", "mode": "background"}),
        );
        let output = "Background command started.\nPID: 123\nLog: /tmp/mermaid-bg.log\nReady: matched pattern \"Local:\"\nDetected URL: http://127.0.0.1:5173\n\n--- startup output ---\nLocal: http://127.0.0.1:5173";
        let outcome = ToolOutcome::success(output, "background process started", 0.8)
            .with_metadata(ToolRunMetadata {
                detail: ToolMetadata::ExecuteCommand {
                    command: "npm run dev".to_string(),
                    working_dir: None,
                    exit_code: None,
                    timed_out: false,
                    background: true,
                    stdout_lines: 1,
                    stderr_lines: 0,
                    detected_urls: vec!["http://127.0.0.1:5173".to_string()],
                    pid: Some(123),
                    log_path: Some("/tmp/mermaid-bg.log".to_string()),
                },
                process: Some(ManagedProcess {
                    id: "bg-123".to_string(),
                    pid: 123,
                    command: "npm run dev".to_string(),
                    cwd: None,
                    log_path: "/tmp/mermaid-bg.log".to_string(),
                    detected_url: Some("http://127.0.0.1:5173".to_string()),
                    status: ManagedProcessStatus::Running,
                }),
                ..ToolRunMetadata::default()
            });
        let action = action_display_for(&call, &outcome);

        match action.details {
            ActionDetails::Preview { text, .. } => {
                assert!(text.contains("Success, background process started"));
                assert!(text.contains("PID: 123"));
                assert!(text.contains("Log: /tmp/mermaid-bg.log"));
                assert!(text.contains("Detected URL: http://127.0.0.1:5173"));
                assert!(!text.contains("startup output"));
            },
            other => panic!("expected preview details, got {:?}", other),
        }
        let metadata = action.metadata.expect("metadata");
        let process = metadata.process.expect("process metadata");
        assert_eq!(process.id, "bg-123");
        assert_eq!(process.pid, 123);
        assert_eq!(process.command, "npm run dev");
        assert_eq!(
            process.detected_url.as_deref(),
            Some("http://127.0.0.1:5173")
        );
    }

    #[test]
    fn try_complete_outcomes_returns_none_on_incomplete() {
        let outcomes = vec![Some(ToolOutcome::success("a", "a", 0.1)), None];
        assert!(try_complete_outcomes(&outcomes).is_none());
    }

    #[test]
    fn try_complete_outcomes_returns_vec_on_complete() {
        let outcomes = vec![
            Some(ToolOutcome::success("a", "a", 0.1)),
            Some(ToolOutcome::cancelled()),
        ];
        let result = try_complete_outcomes(&outcomes);
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 2);
    }

    #[test]
    fn fill_outcome_writes_to_correct_slot() {
        let calls = vec![sample_call(1, "read_file"), sample_call(2, "write_file")];
        let mut outcomes = vec![None, None];

        let wrote = fill_outcome(
            &calls,
            &mut outcomes,
            ToolCallId(2),
            ToolOutcome::cancelled(),
        );
        assert!(wrote);
        assert!(outcomes[0].is_none());
        assert!(outcomes[1].is_some());
    }

    #[test]
    fn fill_outcome_stale_call_id_returns_false() {
        let calls = vec![sample_call(1, "read_file")];
        let mut outcomes = vec![None];
        let wrote = fill_outcome(
            &calls,
            &mut outcomes,
            ToolCallId(999),
            ToolOutcome::cancelled(),
        );
        assert!(!wrote);
        assert!(outcomes[0].is_none());
    }

    #[test]
    fn fill_outcome_duplicate_write_ignored() {
        let calls = vec![sample_call(1, "read_file")];
        let mut outcomes = vec![Some(ToolOutcome::success("first", "first", 0.0))];
        let wrote = fill_outcome(
            &calls,
            &mut outcomes,
            ToolCallId(1),
            ToolOutcome::cancelled(),
        );
        assert!(!wrote);
        match &outcomes[0] {
            Some(outcome) if outcome.is_success() => assert_eq!(outcome.output(), "first"),
            _ => panic!("original outcome was overwritten"),
        }
    }

    #[test]
    fn start_generating_produces_fresh_sending_phase() {
        let s = start_generating(TurnId(1), SystemTime::now());
        match s {
            TurnState::Generating {
                phase,
                tokens,
                partial_text,
                ..
            } => {
                assert_eq!(phase, GenPhase::Sending);
                assert_eq!(tokens, 0);
                assert!(partial_text.is_empty());
            },
            _ => panic!("expected Generating"),
        }
    }

    #[test]
    fn start_executing_tools_allocates_outcome_slots() {
        let calls = vec![
            sample_call(1, "a"),
            sample_call(2, "b"),
            sample_call(3, "c"),
        ];
        let s = start_executing_tools(TurnId(1), calls, SystemTime::now());
        match s {
            TurnState::ExecutingTools {
                outcomes, calls, ..
            } => {
                assert_eq!(outcomes.len(), 3);
                assert_eq!(calls.len(), 3);
                assert!(outcomes.iter().all(|o| o.is_none()));
            },
            _ => panic!("expected ExecutingTools"),
        }
    }

    #[test]
    fn commit_assistant_message_preserves_thinking_signature() {
        let m = commit_assistant_message(
            "hello".to_string(),
            "reasoning".to_string(),
            vec![],
            Some("sig_abc".to_string()),
            chrono::Local::now(),
        );
        assert_eq!(m.content, "hello");
        assert_eq!(m.thinking.as_deref(), Some("reasoning"));
        assert_eq!(m.thinking_signature.as_deref(), Some("sig_abc"));
    }

    #[test]
    fn commit_assistant_message_empty_reasoning_is_none() {
        let m = commit_assistant_message(
            "hi".to_string(),
            String::new(),
            vec![],
            None,
            chrono::Local::now(),
        );
        assert!(m.thinking.is_none());
    }

    #[test]
    fn tool_result_messages_align_call_id_and_name() {
        let calls = vec![sample_call(1, "read_file"), sample_call(2, "write_file")];
        let outcomes = vec![
            ToolOutcome::success("contents", "contents", 0.1),
            ToolOutcome::cancelled(),
        ];
        let msgs = tool_result_messages(&calls, outcomes);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, MessageRole::Tool);
        assert_eq!(msgs[0].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(msgs[0].tool_name.as_deref(), Some("read_file"));
        assert_eq!(msgs[0].content, "contents");
        assert!(msgs[1].content.contains("cancelled"));
    }

    #[test]
    fn format_run_duration_scales() {
        assert_eq!(format_run_duration(0), "0s");
        assert_eq!(format_run_duration(5), "5s");
        assert_eq!(format_run_duration(59), "59s");
        assert_eq!(format_run_duration(72), "1m 12s");
        assert_eq!(format_run_duration(600), "10m 0s");
        assert_eq!(format_run_duration(3661), "1h 1m");
    }
}
