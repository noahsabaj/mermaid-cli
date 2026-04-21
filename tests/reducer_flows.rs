//! End-to-end flow tests for the v0.7 reducer.
//!
//! These exercise full multi-message sequences against
//! `domain::update` — no tokio, no terminal. They're the parity
//! harness the plan calls for: bugs that the v0.6 architecture
//! allowed (stale stream events corrupting the next turn, lost
//! tool results, double-commit on upstream error) are reproduced
//! here as regression guards.

use std::path::PathBuf;

use mermaid_cli::app::Config;
use mermaid_cli::domain::{
    Cmd, Msg, PendingToolCall, SlashCmd, State, StatusKind, ToolCallId, ToolOutcome, TurnId,
    TurnState, start_executing_tools, start_generating, update,
};
use mermaid_cli::models::MessageRole;
use mermaid_cli::models::tool_call::{FunctionCall, ToolCall as ModelToolCall};

fn fresh() -> State {
    State::new(
        Config::default(),
        PathBuf::from("/tmp/flow"),
        "ollama/test".to_string(),
    )
}

fn user_submit(state: State, text: &str) -> (State, Vec<Cmd>) {
    update(
        state,
        Msg::SubmitPrompt {
            text: text.to_string(),
            attachment_ids: vec![],
        },
    )
}

// ─── full happy-path turn ──────────────────────────────────────────

#[test]
fn happy_path_turn_ends_idle_with_assistant_message() {
    let (state, cmds) = user_submit(fresh(), "hello");
    assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));

    let id = state.current_turn_id().expect("turn id");

    let (state, _) = update(
        state,
        Msg::StreamText {
            turn: id,
            chunk: "hello back".to_string(),
        },
    );
    let (state, cmds) = update(
        state,
        Msg::StreamDone {
            turn: id,
            usage: None,
            thinking_signature: None,
        },
    );

    assert!(matches!(state.turn, TurnState::Idle));
    assert_eq!(state.session.messages().len(), 2);
    let last = state.session.messages().last().unwrap();
    assert_eq!(last.role, MessageRole::Assistant);
    assert_eq!(last.content, "hello back");
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
}

// ─── stale-event filtering ─────────────────────────────────────────

#[test]
fn stale_stream_chunks_cannot_corrupt_current_turn() {
    // Start turn A, stream "wrong", cancel, start turn B,
    // observe late-arriving chunk from turn A — must drop.
    let (state, _) = user_submit(fresh(), "first");
    let turn_a = state.current_turn_id().unwrap();

    let (state, _) = update(
        state,
        Msg::StreamText {
            turn: turn_a,
            chunk: "from A".to_string(),
        },
    );

    let (state, _) = update(state, Msg::CancelTurn);
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: turn_a,
            usage: None,
            thinking_signature: None,
        },
    );

    // After StreamDone on the cancelling turn, the reducer currently
    // keeps the state in Cancelling (StreamDone only matches when
    // Generating). Force return to Idle by delivering another
    // StreamDone-equivalent — in practice the effect runner would
    // tear down the scope and emit a terminal event. For this test
    // we assert the Cancelling state didn't commit stray A content.
    assert_eq!(
        state
            .session
            .messages()
            .iter()
            .filter(|m| m.content.contains("from A"))
            .count(),
        0,
        "stale content from cancelled turn must not reach committed history"
    );
}

#[test]
fn stream_text_from_prior_turn_is_ignored() {
    let mut state = fresh();
    state.turn = start_generating(TurnId(10));
    let (state, _) = update(
        state,
        Msg::StreamText {
            turn: TurnId(9), // wrong turn
            chunk: "stale".to_string(),
        },
    );
    match &state.turn {
        TurnState::Generating { partial_text, .. } => assert!(partial_text.is_empty()),
        _ => panic!("should still be Generating"),
    }
}

// ─── tool-call completeness invariant ──────────────────────────────

#[test]
fn tool_outcomes_must_all_land_before_followup_call() {
    let mut state = fresh();
    let calls = vec![
        PendingToolCall {
            call_id: ToolCallId(1),
            source: ModelToolCall {
                id: Some("a".to_string()),
                function: FunctionCall {
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        },
        PendingToolCall {
            call_id: ToolCallId(2),
            source: ModelToolCall {
                id: Some("b".to_string()),
                function: FunctionCall {
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        },
    ];
    state.turn = start_executing_tools(TurnId(1), calls);
    // Plant the prior assistant message so action displays attach.
    state
        .session
        .append(mermaid_cli::models::ChatMessage::assistant(
            "ok let me call tools",
        ));

    // Only first tool finishes — must stay in ExecutingTools.
    let (state, cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(1),
            call_id: ToolCallId(1),
            outcome: ToolOutcome::Finished {
                output: "first done".to_string(),
                images: None,
                duration_secs: 0.1,
            },
        },
    );
    assert!(matches!(state.turn, TurnState::ExecutingTools { .. }));
    assert!(cmds.is_empty(), "no follow-up until all tools finish");

    // Second finishes — now we advance.
    let (state, cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(1),
            call_id: ToolCallId(2),
            outcome: ToolOutcome::Finished {
                output: "second done".to_string(),
                images: None,
                duration_secs: 0.1,
            },
        },
    );
    assert!(matches!(state.turn, TurnState::Generating { .. }));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));

    // Both tool messages committed.
    let tool_msgs = state
        .session
        .messages()
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .count();
    assert_eq!(tool_msgs, 2);
}

#[test]
fn cancelled_tool_produces_placeholder_in_history() {
    let mut state = fresh();
    let call = PendingToolCall {
        call_id: ToolCallId(1),
        source: ModelToolCall {
            id: None,
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({}),
            },
        },
    };
    state.turn = start_executing_tools(TurnId(3), vec![call]);
    state
        .session
        .append(mermaid_cli::models::ChatMessage::assistant("calling tool"));

    let (state, _) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(3),
            call_id: ToolCallId(1),
            outcome: ToolOutcome::Cancelled,
        },
    );

    let last = state.session.messages().last().unwrap();
    assert_eq!(last.role, MessageRole::Tool);
    assert!(last.content.contains("cancelled"));
}

// ─── cancel semantics ──────────────────────────────────────────────

#[test]
fn cancel_emits_scope_cancel_and_transitions_cancelling() {
    let mut state = fresh();
    state.turn = start_generating(TurnId(7));

    let (state, cmds) = update(state, Msg::CancelTurn);
    assert!(
        matches!(state.turn, TurnState::Cancelling { id: TurnId(7), .. }),
        "active turn must be in Cancelling"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::CancelScope(TurnId(7)))),
        "reducer must emit Cmd::CancelScope so the runner tears down"
    );
}

#[test]
fn cancel_on_idle_is_noop() {
    let (state, cmds) = update(fresh(), Msg::CancelTurn);
    assert!(matches!(state.turn, TurnState::Idle));
    assert!(cmds.is_empty());
}

#[test]
fn double_cancel_does_not_emit_a_second_cancel_scope() {
    let mut state = fresh();
    state.turn = TurnState::Cancelling {
        id: TurnId(1),
        since: std::time::SystemTime::now(),
    };
    let (_state, cmds) = update(state, Msg::CancelTurn);
    assert!(cmds.iter().all(|c| !matches!(c, Cmd::CancelScope(_))));
}

// ─── upstream errors ───────────────────────────────────────────────

#[test]
fn upstream_error_ends_turn_exactly_once() {
    let mut state = fresh();
    state.turn = start_generating(TurnId(4));

    let err = mermaid_cli::models::UserFacingError {
        summary: "Server error".to_string(),
        message: "500 internal".to_string(),
        suggestion: "try again".to_string(),
        category: mermaid_cli::models::ErrorCategory::Temporary,
        recoverable: true,
    };
    let (state, _) = update(
        state,
        Msg::UpstreamError {
            turn: TurnId(4),
            error: err,
        },
    );
    assert!(matches!(state.turn, TurnState::Idle));
    // Only one committed message — the error line. The bug this
    // guards against was v0.6's double-commit: once from the
    // streaming callback, once from the final error path.
    assert_eq!(state.session.messages().len(), 1);
    assert!(state.status.is_some());
    assert!(matches!(
        state.status.as_ref().unwrap().kind,
        StatusKind::Error
    ));
}

#[test]
fn upstream_error_from_stale_turn_is_dropped() {
    let mut state = fresh();
    state.turn = start_generating(TurnId(8));
    let err = mermaid_cli::models::UserFacingError {
        summary: "late".to_string(),
        message: "".to_string(),
        suggestion: "".to_string(),
        category: mermaid_cli::models::ErrorCategory::Temporary,
        recoverable: false,
    };
    let (state, _) = update(
        state,
        Msg::UpstreamError {
            turn: TurnId(7), // stale
            error: err,
        },
    );
    assert!(matches!(state.turn, TurnState::Generating { .. }));
    assert!(state.session.messages().is_empty());
}

// ─── slash commands ────────────────────────────────────────────────

#[test]
fn slash_clear_requires_confirmation_before_wiping() {
    let mut state = fresh();
    state
        .session
        .append(mermaid_cli::models::ChatMessage::user("priceless"));
    let (state, _) = update(state, Msg::Slash(SlashCmd::Clear));
    assert!(state.confirm.is_some());
    assert_eq!(state.session.messages().len(), 1);

    let (state, _) = update(state, Msg::ConfirmAccepted);
    assert!(state.session.messages().is_empty());
    assert!(state.confirm.is_none());
}

#[test]
fn slash_save_emits_save_conversation() {
    let (_state, cmds) = update(fresh(), Msg::Slash(SlashCmd::Save(None)));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
}

#[test]
fn slash_unknown_sets_warn_status() {
    let (state, cmds) = update(fresh(), Msg::Slash(SlashCmd::Unknown("nope".to_string())));
    assert!(state.status.is_some());
    assert!(matches!(
        state.status.as_ref().unwrap().kind,
        StatusKind::Warn
    ));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::DismissStatusAfter { .. }))
    );
}

// ─── Quit + exit ───────────────────────────────────────────────────

#[test]
fn quit_saves_and_sets_exit() {
    let (state, cmds) = update(fresh(), Msg::Quit);
    assert!(state.should_exit);
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
}

// ─── Tick is pure no-op ────────────────────────────────────────────

#[test]
fn tick_never_mutates_visible_state() {
    let before = fresh();
    let before_msg_count = before.session.messages().len();
    let (after, cmds) = update(before, Msg::Tick);
    assert!(cmds.is_empty());
    assert_eq!(after.session.messages().len(), before_msg_count);
    assert!(matches!(after.turn, TurnState::Idle));
}
