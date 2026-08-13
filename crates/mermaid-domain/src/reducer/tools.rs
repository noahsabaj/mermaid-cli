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
use mermaid_model::models::ChatMessage;

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
                fold_token_usage(
                    &mut state.session,
                    &mut state.runtime,
                    usage,
                    UsageFold::Subagent,
                );
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
                note_plan_tool_outcome(
                    &mut state.runtime,
                    state.session.plan.is_some(),
                    &call.source.function.name,
                    &outcome,
                );
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

    // Plan-tool transitions happen at this boundary — after the outcome slots
    // are filled, before the follow-up model call is built — so the next
    // request's system prompt, tool list, and dispatch flooring all see the
    // new plan state, and an approval's queued kickoff rides the drain below.
    if plan_tool_transition(state, cmds, call_id, &outcome) {
        // A handoff replaced the conversation. The executing turn belongs to
        // the exploration transcript (already saved); cancel its scope and go
        // Idle — the handoff's queued kickoff drives the next turn in the new
        // conversation. Appending this turn's tool results would strand them
        // in the wrong transcript.
        if let Some(id) = state.turn.id() {
            cmds.push(Cmd::CancelScope(id));
        }
        state.turn = TurnState::Idle;
        state.ui.live_tool_status.clear();
        return;
    }

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
/// Prefix of the plan-mode tail reminder — how `push_plan_reminder` retracts
/// the previous instance before re-appending at the tail (and how plan-exit
/// paths retract a stale one).
pub const PLAN_REMINDER_PREFIX: &str = "Reminder: plan mode is active";

/// Model calls after an arming plan denial before the tail reminder escalates
/// to the corrective variant. Lower than `TASK_STALENESS_CALLS`: the observed
/// doom loop burned 7+ minutes of pure denials, and the escalation is cheap
/// (hidden, swept at turn-end).
pub const PLAN_THRASH_CALLS: u32 = 3;

/// Tools whose denial means the model tried to CHANGE something. Only these
/// arm the doom-loop breaker.
///
/// The breaker's whole premise is "mutation attempts that never produce a
/// plan write". Arming on any denial carrying `PLAN_DENIAL_MARKER` also
/// caught the plan profile's capability denials — `[plan] web = deny` or
/// `memory = deny` — so a purely read-only Ground phase could trip the
/// "STOP attempting other mutations" corrective and cut research short.
pub const PLAN_MUTATING_TOOLS: &[&str] =
    &["write_file", "edit_file", "apply_patch", "execute_command"];

/// Plan doom-loop bookkeeping at the tool boundary: the FIRST denied MUTATION
/// arms the breaker (a read-heavy Ground phase alone must never trip it — the
/// doom-loop signature is mutation denials without a subsequent plan write);
/// a call that actually WROTE THE PLAN disarms it.
///
/// Both conditions are facts recorded upstream, not proxies. The disarm reads
/// `ToolRunMetadata::plan_file_written`, stamped by the gate that approved the
/// write, so it covers all three authoring spellings — including the shell
/// redirect the escalated corrective itself recommends, which the old
/// tool-name check missed, leaving the breaker armed forever and re-injecting
/// "the plan file does not exist" at a model that had just written it.
pub fn note_plan_tool_outcome(
    runtime: &mut crate::RuntimeState,
    planning: bool,
    tool: &str,
    outcome: &ToolOutcome,
) {
    if !planning {
        return;
    }
    if outcome.status == crate::ToolStatus::Error
        && PLAN_MUTATING_TOOLS.contains(&tool)
        && outcome.model_content.contains(&plan_denial_signature())
    {
        runtime.plan_thrash_armed = true;
    }
    if outcome.metadata.plan_file_written && outcome.status == crate::ToolStatus::Success {
        runtime.plan_thrash_armed = false;
        runtime.plan_calls_since_denial = 0;
    }
}

/// Dispatch-time context-delta injector: diff the mode-defining facts against
/// what the model was last told (`AdvertisedContext`, persisted on the
/// conversation), inject ONE persistent `ContextMarker` describing every
/// change, and re-stamp the snapshot. The single un-bypassable announcement
/// path for plan entry/exit, safety flips, and model swaps — the transitions
/// themselves stay message-log-free, and rapid flips between dispatches
/// (plan on, plan off) collapse to no marker at all.
///
/// A `None` snapshot (fresh conversation, `/clear`, fresh handoff, or a save
/// from before the field existed) establishes the baseline silently: the
/// system prompt already states current modes; only CHANGES need a timeline
/// event. Subagents re-stamp silently too — children don't plan and their
/// modes are fixed by the parent.
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
    let text = context_delta_text(&prev, &live, state.session.messages());
    push_system_kind(
        state,
        cmds,
        text,
        mermaid_model::models::ChatMessageKind::ContextMarker,
    );
}

/// Compose the single coalesced marker for every delta between two advertised
/// contexts. Plan entry with a `[plan]` model override yields ONE message
/// covering both; plan exit already names the live safety mode, so a safety
/// sentence is added only when the mode changed without a plan flip.
pub fn context_delta_text(
    prev: &crate::state::AdvertisedContext,
    live: &crate::state::AdvertisedContext,
    messages: &[ChatMessage],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    match (&prev.plan_path, &live.plan_path) {
        (None, Some(path)) => parts.push(format!(
            "Plan mode is now ON. A read-only policy floor is in effect: do not implement, \
             edit files, or run mutating commands. Author the plan at {} using write_file or \
             apply_patch — the only writable path. Task checklist tools are disabled (the \
             checklist is seeded from the approved plan's Tasks section). Call exit_plan_mode \
             when the plan is decision-complete.",
            path.display()
        )),
        (Some(_), None) => parts.push(format!(
            "Plan mode is now OFF; safety mode is {}.",
            live.safety_mode.as_str()
        )),
        // Path change without an exit is not currently reachable; treat it
        // as a re-entry for totality.
        (Some(a), Some(b)) if a != b => parts.push(format!(
            "The plan file moved: author the plan at {} now.",
            b.display()
        )),
        _ => {},
    }
    // A real mode switch the model must know about. Plan entry and exit each
    // already say what the mode is, so a redundant second sentence is
    // suppressed for those transitions.
    //
    // This is also where the contradiction used to be born: Shift+Tab while
    // planning changed `safety_mode` out from under the still-active plan
    // floor, and this emitted a permanent, never-swept "Safety mode changed to
    // full_access" that the model read as permission to mutate. It cannot
    // happen now — while planning the live mode IS `Plan`, and Shift+Tab
    // re-targets the staged resume mode instead of this value.
    if prev.safety_mode != live.safety_mode
        && !prev.safety_mode.is_planning()
        && !live.safety_mode.is_planning()
    {
        parts.push(format!(
            "Safety mode changed from {} to {} (set by the user).",
            prev.safety_mode.as_str(),
            live.safety_mode.as_str()
        ));
    }
    if prev.model_id != live.model_id {
        parts.push(format!("The active model is now {}.", live.model_id));
    }
    // Leaving plan mode past standing plan denials: fold the re-attempt
    // steering into the marker so the model does not trust stale blocks.
    // (`neutralize_superseded_plan_denials` also rewrites the denials
    // themselves per-request; this sentence covers the model's own memory
    // of them within the live context.)
    if prev.plan_path.is_some() && live.plan_path.is_none() && history_has_plan_denial(messages) {
        parts.push(
            "Earlier plan-mode policy blocks no longer apply — re-attempt gated actions \
             instead of assuming they'll fail."
                .to_string(),
        );
    }
    parts.join(" ")
}

/// Per-dispatch plan reminder: while a plan is being drafted, keep a compact
/// steering note at the HISTORY TAIL — the one position weak models reliably
/// read (the observed failure mode was ignoring the same rules at the system
/// tail behind ~70k tokens of history). Retract-then-reappend keeps exactly
/// one instance, always last; `RecoveryNudge` kind means it is hidden from
/// the transcript and swept at every turn-end for free. Byte-stable per plan
/// session (the path is fixed at entry), so prompt-cache churn stays confined
/// to the already-churning tail region.
pub fn push_plan_reminder(state: &mut State, cmds: &mut Vec<Cmd>) {
    let Some(plan) = &state.session.plan else {
        return;
    };
    if state.session.is_subagent {
        return;
    }
    let plan_path = plan.plan_path.display().to_string();
    state.session.conversation.messages_mut().retain(|m| {
        m.kind != mermaid_model::models::ChatMessageKind::RecoveryNudge
            || !m.content.starts_with(PLAN_REMINDER_PREFIX)
    });
    // Doom-loop escalation: once a plan denial armed the breaker, count model
    // calls; at the threshold, swap this dispatch's reminder for the
    // corrective and re-arm (the task-staleness pattern). A successful plan
    // write disarms via `note_plan_tool_outcome`.
    let escalate = state.runtime.plan_thrash_armed && {
        state.runtime.plan_calls_since_denial += 1;
        if state.runtime.plan_calls_since_denial >= PLAN_THRASH_CALLS {
            state.runtime.plan_calls_since_denial = 0;
            true
        } else {
            false
        }
    };
    let text = if escalate {
        format!(
            "{PLAN_REMINDER_PREFIX} and you keep hitting plan-mode policy blocks without \
             writing the plan. STOP attempting other mutations — they will all be denied. \
             Write your current plan to {plan_path} NOW by calling the write_file tool \
             (apply_patch also works; a shell redirect writing ONLY that file works too). \
             The plan file does not exist until you write it, and exit_plan_mode fails \
             until it does."
        )
    } else {
        format!(
            "{PLAN_REMINDER_PREFIX} — read-only floor; do not implement. Author or update the \
             plan at {plan_path} with write_file or apply_patch (the only writable path). Call \
             exit_plan_mode when the plan is decision-complete."
        )
    };
    push_system_kind(
        state,
        cmds,
        text,
        mermaid_model::models::ChatMessageKind::RecoveryNudge,
    );
}

/// Are the checklist WRITERS (`task_create`/`task_update`) withdrawn right now?
///
/// One predicate, two consumers: the advertised tool set and the task-staleness
/// nudge. They disagreed — the nudge kept telling the model to "update it
/// (`task_update`)" for a tool that was neither advertised nor permitted, and
/// since only a successful update resets the counter, the contradiction
/// re-injected itself every `TASK_STALENESS_CALLS` dispatches for the whole
/// planning session.
pub fn checklist_writers_suppressed(state: &State) -> bool {
    state.session.safety_mode.is_planning()
        && state.settings.plan.permissions.tasks != crate::PlanPermLevel::Allow
}

pub fn push_call_model(state: &mut State, cmds: &mut Vec<Cmd>, turn: TurnId) {
    // Mode changes become history events BEFORE anything else rides this
    // request — the marker must precede the tail reminder.
    advertise_context_changes(state, cmds);
    // Structural plan-rot guard: count model-call cycles while a task sits
    // in_progress with no checklist update (`handle_tasks_updated` resets the
    // counter). At the threshold, inject a targeted nudge into THIS request
    // and re-arm — prompt discipline alone demonstrably decays mid-run.
    // ...unless the writers are withdrawn, in which case the nudge would name
    // a tool the model cannot call and the counter could never be reset.
    match state.session.conversation.tasks.active() {
        Some(active) if !checklist_writers_suppressed(state) => {
            state.runtime.calls_since_task_update += 1;
            if state.runtime.calls_since_task_update >= TASK_STALENESS_CALLS {
                state.runtime.calls_since_task_update = 0;
                let notice = format!(
                    "Task #{} '{}' has been in_progress for {} model calls without a                      checklist update. Update, split, or complete it (task_update) so                      the checklist reflects reality.",
                    active.id, active.subject, TASK_STALENESS_CALLS
                );
                push_task_notice(state, notice);
            }
        },
        // No active task, or the writers are withdrawn: hold the counter at
        // zero so planning never leaves a primed nudge for the run after it.
        _ => state.runtime.calls_since_task_update = 0,
    }
    // The plan tail reminder is appended LAST so it is the most recent thing
    // the model reads.
    push_plan_reminder(state, cmds);
    let request = build_chat_request(state);
    state.pending_hook_context.clear();
    state.pending_task_notices.clear();
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

// ---------- Plan mode ----------
