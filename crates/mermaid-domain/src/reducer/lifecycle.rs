use crate::cmd::Cmd;
use crate::reducer::*;
use crate::state::{State, ToolOutcome, TurnState};
use crate::transition::{commit_assistant_message, tool_result_messages};
use mermaid_model::ids::TurnId;

/// When a turn is aborted while its tools are mid-flight, the `assistant`
/// message carrying the `tool_calls` is already committed to history but the
/// matching `tool` result messages never will be. Left as-is, the next request
/// sends `tool_use` blocks with no `tool_result` and providers (Anthropic in
/// particular) reject it with a 400. Commit a `cancelled` placeholder result
/// for every outstanding call so history stays well-formed — the same repair
/// compaction performs for orphaned tool calls (#71), applied to the live
/// cancel/quit paths. Leaves `state.turn` untouched for any non-`ExecutingTools`
/// state; the caller sets the real target state afterwards.
pub fn seal_orphaned_tool_calls(state: &mut State) {
    match std::mem::replace(&mut state.turn, TurnState::Idle) {
        TurnState::ExecutingTools {
            calls, outcomes, ..
        } => {
            let sealed: Vec<ToolOutcome> = outcomes
                .into_iter()
                .map(|o| o.unwrap_or_else(ToolOutcome::cancelled))
                .collect();
            for m in tool_result_messages(&calls, sealed) {
                state.session.append(m, state.now);
            }
            // turn is now `Idle`; caller decides the next state.
        },
        other => {
            // Not mid-tools — restore the state we took.
            state.turn = other;
        },
    }
}

/// Release both parked-tool-request queues together. `pending_approval` and
/// `pending_question` are drained only by the user answering — `ResolveApproval`
/// / `ResolveQuestion` — but a cancelled `TurnScope` tears down the parked tool
/// task, so those replies can never come. Left behind, a stale approval or
/// question modal survives `/load`, `/clear`, Ctrl+C, and quit with nothing to
/// answer it (D2/D3/D4). Every turn-cancel/reset site clears both through here.
pub fn clear_parked_tool_requests(state: &mut State) {
    state.pending_approval.clear();
    state.pending_question.clear();
}

pub fn handle_cancel_turn(state: &mut State, cmds: &mut Vec<Cmd>) {
    let Some(id) = state.turn.id() else {
        return;
    };
    // Already cancelling: don't double-cancel.
    if matches!(state.turn, TurnState::Cancelling { .. }) {
        return;
    }
    cmds.push(Cmd::CancelScope(id));
    // The cancelled turn's tool tasks are aborted; their parked approval and
    // question requests can never be answered, so drop both.
    clear_parked_tool_requests(state);
    // If tools were mid-flight, close the tool_use/tool_result pairing before
    // leaving `ExecutingTools`, or the next request would be malformed.
    seal_orphaned_tool_calls(state);
    state.turn = TurnState::Cancelling {
        id,
        since: std::time::SystemTime::from(state.now),
    };
}

pub fn request_exit(state: &mut State, cmds: &mut Vec<Cmd>) {
    if state.should_exit {
        return;
    }
    if let Some(id) = state.turn.id() {
        cmds.push(Cmd::CancelScope(id));
    }
    state.should_exit = true;
    state.ui.pending_msgs.clear();
    clear_parked_tool_requests(state);
    // Quitting mid-stream: preserve whatever the model already produced so
    // `--continue` shows what was on screen. Commit the in-flight partial
    // as an assistant message with an interrupted marker before saving —
    // keeping the continuation stamp so a reloaded transcript still stitches.
    let now = state.now;
    if let TurnState::Generating {
        partial_text,
        partial_reasoning,
        provider_continuation,
        continuation,
        ..
    } = &mut state.turn
        && !partial_text.trim().is_empty()
    {
        let text = std::mem::take(partial_text);
        let reasoning = std::mem::take(partial_reasoning);
        let sig = provider_continuation.take();
        let continuation = *continuation;
        let msg = commit_assistant_message(
            format!("{text}\n\n_[interrupted]_"),
            reasoning,
            Vec::new(),
            sig,
            now,
            continuation,
        );
        state.session.append(msg, state.now);
    }
    // Quitting mid-tool-execution: seal the orphaned `tool_calls` with cancelled
    // placeholders so the saved history a later `--continue` reloads isn't a
    // malformed `assistant(tool_calls)` with no results.
    seal_orphaned_tool_calls(state);
    // The run is over — any live recovery nudge is spent; don't persist it.
    sweep_spent_nudges(state);
    // Quitting mid-run still ends the run: the saved log records how long it
    // worked and what it spent, after the interrupted partial above so the
    // summary reads in order.
    finish_run(state, cmds, RunEnd::Interrupted);
    cmds.push(state.session.save_conversation_cmd());
    cmds.push(Cmd::Exit);
}

/// Handle `Msg::TurnCancelled(turn)`. The effect runner's `drop_scope`
/// emits this after the cancelled turn's `TurnScope` drains. Transitions
/// `Cancelling(id) → Idle` when the ids match; also closes out the
/// degenerate case where the scope drained but state is already `Idle`
/// (e.g. the stream-done raced). Drains one queued message on the way
/// out, same as the no-tool-calls tail of `handle_stream_done`.
///
/// Stale filter at the top of `update_step` catches mismatched turn ids
/// before we get here, so this handler is branch-light.
pub fn handle_turn_cancelled(state: &mut State, cmds: &mut Vec<Cmd>, turn: TurnId) {
    match state.turn {
        TurnState::Cancelling { id, .. } if id == turn => {
            state.turn = TurnState::Idle;
            state.ui.live_tool_status.clear();
            // The cancel ends the run: record how long it worked and what it
            // spent before this point.
            finish_run(state, cmds, RunEnd::Interrupted);
            // The cancelled turn abandoned whatever recovery its nudge was
            // steering — retire it so the hidden instruction can't leak into
            // the user's next, unrelated request.
            if sweep_spent_nudges(state) {
                cmds.push(state.session.save_conversation_cmd());
            }
            drain_next_queued_message(state);
        },
        _ => {
            // Stream already completed / already idle / stale id —
            // silently no-op. The filter at update_step's top would
            // have caught a truly stale id; this branch handles the
            // benign race where the scope drained after a successful
            // StreamDone committed normally.
        },
    }
}
