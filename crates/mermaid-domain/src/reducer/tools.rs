use crate::action_display::action_display_for;
use crate::cmd::Cmd;
use crate::reducer::*;
use crate::request::*;
use crate::state::{State, ToolOutcome, TurnState};
use crate::transition::{
    fill_outcome, start_generating, tool_result_messages, try_complete_outcomes,
};
use crate::{ProgressEvent, SubagentPhase};
use mermaid_model::ids::TurnId;

/// Route a typed `ProgressEvent`.
///
/// Tool stdout / status / byte-progress and subagent chatter are intentionally
/// dropped: surfacing each line to the status banner flickered a fresh line
/// above the input every few milliseconds (build output, pids, streamed file
/// contents) which read as noise. The status *line* already names the in-flight
/// tool, and a tool's full output lands in the chat transcript when it
/// finishes. Only image artifacts are handled here — they attach to the
/// in-flight assistant message for inline display.
pub fn handle_tool_progress(
    state: &mut State,
    _cmds: &mut Vec<Cmd>,
    turn: TurnId,
    call_id: mermaid_model::ids::ToolCallId,
    event: crate::ProgressEvent,
) {
    use base64::{Engine as _, engine::general_purpose};

    match event {
        ProgressEvent::Artifact { mime, data, .. }
            if mime.starts_with("image/")
                && matches!(
                    state.turn,
                    TurnState::ExecutingTools { .. } | TurnState::Generating { .. }
                ) =>
        {
            let encoded = general_purpose::STANDARD.encode(&data);
            state.session.attach_image(encoded);
        },
        // Live subagent activity → the per-call status the agent panel and
        // status line show next to the tool label. Only while the owning turn
        // is executing; a stale turn's progress must not repopulate a cleared
        // map.
        ProgressEvent::SubagentToolCall {
            tool_name, phase, ..
        } if matches!(&state.turn, TurnState::ExecutingTools { id, .. } if *id == turn) => {
            let detail = match phase {
                SubagentPhase::Started => format!("{tool_name}…"),
                SubagentPhase::Finished => format!("{tool_name} done"),
                SubagentPhase::Errored => format!("{tool_name} failed"),
            };
            state
                .ui
                .live_tool_status
                .entry(call_id)
                .or_default()
                .activity = detail;
        },
        ProgressEvent::SubagentActivity(label) if matches!(&state.turn, TurnState::ExecutingTools { id, .. } if *id == turn) =>
        {
            let trimmed = label.trim();
            if !trimmed.is_empty() {
                state
                    .ui
                    .live_tool_status
                    .entry(call_id)
                    .or_default()
                    .activity = trimmed.to_string();
            }
        },
        ProgressEvent::SubagentTokens(tokens) if matches!(&state.turn, TurnState::ExecutingTools { id, .. } if *id == turn) =>
        {
            state.ui.live_tool_status.entry(call_id).or_default().tokens = tokens;
        },
        _ => {},
    }
}

pub fn handle_tool_finished(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    turn: TurnId,
    call_id: mermaid_model::ids::ToolCallId,
    outcome: ToolOutcome,
) {
    // Borrow calls + outcomes simultaneously via a helper to avoid
    // double mutable borrow on `state.turn`.
    let completed = match &mut state.turn {
        TurnState::ExecutingTools {
            id,
            calls,
            outcomes,
            ..
        } if *id == turn => {
            if !fill_outcome(calls, outcomes, call_id, outcome.clone()) {
                return;
            }
            state.ui.live_tool_status.remove(&call_id);
            // Fold tool-consumed provider usage (a subagent's child-session
            // total) into the session counters, so the footer and the
            // end-of-run "used N tokens" summary count the whole tree.
            if let Some(usage) = outcome.metadata.token_usage.as_ref() {
                let session_model = state.session.model_id.clone();
                let attribution = match &outcome.metadata.detail {
                    _ if !outcome.metadata.usage_by_model.is_empty() => {
                        UsageAttribution::Split(&outcome.metadata.usage_by_model)
                    },
                    crate::ToolMetadata::Subagent { model_id, .. } => {
                        UsageAttribution::Model(model_id)
                    },
                    _ => UsageAttribution::Model(&session_model),
                };
                fold_token_usage(
                    &mut state.session,
                    &mut state.runtime,
                    usage,
                    UsageFold::Subagent,
                    attribution,
                );
            }
            // The model asked to checkpoint: the follow-up model call this
            // turn ends in compacts first (see `push_call_model`).
            if outcome.is_success()
                && let crate::ToolMetadata::CompactionRequest { focus } = &outcome.metadata.detail
            {
                state.runtime.requested_compaction = Some(crate::RequestedCompaction {
                    focus: focus.clone(),
                });
            }
            // Fold this mutation's exact line counts into the run totals for
            // the end-of-run `+N/-M` summary (zero for non-mutating tools).
            state
                .runtime
                .run_line_changes
                .add(outcome.metadata.lines_added, outcome.metadata.lines_removed);
            // Attach action display to the last assistant message so
            // the renderer can show it.
            if let Some(call) = calls.iter().find(|c| c.call_id == call_id) {
                // A finished shell command may have scribbled on the terminal
                // (a child that opened /dev/tty writes straight past ratatui's
                // back buffer). Request a full repaint. Exec only: read/edit/
                // search tools can't touch the tty, and clearing on every tool
                // would flash during rapid tool loops.
                if call.source.function.name == "execute_command" {
                    state.ui.full_redraw_seq = state.ui.full_redraw_seq.wrapping_add(1);
                }
                let action = action_display_for(call, &outcome);
                if let Some(process) = action
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.process.clone())
                {
                    cmds.push(Cmd::SaveProcess(process.clone()));
                    state.runtime.register_process(process);
                }
                state.session.attach_action(action);
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
        // The executing turn is over; no call in it is live anymore.
        state.ui.live_tool_status.clear();
        // Append each tool message to the conversation, then kick off
        // the follow-up model call.
        let tool_msgs = tool_result_messages(&calls, completed_outcomes);
        for m in tool_msgs {
            state.session.append(m, state.now);
        }
        // Mid-run steering: deliver EVERY queued message at this tool
        // boundary, FIFO, as committed user messages — the follow-up model
        // call sees them mid-run instead of after the run ends. Wire order
        // (assistant tool_use → user tool_results → user steering text) is
        // legal for every adapter; `normalize_history` runs on the request
        // clone as always. Draining here empties the queue, so the turn-end
        // one-at-a-time drain can never double-submit. A message queued
        // mid-STREAM (no tool boundary before the run ends) still arrives
        // via that turn-end path. Run counters are untouched: steering
        // continues the same run.
        let steered = !state.ui.queued_messages.is_empty();
        while let Some(queued) = state.ui.queued_messages.pop_front() {
            commit_user_message(state, queued.text, &queued.attachment_ids);
        }
        if steered {
            // Steered text is user-authored; persist it now rather than
            // relying on the next StreamDone's save (a crash between this
            // CallModel and its StreamDone would otherwise lose it).
            cmds.push(state.session.save_conversation_cmd());
        }
        let next_turn = state.ids.fresh_turn();
        state.turn = start_generating(next_turn, std::time::SystemTime::from(state.now));
        push_call_model(state, cmds, next_turn);
    }
}

/// Construct the request the model sees for this turn, pulling in the
/// current message log + the active `MERMAID.md` suffix + the
/// reasoning choice + the tools surface.
/// Byte cap on buffered hook context; excess strings are dropped with the
/// count noted in the log (never sent to the model unbounded).
pub const MAX_HOOK_CONTEXT_BYTES: usize = 16 * 1024;

/// Buffer `additionalContext` strings from `before_tool_use` hooks for the
/// next dispatched model request. Turn-gated (the stale filter already drops
/// mismatched turns; re-check here for defense in depth, like
/// `handle_upstream_error`).
pub fn handle_hook_context(state: &mut State, turn: TurnId, texts: Vec<String>) {
    if state.turn.id() != Some(turn) {
        return;
    }
    for text in texts {
        let used: usize = state.pending_hook_context.iter().map(String::len).sum();
        if used + text.len() > MAX_HOOK_CONTEXT_BYTES {
            tracing::warn!("dropping hook context over the {MAX_HOOK_CONTEXT_BYTES}-byte cap");
            break;
        }
        state.pending_hook_context.push(text);
    }
}

/// Dispatch a model call: build the request (which folds in any pending hook
/// context), then CLEAR the hook-context buffer — it is consumed exactly once,
/// by the next real dispatch. Display-only builders (`/context` estimates) and
/// the compaction request call `build_chat_request` directly and do not clear.
/// Dispatch-time context-delta injector: diff the mode-defining facts against
/// what the model was last told (`AdvertisedContext`, persisted on the
/// conversation), inject ONE persistent `ContextMarker` describing every
/// change, and re-stamp the snapshot. The single un-bypassable announcement
/// path for safety flips and model swaps — the transitions themselves stay
/// message-log-free, and rapid flips between dispatches (read-only on, off)
/// collapse to no marker at all.
///
/// A `None` snapshot (fresh conversation, `/clear`, fresh handoff, or a save
/// from before the field existed) establishes the baseline silently: the
/// system prompt already states current modes; only CHANGES need a timeline
/// event. Subagents re-stamp silently too — their modes are fixed by the
/// parent.
pub fn advertise_context_changes(state: &mut State, cmds: &mut Vec<Cmd>) {
    let live = crate::state::AdvertisedContext::observe(&state.session);
    let prev = match state
        .session
        .conversation
        .advertised_context
        .replace(live.clone())
    {
        Some(prev) => prev,
        None => return,
    };
    if state.session.is_subagent || prev == live {
        return;
    }
    let text = context_delta_text(&prev, &live);
    push_system_kind(
        state,
        cmds,
        text,
        mermaid_model::models::ChatMessageKind::ContextMarker,
    );
}

/// Compose the single coalesced marker for every delta between two advertised
/// contexts.
#[must_use]
pub fn context_delta_text(
    prev: &crate::state::AdvertisedContext,
    live: &crate::state::AdvertisedContext,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if prev.safety_mode != live.safety_mode {
        parts.push(format!(
            "Safety mode changed from {} to {} (set by the user).",
            prev.safety_mode.as_str(),
            live.safety_mode.as_str()
        ));
    }
    if prev.model_id != live.model_id {
        parts.push(format!("The active model is now {}.", live.model_id));
    }
    parts.join(" ")
}

pub fn push_call_model(state: &mut State, cmds: &mut Vec<Cmd>, turn: TurnId) {
    // Mode changes become history events BEFORE anything else rides this
    // request.
    advertise_context_changes(state, cmds);
    // Checklist staleness nudge: count model-call cycles while a task sits
    // in_progress with no checklist update (`handle_tasks_updated` resets the
    // counter), and at the threshold inject a reminder into THIS request and
    // re-arm. It is coaching, so it rides on the guidance pack's switch.
    let coached = state
        .settings
        .guidance_pack_enabled(&state.session.model_id);
    match state.session.conversation.tasks.active() {
        Some(active) if coached => {
            state.runtime.calls_since_task_update += 1;
            if state.runtime.calls_since_task_update >= TASK_STALENESS_CALLS {
                state.runtime.calls_since_task_update = 0;
                let notice = format!(
                    "Task #{} '{}' has been in_progress for {} model calls without a \
                     checklist update. Update, split, or complete it (task_update) so \
                     the checklist reflects reality.",
                    active.id, active.subject, TASK_STALENESS_CALLS
                );
                push_task_notice(state, notice);
            }
        },
        // No active task, or no coaching: hold the counter at zero.
        _ => state.runtime.calls_since_task_update = 0,
    }
    let request = build_chat_request(state);
    state.pending_hook_context.clear();
    state.pending_task_notices.clear();
    // A requested checkpoint rides exactly one dispatch, like hook context.
    state.runtime.requested_compaction = None;
    cmds.push(Cmd::CallModel { turn, request });
}

/// Content prefix of a [`safety_loosened_note`] — how
/// [`note_safety_mode_change`] recognizes its own pending nudge to retract it.
pub const SAFETY_NUDGE_PREFIX: &str = "Safety mode is now ";

/// One-line note injected for the model when the user leaves `read_only` while
/// stale read-only denials are in history, so it re-attempts gated actions
/// instead of trusting the old blocks. Stamped `RecoveryNudge`: hidden from the
/// transcript (the status bar already shows the mode) and swept once the
/// request it steers has gone out. Pairs with
/// `neutralize_superseded_policy_denials`, which rewrites the denials
/// themselves on every request.
#[must_use]
pub fn safety_loosened_note(mode: mermaid_model::safety::SafetyMode) -> String {
    format!(
        "{SAFETY_NUDGE_PREFIX}{}; earlier read-only policy blocks no longer apply. \
         Re-attempt gated actions instead of assuming they'll fail.",
        mode.as_str()
    )
}

/// Model-facing side of a safety-mode switch. Keeps AT MOST ONE pending
/// loosened-mode nudge, always naming the current mode:
///
/// - retracts any still-pending nudge first — it names a stale mode and, on a
///   tighten back to `read_only`, would contradict the standing denials;
/// - (re-)injects one only while a leave-read_only event is pending: either
///   this switch leaves `read_only`, or a pending nudge proves an unsent earlier
///   leave (the user is still cycling, e.g. `read_only` → ask → auto).
///
/// A loosening long after `read_only` (no pending nudge) stays silent — the
/// per-request denial rewrite already covers it, and re-announcing on every
/// loosening step was the old bug.
pub fn note_safety_mode_change(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    previous: mermaid_model::safety::SafetyMode,
    next: mermaid_model::safety::SafetyMode,
) {
    use mermaid_model::models::ChatMessageKind;
    use mermaid_model::safety::SafetyMode;
    let messages = state.session.conversation.messages_mut();
    let before = messages.len();
    messages.retain(|m| {
        m.kind != ChatMessageKind::RecoveryNudge || !m.content.starts_with(SAFETY_NUDGE_PREFIX)
    });
    let leave_pending = messages.len() < before;
    if (previous == SafetyMode::ReadOnly || leave_pending)
        && next != SafetyMode::ReadOnly
        && history_has_readonly_denial(state.session.messages())
    {
        push_system_kind(
            state,
            cmds,
            safety_loosened_note(next),
            ChatMessageKind::RecoveryNudge,
        );
    }
    // No save here for the retract-only path: both callers persist the mode
    // switch right after this returns.
}
