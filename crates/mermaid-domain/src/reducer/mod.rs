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
//!   4. **Cancellation is explicit.** The way to stop in-flight work is
//!      `Cmd::CancelScope(turn)`, which cancels the turn's token so every
//!      scoped task unwinds at its next `.await`. The sole `JoinHandle::abort`
//!      is in `providers::tool::exec`: the detachable command driver is a raw
//!      (non-scoped) task by design — Ctrl+B lets it outlive the turn — so on
//!      Esc-cancel it's aborted explicitly after its process tree is killed.
//!
//! Internal split:
//!
//!   - `update_step(State, Msg) -> (State, Vec<Cmd>)` — a single
//!     reducer call. Pure, deterministic, exhaustive match.
//!   - `update(State, Msg) -> (State, Vec<Cmd>)` — runs a step,
//!     then drains `state.ui.pending_msgs` in a bounded loop so
//!     handlers can enqueue follow-up events (Enter-on-slash,
//!     queued-message auto-submit) without self-invoking the
//!     reducer.

pub(crate) mod input;
pub(crate) mod lifecycle;
pub(crate) mod plan_flow;
pub(crate) mod slash;
pub(crate) mod streaming;
pub(crate) mod subagents;
pub(crate) mod tools;

#[cfg(test)]
mod tests;

pub use input::*;
pub use lifecycle::*;
pub use plan_flow::*;
pub use slash::*;
pub use streaming::*;
pub use subagents::*;
pub use tools::*;

use crate::cmd::Cmd;
use crate::compaction::format_compact_count;
use crate::msg::Msg;
use crate::state::{GenPhase, McpServerEntry, McpServerStatus, State, TurnState};
use mermaid_model::models::ProviderContinuation;

pub const MAX_PENDING_DRAIN: usize = 16;

/// Cap on `state.ui.queued_messages` — the user-typed prompts queued while a
/// turn is in flight. Holding Enter during a long turn would otherwise grow it
/// without bound; past the cap the oldest queued prompt is dropped.
pub const MAX_QUEUED_MESSAGES: usize = 32;

/// The public reducer entry point. Runs one `update_step` for the
/// incoming `msg`, then drains any follow-up `Msg`s the handler
/// pushed onto `state.ui.pending_msgs`. All emitted `Cmd`s coalesce
/// into the returned vector.
pub fn update(mut state: State, msg: Msg) -> (State, Vec<Cmd>) {
    let turn_active_before = !matches!(state.turn, TurnState::Idle);
    let (new_state, mut cmds) = update_step(state, msg);
    state = new_state;
    let mut depth = 0usize;
    while let Some(follow) = state.ui.pending_msgs.pop_front() {
        if depth >= MAX_PENDING_DRAIN {
            tracing::warn!(
                max = MAX_PENDING_DRAIN,
                remaining = state.ui.pending_msgs.len(),
                "reducer: pending_msgs drain cap hit — follow-ups dropped this tick"
            );
            state.ui.pending_msgs.clear();
            break;
        }
        let (s, c) = update_step(state, follow);
        state = s;
        cmds.extend(c);
        depth += 1;
    }
    // Keep the terminal title in sync with the run state, and ring the bell when
    // a run finishes while the terminal is unfocused. Only on an active/idle
    // flip, so Tick/resize stay free; the title emit is diffed (no-op if same).
    let turn_active_after = !matches!(state.turn, TurnState::Idle);
    if turn_active_before != turn_active_after {
        emit_title_if_changed(&mut state, &mut cmds);
        if !turn_active_after && state.ui.terminal_unfocused {
            cmds.push(Cmd::AlertUser);
        }
    }
    (state, cmds)
}

/// Single-step reducer: one `Msg` in, new `State` + `Cmd`s out.
/// Callers interested in re-entry (queued follow-up messages) go
/// through `update()`; this function returns after a single pass.
///
/// The two wildcard lints are denied here and nowhere else. AGENTS.md promises
/// "no wildcard `_ =>` arms that hide new `Msg`s", and until now nothing
/// enforced it — the top-level `match msg` merely happened to stay exhaustive.
/// With the deny, adding a `Msg` variant is a compile error until every arm has
/// considered it, which is the property the promise was about.
///
/// Both lints, because either alone leaves a hole: `wildcard_enum_match_arm`
/// fires when `_` covers two or more remaining variants, and
/// `match_wildcard_for_single_variants` when it covers exactly one. A `_` arm
/// added next to a single unhandled `Msg` would slip past the first lint
/// entirely.
///
/// Function-scoped, so the nested matches on `KeyCode` and friends elsewhere in
/// this file keep their legitimate `_ =>` arms — 96 of them workspace-wide, and
/// not what this rule is about.
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
#[deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
pub fn update_step(mut state: State, msg: Msg) -> (State, Vec<Cmd>) {
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
        Msg::ClipboardRead(read) => {
            handle_clipboard_read(&mut state, &mut cmds, read);
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
            request_exit(&mut state, &mut cmds);
        },
        Msg::RuntimeSignal(signal) => {
            state.runtime.record_signal(signal);
            request_exit(&mut state, &mut cmds);
        },

        // ── Streaming ───────────────────────────────────────────────
        Msg::StreamText { turn, chunk } => {
            if let TurnState::Generating {
                id,
                partial_text,
                partial_reasoning,
                phase,
                tokens,
                ..
            } = &mut state.turn
                && *id == turn
            {
                partial_text.push_str(&chunk);
                *phase = GenPhase::Streaming;
                // Rough live estimate of all generated tokens (answer + thinking);
                // the actual count comes in `Done`.
                *tokens = (partial_text.len() + partial_reasoning.len()) / 4;
            }
        },
        Msg::StreamReasoning { turn, chunk } => {
            if let TurnState::Generating {
                id,
                partial_text,
                partial_reasoning,
                phase,
                provider_continuation,
                tokens,
                ..
            } = &mut state.turn
                && *id == turn
            {
                partial_reasoning.push_str(&chunk.text);
                *phase = GenPhase::Thinking;
                if let Some(sig) = chunk.signature {
                    *provider_continuation =
                        Some(ProviderContinuation::Anthropic { signature: sig });
                }
                // Count thinking tokens too, so the live counter climbs during a
                // long reasoning phase instead of sitting at 0 until answer text.
                *tokens = (partial_text.len() + partial_reasoning.len()) / 4;
            }
        },
        Msg::StreamToolCall { turn, call } => {
            handle_stream_tool_call(&mut state, turn, call);
        },
        Msg::ContextUsageEstimated { turn, snapshot } => {
            if state.turn.accepts(turn) {
                state.session.context_usage = Some(snapshot);
            }
        },
        Msg::BuiltinToolSchemaTokens(tokens) => {
            // Model-level metadata (the schema cost dispatch appends to every
            // request), not turn-scoped — intentionally not stale-filtered.
            state.runtime.builtin_tool_schema_tokens = tokens;
        },
        Msg::ProviderContextResolved {
            model_id,
            model_max,
            effective,
            source,
            max_output,
        } => {
            // Drop a probe that landed after a `/model` switch (it describes the
            // previous model, not the one now active) — mirrors
            // OllamaPlacementResolved.
            if model_id == state.session.model_id {
                // Refresh the capability snapshot with the live values (the
                // vision-probe pattern): a discovered window turns `Context:
                // unknown` into a real number and re-enables proactive
                // auto-compaction for remote providers; a discovered output
                // ceiling feeds the truncation classifier and `model-info`.
                if let Some(window) = effective.or(model_max) {
                    state.runtime.provider_capabilities.max_context_tokens = Some(window);
                }
                if max_output.is_some() {
                    state.runtime.provider_capabilities.max_output_tokens = max_output;
                }
                state.runtime.ollama_context = Some(mermaid_model::tool_run::OllamaContextInfo {
                    model_max,
                    effective,
                    source,
                });
                // Proactive, once-per-session: if auto-fit capped the window far
                // below the model's max, explain what happened + how to get more.
                let big_gap = matches!(
                    (model_max, effective, source),
                    (Some(mm), Some(eff), Some(src)) if src.is_auto() && mm >= eff.saturating_mul(2)
                );
                if big_gap
                    && state
                        .runtime
                        .hinted_models
                        .insert(state.session.model_id.clone())
                    && let (Some(mm), Some(eff)) = (model_max, effective)
                {
                    let model_id = state.session.model_id.clone();
                    push_system(
                        &mut state,
                        &mut cmds,
                        format!(
                            "{model_id} supports up to {} tokens; Mermaid auto-fit the window to {} for your GPU. \
                             `/context max` uses the full window; `/context offload on` allows RAM (slower).",
                            format_compact_count(mm),
                            format_compact_count(eff)
                        ),
                    );
                }
            }
        },
        Msg::ProviderVisionResolved {
            model_id,
            supports_vision,
            warn,
        } => {
            // Drop a probe that landed after a `/model` switch (it describes the
            // previous model, not the one now active) — mirrors
            // ProviderContextResolved / OllamaPlacementResolved.
            if model_id == state.session.model_id {
                // Refresh the display/telemetry snapshot (Ollama's was a static
                // `false`), so `/doctor` stops under-reporting vision.
                if let Some(v) = supports_vision {
                    state.runtime.provider_capabilities.supports_vision = v;
                }
                // One-shot, and only when an image is actually in play (`warn`):
                // a model that can't see images silently ignores them, which
                // looks like a bug. `None` (unknown) / `Some(true)` never warn.
                if warn
                    && supports_vision == Some(false)
                    && state.runtime.vision_warned.insert(model_id.clone())
                {
                    push_system(
                        &mut state,
                        &mut cmds,
                        format!(
                            "Heads up: {model_id} reports no vision capability, so attached \
                             images are not seen by the model. Switch to a vision-capable \
                             model to send images."
                        ),
                    );
                }
            }
        },
        Msg::OllamaPlacementResolved {
            model_id,
            size_vram_bytes,
            total_bytes,
            suggested_num_ctx,
        } => {
            // Drop a probe that landed after a `/model` switch (it describes the
            // previous model, not the one now active).
            if model_id == state.session.model_id {
                let placement = mermaid_model::tool_run::OllamaPlacement {
                    size_vram_bytes,
                    total_bytes,
                };
                state.runtime.ollama_placement = Some(placement);

                if placement.offloaded() && !state.settings.ollama.allow_ram_offload {
                    // Don't auto-resize a window the user explicitly pinned.
                    let user_pinned = state
                        .settings
                        .ollama_num_ctx_per_model
                        .contains_key(&model_id);
                    let already = state
                        .runtime
                        .ollama_converged_num_ctx
                        .get(&model_id)
                        .copied();
                    // Never shrink below what the conversation already needs: a
                    // window smaller than the prompt wedges the session (every
                    // turn truncates). If the fitting window can't hold the
                    // conversation, keep the larger one and warn instead.
                    let convo_tokens =
                        crate::compaction::estimate_messages_tokens(state.session.messages());
                    // Auto-converge: adopt a new, smaller window the probe says
                    // fits VRAM — only in auto mode, only if it changed, and only
                    // if it still holds the conversation. `None` ⇒ shrinking can't
                    // help (weights-bound), so the model just doesn't fit.
                    let converge_to = suggested_num_ctx
                        .filter(|_| !user_pinned)
                        .filter(|t| already != Some(*t))
                        .filter(|t| *t as usize >= convo_tokens);

                    if let Some(target) = converge_to {
                        state
                            .runtime
                            .ollama_converged_num_ctx
                            .insert(model_id, target);
                        let model = state.session.model_id.clone();
                        push_system(
                            &mut state,
                            &mut cmds,
                            format!(
                                "Reduced {model}'s context to {} so it fits your GPU.",
                                format_compact_count(target as usize)
                            ),
                        );
                    } else if state.runtime.offload_warned.insert(model_id) {
                        // Warn once. Tailor the advice: a user-pinned window can be
                        // lowered to help; an auto window that couldn't shrink to
                        // fit means the model is simply larger than the free VRAM,
                        // so shrinking won't help — point elsewhere.
                        let pct = placement.percent_on_cpu();
                        let model = state.session.model_id.clone();
                        let msg = if user_pinned {
                            format!(
                                "~{pct}% of {model} is running on CPU/RAM, which is much slower. \
                                 Lower the window with `/context <n>`, or accept RAM with `/context offload on`.",
                            )
                        } else {
                            format!(
                                "~{pct}% of {model} is running on CPU/RAM, which is much slower — \
                                 it's larger than your free VRAM. Try a smaller model, or accept RAM with `/context offload on`.",
                            )
                        };
                        push_system(&mut state, &mut cmds, msg);
                    }
                }
            }
        },
        Msg::CompactionFinished { turn, result } => {
            handle_compaction_finished(&mut state, &mut cmds, turn, result);
        },
        Msg::CompactionFailed {
            turn,
            trigger,
            message,
            kind,
        } => {
            handle_compaction_failed(&mut state, turn, trigger, message, kind);
        },
        Msg::StreamDone {
            turn,
            usage,
            provider_continuation,
            stop_reason,
        } => {
            handle_stream_done(
                &mut state,
                &mut cmds,
                turn,
                usage,
                provider_continuation,
                stop_reason,
            );
        },
        Msg::UpstreamError { turn, error } => {
            handle_upstream_error(&mut state, &mut cmds, turn, error);
        },
        Msg::TurnCancelled(turn) => {
            handle_turn_cancelled(&mut state, &mut cmds, turn);
        },

        // ── Tools ───────────────────────────────────────────────────
        Msg::ToolStarted {
            turn: _,
            call_id: _,
        } => {
            // Informational — render layer derives spinner state from
            // `outcomes[i].is_none()`, so no state change needed yet.
        },
        Msg::ToolProgress {
            turn,
            call_id,
            event,
        } => {
            handle_tool_progress(&mut state, &mut cmds, turn, call_id, event);
        },
        Msg::ToolFinished {
            turn,
            call_id,
            outcome,
        } => {
            handle_tool_finished(&mut state, &mut cmds, turn, call_id, outcome);
        },
        Msg::ApprovalRequested {
            turn,
            call_id,
            tool,
            risk,
            kind,
            prompt,
            allowlist_scope,
        } => {
            // Drop approval requests for a turn that's already being cancelled:
            // its tool task is unwinding, so surfacing (and parking on) a modal
            // would outlive the turn (#74). The stale-filter lets a same-id
            // `Cancelling` turn through, so guard the state explicitly here.
            if matches!(state.turn, TurnState::Cancelling { .. }) {
                return (state, cmds);
            }
            // Enqueue a modal; the parked tool task waits until the user
            // answers (key handler → Cmd::ResolveApproval). FIFO so multiple
            // gated tools in one turn are shown one at a time.
            state
                .pending_approval
                .push_back(crate::state::PendingApproval {
                    turn,
                    call_id,
                    tool,
                    risk,
                    kind,
                    prompt,
                    allowlist_scope,
                    selected_option: 0,
                });
            // A gated action is waiting on the user — ring the bell if they're
            // looking elsewhere.
            if state.ui.terminal_unfocused {
                cmds.push(Cmd::AlertUser);
            }
        },
        Msg::QuestionAsked {
            turn,
            call_id,
            questions,
        } => {
            // Same cancellation guard as approvals: drop a question for a turn
            // that's already unwinding (#74) so its modal can't outlive the turn.
            if matches!(state.turn, TurnState::Cancelling { .. }) {
                return (state, cmds);
            }
            state
                .pending_question
                .push_back(mermaid_model::question::PendingQuestionSet::new(
                    turn, call_id, questions,
                ));
        },
        Msg::TasksUpdated { store } => {
            // Deliberately NOT turn-gated: the broker (single writer) has
            // already committed this snapshot, and `/todos` edits arrive
            // outside any turn. Gating would only let the render copy drift.
            handle_tasks_updated(&mut state, &mut cmds, store);
        },
        Msg::TaskNotice { text } => {
            push_task_notice(&mut state, text);
        },

        // ── MCP ─────────────────────────────────────────────────────
        // F5: upsert semantics. State::new seeds entries for configured
        // servers in `Starting` status, so these handlers normally find
        // an existing entry to update. But a server discovered at
        // runtime (hypothetical future path) should still land in the
        // map — insert rather than silently drop.
        Msg::McpServerReady { name, tools } => {
            state
                .mcp
                .servers
                .entry(name)
                .and_modify(|e| {
                    e.status = McpServerStatus::Ready;
                    e.tools = tools.clone();
                })
                .or_insert_with(|| McpServerEntry {
                    config: crate::McpServerConfig::default(),
                    status: McpServerStatus::Ready,
                    tools,
                });
        },
        Msg::McpServerErrored { name, reason } => {
            let status = McpServerStatus::Errored {
                reason: reason.clone(),
            };
            state
                .mcp
                .servers
                .entry(name.clone())
                .and_modify(|e| e.status = status.clone())
                .or_insert_with(|| McpServerEntry {
                    config: crate::McpServerConfig::default(),
                    status,
                    tools: Vec::new(),
                });
            push_system(
                &mut state,
                &mut cmds,
                format!("MCP server {name} errored: {reason}"),
            );
        },
        Msg::McpServerStopped { name } => {
            if let Some(entry) = state.mcp.servers.get_mut(&name) {
                entry.status = McpServerStatus::Stopped;
            }
        },

        // ── Persistence / misc ─────────────────────────────────────
        Msg::HookContext { turn, texts } => {
            handle_hook_context(&mut state, turn, texts);
        },
        Msg::InstructionsChanged(loaded) => {
            state.instructions = loaded;
        },
        Msg::SessionProvenanceResolved(provenance) => {
            let conversation = &mut state.session.conversation;
            if conversation.git_branch.is_none() {
                conversation.git_branch = provenance.git_branch;
            }
            if conversation.git_sha.is_none() {
                conversation.git_sha = provenance.git_sha;
            }
            if conversation.cli_version.is_none() {
                conversation.cli_version = provenance.cli_version;
            }
        },
        Msg::MemoryChanged(loaded) => {
            state.memory = loaded;
        },
        Msg::SessionSaved => {
            // Silent. Reducer already committed; save is just durability.
        },
        Msg::ScratchpadReady { session_id, path } => {
            // Stamp only while the id still names the live conversation — a
            // `/clear` or `/load` racing the effect's mkdir leaves a ready
            // for a discarded id, which must not attach to the new session
            // (its own `EnsureScratchpad` is already in flight).
            if state.session.conversation.id == session_id {
                state.session.scratchpad = Some(path);
            }
        },
        Msg::QueryResult(result) => {
            handle_query_result(&mut state, &mut cmds, result);
        },
        Msg::RuntimeText(text) => {
            append_runtime_note(&mut state, &mut cmds, text);
        },
        Msg::ModelPullFinished { model } => {
            push_system(&mut state, &mut cmds, format!("Pulled {model}"));
        },
        Msg::ModelPullProgress(_line) => {
            // Pull progress used to stream into the status banner. With the
            // banner gone we don't surface line-by-line progress (it would spam
            // the transcript); the final ModelPullFinished posts one line.
        },

        // ── Housekeeping ────────────────────────────────────────────
        Msg::Tick => {
            // No state change here. The driver stamps `state.now` before every
            // tick (Cause 3), and render derives the elapsed-time spinner from
            // `state.now` — so a 60 Hz Tick advances the display without the
            // reducer or render ever reading the wall clock.
        },
        Msg::Resize { .. } => {
            // Render layer recomputes layout from the new area — no
            // reducer state depends on raw terminal dimensions.
        },
        Msg::MouseScroll { delta } => {
            // F13: accumulate into a counter. Render layer diffs
            // against its last-seen value and applies the resulting
            // delta to ChatState. `saturating_add` never overflows.
            state.ui.mouse_scroll_accum = state.ui.mouse_scroll_accum.saturating_add(delta as i32);
        },
        Msg::FocusChanged(focused) => {
            // Track focus so the attention bell only fires when the user is
            // looking elsewhere. UI-only; never affects the model.
            state.ui.terminal_unfocused = !focused;
        },
        Msg::TransientStatus { text } => {
            // Generic async feedback from effect handlers ("clipboard is empty",
            // "config saved", etc.). Routed into the chat transcript because it
            // is worth reading after the fact.
            push_system(&mut state, &mut cmds, text);
        },
        Msg::Toast { text } => {
            // Feedback on a keystroke the user just made. It expires on its own
            // against `state.now`; the 60 Hz tick redraws it away. Never a
            // transcript row — that is what left "Copied N chars to clipboard"
            // parked above the input for the rest of the session.
            state.ui.toast = Some((text, state.now + crate::state::TOAST_TTL));
        },
        Msg::EditorReturned { text } => {
            // $EDITOR compose round-trip (Ctrl+O / /editor). `Some` replaces
            // the whole draft — an empty string is a deliberate clear.
            // Editor failures arrive as `Msg::TransientStatus`, not `None`.
            if let Some(text) = text {
                state.ui.input_buffer = text;
                state.ui.input_cursor = state.ui.input_buffer.len();
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
            }
        },
        Msg::BackgroundAgentStarted {
            agent_id,
            description,
        } => {
            state
                .runtime
                .background_agents
                .push(crate::BackgroundAgent {
                    agent_id,
                    description,
                    started: std::time::SystemTime::from(state.now),
                    activity: "running…".to_string(),
                    tokens: 0,
                });
        },
        Msg::BackgroundAgentProgress {
            agent_id,
            activity,
            tokens,
        } => {
            if let Some(agent) = state
                .runtime
                .background_agents
                .iter_mut()
                .find(|a| a.agent_id == agent_id)
            {
                if !activity.is_empty() {
                    agent.activity = activity;
                }
                agent.tokens = tokens;
            }
        },
        Msg::BackgroundAgentFinished {
            agent_id,
            description,
            report,
            success,
            cancelled,
            usage,
            tokens,
            duration_secs,
        } => {
            state
                .runtime
                .background_agents
                .retain(|a| a.agent_id != agent_id);
            // Fold the detached child's spend into the session totals, same
            // as `handle_tool_finished` does for foreground agent calls.
            // Cancelled children fold too — that work was still billed.
            if let Some(usage) = usage.as_ref() {
                fold_token_usage(
                    &mut state.session,
                    &mut state.runtime,
                    usage,
                    UsageFold::Detached,
                );
            }
            if cancelled {
                // Killed on purpose (`/agents kill`, the agent tool's kill
                // action): note it and stop — a deliberately killed child's
                // partial report shouldn't spend a model turn.
                push_system(
                    &mut state,
                    &mut cmds,
                    format!(
                        "Background agent '{description}' ({agent_id}) cancelled — {} tokens, took {duration_secs}s.",
                        crate::compaction::format_compact_count(tokens),
                    ),
                );
                return (state, cmds);
            }
            let verdict = if success { "finished" } else { "failed" };
            push_system(
                &mut state,
                &mut cmds,
                format!(
                    "Background agent '{description}' ({agent_id}) {verdict} — {} tokens, took {duration_secs}s. Report queued.",
                    crate::compaction::format_compact_count(tokens),
                ),
            );
            // Deliver the report through the queued-message path: it submits
            // immediately if the parent is idle, or after the current turn
            // ends — exactly like a user-queued prompt.
            if state.ui.queued_messages.len() >= MAX_QUEUED_MESSAGES {
                state.ui.queued_messages.pop_front();
            }
            state
                .ui
                .queued_messages
                .push_back(crate::state::QueuedMessage {
                    text: format!(
                        "[background agent '{description}' ({agent_id}) {verdict}]\n{report}"
                    ),
                    attachment_ids: Vec::new(),
                });
            if matches!(state.turn, TurnState::Idle) {
                drain_next_queued_message(&mut state);
            }
        },
        Msg::OpenImageAt {
            message_index,
            image_index,
            image_number,
        } => {
            handle_open_image_at(
                &mut state,
                &mut cmds,
                message_index,
                image_index,
                image_number,
            );
        },
        Msg::CopySelection(text) => {
            // The selection itself lives in the render layer; the main loop
            // resolves it to text and hands it here so the clipboard write is an
            // `update()`-emitted Cmd (recorded for replay) rather than an
            // out-of-band dispatch (#18).
            if !text.is_empty() {
                cmds.push(Cmd::CopyToClipboard(text));
            }
        },
    }

    (state, cmds)
}

/// Emit `Cmd::SetTerminalTitle` iff the derived title changed since
/// the last emission. Called from arms that actually mutate
/// `state.session.conversation.title` (`SubmitPrompt`, `ConversationLoaded`,
/// `ConfirmAccepted` → `ClearConversation`) — never at the tail of every
/// `update()` so `Tick`/resize/etc. stay free.
pub fn emit_title_if_changed(state: &mut State, cmds: &mut Vec<Cmd>) {
    let title = desired_title(state);
    if state.ui.last_title_dispatched.as_deref() != Some(title.as_str()) {
        cmds.push(Cmd::SetTerminalTitle(title.clone()));
        state.ui.last_title_dispatched = Some(title);
    }
}

/// The terminal title, reflecting run state: `mermaid · working` while a turn is
/// in flight, else `mermaid · <conversation title>` (or just `mermaid`). Plain
/// text with a middot separator — no emoji.
pub fn desired_title(state: &State) -> String {
    if !matches!(state.turn, TurnState::Idle) {
        return "mermaid · working".to_string();
    }
    let conv = state.session.conversation.title.trim();
    if conv.is_empty() {
        "mermaid".to_string()
    } else {
        format!("mermaid · {conv}")
    }
}
