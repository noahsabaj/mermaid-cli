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

use crate::agents::{ActionDetails, ActionDisplay, ActionResult};
use crate::models::tool_call::ToolCall as ModelToolCall;
use crate::models::{ChatMessage, MessageRole};

use super::ids::{ToolCallId, TurnId};
use super::state::{GenPhase, PendingToolCall, TurnState, ToolOutcome};

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
pub fn try_complete_outcomes(
    outcomes: &[Option<ToolOutcome>],
) -> Option<Vec<ToolOutcome>> {
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
pub fn start_generating(id: TurnId) -> TurnState {
    TurnState::Generating {
        id,
        started: SystemTime::now(),
        partial_text: String::new(),
        partial_reasoning: String::new(),
        tokens: 0,
        phase: GenPhase::Sending,
        thinking_signature: None,
    }
}

/// Transition `Generating → ExecutingTools`. Allocates `None` slots
/// for every call so the invariant ("outcomes.len() == calls.len()")
/// is upheld by construction.
pub fn start_executing_tools(id: TurnId, calls: Vec<PendingToolCall>) -> TurnState {
    let outcomes = vec![None; calls.len()];
    TurnState::ExecutingTools {
        id,
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
) -> ChatMessage {
    let thinking = if partial_reasoning.is_empty() {
        None
    } else {
        Some(partial_reasoning)
    };
    let mut msg = ChatMessage {
        role: MessageRole::Assistant,
        content: partial_text,
        timestamp: chrono::Local::now(),
        actions: Vec::new(),
        thinking,
        images: None,
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
pub fn action_display_for(
    call: &PendingToolCall,
    outcome: &ToolOutcome,
) -> ActionDisplay {
    let (action_type, target) = display_info_for(call);
    let (result, duration) = match outcome {
        ToolOutcome::Finished {
            output,
            images,
            duration_secs,
        } => (
            ActionResult::Success {
                output: output.clone(),
                images: images.clone(),
            },
            Some(*duration_secs),
        ),
        ToolOutcome::Error { error, duration_secs } => (
            ActionResult::Error {
                error: error.clone(),
            },
            Some(*duration_secs),
        ),
        ToolOutcome::Cancelled => (
            ActionResult::Error {
                error: "[cancelled]".to_string(),
            },
            None,
        ),
    };
    ActionDisplay {
        action_type,
        target,
        result,
        details: ActionDetails::Simple,
        duration_seconds: duration,
    }
}

/// Best-effort name + target extraction from a tool call, for display.
/// Reuses `AgentAction::display_info` if the tool call parses cleanly;
/// falls back to the raw function name otherwise.
fn display_info_for(call: &PendingToolCall) -> (String, String) {
    match call.source.to_agent_action() {
        Ok(action) => {
            let (t, target) = action.display_info();
            (t.to_string(), target)
        },
        Err(_) => (call.source.function.name.clone(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::tool_call::{FunctionCall, ToolCall as ModelToolCall};

    fn sample_call(id: u64, name: &str) -> PendingToolCall {
        PendingToolCall {
            call_id: ToolCallId(id),
            source: ModelToolCall {
                id: Some(format!("c{}", id)),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        }
    }

    #[test]
    fn try_complete_outcomes_returns_none_on_incomplete() {
        let outcomes = vec![
            Some(ToolOutcome::Finished {
                output: "a".to_string(),
                images: None,
                duration_secs: 0.1,
            }),
            None,
        ];
        assert!(try_complete_outcomes(&outcomes).is_none());
    }

    #[test]
    fn try_complete_outcomes_returns_vec_on_complete() {
        let outcomes = vec![
            Some(ToolOutcome::Finished {
                output: "a".to_string(),
                images: None,
                duration_secs: 0.1,
            }),
            Some(ToolOutcome::Cancelled),
        ];
        let result = try_complete_outcomes(&outcomes);
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 2);
    }

    #[test]
    fn fill_outcome_writes_to_correct_slot() {
        let calls = vec![
            sample_call(1, "read_file"),
            sample_call(2, "write_file"),
        ];
        let mut outcomes = vec![None, None];

        let wrote = fill_outcome(
            &calls,
            &mut outcomes,
            ToolCallId(2),
            ToolOutcome::Cancelled,
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
            ToolOutcome::Cancelled,
        );
        assert!(!wrote);
        assert!(outcomes[0].is_none());
    }

    #[test]
    fn fill_outcome_duplicate_write_ignored() {
        let calls = vec![sample_call(1, "read_file")];
        let mut outcomes = vec![Some(ToolOutcome::Finished {
            output: "first".to_string(),
            images: None,
            duration_secs: 0.0,
        })];
        let wrote = fill_outcome(
            &calls,
            &mut outcomes,
            ToolCallId(1),
            ToolOutcome::Cancelled,
        );
        assert!(!wrote);
        match &outcomes[0] {
            Some(ToolOutcome::Finished { output, .. }) => {
                assert_eq!(output, "first");
            },
            _ => panic!("original outcome was overwritten"),
        }
    }

    #[test]
    fn start_generating_produces_fresh_sending_phase() {
        let s = start_generating(TurnId(1));
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
        let calls = vec![sample_call(1, "a"), sample_call(2, "b"), sample_call(3, "c")];
        let s = start_executing_tools(TurnId(1), calls);
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
        );
        assert!(m.thinking.is_none());
    }

    #[test]
    fn tool_result_messages_align_call_id_and_name() {
        let calls = vec![sample_call(1, "read_file"), sample_call(2, "write_file")];
        let outcomes = vec![
            ToolOutcome::Finished {
                output: "contents".to_string(),
                images: None,
                duration_secs: 0.1,
            },
            ToolOutcome::Cancelled,
        ];
        let msgs = tool_result_messages(&calls, outcomes);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, MessageRole::Tool);
        assert_eq!(msgs[0].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(msgs[0].tool_name.as_deref(), Some("read_file"));
        assert_eq!(msgs[0].content, "contents");
        assert!(msgs[1].content.contains("cancelled"));
    }
}
