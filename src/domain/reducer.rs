//! The pure reducer: `fn update(State, Msg) -> (State, Vec<Cmd>)`.
//!
//! Four rules — every future change to this file is checked against them:
//!
//!   1. **No I/O, no async.** The function is `sync`, never awaits,
//!      never opens files. All side-effects are returned as `Cmd`.
//!   2. **No wildcards.** The `match msg` is exhaustive. Adding a new
//!      `Msg` variant is a compile error until every arm is handled.
//!   3. **Stale-filter first.** Any `Msg` carrying a `TurnId` that
//!      doesn't match `state.turn.id()` is dropped without state
//!      change. This is the architectural safeguard that the previous
//!      `check_interrupt` polling tried to enforce by convention.
//!   4. **Cancellation is explicit.** The only way to abort in-flight
//!      work is `Cmd::CancelScope(turn)`. No `handle.abort()` anywhere.
//!
//! The reducer is "transitional" for this commit — it recognizes the
//! full `Msg` vocabulary but several arms currently no-op with a TODO
//! breadcrumb. The scaffolding is here so future commits can fill in
//! behavior one arm at a time while tests pin down the regressions
//! that would otherwise be invisible.

use crate::constants::{DEFAULT_MAX_TOKENS, DEFAULT_TEMPERATURE};
use crate::models::{ChatMessage, MessageRole};
use crate::prompts::get_system_prompt;

use super::cmd::{ChatRequest, Cmd};
use super::ids::TurnId;
use super::msg::{KeyCode, KeyMods, Msg, Paste, SlashCmd};
use super::state::{
    GenPhase, McpServerStatus, State, StatusKind, StatusLine, ToolOutcome, TurnState, UiMode,
};
use super::transition::{
    action_display_for, commit_assistant_message, fill_outcome, start_generating,
    tool_result_messages, try_complete_outcomes,
};

/// Single entry point. Takes state by value, returns the new state
/// plus any side-effects to dispatch.
pub fn update(mut state: State, msg: Msg) -> (State, Vec<Cmd>) {
    let mut cmds = Vec::new();

    // Stale-event filter: if this is an effect result for a turn we're
    // no longer on, drop without state change. `turn_id()` returns
    // `None` for non-turn-scoped messages, which short-circuits the
    // check (Some(id) != None).
    if let Some(event_turn) = msg.turn_id()
        && !state.turn.accepts(event_turn)
    {
        tracing::trace!(
            event_turn = %event_turn,
            active_turn = ?state.turn.id(),
            kind = ?msg.kind(),
            "reducer: dropped stale message"
        );
        return (state, cmds);
    }

    match msg {
        // ── User intent ─────────────────────────────────────────────
        Msg::Key(key) => {
            handle_key(&mut state, &mut cmds, key.code, key.modifiers);
        },
        Msg::Paste(paste) => {
            handle_paste(&mut state, &mut cmds, paste);
        },
        Msg::SubmitPrompt {
            text,
            attachment_ids,
        } => {
            handle_submit_prompt(&mut state, &mut cmds, text, &attachment_ids);
        },
        Msg::Slash(cmd) => {
            handle_slash(&mut state, &mut cmds, cmd);
        },
        Msg::CancelTurn => {
            handle_cancel_turn(&mut state, &mut cmds);
        },
        Msg::ConfirmAccepted => {
            handle_confirm_accepted(&mut state, &mut cmds);
        },
        Msg::ConfirmDeclined => {
            state.confirm = None;
        },
        Msg::Quit => {
            state.should_exit = true;
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
            cmds.push(Cmd::Exit);
        },

        // ── Streaming ───────────────────────────────────────────────
        Msg::StreamText { turn, chunk } => {
            if let TurnState::Generating {
                id,
                partial_text,
                phase,
                tokens,
                ..
            } = &mut state.turn
                && *id == turn
            {
                partial_text.push_str(&chunk);
                *phase = GenPhase::Streaming;
                // Rough token estimate — actual count comes in `Done`.
                *tokens = partial_text.len() / 4;
            }
        },
        Msg::StreamReasoning { turn, chunk } => {
            if let TurnState::Generating {
                id,
                partial_reasoning,
                phase,
                thinking_signature,
                ..
            } = &mut state.turn
                && *id == turn
            {
                partial_reasoning.push_str(&chunk.text);
                *phase = GenPhase::Thinking;
                if let Some(sig) = chunk.signature {
                    *thinking_signature = Some(sig);
                }
            }
        },
        Msg::StreamToolCall { turn, call } => {
            handle_stream_tool_call(&mut state, turn, call);
        },
        Msg::StreamDone {
            turn,
            usage,
            thinking_signature,
        } => {
            handle_stream_done(&mut state, &mut cmds, turn, usage, thinking_signature);
        },
        Msg::UpstreamError { turn: _, error } => {
            handle_upstream_error(&mut state, error);
        },

        // ── Tools ───────────────────────────────────────────────────
        Msg::ToolStarted { turn: _, call_id: _ } => {
            // Informational — render layer derives spinner state from
            // `outcomes[i].is_none()`, so no state change needed yet.
        },
        Msg::ToolProgress {
            turn: _,
            call_id: _,
            chunk: _,
        } => {
            // Reserved for streaming subprocess output; render layer
            // not wired for it yet.
        },
        Msg::ToolFinished {
            turn,
            call_id,
            outcome,
        } => {
            handle_tool_finished(&mut state, &mut cmds, turn, call_id, outcome);
        },

        // ── Subagents ───────────────────────────────────────────────
        Msg::SubagentStatusChanged {
            turn: _,
            subagent,
            status,
        } => {
            if let TurnState::RunningSubagents { progress, .. } = &mut state.turn
                && let Some(entry) = progress.iter_mut().find(|p| p.id == subagent)
            {
                entry.status = status;
            }
        },
        Msg::SubagentToolUseTick {
            turn: _,
            subagent,
        } => {
            if let TurnState::RunningSubagents { progress, .. } = &mut state.turn
                && let Some(entry) = progress.iter_mut().find(|p| p.id == subagent)
            {
                entry.tool_uses = entry.tool_uses.saturating_add(1);
            }
        },

        // ── MCP ─────────────────────────────────────────────────────
        Msg::McpServerReady { name, tools } => {
            if let Some(entry) = state.mcp.servers.get_mut(&name) {
                entry.status = McpServerStatus::Ready;
                entry.tools = tools;
            }
        },
        Msg::McpServerErrored { name, reason } => {
            if let Some(entry) = state.mcp.servers.get_mut(&name) {
                entry.status = McpServerStatus::Errored {
                    reason: reason.clone(),
                };
            }
            state.status = Some(StatusLine {
                text: format!("MCP server {} errored: {}", name, reason),
                kind: StatusKind::Error,
                shown_at: std::time::SystemTime::now(),
            });
        },
        Msg::McpServerStopped { name } => {
            if let Some(entry) = state.mcp.servers.get_mut(&name) {
                entry.status = McpServerStatus::Stopped;
            }
        },

        // ── Persistence / misc ─────────────────────────────────────
        Msg::InstructionsChanged(loaded) => {
            state.instructions = loaded;
        },
        Msg::SessionSaved => {
            // Silent. Reducer already committed; save is just durability.
        },
        Msg::ConversationLoaded(history) => {
            state.session.conversation = history;
            state.turn = TurnState::Idle;
        },
        Msg::ModelPullFinished { model } => {
            state.status = Some(StatusLine {
                text: format!("Pulled {}", model),
                kind: StatusKind::Info,
                shown_at: std::time::SystemTime::now(),
            });
            cmds.push(Cmd::DismissStatusAfter { ms: 2_000 });
        },

        // ── Housekeeping ────────────────────────────────────────────
        Msg::Tick => {
            // Nothing stateful yet; render uses `SystemTime::now()`
            // directly for elapsed-time display.
        },
        Msg::StatusDismiss => {
            state.status = None;
        },
        Msg::Resize { .. } => {
            // Render layer recomputes layout from the new area — no
            // reducer state depends on raw terminal dimensions.
        },
    }

    (state, cmds)
}

// ─── helpers ────────────────────────────────────────────────────────

fn handle_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode, mods: KeyMods) {
    // Ctrl+C: cancel if busy, quit if idle with empty input.
    if mods.ctrl && code == KeyCode::Char('c') {
        if state.is_busy() {
            handle_cancel_turn(state, cmds);
        } else if state.ui.input_buffer.is_empty() {
            state.should_exit = true;
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
            cmds.push(Cmd::Exit);
        } else {
            state.ui.input_buffer.clear();
        }
        return;
    }

    // Ctrl+D on empty input quits.
    if mods.ctrl && code == KeyCode::Char('d') && state.ui.input_buffer.is_empty() {
        state.should_exit = true;
        cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        cmds.push(Cmd::Exit);
        return;
    }

    // The rest is rudimentary text input — real keybind handling
    // (arrow-key history nav, tab completion, Alt+T reasoning cycle,
    // etc.) lands in commit 6/7 when we port the event source. For
    // now we just accumulate printable chars so the reducer can be
    // exercised in tests.
    if mods.is_empty() || mods.shift {
        match code {
            KeyCode::Char(c) => state.ui.input_buffer.push(c),
            KeyCode::Backspace => {
                state.ui.input_buffer.pop();
            },
            _ => {},
        }
    }
}

fn handle_paste(state: &mut State, cmds: &mut Vec<Cmd>, paste: Paste) {
    match paste {
        Paste::Text(t) => state.ui.input_buffer.push_str(&t),
        Paste::Image { bytes, format } => {
            let id = state.ids.tool_call.next();
            let temp_path =
                std::env::temp_dir().join(format!("mermaid-img-{}.{}", id, format));
            state.ui.attachments.push(super::state::Attachment {
                id,
                base64_data: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &bytes,
                ),
                temp_path,
                size_bytes: bytes.len(),
                format: format.clone(),
            });
            cmds.push(Cmd::WriteImageToTemp { id, bytes, format });
        },
    }
}

fn handle_submit_prompt(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    text: String,
    attachment_ids: &[u64],
) {
    if !matches!(state.turn, TurnState::Idle) {
        return;
    }
    if text.trim().is_empty() {
        return;
    }

    // Consume attachments by ID — ignoring stale IDs gracefully.
    let mut images: Vec<String> = Vec::new();
    state.ui.attachments.retain(|a| {
        if attachment_ids.contains(&a.id) {
            images.push(a.base64_data.clone());
            false
        } else {
            true
        }
    });

    let mut user_msg = ChatMessage::user(text.clone());
    if !images.is_empty() {
        user_msg = user_msg.with_images(images);
    }
    state.session.append(user_msg);
    state.session.conversation.add_to_input_history(text);
    state.ui.input_buffer.clear();

    let turn = state.ids.fresh_turn();
    state.turn = start_generating(turn);
    cmds.push(Cmd::CallModel {
        turn,
        request: build_chat_request(state),
    });
    cmds.push(Cmd::RefreshInstructions);
}

fn handle_slash(state: &mut State, cmds: &mut Vec<Cmd>, cmd: SlashCmd) {
    match cmd {
        SlashCmd::Model(None) => {
            state.status = Some(StatusLine {
                text: format!("Current model: {}", state.session.model_id),
                kind: StatusKind::Info,
                shown_at: std::time::SystemTime::now(),
            });
            cmds.push(Cmd::DismissStatusAfter { ms: 3_000 });
        },
        SlashCmd::Model(Some(new_model)) => {
            state.session.model_id = new_model.clone();
            cmds.push(Cmd::PersistLastModel(new_model));
        },
        SlashCmd::Reasoning(None) => {
            state.status = Some(StatusLine {
                text: format!("Reasoning: {}", state.session.reasoning.as_str()),
                kind: StatusKind::Info,
                shown_at: std::time::SystemTime::now(),
            });
            cmds.push(Cmd::DismissStatusAfter { ms: 3_000 });
        },
        SlashCmd::Reasoning(Some(level)) => {
            state.session.reasoning = level;
            cmds.push(Cmd::PersistReasoningFor {
                model_id: state.session.model_id.clone(),
                level,
            });
        },
        SlashCmd::Clear => {
            // Guard with a confirmation modal.
            state.confirm = Some(super::state::Confirmation {
                prompt: "Clear conversation history?".to_string(),
                accept_msg_token: super::state::ConfirmationTarget::ClearConversation,
            });
        },
        SlashCmd::Save(_name) => {
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        SlashCmd::Load(Some(id)) => {
            cmds.push(Cmd::LoadConversation(id));
        },
        SlashCmd::Load(None) | SlashCmd::List => {
            state.ui.mode = UiMode::ConversationList;
        },
        SlashCmd::CloudSetup => {
            // Handed off to the terminal app-layer in the old code;
            // reserved here for the event source to route.
        },
        SlashCmd::Help => {
            state.status = Some(StatusLine {
                text: "See /help output in chat".to_string(),
                kind: StatusKind::Info,
                shown_at: std::time::SystemTime::now(),
            });
            cmds.push(Cmd::DismissStatusAfter { ms: 2_000 });
        },
        SlashCmd::Quit => {
            state.should_exit = true;
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
            cmds.push(Cmd::Exit);
        },
        SlashCmd::Unknown(name) => {
            state.status = Some(StatusLine {
                text: format!("Unknown command: /{}", name),
                kind: StatusKind::Warn,
                shown_at: std::time::SystemTime::now(),
            });
            cmds.push(Cmd::DismissStatusAfter { ms: 2_500 });
        },
    }
}

fn handle_cancel_turn(state: &mut State, cmds: &mut Vec<Cmd>) {
    let Some(id) = state.turn.id() else {
        return;
    };
    // Already cancelling: don't double-cancel.
    if matches!(state.turn, TurnState::Cancelling { .. }) {
        return;
    }
    cmds.push(Cmd::CancelScope(id));
    state.turn = TurnState::Cancelling {
        id,
        since: std::time::SystemTime::now(),
    };
}

fn handle_confirm_accepted(state: &mut State, _cmds: &mut [Cmd]) {
    let Some(confirm) = state.confirm.take() else {
        return;
    };
    match confirm.accept_msg_token {
        super::state::ConfirmationTarget::ClearConversation => {
            state.session.conversation.messages.clear();
            state.session.conversation.updated_at = chrono::Local::now();
        },
        super::state::ConfirmationTarget::OverwriteSavedConversation { .. } => {
            // Reserved — effect layer in commit 6 will handle actual
            // filesystem replacement.
        },
    }
}

fn handle_stream_tool_call(_state: &mut State, _turn: TurnId, _call: crate::models::tool_call::ToolCall) {
    // Reserved. In the full cutover (commit 3), model-emitted tool
    // calls buffer on the `Generating` variant and transition to
    // `ExecutingTools` on `StreamDone`. For the scaffold we no-op so
    // the Msg variant is live but the behaviour lands wired-in later.
}

fn handle_stream_done(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    turn: TurnId,
    _usage: Option<crate::models::TokenUsage>,
    thinking_signature: Option<String>,
) {
    // Take the generating state out so we can replace it.
    let generating = match std::mem::replace(&mut state.turn, TurnState::Idle) {
        TurnState::Generating {
            id,
            partial_text,
            partial_reasoning,
            thinking_signature: accumulated_sig,
            ..
        } if id == turn => (partial_text, partial_reasoning, accumulated_sig),
        other => {
            state.turn = other;
            return;
        },
    };

    let (partial_text, partial_reasoning, accumulated_sig) = generating;
    let final_sig = thinking_signature.or(accumulated_sig);

    // No tool calls in this scaffold pass; commit assistant message
    // and return to Idle. (Commit 3 wires tool calls through via a
    // `pending: Vec<PendingToolCall>` slot on the Generating variant.)
    let msg = commit_assistant_message(partial_text, partial_reasoning, vec![], final_sig);
    state.session.append(msg);
    cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
}

fn handle_upstream_error(state: &mut State, error: crate::models::UserFacingError) {
    // End the current turn. Commit an error line so the user sees it
    // and the assistant history isn't left dangling.
    state.turn = TurnState::Idle;
    let line = format!("{}: {}", error.summary, error.message);
    let mut msg = ChatMessage {
        role: MessageRole::Assistant,
        content: line.clone(),
        timestamp: chrono::Local::now(),
        actions: Vec::new(),
        thinking: None,
        images: None,
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
        thinking_signature: None,
    };
    msg.actions.push(crate::agents::ActionDisplay {
        action_type: "Error".to_string(),
        target: error.summary.clone(),
        result: crate::agents::ActionResult::Error {
            error: error.message.clone(),
        },
        details: crate::agents::ActionDetails::Simple,
        duration_seconds: None,
    });
    state.session.append(msg);
    state.status = Some(StatusLine {
        text: line,
        kind: StatusKind::Error,
        shown_at: std::time::SystemTime::now(),
    });
}

fn handle_tool_finished(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    turn: TurnId,
    call_id: super::ids::ToolCallId,
    outcome: ToolOutcome,
) {
    // Borrow calls + outcomes simultaneously via a helper to avoid
    // double mutable borrow on `state.turn`.
    let completed = match &mut state.turn {
        TurnState::ExecutingTools {
            id,
            calls,
            outcomes,
        } if *id == turn => {
            if !fill_outcome(calls, outcomes, call_id, outcome.clone()) {
                return;
            }
            // Attach action display to the last assistant message so
            // the renderer can show it.
            if let Some(call) = calls.iter().find(|c| c.call_id == call_id)
                && let Some(last) = state.session.conversation.messages.last_mut()
                && last.role == MessageRole::Assistant
            {
                last.actions.push(action_display_for(call, &outcome));
            }
            try_complete_outcomes(outcomes)
        },
        _ => None,
    };

    if let Some(completed_outcomes) = completed
        && let TurnState::ExecutingTools { id, calls, .. } =
            std::mem::replace(&mut state.turn, TurnState::Idle)
        && id == turn
    {
        // Append each tool message to the conversation, then kick off
        // the follow-up model call.
        let tool_msgs = tool_result_messages(&calls, completed_outcomes);
        for m in tool_msgs {
            state.session.append(m);
        }
        let next_turn = state.ids.fresh_turn();
        state.turn = start_generating(next_turn);
        cmds.push(Cmd::CallModel {
            turn: next_turn,
            request: build_chat_request(state),
        });
    }
}

/// Construct the request the model sees for this turn, pulling in the
/// current message log + the active `MERMAID.md` suffix + the
/// reasoning choice + the tools surface.
pub fn build_chat_request(state: &State) -> ChatRequest {
    let instructions = state.instructions.as_ref().map(|i| i.content.clone());
    ChatRequest {
        model_id: state.session.model_id.clone(),
        messages: state.session.messages().to_vec(),
        system_prompt: get_system_prompt(),
        instructions,
        reasoning: state.session.reasoning,
        temperature: state
            .settings
            .default_model
            .temperature
            .max(DEFAULT_TEMPERATURE),
        max_tokens: state
            .settings
            .default_model
            .max_tokens
            .max(DEFAULT_MAX_TOKENS),
        tools: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Config;
    use crate::domain::msg::{Key, KeyCode, KeyMods};
    use crate::domain::state::{McpServerEntry, McpState, PendingToolCall, UiState};
    use crate::domain::transition::start_executing_tools;
    use std::path::PathBuf;

    fn fresh_state() -> State {
        State::new(
            Config::default(),
            PathBuf::from("/tmp/project"),
            "ollama/test".to_string(),
        )
    }

    #[test]
    fn quit_sets_exit_flag_and_emits_save_and_exit() {
        let state = fresh_state();
        let (state, cmds) = update(state, Msg::Quit);
        assert!(state.should_exit);
        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0], Cmd::SaveConversation(_)));
        assert!(matches!(cmds[1], Cmd::Exit));
    }

    #[test]
    fn ctrl_c_on_idle_empty_input_exits() {
        let state = fresh_state();
        let msg = Msg::Key(Key {
            code: KeyCode::Char('c'),
            modifiers: KeyMods::ctrl(),
        });
        let (state, cmds) = update(state, msg);
        assert!(state.should_exit);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
    }

    #[test]
    fn ctrl_c_on_idle_with_input_clears_input_only() {
        let mut state = fresh_state();
        state.ui.input_buffer = "partial".to_string();
        let msg = Msg::Key(Key {
            code: KeyCode::Char('c'),
            modifiers: KeyMods::ctrl(),
        });
        let (state, cmds) = update(state, msg);
        assert!(!state.should_exit);
        assert!(state.ui.input_buffer.is_empty());
        assert!(cmds.is_empty());
    }

    #[test]
    fn ctrl_c_during_turn_transitions_to_cancelling() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5));
        let msg = Msg::Key(Key {
            code: KeyCode::Char('c'),
            modifiers: KeyMods::ctrl(),
        });
        let (state, cmds) = update(state, msg);
        assert!(matches!(state.turn, TurnState::Cancelling { id: TurnId(5), .. }));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::CancelScope(TurnId(5)))));
    }

    #[test]
    fn double_cancel_does_not_emit_twice() {
        let mut state = fresh_state();
        state.turn = TurnState::Cancelling {
            id: TurnId(1),
            since: std::time::SystemTime::now(),
        };
        let (_state, cmds) = update(state, Msg::CancelTurn);
        assert!(cmds.is_empty());
    }

    #[test]
    fn submit_prompt_on_idle_transitions_to_generating() {
        let state = fresh_state();
        let msg = Msg::SubmitPrompt {
            text: "hi there".to_string(),
            attachment_ids: vec![],
        };
        let (state, cmds) = update(state, msg);
        assert!(matches!(state.turn, TurnState::Generating { .. }));
        // CallModel + RefreshInstructions
        assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::RefreshInstructions)));
        // user message committed
        assert_eq!(state.session.messages().len(), 1);
        assert_eq!(state.session.messages()[0].content, "hi there");
    }

    #[test]
    fn submit_prompt_when_busy_is_dropped() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(1));
        let msg = Msg::SubmitPrompt {
            text: "ignored".to_string(),
            attachment_ids: vec![],
        };
        let (state, cmds) = update(state, msg);
        assert!(matches!(state.turn, TurnState::Generating { id: TurnId(1), .. }));
        assert!(cmds.is_empty());
        assert!(state.session.messages().is_empty());
    }

    #[test]
    fn submit_prompt_trims_empty_input() {
        let state = fresh_state();
        let msg = Msg::SubmitPrompt {
            text: "   \n\t".to_string(),
            attachment_ids: vec![],
        };
        let (state, cmds) = update(state, msg);
        assert!(matches!(state.turn, TurnState::Idle));
        assert!(cmds.is_empty());
    }

    #[test]
    fn stale_stream_text_dropped_silently() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5));
        let msg = Msg::StreamText {
            turn: TurnId(4), // stale!
            chunk: "should be dropped".to_string(),
        };
        let (state, _cmds) = update(state, msg);
        if let TurnState::Generating { partial_text, .. } = &state.turn {
            assert!(partial_text.is_empty());
        } else {
            panic!("expected Generating");
        }
    }

    #[test]
    fn current_turn_stream_text_accumulates() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5));
        let (state, _) = update(
            state,
            Msg::StreamText {
                turn: TurnId(5),
                chunk: "hello ".to_string(),
            },
        );
        let (state, _) = update(
            state,
            Msg::StreamText {
                turn: TurnId(5),
                chunk: "world".to_string(),
            },
        );
        if let TurnState::Generating {
            partial_text,
            phase,
            ..
        } = &state.turn
        {
            assert_eq!(partial_text, "hello world");
            assert_eq!(*phase, GenPhase::Streaming);
        } else {
            panic!("expected Generating");
        }
    }

    #[test]
    fn reasoning_chunk_transitions_phase_to_thinking() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5));
        let (state, _) = update(
            state,
            Msg::StreamReasoning {
                turn: TurnId(5),
                chunk: crate::models::ReasoningChunk {
                    text: "weighing...".to_string(),
                    signature: None,
                },
            },
        );
        if let TurnState::Generating {
            phase,
            partial_reasoning,
            ..
        } = &state.turn
        {
            assert_eq!(*phase, GenPhase::Thinking);
            assert_eq!(partial_reasoning, "weighing...");
        } else {
            panic!("expected Generating");
        }
    }

    #[test]
    fn stream_done_commits_assistant_message_and_returns_to_idle() {
        let mut state = fresh_state();
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: "final answer".to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            thinking_signature: None,
        };
        let (state, cmds) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                thinking_signature: None,
            },
        );
        assert!(matches!(state.turn, TurnState::Idle));
        assert_eq!(state.session.messages().len(), 1);
        assert_eq!(state.session.messages()[0].content, "final answer");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
    }

    #[test]
    fn upstream_error_ends_turn_and_records_line() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(1));
        let err = crate::models::UserFacingError {
            summary: "Server error".to_string(),
            message: "500 internal".to_string(),
            suggestion: "retry".to_string(),
            category: crate::models::ErrorCategory::Temporary,
            recoverable: true,
        };
        let (state, _) = update(
            state,
            Msg::UpstreamError {
                turn: TurnId(1),
                error: err,
            },
        );
        assert!(matches!(state.turn, TurnState::Idle));
        assert_eq!(state.session.messages().len(), 1);
        let m = &state.session.messages()[0];
        assert!(m.content.contains("Server error"));
        assert_eq!(m.actions.len(), 1);
    }

    #[test]
    fn slash_model_with_arg_persists_and_updates_session() {
        let state = fresh_state();
        let (state, cmds) = update(state, Msg::Slash(SlashCmd::Model(Some("anthropic/opus".to_string()))));
        assert_eq!(state.session.model_id, "anthropic/opus");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::PersistLastModel(_))));
    }

    #[test]
    fn slash_reasoning_persists_per_model() {
        let state = fresh_state();
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Reasoning(Some(
                crate::models::ReasoningLevel::High,
            ))),
        );
        assert_eq!(state.session.reasoning, crate::models::ReasoningLevel::High);
        let emitted = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::PersistReasoningFor { model_id, level } => {
                    Some((model_id.clone(), *level))
                },
                _ => None,
            })
            .expect("persist cmd emitted");
        assert_eq!(emitted.0, "ollama/test");
        assert_eq!(emitted.1, crate::models::ReasoningLevel::High);
    }

    #[test]
    fn slash_clear_raises_confirmation() {
        let state = fresh_state();
        let (state, _) = update(state, Msg::Slash(SlashCmd::Clear));
        assert!(state.confirm.is_some());
    }

    #[test]
    fn confirm_accepted_for_clear_wipes_messages() {
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("one"));
        state.session.append(ChatMessage::assistant("two"));
        state.confirm = Some(super::super::state::Confirmation {
            prompt: "Clear conversation history?".to_string(),
            accept_msg_token: super::super::state::ConfirmationTarget::ClearConversation,
        });
        let (state, _) = update(state, Msg::ConfirmAccepted);
        assert!(state.session.messages().is_empty());
        assert!(state.confirm.is_none());
    }

    #[test]
    fn confirm_declined_clears_without_action() {
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("kept"));
        state.confirm = Some(super::super::state::Confirmation {
            prompt: "Clear conversation history?".to_string(),
            accept_msg_token: super::super::state::ConfirmationTarget::ClearConversation,
        });
        let (state, _) = update(state, Msg::ConfirmDeclined);
        assert_eq!(state.session.messages().len(), 1);
        assert!(state.confirm.is_none());
    }

    #[test]
    fn mcp_server_ready_updates_entry_status() {
        let mut state = fresh_state();
        state.mcp = McpState::default();
        state.mcp.servers.insert(
            "s1".to_string(),
            McpServerEntry {
                config: crate::app::McpServerConfig {
                    command: "echo".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                },
                status: McpServerStatus::Starting,
                tools: vec![],
            },
        );
        let (state, _) = update(
            state,
            Msg::McpServerReady {
                name: "s1".to_string(),
                tools: vec![],
            },
        );
        assert_eq!(state.mcp.servers["s1"].status, McpServerStatus::Ready);
    }

    #[test]
    fn mcp_server_errored_sets_status_and_emits_status_line() {
        let mut state = fresh_state();
        state.mcp.servers.insert(
            "s1".to_string(),
            McpServerEntry {
                config: crate::app::McpServerConfig {
                    command: "echo".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                },
                status: McpServerStatus::Starting,
                tools: vec![],
            },
        );
        let (state, _) = update(
            state,
            Msg::McpServerErrored {
                name: "s1".to_string(),
                reason: "exit 1".to_string(),
            },
        );
        match &state.mcp.servers["s1"].status {
            McpServerStatus::Errored { reason } => assert_eq!(reason, "exit 1"),
            _ => panic!("expected Errored"),
        }
        assert!(state.status.is_some());
    }

    #[test]
    fn status_dismiss_clears_status_line() {
        let mut state = fresh_state();
        state.status = Some(StatusLine {
            text: "info".to_string(),
            kind: StatusKind::Info,
            shown_at: std::time::SystemTime::now(),
        });
        let (state, _) = update(state, Msg::StatusDismiss);
        assert!(state.status.is_none());
    }

    #[test]
    fn tool_finished_with_all_outcomes_triggers_follow_up_call_model() {
        let mut state = fresh_state();
        let call = PendingToolCall {
            call_id: super::super::ids::ToolCallId(1),
            source: crate::models::tool_call::ToolCall {
                id: Some("c1".to_string()),
                function: crate::models::tool_call::FunctionCall {
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "foo"}),
                },
            },
        };
        state.turn = start_executing_tools(TurnId(3), vec![call]);
        // The reducer looks up the "last assistant message" to attach
        // an ActionDisplay — plant one so the lookup doesn't silently
        // no-op in this test.
        state.session.append(ChatMessage::assistant("tools follow"));

        let (state, cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(3),
                call_id: super::super::ids::ToolCallId(1),
                outcome: ToolOutcome::Finished {
                    output: "file contents".to_string(),
                    images: None,
                    duration_secs: 0.05,
                },
            },
        );

        assert!(matches!(state.turn, TurnState::Generating { .. }));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
        // Tool result message was appended.
        let last = state.session.messages().last().unwrap();
        assert_eq!(last.role, MessageRole::Tool);
    }

    #[test]
    fn tool_finished_partial_stays_in_executing() {
        let mut state = fresh_state();
        let calls = vec![
            PendingToolCall {
                call_id: super::super::ids::ToolCallId(1),
                source: crate::models::tool_call::ToolCall {
                    id: Some("c1".to_string()),
                    function: crate::models::tool_call::FunctionCall {
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({}),
                    },
                },
            },
            PendingToolCall {
                call_id: super::super::ids::ToolCallId(2),
                source: crate::models::tool_call::ToolCall {
                    id: Some("c2".to_string()),
                    function: crate::models::tool_call::FunctionCall {
                        name: "write_file".to_string(),
                        arguments: serde_json::json!({}),
                    },
                },
            },
        ];
        state.turn = start_executing_tools(TurnId(3), calls);
        state.session.append(ChatMessage::assistant("tools follow"));

        let (state, cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(3),
                call_id: super::super::ids::ToolCallId(1),
                outcome: ToolOutcome::Cancelled,
            },
        );

        // Still in ExecutingTools (second tool pending).
        match &state.turn {
            TurnState::ExecutingTools { outcomes, .. } => {
                assert_eq!(outcomes.len(), 2);
                assert!(outcomes[0].is_some());
                assert!(outcomes[1].is_none());
            },
            _ => panic!("should still be ExecutingTools"),
        }
        assert!(cmds.is_empty());
    }

    #[test]
    fn stale_tool_finished_dropped_silently() {
        let mut state = fresh_state();
        state.turn = start_executing_tools(
            TurnId(3),
            vec![PendingToolCall {
                call_id: super::super::ids::ToolCallId(1),
                source: crate::models::tool_call::ToolCall {
                    id: None,
                    function: crate::models::tool_call::FunctionCall {
                        name: "x".to_string(),
                        arguments: serde_json::json!({}),
                    },
                },
            }],
        );

        let (state, cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(999),
                call_id: super::super::ids::ToolCallId(1),
                outcome: ToolOutcome::Cancelled,
            },
        );
        match &state.turn {
            TurnState::ExecutingTools { outcomes, .. } => {
                assert!(outcomes[0].is_none());
            },
            _ => panic!("unchanged state expected"),
        }
        assert!(cmds.is_empty());
    }

    #[test]
    fn tick_is_noop() {
        let before = fresh_state();
        let (after, cmds) = update(before.clone(), Msg::Tick);
        assert!(cmds.is_empty());
        assert!(matches!(after.turn, TurnState::Idle));
    }

    #[test]
    fn resize_is_noop() {
        let (state, cmds) = update(fresh_state(), Msg::Resize { width: 80, height: 24 });
        assert!(cmds.is_empty());
        assert!(matches!(state.turn, TurnState::Idle));
    }

    #[test]
    fn ui_state_default_is_empty() {
        let s = UiState::default();
        assert!(s.input_buffer.is_empty());
        assert_eq!(s.chat_scroll, 0);
        assert!(matches!(s.mode, UiMode::EditingInput));
    }
}
