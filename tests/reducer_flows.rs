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
    Cmd, CompactionRecord, CompactionResult, CompactionTrigger, ContextUsageSnapshot, Msg,
    PendingToolCall, PromptTokenBreakdown, SlashCmd, State, ToolCallId, ToolOutcome, TurnId,
    TurnState, start_executing_tools, start_generating, update,
};
use mermaid_cli::models::tool_call::{FunctionCall, ToolCall as ModelToolCall};
use mermaid_cli::models::{ChatMessage, ChatMessageKind, MessageRole};

fn fresh() -> State {
    State::new(
        Config::default(),
        PathBuf::from("/tmp/flow"),
        "ollama/test".to_string(),
        chrono::Local::now(),
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
            stop_reason: None,
        },
    );

    assert!(matches!(state.turn, TurnState::Idle));
    // user prompt + assistant answer + the run-end summary line ("Worked for …").
    assert_eq!(state.session.messages().len(), 3);
    let assistant = &state.session.messages()[1];
    assert_eq!(assistant.role, MessageRole::Assistant);
    assert_eq!(assistant.content, "hello back");
    let summary = state.session.messages().last().unwrap();
    assert_eq!(summary.kind, ChatMessageKind::RunSummary);
    assert!(summary.content.contains("Worked for"));
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
            stop_reason: None,
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
    state.turn = start_generating(TurnId(10), std::time::SystemTime::now());
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
    state.turn = start_executing_tools(TurnId(1), calls, std::time::SystemTime::now());
    // Plant the prior assistant message so action displays attach.
    state.session.append(
        mermaid_cli::models::ChatMessage::assistant("ok let me call tools"),
        state.now,
    );

    // Only first tool finishes — must stay in ExecutingTools.
    let (state, cmds) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(1),
            call_id: ToolCallId(1),
            outcome: ToolOutcome::success("first done", "first done", 0.1),
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
            outcome: ToolOutcome::success("second done", "second done", 0.1),
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
    state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
    state.session.append(
        mermaid_cli::models::ChatMessage::assistant("calling tool"),
        state.now,
    );

    let (state, _) = update(
        state,
        Msg::ToolFinished {
            turn: TurnId(3),
            call_id: ToolCallId(1),
            outcome: ToolOutcome::cancelled(),
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
    state.turn = start_generating(TurnId(7), std::time::SystemTime::now());

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

/// Full cancellation lifecycle: Idle → Generating → Cancelling →
/// TurnCancelled → Idle. Before F1 the reducer had no arm for the
/// terminal event and the TUI stuck in `Cancelling` until an
/// `UpstreamError` from the aborted provider happened to land — a
/// side-effect that couldn't be relied on once providers started
/// returning `ModelError::Cancelled` silently.
#[test]
fn cancel_then_turn_cancelled_returns_to_idle() {
    let (state, _) = user_submit(fresh(), "will be cancelled");
    let id = state.current_turn_id().expect("turn in flight");

    let (state, cmds) = update(state, Msg::CancelTurn);
    assert!(matches!(state.turn, TurnState::Cancelling { .. }));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::CancelScope(_))));

    // Runner would emit this after the TurnScope drains.
    let (state, _) = update(state, Msg::TurnCancelled(id));
    assert!(
        matches!(state.turn, TurnState::Idle),
        "TurnCancelled should clear Cancelling; got {:?}",
        state.turn,
    );
}

/// A `TurnCancelled` for a non-current turn is filtered out before the
/// handler runs. Protects against the effect runner emitting a stale
/// terminal event for a turn the reducer already finished via another
/// path (e.g. successful StreamDone raced cancel).
#[test]
fn stale_turn_cancelled_does_not_mutate_state() {
    let (state, _) = user_submit(fresh(), "active turn");
    let live = state.current_turn_id().unwrap();
    let stale = TurnId(live.0 + 100);

    let (state, cmds) = update(state, Msg::TurnCancelled(stale));
    assert!(matches!(state.turn, TurnState::Generating { .. }));
    assert!(cmds.is_empty());
}

// ─── upstream errors ───────────────────────────────────────────────

#[test]
fn upstream_error_ends_turn_exactly_once() {
    let mut state = fresh();
    state.turn = start_generating(TurnId(4), std::time::SystemTime::now());

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
    // The error is surfaced only through the ActionDisplay in chat — a single
    // channel. The bug this guards against was a second copy (the removed status
    // banner rendered the same error again above the input).
    let last = state.session.messages().last().expect("error message");
    assert_eq!(
        last.actions.len(),
        1,
        "error surfaced through one ActionDisplay"
    );
}

#[test]
fn upstream_error_from_stale_turn_is_dropped() {
    let mut state = fresh();
    state.turn = start_generating(TurnId(8), std::time::SystemTime::now());
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
    state.session.append(
        mermaid_cli::models::ChatMessage::user("priceless"),
        state.now,
    );
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
fn slash_compact_emits_compaction_command() {
    let mut state = fresh();
    state
        .session
        .append(ChatMessage::user("old prompt"), state.now);
    state
        .session
        .append(ChatMessage::assistant("old answer"), state.now);
    state
        .session
        .append(ChatMessage::user("new prompt"), state.now);

    let (state, cmds) = update(
        state,
        Msg::Slash(SlashCmd::Compact(Some("focus on tests".to_string()))),
    );

    // The live "Compacting…" indicator is driven by TurnState::Compacting (the
    // blue status line); there is no separate gray status message anymore.
    assert!(matches!(state.turn, TurnState::Compacting { .. }));
    assert!(cmds.iter().any(|cmd| {
        matches!(
            cmd,
            Cmd::CompactConversation { request, .. }
                if request.instructions.as_deref() == Some("focus on tests")
        )
    }));
}

#[test]
fn compaction_finished_replaces_history_and_archives_head() {
    let mut state = fresh();
    state
        .session
        .append(ChatMessage::user("old prompt"), state.now);
    state
        .session
        .append(ChatMessage::assistant("old answer"), state.now);
    state
        .session
        .append(ChatMessage::user("new prompt"), state.now);
    let (state, cmds) = update(state, Msg::Slash(SlashCmd::Compact(None)));
    let turn = state.turn.id().expect("compaction turn");
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::CompactConversation { .. }))
    );

    let before = ContextUsageSnapshot::from_estimate(
        PromptTokenBreakdown {
            system_tokens: 10,
            instructions_tokens: 0,
            message_tokens: 90,
            tool_schema_tokens: 0,
            image_count: 0,
            message_count: 3,
            tool_count: 0,
        },
        Some(1_000),
    );
    let after = ContextUsageSnapshot::from_estimate(
        PromptTokenBreakdown {
            system_tokens: 10,
            instructions_tokens: 0,
            message_tokens: 20,
            tool_schema_tokens: 0,
            image_count: 0,
            message_count: 3,
            tool_count: 0,
        },
        Some(1_000),
    );
    let mut checkpoint = ChatMessage::user("MERMAID CONTEXT CHECKPOINT\n## Goal\n- continue");
    checkpoint.kind = ChatMessageKind::ContextCheckpoint;
    let replacement = vec![
        checkpoint,
        ChatMessage::assistant("Context compacted: 100 -> 30 tokens."),
        ChatMessage::user("new prompt"),
    ];
    let result = CompactionResult {
        record: CompactionRecord {
            id: "compact_test".to_string(),
            trigger: CompactionTrigger::Manual,
            created_at: chrono::Local::now(),
            before_tokens: 100,
            after_tokens: 30,
            archived_message_count: 2,
            preserved_message_count: 1,
            summary_tokens: 10,
            duration_secs: 0.5,
            verified: true,
            verification_error: None,
            focus: None,
            archive_path: None,
        },
        replacement_messages: replacement,
        archived_messages: vec![
            ChatMessage::user("old prompt"),
            ChatMessage::assistant("old answer"),
        ],
        before_snapshot: before,
        after_snapshot: after,
        usage: None,
    };

    let (state, cmds) = update(state, Msg::CompactionFinished { turn, result });
    assert!(matches!(state.turn, TurnState::Idle));
    assert_eq!(state.session.messages().len(), 3);
    assert_eq!(
        state.session.messages()[0].kind,
        ChatMessageKind::ContextCheckpoint
    );
    assert_eq!(state.session.conversation.compactions.len(), 1);
    // Compaction now emits a single SaveCompactionArchive that embeds the
    // stripped conversation (archive + conversation persisted in order by one
    // effect task), rather than two independent save commands.
    let archive_cmd = cmds
        .iter()
        .find(|cmd| matches!(cmd, Cmd::SaveCompactionArchive { .. }));
    assert!(archive_cmd.is_some());
    if let Some(Cmd::SaveCompactionArchive { conversation, .. }) = archive_cmd {
        assert_eq!(conversation.compactions.len(), 1);
    }
}

#[test]
fn truncation_note_skipped_when_tool_calls_pending() {
    // #72: a `Length` stop with buffered tool calls must NOT insert a "⚠
    // truncated" system message between the assistant's tool_calls and the tool
    // results — that ordering 400s Anthropic. The note is dropped; the turn
    // proceeds to ExecutingTools.
    let (state, _) = user_submit(fresh(), "do a thing");
    let id = state.current_turn_id().unwrap();
    let (state, _) = update(
        state,
        Msg::StreamToolCall {
            turn: id,
            call: ModelToolCall {
                id: Some("call_1".to_string()),
                function: FunctionCall {
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "x"}),
                },
            },
        },
    );
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: id,
            usage: None,
            thinking_signature: None,
            stop_reason: Some(mermaid_cli::models::FinishReason::Length),
        },
    );
    assert!(
        matches!(state.turn, TurnState::ExecutingTools { .. }),
        "expected ExecutingTools; got {:?}",
        state.turn
    );
    assert!(
        !state
            .session
            .messages()
            .iter()
            .any(|m| m.role == MessageRole::System && m.content.contains("truncated")),
        "truncation note must not be inserted between tool_calls and their results"
    );
}

#[test]
fn truncation_note_shown_when_no_tool_calls() {
    // The mirror of #72: a `Length` stop with no tool calls still surfaces the
    // note (the turn ends, so ordering is safe).
    let (state, _) = user_submit(fresh(), "summarize");
    let id = state.current_turn_id().unwrap();
    let (state, _) = update(
        state,
        Msg::StreamDone {
            turn: id,
            usage: None,
            thinking_signature: None,
            stop_reason: Some(mermaid_cli::models::FinishReason::Length),
        },
    );
    assert!(matches!(state.turn, TurnState::Idle));
    assert!(
        state
            .session
            .messages()
            .iter()
            .any(|m| m.role == MessageRole::System && m.content.contains("truncated")),
        "a Length stop with no tool calls should surface the truncation note"
    );
}

#[test]
fn manual_compaction_finish_drains_queued_message() {
    // #73: a message typed during `/compact` must auto-submit when compaction
    // finishes, not sit in the queue until some later turn happens to end.
    let mut state = fresh();
    state
        .session
        .append(ChatMessage::user("old prompt"), state.now);
    state
        .session
        .append(ChatMessage::assistant("old answer"), state.now);
    state
        .session
        .append(ChatMessage::user("new prompt"), state.now);
    let (state, _) = update(state, Msg::Slash(SlashCmd::Compact(None)));
    let turn = state.turn.id().expect("compaction turn");

    // User types while compaction runs → queued (turn is Compacting, not Idle).
    let (state, _) = update(
        state,
        Msg::SubmitPrompt {
            text: "while you were compacting".to_string(),
            attachment_ids: vec![],
        },
    );
    assert_eq!(
        state.ui.queued_messages.len(),
        1,
        "message should queue during compaction"
    );

    let snap = |msg_tokens| {
        ContextUsageSnapshot::from_estimate(
            PromptTokenBreakdown {
                system_tokens: 10,
                instructions_tokens: 0,
                message_tokens: msg_tokens,
                tool_schema_tokens: 0,
                image_count: 0,
                message_count: 3,
                tool_count: 0,
            },
            Some(1_000),
        )
    };
    let mut checkpoint = ChatMessage::user("MERMAID CONTEXT CHECKPOINT\n## Goal\n- continue");
    checkpoint.kind = ChatMessageKind::ContextCheckpoint;
    let result = CompactionResult {
        record: CompactionRecord {
            id: "compact_test".to_string(),
            trigger: CompactionTrigger::Manual,
            created_at: chrono::Local::now(),
            before_tokens: 100,
            after_tokens: 30,
            archived_message_count: 2,
            preserved_message_count: 1,
            summary_tokens: 10,
            duration_secs: 0.5,
            verified: true,
            verification_error: None,
            focus: None,
            archive_path: None,
        },
        replacement_messages: vec![checkpoint, ChatMessage::user("new prompt")],
        archived_messages: vec![
            ChatMessage::user("old prompt"),
            ChatMessage::assistant("old answer"),
        ],
        before_snapshot: snap(90),
        after_snapshot: snap(20),
        usage: None,
    };

    let (state, cmds) = update(state, Msg::CompactionFinished { turn, result });
    // The queued message drained → the folded SubmitPrompt auto-started a turn.
    assert!(
        state.ui.queued_messages.is_empty(),
        "queued message must drain on manual compaction finish (#73)"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
        "draining the queued message should auto-submit it (CallModel emitted)"
    );
    assert!(matches!(state.turn, TurnState::Generating { .. }));
}

#[test]
fn slash_unknown_posts_to_transcript() {
    // An unknown command posts to the chat transcript.
    let (state, cmds) = update(fresh(), Msg::Slash(SlashCmd::Unknown("nope".to_string())));
    assert!(
        state
            .session
            .messages()
            .last()
            .is_some_and(|m| m.content.contains("Unknown command: /nope")),
        "unknown command posts a note to the chat transcript"
    );
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
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
fn stream_tool_call_buffers_then_stream_done_transitions_to_executing_tools() {
    // The v7 critical path: a model-emitted tool call in a streaming
    // response must buffer on `TurnState::Generating.pending_tool_calls`
    // and transition to `ExecutingTools` when `StreamDone` arrives
    // non-empty. Guards the "tool call arrives, nothing happens"
    // regression that was live in v0.7.0.
    let (state, _) = user_submit(fresh(), "do a thing");
    let id = state.current_turn_id().unwrap();

    let (state, _) = update(
        state,
        Msg::StreamToolCall {
            turn: id,
            call: ModelToolCall {
                id: Some("call_1".to_string()),
                function: FunctionCall {
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "x"}),
                },
            },
        },
    );

    let (state, cmds) = update(
        state,
        Msg::StreamDone {
            turn: id,
            usage: None,
            thinking_signature: None,
            stop_reason: None,
        },
    );

    assert!(
        matches!(state.turn, TurnState::ExecutingTools { .. }),
        "expected ExecutingTools; got {:?}",
        state.turn
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::ExecuteTool { .. })),
        "expected Cmd::ExecuteTool; got {:?}",
        cmds.iter().map(|c| c.tag()).collect::<Vec<_>>()
    );
}

#[test]
fn tool_progress_artifact_routes_image_to_assistant_message() {
    // Commit 2 wire: an `Artifact` event with `image/*` mime arriving
    // during ExecutingTools should land base64-encoded on the last
    // assistant message's `images` field so the chat widget renders
    // it without waiting for ToolFinished.
    use mermaid_cli::providers::ProgressEvent;

    // Build a state with a committed assistant message and
    // ExecutingTools turn state (the shape a tool runs inside).
    let (state, _) = user_submit(fresh(), "take a screenshot");
    let id = state.current_turn_id().unwrap();

    // Simulate the assistant having already committed a message
    // (e.g. after StreamDone with a tool call). We bypass the full
    // flow and construct the state manually.
    let mut state = state;
    state.session.append(
        mermaid_cli::models::ChatMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            timestamp: chrono::Local::now(),
            kind: mermaid_cli::models::ChatMessageKind::Normal,
            metadata: None,
            actions: vec![],
            thinking: None,
            images: None,
            image_numbers: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            thinking_signature: None,
        },
        state.now,
    );
    state.turn = start_executing_tools(
        id,
        vec![PendingToolCall {
            call_id: ToolCallId(1),
            source: ModelToolCall {
                id: Some("c1".to_string()),
                function: FunctionCall {
                    name: "screenshot".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        }],
        std::time::SystemTime::now(),
    );

    let data = vec![0x89, 0x50, 0x4E, 0x47]; // PNG magic bytes — content doesn't matter
    let (state, _) = update(
        state,
        Msg::ToolProgress {
            turn: id,
            call_id: ToolCallId(1),
            event: ProgressEvent::Artifact {
                mime: "image/png".to_string(),
                data: data.clone(),
                caption: Some("preview".to_string()),
            },
        },
    );

    let last = state.session.messages().last().expect("last msg");
    assert_eq!(last.role, MessageRole::Assistant);
    let imgs = last.images.as_ref().expect("images attached");
    assert_eq!(imgs.len(), 1, "one artifact appended");
    // Roundtrip: base64-decode and confirm bytes match.
    use base64::{Engine as _, engine::general_purpose};
    let decoded = general_purpose::STANDARD.decode(&imgs[0]).unwrap();
    assert_eq!(decoded, data);
}

/// F5: configured MCP servers must seed the state map so their
/// `McpServerReady` events can land. Before this, `state.mcp.servers`
/// started empty and `get_mut` silently dropped ready events —
/// configured MCP tools never reached the outgoing `ChatRequest.tools`.
#[test]
fn configured_mcp_servers_seed_state_and_ready_updates() {
    use mermaid_cli::app::{Config as AppConfig, McpServerConfig};
    use mermaid_cli::domain::{McpServerStatus, McpToolSpec};

    let mut cfg = AppConfig::default();
    cfg.mcp_servers.insert(
        "context7".to_string(),
        McpServerConfig {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@upstash/context7-mcp".to_string()],
            env: std::collections::HashMap::new(),
            ..Default::default()
        },
    );
    let state = State::new(
        cfg,
        PathBuf::from("/tmp/p"),
        "ollama/test".to_string(),
        chrono::Local::now(),
    );

    // Seed placed the entry in Starting status before any effects run.
    let entry = state
        .mcp
        .servers
        .get("context7")
        .expect("configured server must be seeded");
    assert_eq!(entry.status, McpServerStatus::Starting);
    assert!(entry.tools.is_empty());

    // Ready event with a tool list upgrades status and records tools.
    let (state, _) = update(
        state,
        Msg::McpServerReady {
            name: "context7".to_string(),
            tools: vec![McpToolSpec {
                name: "resolve-library-id".to_string(),
                description: "Resolve a library name".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
        },
    );
    let entry = &state.mcp.servers["context7"];
    assert_eq!(entry.status, McpServerStatus::Ready);
    assert_eq!(entry.tools.len(), 1);
    assert_eq!(entry.tools[0].name, "resolve-library-id");
}

#[test]
fn tick_never_mutates_visible_state() {
    let before = fresh();
    let before_msg_count = before.session.messages().len();
    let (after, cmds) = update(before, Msg::Tick);
    assert!(cmds.is_empty());
    assert_eq!(after.session.messages().len(), before_msg_count);
    assert!(matches!(after.turn, TurnState::Idle));
}

// ─── ask_user_question modal interaction ───────────────────────────
//
// Drive the question modal's key-handling state machine through `update`,
// seeding a pending question set directly (the broker/effect round-trip is
// unit-tested separately in `providers::questions`).

use mermaid_cli::domain::{
    Key, KeyCode, KeyMods, PendingQuestionSet, Question, QuestionKind, QuestionOption,
    QuestionResolution, TextValidate,
};

fn opt(label: &str) -> QuestionOption {
    QuestionOption {
        label: label.to_string(),
        description: Some("desc".to_string()),
        recommended: false,
        preview: None,
    }
}

fn question(header: &str, text: &str, multi: bool, labels: &[&str]) -> Question {
    Question {
        header: header.to_string(),
        question: text.to_string(),
        kind: if multi {
            QuestionKind::MultiSelect
        } else {
            QuestionKind::Select
        },
        options: labels.iter().map(|l| opt(l)).collect(),
        memory_key: None,
    }
}

fn seed_questions(questions: Vec<Question>) -> State {
    let mut s = fresh();
    s.pending_question
        .push_back(PendingQuestionSet::new(TurnId(1), ToolCallId(1), questions));
    s
}

fn press(state: State, code: KeyCode) -> (State, Vec<Cmd>) {
    update(
        state,
        Msg::Key(Key {
            code,
            modifiers: KeyMods::NONE,
        }),
    )
}

fn resolution(cmds: &[Cmd]) -> Option<&QuestionResolution> {
    cmds.iter().find_map(|c| match c {
        Cmd::ResolveQuestion { resolution, .. } => Some(resolution),
        _ => None,
    })
}

fn assert_answered(cmds: &[Cmd], expected: &[&[&str]]) {
    match resolution(cmds) {
        Some(QuestionResolution::Answered { answers: ans, .. }) => {
            assert_eq!(ans.len(), expected.len(), "answer count");
            for (a, exp) in ans.iter().zip(expected) {
                let got: Vec<&str> = a.selected.iter().map(|s| s.as_str()).collect();
                assert_eq!(&got, exp, "selected labels for {:?}", a.question);
            }
        },
        other => panic!("expected Answered, got {other:?}"),
    }
}

#[test]
fn single_select_number_key_resolves_immediately() {
    let state = seed_questions(vec![question(
        "DB",
        "Which database?",
        false,
        &["PostgreSQL", "SQLite"],
    )]);
    let (state, cmds) = press(state, KeyCode::Char('1'));
    assert!(state.pending_question.is_empty(), "resolved and popped");
    assert_answered(&cmds, &[&["PostgreSQL"]]);
}

#[test]
fn single_select_arrow_then_enter_picks_second() {
    let state = seed_questions(vec![question("DB", "Which?", false, &["A", "B"])]);
    let (state, _) = press(state, KeyCode::Down);
    let (state, cmds) = press(state, KeyCode::Enter);
    assert!(state.pending_question.is_empty());
    assert_answered(&cmds, &[&["B"]]);
}

#[test]
fn multi_select_toggles_then_submits_via_review() {
    let state = seed_questions(vec![question(
        "Feat",
        "Which features?",
        true,
        &["Auth", "API", "Jobs"],
    )]);
    let (state, _) = press(state, KeyCode::Char('1')); // toggle Auth (cursor→0)
    let (state, _) = press(state, KeyCode::Char('3')); // toggle Jobs (cursor→2)
    let (state, _) = press(state, KeyCode::Down); // → Other row
    let (state, _) = press(state, KeyCode::Down); // → Submit row
    let (state, _) = press(state, KeyCode::Enter); // Submit → review screen
    assert!(
        !state.pending_question.is_empty(),
        "review screen, not resolved"
    );
    let (state, cmds) = press(state, KeyCode::Enter); // review: Submit answers
    assert!(state.pending_question.is_empty());
    assert_answered(&cmds, &[&["Auth", "Jobs"]]);
}

#[test]
fn batched_questions_advance_then_review_submit() {
    let state = seed_questions(vec![
        question("Runtime", "Which runtime?", false, &["Node", "Python"]),
        question("Style", "Which styling?", false, &["Tailwind", "CSS"]),
    ]);
    let (state, _) = press(state, KeyCode::Enter); // Q1: Node → advance to Q2
    let (state, _) = press(state, KeyCode::Char('2')); // Q2: CSS → advance to review
    assert!(!state.pending_question.is_empty(), "review screen");
    let (state, cmds) = press(state, KeyCode::Enter); // submit
    assert!(state.pending_question.is_empty());
    assert_answered(&cmds, &[&["Node"], &["CSS"]]);
}

#[test]
fn esc_dismisses_the_question() {
    let state = seed_questions(vec![question("DB", "Which?", false, &["A", "B"])]);
    let (state, cmds) = press(state, KeyCode::Escape);
    assert!(state.pending_question.is_empty());
    assert!(matches!(
        resolution(&cmds),
        Some(QuestionResolution::Dismissed)
    ));
}

#[test]
fn other_free_text_becomes_the_answer() {
    let state = seed_questions(vec![question("DB", "Which?", false, &["A", "B"])]);
    let (state, _) = press(state, KeyCode::Down); // → option B
    let (mut state, _) = press(state, KeyCode::Down); // → Other row
    for ch in "duck".chars() {
        let (s, _) = press(state, KeyCode::Char(ch));
        state = s;
    }
    let (state, cmds) = press(state, KeyCode::Enter);
    assert!(state.pending_question.is_empty());
    assert_answered(&cmds, &[&["duck"]]);
}

#[test]
fn note_rides_back_with_the_answer() {
    let state = seed_questions(vec![question("DB", "Which?", false, &["A", "B"])]);
    let (mut state, _) = press(state, KeyCode::Char('n')); // open note editing
    for ch in "prefer A".chars() {
        let (s, _) = press(state, KeyCode::Char(ch));
        state = s;
    }
    let (state, _) = press(state, KeyCode::Enter); // exit note editing (keeps note)
    let (state, cmds) = press(state, KeyCode::Char('1')); // pick A -> resolves
    assert!(state.pending_question.is_empty());
    match resolution(&cmds) {
        Some(QuestionResolution::Answered { answers: ans, .. }) => {
            assert_eq!(ans[0].selected, vec!["A".to_string()]);
            assert_eq!(ans[0].note.as_deref(), Some("prefer A"));
        },
        other => panic!("expected Answered, got {other:?}"),
    }
}

fn input_q(kind: QuestionKind) -> Question {
    Question {
        header: "H".to_string(),
        question: "Q?".to_string(),
        kind,
        options: vec![],
        memory_key: None,
    }
}

fn rank_q(labels: &[&str]) -> Question {
    Question {
        header: "H".to_string(),
        question: "Q?".to_string(),
        kind: QuestionKind::Rank,
        options: labels.iter().map(|l| opt(l)).collect(),
        memory_key: None,
    }
}

#[test]
fn text_input_value_rides_back() {
    let state = seed_questions(vec![input_q(QuestionKind::Text {
        validate: TextValidate::Any,
    })]);
    let mut state = state;
    for ch in "hello".chars() {
        let (s, _) = press(state, KeyCode::Char(ch));
        state = s;
    }
    let (state, cmds) = press(state, KeyCode::Enter);
    assert!(state.pending_question.is_empty());
    assert_answered(&cmds, &[&["hello"]]);
}

#[test]
fn number_steps_with_up_arrow() {
    let state = seed_questions(vec![input_q(QuestionKind::Number {
        min: Some(0.0),
        max: Some(10.0),
        step: Some(2.0),
        slider: false,
    })]);
    let (state, _) = press(state, KeyCode::Up); // 0 -> 2
    let (state, _) = press(state, KeyCode::Up); // 2 -> 4
    let (state, cmds) = press(state, KeyCode::Enter);
    assert!(state.pending_question.is_empty());
    assert_answered(&cmds, &[&["4"]]);
}

#[test]
fn invalid_number_blocks_submit() {
    let state = seed_questions(vec![input_q(QuestionKind::Text {
        validate: TextValidate::Number,
    })]);
    let mut state = state;
    for ch in "abc".chars() {
        let (s, _) = press(state, KeyCode::Char(ch));
        state = s;
    }
    let (state, cmds) = press(state, KeyCode::Enter);
    assert!(
        !state.pending_question.is_empty(),
        "invalid value must not submit"
    );
    assert!(resolution(&cmds).is_none());
}

#[test]
fn rank_reorder_moves_item_and_submits() {
    let state = seed_questions(vec![rank_q(&["A", "B", "C"])]);
    let (state, _) = press(state, KeyCode::Char(' ')); // grab A (cursor 0)
    let (state, _) = press(state, KeyCode::Down); // -> B, A, C
    let (state, _) = press(state, KeyCode::Down); // -> B, C, A
    let (state, _) = press(state, KeyCode::Char(' ')); // drop
    let (state, cmds) = press(state, KeyCode::Enter);
    assert!(state.pending_question.is_empty());
    assert_answered(&cmds, &[&["B", "C", "A"]]);
}

#[test]
fn chat_about_this_reformulates() {
    let state = seed_questions(vec![question("DB", "Which?", false, &["A", "B"])]);
    let (state, cmds) = press(state, KeyCode::Char('c'));
    assert!(state.pending_question.is_empty());
    assert!(matches!(
        resolution(&cmds),
        Some(QuestionResolution::Reformulate)
    ));
}

#[test]
fn r_toggles_remember_flag() {
    let state = seed_questions(vec![question("DB", "Which?", false, &["A", "B"])]);
    let (state, _) = press(state, KeyCode::Char('r')); // remember -> true
    let (_state, cmds) = press(state, KeyCode::Char('1')); // pick A -> resolves
    match resolution(&cmds) {
        Some(QuestionResolution::Answered { remember, .. }) => assert!(*remember),
        other => panic!("expected Answered, got {other:?}"),
    }
}
