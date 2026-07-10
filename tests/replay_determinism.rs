//! The reducer-purity property behind `--replay`: folding the same `Msg`
//! sequence under the same injected clock ALWAYS produces the same `State`.
//!
//! `update(State, Msg)` must read time only from `state.now` and mint ids
//! only from in-state counters — no wall clock, no randomness, no ambient
//! environment. These tests drive a realistic session script (prompt →
//! streamed tool call → tool outcome → follow-up answer → slash command →
//! quit) through the public API twice and require byte-identical `Debug`
//! state, plus a clock-shift probe proving the *only* time-dependence is
//! the injected clock itself.

use std::path::PathBuf;

use mermaid_cli::app::Config;
use mermaid_cli::domain::{Msg, State, ToolCallId, ToolOutcome, TurnId, start_generating, update};
use mermaid_cli::models::tool_call::{FunctionCall, ToolCall as ModelToolCall};
use mermaid_cli::models::{FinishReason, MessageRole, TokenUsage};

fn fixed_ts(offset_secs: i64) -> chrono::DateTime<chrono::Local> {
    chrono::DateTime::parse_from_rfc3339("2026-07-02T09:30:00.250+00:00")
        .unwrap()
        .with_timezone(&chrono::Local)
        + chrono::Duration::seconds(offset_secs)
}

/// A realistic session script: each entry is (clock offset, Msg), exactly
/// the shape a `--record` log stores. Turn/call ids follow what the
/// allocator hands out live (turns from 1, tool calls from 1); any entry
/// the reducer drops as stale is still a deterministic drop.
fn script() -> Vec<(i64, Msg)> {
    vec![
        (
            1,
            Msg::SubmitPrompt {
                text: "read the readme".to_string(),
                attachment_ids: Vec::new(),
            },
        ),
        (
            2,
            Msg::StreamText {
                turn: TurnId(1),
                chunk: "Let me look. ".to_string(),
            },
        ),
        (
            2,
            Msg::StreamToolCall {
                turn: TurnId(1),
                call: ModelToolCall {
                    id: Some("call_a".to_string()),
                    function: FunctionCall {
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "README.md"}),
                    },
                },
            },
        ),
        (
            3,
            Msg::StreamDone {
                turn: TurnId(1),
                usage: Some(TokenUsage::provider(100, 20)),
                provider_continuation: None,
                stop_reason: Some(FinishReason::ToolUse),
            },
        ),
        (
            3,
            Msg::ToolStarted {
                turn: TurnId(1),
                call_id: ToolCallId(1),
            },
        ),
        (
            4,
            Msg::ToolFinished {
                turn: TurnId(1),
                call_id: ToolCallId(1),
                outcome: ToolOutcome::success("# Mermaid\n...", "read 120 lines", 0.2),
            },
        ),
        // Follow-up generation after tools — the reducer starts a fresh
        // model call; stream events for it carry the next turn id.
        (
            5,
            Msg::StreamText {
                turn: TurnId(2),
                chunk: "The readme says…".to_string(),
            },
        ),
        (
            6,
            Msg::StreamDone {
                turn: TurnId(2),
                usage: Some(TokenUsage::provider(150, 40)),
                provider_continuation: None,
                stop_reason: Some(FinishReason::Stop),
            },
        ),
        (7, Msg::Slash(mermaid_cli::domain::SlashCmd::Usage)),
        (7, Msg::Tick),
        (8, Msg::Quit),
    ]
}

fn fold(base_ts_offset: i64) -> State {
    let mut state = State::new(
        Config::default(),
        PathBuf::from("/tmp/determinism"),
        "ollama/test".to_string(),
        fixed_ts(base_ts_offset),
    );
    for (offset, msg) in script() {
        state.now = fixed_ts(base_ts_offset + offset);
        let (next, _cmds) = update(state, msg);
        state = next;
        if state.should_exit {
            break;
        }
    }
    state
}

#[test]
fn same_script_same_clock_folds_to_identical_state() {
    let a = fold(0);
    let b = fold(0);
    assert_eq!(
        format!("{a:?}"),
        format!("{b:?}"),
        "update() must be a pure function of (State, Msg) — \
         two folds of the same script diverged"
    );
}

#[test]
fn the_script_exercises_the_full_turn_machinery() {
    // Guard against the script silently degrading into no-ops (e.g. a
    // future turn-allocation change making every effect msg stale): the
    // fold must actually commit a user prompt, a tool result, and an
    // assistant answer.
    let state = fold(0);
    let messages = state.session.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.role == MessageRole::User && m.content == "read the readme"),
        "user prompt missing from transcript"
    );
    assert!(
        messages.iter().any(|m| m.role == MessageRole::Tool),
        "tool outcome never committed — turn ids in the script have \
         drifted from what the allocator hands out"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.role == MessageRole::Assistant && m.content.contains("readme says")),
        "follow-up assistant answer missing"
    );
    assert!(state.should_exit, "Quit must end the fold");
}

#[test]
fn tick_is_a_reducer_noop() {
    // The recorder ELIDES `Msg::Tick` from `--record` logs on the strength
    // of this invariant: the Tick arm mutates nothing (render derives the
    // spinner and elapsed time from `state.now`), so recording a 60 Hz tick
    // stream would bloat logs by megabytes per hour for zero replay
    // fidelity. If this test fails because Tick has grown state effects,
    // ticks must be recorded again — see `Recorder::record_msg`.
    let idle = State::new(
        Config::default(),
        PathBuf::from("/tmp/tick"),
        "ollama/test".to_string(),
        fixed_ts(0),
    );
    let mut busy = State::new(
        Config::default(),
        PathBuf::from("/tmp/tick"),
        "ollama/test".to_string(),
        fixed_ts(0),
    );
    busy.turn = start_generating(TurnId(1), std::time::SystemTime::from(fixed_ts(0)));
    let after_session = fold(0); // full script: transcript + should_exit

    for state in [idle, busy, after_session] {
        let before = format!("{state:?}");
        let (after, cmds) = update(state, Msg::Tick);
        assert_eq!(
            before,
            format!("{after:?}"),
            "Msg::Tick mutated state — record ticks again before shipping this"
        );
        assert!(cmds.is_empty(), "Msg::Tick emitted commands");
    }
}

#[test]
fn clock_shift_changes_only_clock_derived_state() {
    // Fold the same script an hour apart. Transcript CONTENT must be
    // identical — only clock-derived fields (conversation id/title,
    // timestamps) may differ. This pins down that the injected clock is
    // the reducer's one and only source of time.
    let a = fold(0);
    let b = fold(3600);

    let contents = |s: &State| -> Vec<(MessageRole, String)> {
        s.session
            .messages()
            .iter()
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect()
    };
    assert_eq!(
        contents(&a),
        contents(&b),
        "content must not depend on the clock"
    );
    assert_ne!(
        a.session.conversation.id, b.session.conversation.id,
        "conversation id must derive from the injected clock"
    );
    assert_eq!(
        a.session.conversation.id,
        format!("{}", fixed_ts(0).format("%Y%m%d_%H%M%S_%3f")),
        "id must be exactly the header clock, formatted"
    );
}
