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

use mermaid_model::models::tool_call::ToolCall as ModelToolCall;
use mermaid_model::models::{ChatMessage, MessageRole, ProviderContinuation};

use super::state::{GenPhase, PendingToolCall, ToolOutcome, TurnState};
use mermaid_model::ids::{ToolCallId, TurnId};

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
#[must_use]
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
#[must_use]
pub fn start_generating(id: TurnId, now: SystemTime) -> TurnState {
    start_generating_with(id, now, false)
}

/// `start_generating` with an explicit continuation flag. Used by the
/// auto-continue tail (and the paths that must carry its flag forward:
/// empty-retry, truncation-recovery resume) so the eventual commit stamps
/// `ChatMessageKind::Continuation`.
#[must_use]
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
/// for every call so the invariant ("`outcomes.len()` == `calls.len()`")
/// is upheld by construction. `now` is the reducer step's injected clock
/// (`state.now`) so `started` is deterministic on replay (Cause 3).
#[must_use]
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
#[must_use]
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
        mermaid_model::models::ChatMessageKind::Continuation
    } else {
        mermaid_model::models::ChatMessageKind::Normal
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
/// The OpenAI-compatible wire format requires (`tool_call_id`, `tool_name`,
/// content) — we pull name from the original call.
#[must_use]
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

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_model::models::tool_call::{FunctionCall, ToolCall as ModelToolCall};

    fn sample_call(id: u64, name: &str) -> PendingToolCall {
        sample_call_args(id, name, serde_json::json!({}))
    }

    fn sample_call_args(id: u64, name: &str, arguments: serde_json::Value) -> PendingToolCall {
        PendingToolCall {
            call_id: ToolCallId(id),
            source: ModelToolCall {
                id: Some(format!("c{id}")),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments,
                },
            },
        }
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
        assert_eq!(m.kind, mermaid_model::models::ChatMessageKind::Normal);
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
        assert_eq!(m.kind, mermaid_model::models::ChatMessageKind::Continuation);
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
}
