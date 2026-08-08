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
use crate::models::{ChatMessage, MessageRole, ProviderContinuation};

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
    // `Option<Vec<T>>: FromIterator<Option<T>>` short-circuits to `None` on the
    // first empty slot — same semantics as the explicit loop, one line.
    outcomes.iter().cloned().collect()
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
    start_generating_with(id, now, false)
}

/// `start_generating` with an explicit continuation flag. Used by the
/// auto-continue tail (and the paths that must carry its flag forward:
/// empty-retry, truncation-recovery resume) so the eventual commit stamps
/// `ChatMessageKind::Continuation`.
pub fn start_generating_with(id: TurnId, now: SystemTime, continuation: bool) -> TurnState {
    TurnState::Generating {
        id,
        started: now,
        partial_text: String::new(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Sending,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation,
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
/// provider continuation state needed for the next model call.
pub fn commit_assistant_message(
    partial_text: String,
    partial_reasoning: String,
    tool_calls: Vec<ModelToolCall>,
    provider_continuation: Option<ProviderContinuation>,
    now: chrono::DateTime<chrono::Local>,
    continuation: bool,
) -> ChatMessage {
    let thinking = if partial_reasoning.is_empty() {
        None
    } else {
        Some(partial_reasoning)
    };
    let kind = if continuation {
        crate::models::ChatMessageKind::Continuation
    } else {
        crate::models::ChatMessageKind::Normal
    };
    ChatMessage {
        role: MessageRole::Assistant,
        content: partial_text,
        timestamp: now,
        kind,
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
        provider_continuation,
    }
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
    // `write_file` that overwrote an existing file is an *update*, not a fresh
    // write — relabel it so the transcript reads "Update" (as Claude Code does),
    // reserving "Write" for a genuinely new file. The tool records which case it
    // was in its outcome metadata; the pending call couldn't know yet.
    let (action_type, target) = match &outcome.metadata.detail {
        ToolMetadata::WriteFile {
            created: Some(false),
            ..
        } => ("Update".to_string(), target),
        // `apply_patch` is the model's tool name; the transcript should read the
        // action verb + the file(s) it touched, derived from the outcome's
        // per-file A/M/D/R lists (unknown at call time, filled in post-exec).
        ToolMetadata::ApplyPatch {
            added,
            modified,
            deleted,
            renamed,
            ..
        } => apply_patch_action(added, modified, deleted, renamed).unwrap_or((action_type, target)),
        _ => (action_type, target),
    };
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

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
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
            let mut detail = format!(
                "{} {} returned",
                result_count,
                pluralize("result", result_count)
            );
            if let ToolMetadata::WebSearch {
                backend,
                failed_queries,
                truncated,
                ..
            } = &outcome.metadata.detail
            {
                if !backend.is_empty() {
                    detail.push_str(&format!(" via {backend}"));
                }
                if *failed_queries > 0 {
                    detail.push_str(&format!(" · {failed_queries} failed"));
                }
                if *truncated {
                    detail.push_str(" · truncated");
                }
            }
            ActionDetails::Preview {
                text: success_summary(detail, duration),
                line_count: None,
            }
        },
        "web_fetch" => {
            let line_count = outcome
                .metadata
                .line_count
                .or_else(|| metadata_line_count(&outcome.metadata.detail))
                .unwrap_or_else(|| outcome.output().lines().count());
            let mut detail = format!("{} {} fetched", line_count, pluralize("line", line_count));
            if let ToolMetadata::WebFetch {
                url,
                final_url,
                backend,
                status,
                media_type,
                extraction,
                output_byte_count,
                truncated,
                pattern,
                match_count,
                snapshot_id,
                ..
            } = &outcome.metadata.detail
            {
                if !backend.is_empty() {
                    detail.push_str(&format!(" via {backend}"));
                }
                if let Some(status) = status {
                    detail.push_str(&format!(" · HTTP {status}"));
                }
                if let Some(media_type) = media_type {
                    detail.push_str(&format!(" · {media_type}"));
                }
                if !extraction.is_empty() {
                    detail.push_str(&format!(" · {extraction}"));
                }
                if *output_byte_count > 0 {
                    detail.push_str(&format!(" · {output_byte_count} extracted bytes"));
                }
                if let Some(pattern) = pattern {
                    let match_count = match_count.unwrap_or(0);
                    let pattern = crate::utils::redact_secrets(pattern);
                    detail.push_str(&format!(
                        " · {match_count} {} for {:?}",
                        pluralize("match", match_count),
                        crate::utils::truncate_middle(&pattern, 48)
                    ));
                }
                if let Some(final_url) = final_url.as_deref().filter(|final_url| *final_url != url)
                {
                    detail.push_str(&format!(
                        " · final {}",
                        crate::utils::truncate_middle(final_url, 96)
                    ));
                }
                if *truncated {
                    detail.push_str(" · truncated");
                }
                if let Some(snapshot_id) = snapshot_id {
                    detail.push_str(&format!(" · {snapshot_id}"));
                }
            }
            ActionDetails::Preview {
                text: success_summary(detail, duration),
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
        "apply_patch" => {
            if let Some(diff) = outcome.metadata.display_diff.clone() {
                ActionDetails::Diff {
                    summary: diff_success_summary(&diff, duration),
                    diff,
                }
            } else {
                // Fallback when a patch produced no display diff (effectively
                // never — apply_patch always sets one). `outcome.summary` already
                // carries its own timing, so use it directly (no re-wrap).
                ActionDetails::Preview {
                    text: outcome.summary.clone(),
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
                && usage.total_tokens() > 0
            {
                bits.push(format!(
                    "{} tokens",
                    super::compaction::format_compact_count(usage.total_tokens())
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
        "task_create" | "task_update" => {
            // Progress after the call, plus the model's pivot rationale when
            // it gave one — the user should see WHY the plan changed shape.
            let mut text = if let ToolMetadata::Tasks {
                completed, total, ..
            } = &outcome.metadata.detail
            {
                format!("Tasks {completed}/{total}")
            } else {
                outcome.summary.clone()
            };
            if let Some(explanation) = args.get("explanation").and_then(|v| v.as_str())
                && !explanation.trim().is_empty()
            {
                text.push_str(" · ");
                text.push_str(explanation.trim());
            }
            ActionDetails::Preview {
                text: success_summary(text, duration),
                line_count: None,
            }
        },
        // Fallback for non-answered resolutions (dismissed, chat-about-this,
        // headless) and recordings from before answers rode the metadata —
        // an answered call renders from `ToolMetadata::Questions` instead.
        "ask_user_question" => ActionDetails::Preview {
            text: success_summary(outcome.summary.clone(), duration),
            line_count: None,
        },
        _ => ActionDetails::Simple,
    }
}

/// Verb + target for an `apply_patch` outcome, derived from the files it
/// actually touched. A single-file patch reads with the operation-specific verb
/// (like `write_file`/`delete_file`); a multi-file patch folds to
/// `Update(N files)` (the diff body lists each file). `None` on an empty patch
/// so the caller keeps the original label.
fn apply_patch_action(
    added: &[String],
    modified: &[String],
    deleted: &[String],
    renamed: &[(String, String)],
) -> Option<(String, String)> {
    match added.len() + modified.len() + deleted.len() + renamed.len() {
        0 => None,
        1 => modified
            .first()
            .map(|p| ("Update".to_string(), p.clone()))
            .or_else(|| added.first().map(|p| ("Write".to_string(), p.clone())))
            .or_else(|| deleted.first().map(|p| ("Delete".to_string(), p.clone())))
            .or_else(|| {
                renamed
                    .first()
                    .map(|(_, dst)| ("Update".to_string(), dst.clone()))
            }),
        n => Some(("Update".to_string(), format!("{n} files"))),
    }
}

fn diff_success_summary(diff: &str, duration: Option<f64>) -> String {
    let (added, removed) = diff_counts(diff);
    let changes = format_line_changes(added, removed);
    match duration {
        Some(seconds) => format!("{}, took {}", changes, format_duration(seconds)),
        None => changes,
    }
}

/// Claude-Code-style line-change phrasing: "Added N lines", "Removed N lines",
/// or "Added N lines, removed M lines" — a plain statement, no status prefix.
fn format_line_changes(added: usize, removed: usize) -> String {
    match (added, removed) {
        (0, 0) => "No changes".to_string(),
        (a, 0) => format!("Added {} {}", a, pluralize("line", a)),
        (0, r) => format!("Removed {} {}", r, pluralize("line", r)),
        (a, r) => format!(
            "Added {} {}, removed {} {}",
            a,
            pluralize("line", a),
            r,
            pluralize("line", r),
        ),
    }
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
        Some(seconds) => format!("{}, took {}", detail, format_duration(seconds)),
        None => detail,
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
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
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
        // `apply_patch` can touch several files in one call, so the bare call has
        // no single target here — the per-file A/M/D/R summary rides in the diff
        // detail (`action_details_for`).
        "apply_patch" => ("Apply patch".to_string(), String::new()),
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
            (
                "Web Search".to_string(),
                crate::utils::redact_secrets(&target),
            )
        },
        "web_fetch" => {
            let target = string_arg("url")
                .map(|url| crate::utils::sanitize_url_for_display(&url))
                .or_else(|| string_arg("snapshot_id"))
                .unwrap_or_default();
            ("Web Fetch".to_string(), target)
        },
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
        "task_create" => {
            let count = args
                .get("tasks")
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len);
            (
                "Tasks".to_string(),
                format!("plan {count} {}", pluralize("step", count)),
            )
        },
        "task_update" => {
            let count = args
                .get("updates")
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len);
            (
                "Tasks".to_string(),
                format!("update {count} {}", pluralize("step", count)),
            )
        },
        "task_list" => ("Tasks".to_string(), "list".to_string()),
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
    fn ask_user_question_fallback_previews_the_summary() {
        // Non-answered resolutions (dismissed / chat-about-this / headless)
        // carry no `Questions` metadata; the transcript shows the summary
        // instead of a bare duration.
        let call = sample_call(1, "ask_user_question");
        let outcome = ToolOutcome::success(
            "The user dismissed the question(s) without answering.",
            "dismissed without answering",
            4.0,
        );
        let action = action_display_for(&call, &outcome);
        match &action.details {
            ActionDetails::Preview { text, .. } => {
                assert!(text.contains("dismissed without answering"), "got {text}");
                assert!(text.contains("took 4.0s"), "got {text}");
            },
            other => panic!("expected Preview details, got {other:?}"),
        }
    }

    #[test]
    fn agent_action_reports_child_model_and_tokens() {
        let call = sample_call_args(1, "agent", serde_json::json!({"description": "explore"}));
        let usage = crate::models::TokenUsage::provider(9_000, 3_300);
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
                assert!(!text.contains("Success"), "no Success prefix: {text}");
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
                assert!(text.contains("3 lines read"));
                assert!(!text.contains("Success"), "no Success prefix: {text}");
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

        assert_eq!(
            action.action_type, "Write",
            "a newly created file reads as Write"
        );
        match action.details {
            ActionDetails::Diff { summary, diff } => {
                assert!(summary.contains("Added 2 lines"), "got {summary}");
                assert!(!summary.contains("Success"), "no Success prefix: {summary}");
                assert!(summary.contains(", took "), "timing is kept: {summary}");
                assert!(diff.contains("+ a"));
                assert!(!diff.contains("+++"), "no diff header clutter");
            },
            other => panic!("expected diff details, got {:?}", other),
        }
    }

    #[test]
    fn action_display_write_over_existing_file_is_labeled_update() {
        // Overwriting a file that already existed is an update, not a fresh
        // write — the tool reports `created: Some(false)` and the label follows.
        let call = sample_call_args(
            1,
            "write_file",
            serde_json::json!({"path": "metadata.json", "content": "{}\n"}),
        );
        let action = action_display_for(
            &call,
            &ToolOutcome::success("Wrote metadata.json (1 lines)", "1 line written", 0.05)
                .with_metadata(ToolRunMetadata {
                    detail: ToolMetadata::WriteFile {
                        path: "metadata.json".to_string(),
                        line_count: 1,
                        byte_count: 3,
                        created: Some(false),
                    },
                    display_diff: Some("   1 - old\n   1 + {}".to_string()),
                    ..ToolRunMetadata::default()
                }),
        );

        assert_eq!(
            action.action_type, "Update",
            "overwriting an existing file reads as Update"
        );
        assert_eq!(action.target, "metadata.json");
        match action.details {
            ActionDetails::Diff { summary, .. } => {
                assert!(
                    summary.contains("Added 1 line, removed 1 line"),
                    "got {summary}"
                );
            },
            other => panic!("expected diff details, got {:?}", other),
        }
    }

    #[test]
    fn format_line_changes_reads_like_claude_code() {
        assert_eq!(format_line_changes(49, 0), "Added 49 lines");
        assert_eq!(format_line_changes(1, 0), "Added 1 line");
        assert_eq!(format_line_changes(0, 9), "Removed 9 lines");
        assert_eq!(format_line_changes(0, 1), "Removed 1 line");
        assert_eq!(format_line_changes(7, 1), "Added 7 lines, removed 1 line");
        assert_eq!(format_line_changes(0, 0), "No changes");
    }

    #[test]
    fn apply_patch_action_labels_by_operation() {
        let files = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        let no_files: Vec<String> = vec![];
        let no_renames: Vec<(String, String)> = vec![];

        // single-file patch → operation-specific verb + that file's path
        assert_eq!(
            apply_patch_action(&no_files, &files(&["a.rs"]), &no_files, &no_renames),
            Some(("Update".to_string(), "a.rs".to_string()))
        );
        assert_eq!(
            apply_patch_action(&files(&["new.rs"]), &no_files, &no_files, &no_renames),
            Some(("Write".to_string(), "new.rs".to_string()))
        );
        assert_eq!(
            apply_patch_action(&no_files, &no_files, &files(&["gone.rs"]), &no_renames),
            Some(("Delete".to_string(), "gone.rs".to_string()))
        );
        assert_eq!(
            apply_patch_action(
                &no_files,
                &no_files,
                &no_files,
                &[("old.rs".to_string(), "new.rs".to_string())]
            ),
            Some(("Update".to_string(), "new.rs".to_string()))
        );
        // multi-file patch → Update(N files)
        assert_eq!(
            apply_patch_action(
                &files(&["a.rs"]),
                &files(&["b.rs", "c.rs"]),
                &no_files,
                &no_renames
            ),
            Some(("Update".to_string(), "3 files".to_string()))
        );
        // empty patch → None (caller keeps the original label)
        assert_eq!(
            apply_patch_action(&no_files, &no_files, &no_files, &no_renames),
            None
        );
    }

    #[test]
    fn action_display_apply_patch_carries_display_diff_when_available() {
        let call = sample_call_args(
            1,
            "apply_patch",
            serde_json::json!({"patch": "*** Begin Patch\n*** Update File: src/main.rs\n-old\n+new\n*** End Patch"}),
        );
        let action = action_display_for(
            &call,
            &ToolOutcome::success("Applied patch: 1 file(s)\nM src/main.rs", "+1 -1", 0.05)
                .with_metadata(ToolRunMetadata {
                    detail: ToolMetadata::ApplyPatch {
                        added: vec![],
                        modified: vec!["src/main.rs".to_string()],
                        deleted: vec![],
                        renamed: vec![],
                        fuzzy: false,
                    },
                    display_diff: Some("=== M src/main.rs ===\n   1 - old\n   1 + new".to_string()),
                    ..ToolRunMetadata::default()
                }),
        );

        assert_eq!(action.action_type, "Update");
        assert_eq!(action.target, "src/main.rs");
        match action.details {
            ActionDetails::Diff { diff, .. } => {
                assert!(diff.contains("- old"));
                assert!(diff.contains("+ new"));
            },
            other => panic!("expected diff details, got {:?}", other),
        }
    }

    #[test]
    fn action_display_web_search_reports_partial_backend_and_truncation() {
        let call = sample_call_args(
            1,
            "web_search",
            serde_json::json!({"queries": [{"query": "rust"}, {"query": "systems"}]}),
        );
        let output = "[SEARCH_RESULTS]\n[1] Title: A\nURL: https://a.test\nContent:\nA\n---\n[2] Title: B\nURL: https://b.test\nContent:\nB\n---\n";
        let outcome = ToolOutcome::success(output, "2 results returned", 15.2).with_metadata(
            ToolRunMetadata {
                detail: ToolMetadata::WebSearch {
                    queries: vec!["rust".to_string(), "systems".to_string()],
                    requested_count: 10,
                    result_count: 2,
                    sources: vec!["https://a.test".to_string(), "https://b.test".to_string()],
                    backend: "mock".to_string(),
                    succeeded_queries: 1,
                    failed_queries: 1,
                    partial: true,
                    truncated: true,
                    failures: vec![crate::domain::WebSearchFailure {
                        query_index: 1,
                        error: "upstream timed out".to_string(),
                    }],
                },
                result_count: Some(2),
                ..ToolRunMetadata::default()
            },
        );
        let action = action_display_for(&call, &outcome);

        match action.details {
            ActionDetails::Preview { text, .. } => {
                assert!(text.contains("2 results returned"));
                assert!(text.contains("via mock"));
                assert!(text.contains("1 failed"));
                assert!(text.contains("truncated"));
                assert!(!text.contains("Success"), "no Success prefix: {text}");
                assert!(text.contains("took 15s"));
            },
            other => panic!("expected preview details, got {:?}", other),
        }
        let metadata = action.metadata.expect("metadata");
        assert_eq!(metadata.result_count, Some(2));
        assert_eq!(metadata.duration_secs, Some(15.2));
    }

    #[test]
    fn action_display_web_fetch_surfaces_safe_provenance() {
        let call = sample_call_args(
            1,
            "web_fetch",
            serde_json::json!({
                "url": "https://example.test/page?token=opaque-secret#private"
            }),
        );
        let outcome =
            ToolOutcome::success("page", "4 lines fetched", 1.2).with_metadata(ToolRunMetadata {
                detail: ToolMetadata::WebFetch {
                    url: "https://example.test/page?token=%5BREDACTED%5D".to_string(),
                    final_url: Some("https://example.test/final".to_string()),
                    status: Some(200),
                    error_kind: None,
                    media_type: Some("text/html".to_string()),
                    charset: Some("utf-8".to_string()),
                    backend: "native".to_string(),
                    extraction: "readability".to_string(),
                    title: Some("Example".to_string()),
                    line_count: 4,
                    byte_count: 128,
                    source_byte_count: 512,
                    output_byte_count: 384,
                    truncated: true,
                    pattern: Some("OPENAI_API_KEY=abc".to_string()),
                    context_lines: Some(2),
                    match_count: Some(1),
                    snapshot_id: Some("web-1".to_string()),
                },
                line_count: Some(4),
                ..ToolRunMetadata::default()
            });
        let action = action_display_for(&call, &outcome);
        assert!(!action.target.contains("opaque-secret"));
        assert!(!action.target.contains("private"));
        match action.details {
            ActionDetails::Preview { text, .. } => {
                assert!(text.contains("via native"));
                assert!(text.contains("HTTP 200"));
                assert!(text.contains("readability"));
                assert!(text.contains("384 extracted bytes"));
                assert!(text.contains("final https://example.test/final"));
                assert!(text.contains("truncated"));
                assert!(text.contains("web-1"));
                assert!(!text.contains("OPENAI_API_KEY=abc"));
                assert!(text.contains("OPENAI_API_KEY=[REDACTED]"));
            },
            other => panic!("expected preview details, got {other:?}"),
        }
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
                    denied_by_sandbox: false,
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
                assert!(text.contains("background process started"));
                assert!(!text.contains("Success"), "no Success prefix: {text}");
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
    fn commit_assistant_message_preserves_provider_continuation() {
        let m = commit_assistant_message(
            "hello".to_string(),
            "reasoning".to_string(),
            vec![],
            Some(ProviderContinuation::Anthropic {
                signature: "sig_abc".to_string(),
            }),
            chrono::Local::now(),
            false,
        );
        assert_eq!(m.content, "hello");
        assert_eq!(m.thinking.as_deref(), Some("reasoning"));
        assert_eq!(
            m.provider_continuation
                .as_ref()
                .and_then(ProviderContinuation::anthropic_signature),
            Some("sig_abc")
        );
    }

    #[test]
    fn commit_assistant_message_empty_reasoning_is_none() {
        let m = commit_assistant_message(
            "hi".to_string(),
            String::new(),
            vec![],
            None,
            chrono::Local::now(),
            false,
        );
        assert!(m.thinking.is_none());
        assert_eq!(m.kind, crate::models::ChatMessageKind::Normal);
    }

    #[test]
    fn commit_assistant_message_stamps_continuation_kind() {
        let m = commit_assistant_message(
            "resumed text".to_string(),
            String::new(),
            vec![],
            None,
            chrono::Local::now(),
            true,
        );
        assert_eq!(m.kind, crate::models::ChatMessageKind::Continuation);
    }

    #[test]
    fn start_generating_with_carries_the_continuation_flag() {
        let t = start_generating_with(TurnId(3), std::time::SystemTime::now(), true);
        assert!(matches!(
            t,
            TurnState::Generating {
                continuation: true,
                ..
            }
        ));
        // The plain constructor stays a non-continuation turn.
        let t = start_generating(TurnId(4), std::time::SystemTime::now());
        assert!(matches!(
            t,
            TurnState::Generating {
                continuation: false,
                ..
            }
        ));
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
