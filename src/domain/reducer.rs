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

use crate::models::{ChatMessage, MessageRole, ProviderContinuation, TokenUsage};
use crate::prompts::get_system_prompt;
use crate::runtime::TaskStatus;

use super::cmd::{ChatRequest, Cmd};
use super::compaction::{
    CompactionArchive, CompactionPolicy, CompactionRequest, CompactionResult, CompactionTrigger,
    context_exceeds_hard_limit, format_compact_count, should_auto_compact,
};
use super::ids::TurnId;
use super::msg::{ClipboardRead, KeyCode, KeyMods, Msg, Paste, SlashCmd};
use super::state::{
    GenPhase, McpServerEntry, McpServerStatus, State, StatusKind, TokenUsageTotals, ToolOutcome,
    TurnState, UiMode,
};
use super::transition::{
    action_display_for, commit_assistant_message, fill_outcome, start_generating,
    tool_result_messages, try_complete_outcomes,
};
use super::{COMMAND_GROUPS, COMMAND_REGISTRY};

/// Cap on how many queued follow-up messages get drained per
/// external `update()` call. Arms typically enqueue zero or one
/// follow-up; this cap catches runaway loops from future arms that
/// might enqueue unboundedly.
const MAX_PENDING_DRAIN: usize = 16;

/// Cap on `state.ui.queued_messages` — the user-typed prompts queued while a
/// turn is in flight. Holding Enter during a long turn would otherwise grow it
/// without bound; past the cap the oldest queued prompt is dropped.
const MAX_QUEUED_MESSAGES: usize = 32;

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
            handle_paste(&mut state, paste);
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
                state.runtime.ollama_context = Some(crate::domain::runtime::OllamaContextInfo {
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
                let placement = crate::domain::runtime::OllamaPlacement {
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
                    let convo_tokens = crate::domain::compaction::estimate_messages_tokens(
                        state.session.messages(),
                    );
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
                .push_back(super::state::PendingApproval {
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
                .push_back(super::question::PendingQuestionSet::new(
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
                    config: crate::app::McpServerConfig::default(),
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
                    config: crate::app::McpServerConfig::default(),
                    status,
                    tools: Vec::new(),
                });
            push_system(
                &mut state,
                &mut cmds,
                format!("MCP server {} errored: {}", name, reason),
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
        Msg::ConversationLoaded(history) => {
            // If a turn was in flight when the user loaded another conversation
            // (`/load` mid-generation), cancel its scope first. Otherwise we
            // overwrite `state.turn` to `Idle` below and lose the only handle —
            // the turn's CancellationToken + JoinSet — that could stop the
            // running model call and tool tasks, orphaning them uncancellable;
            // their parked approval requests could never be answered either (#2).
            if let Some(id) = state.turn.id() {
                cmds.push(Cmd::CancelScope(id));
                // Drop the cancelled turn's parked approval/question modals and
                // its stale running-tool indicators — the tasks behind them are
                // being torn down.
                clear_parked_tool_requests(&mut state);
                state.ui.live_tool_status.clear();
            }
            // Messages queued against the *previous* conversation must not
            // auto-submit into the one being loaded — drop them (mirrors the
            // clears above).
            state.ui.queued_messages.clear();
            state.session.conversation = history;
            state.turn = TurnState::Idle;
            state.ui.mode = UiMode::EditingInput;
            // The pause belonged to the previous conversation's failing
            // compaction; the loaded one starts fresh.
            state.runtime.auto_compact_suppressed = false;
            // The loaded conversation has its own id — the previous session's
            // scratch dir no longer applies. Recompute (same as `/clear`).
            refresh_scratchpad(&mut state, &mut cmds);
            emit_title_if_changed(&mut state, &mut cmds);
        },
        Msg::ConversationsListed(candidates) => {
            if let UiMode::ConversationList { .. } = state.ui.mode {
                state.ui.mode = UiMode::ConversationList {
                    candidates,
                    cursor: 0,
                };
            }
            // If the user already navigated away (Esc before the
            // list landed), the event silently drops.
        },
        Msg::ProjectFilesListed(files) => {
            state.ui.project_files_loading = false;
            state.ui.project_files = Some(files);
            // Stale-while-revalidate: the user has been filtering the old
            // cache; swap the fresh list in and re-rank the open picker.
            recompute_file_matches(&mut state);
        },
        Msg::RuntimeTasksListed(tasks) => {
            state
                .session
                .append(ChatMessage::system(tasks_text(&tasks)), state.now);
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        Msg::RuntimeTaskLoaded { task, events } => {
            state.session.append(
                ChatMessage::system(task_detail_text(task.as_ref(), &events)),
                state.now,
            );
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        Msg::RuntimeProcessesListed(processes) => {
            state
                .session
                .append(ChatMessage::system(processes_text(&processes)), state.now);
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        Msg::RuntimeText(text) => {
            state.session.append(ChatMessage::system(text), state.now);
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        Msg::RuntimeApprovalsListed(approvals) => {
            state
                .session
                .append(ChatMessage::system(approvals_text(&approvals)), state.now);
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        Msg::RuntimeCheckpointsListed(checkpoints) => {
            state.session.append(
                ChatMessage::system(checkpoints_text(&checkpoints)),
                state.now,
            );
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        Msg::ForkCheckpointsFound(checkpoints) => {
            // Reply to the rewind/fork lookup. Files were NOT rewound —
            // point the user at the oldest checkpoint past the cut (each is
            // a PRE-mutation snapshot, so the oldest holds file state
            // closest to the fork point). Empty = nothing to say.
            if let Some(oldest) = checkpoints.first() {
                push_system(
                    &mut state,
                    &mut cmds,
                    format!(
                        "{} file checkpoint(s) were created after this point. Files were \
                         not rewound. /restore {} restores the files changed by the first \
                         mutation after the fork; /checkpoints lists the rest.",
                        checkpoints.len(),
                        oldest.id
                    ),
                );
            }
        },
        Msg::RuntimePluginsListed(plugins) => {
            state
                .session
                .append(ChatMessage::system(plugins_text(&plugins)), state.now);
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        Msg::ModelPullFinished { model } => {
            push_system(&mut state, &mut cmds, format!("Pulled {}", model));
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
            // "config saved", etc.). Routed into the chat transcript instead of
            // the old transient banner above the input.
            push_system(&mut state, &mut cmds, text);
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
                .push(crate::domain::runtime::BackgroundAgent {
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
                        super::compaction::format_compact_count(tokens),
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
                    super::compaction::format_compact_count(tokens),
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
                .push_back(super::state::QueuedMessage {
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
/// `state.session.conversation.title` (SubmitPrompt, ConversationLoaded,
/// ConfirmAccepted → ClearConversation) — never at the tail of every
/// update() so `Tick`/resize/etc. stay free.
fn emit_title_if_changed(state: &mut State, cmds: &mut Vec<Cmd>) {
    let title = desired_title(state);
    if state.ui.last_title_dispatched.as_deref() != Some(title.as_str()) {
        cmds.push(Cmd::SetTerminalTitle(title.clone()));
        state.ui.last_title_dispatched = Some(title);
    }
}

/// The terminal title, reflecting run state: `mermaid · working` while a turn is
/// in flight, else `mermaid · <conversation title>` (or just `mermaid`). Plain
/// text with a middot separator — no emoji.
fn desired_title(state: &State) -> String {
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

// ─── helpers ────────────────────────────────────────────────────────

/// Outcome of one keypress against a question modal: keep showing it, or
/// resolve the whole set one way or the other.
enum QuestionKeyAction {
    Stay,
    Submit,
    Dismiss,
    Reformulate,
}

/// Advance past the current question: resolve immediately for the atomic
/// single-select-single-question case, step to the next question, or land on
/// the review screen.
fn advance_question(set: &mut super::question::PendingQuestionSet) -> QuestionKeyAction {
    let nq = set.questions.len();
    if set.skips_review() {
        return QuestionKeyAction::Submit;
    }
    if set.active + 1 < nq {
        set.active += 1;
    } else {
        set.active = nq; // review screen
    }
    QuestionKeyAction::Stay
}

/// Act on an option row: toggle it (multi-select) or choose it and advance
/// (single-select, which also drops any typed "Other" text).
fn act_on_option(
    set: &mut super::question::PendingQuestionSet,
    q_idx: usize,
    opt_idx: usize,
) -> QuestionKeyAction {
    let multi = set.questions[q_idx].is_multi();
    let sel = &mut set.selections[q_idx];
    if multi {
        if let Some(pos) = sel.chosen.iter().position(|&i| i == opt_idx) {
            sel.chosen.remove(pos);
        } else {
            sel.chosen.push(opt_idx);
        }
        QuestionKeyAction::Stay
    } else {
        sel.chosen = vec![opt_idx];
        sel.other_text.clear();
        advance_question(set)
    }
}

/// Act on the row under the cursor: an option, the "Other" free-text row, or
/// the multi-select Submit row.
fn act_on_row(
    set: &mut super::question::PendingQuestionSet,
    q_idx: usize,
    row: usize,
) -> QuestionKeyAction {
    let n = set.questions[q_idx].options.len();
    let multi = set.questions[q_idx].is_multi();
    if row < n {
        return act_on_option(set, q_idx, row);
    }
    if row == set.other_row(q_idx) {
        // Multi-select captures the typed text directly, so Enter here is a
        // no-op; single-select commits the typed answer (if any) and advances.
        if multi || set.selections[q_idx].other_text.trim().is_empty() {
            return QuestionKeyAction::Stay;
        }
        set.selections[q_idx].chosen.clear();
        return advance_question(set);
    }
    if Some(row) == set.submit_row(q_idx) {
        return advance_question(set);
    }
    QuestionKeyAction::Stay
}

/// Apply one keypress to the front question set, returning whether it resolves.
fn apply_question_key(
    set: &mut super::question::PendingQuestionSet,
    code: KeyCode,
    mods: KeyMods,
) -> QuestionKeyAction {
    // Note-editing sub-mode: keystrokes edit the active question's note until
    // Enter/Esc exits (Esc here leaves the note intact — it does not dismiss).
    if set.editing_note {
        match code {
            KeyCode::Enter | KeyCode::Escape => set.editing_note = false,
            KeyCode::Char(c) if !mods.ctrl && !mods.alt => {
                if let Some(sel) = set.selections.get_mut(set.active) {
                    sel.note.push(c);
                }
            },
            KeyCode::Backspace => {
                if let Some(sel) = set.selections.get_mut(set.active) {
                    sel.note.pop();
                }
            },
            _ => {},
        }
        return QuestionKeyAction::Stay;
    }

    // Esc dismisses the whole set.
    if code == KeyCode::Escape && mods.is_empty() {
        return QuestionKeyAction::Dismiss;
    }

    let nq = set.questions.len();

    // `n` opens note editing for the active question — but not on the review
    // screen, and not when the cursor sits in the Other text field (where `n`
    // is a literal character).
    if code == KeyCode::Char('n')
        && mods.is_empty()
        && set.active < nq
        && set.questions[set.active].is_choice()
        && set.selections[set.active].cursor != set.other_row(set.active)
    {
        set.editing_note = true;
        return QuestionKeyAction::Stay;
    }

    // `c` = "Chat about this": bounce the whole set back to the model to
    // reformulate. Available on choice/rank tabs (not the Other field) and on
    // the review screen; on input tabs `c` is a literal character.
    if code == KeyCode::Char('c')
        && mods.is_empty()
        && (set.active >= nq
            || (set.questions[set.active].is_choice()
                && set.selections[set.active].cursor != set.other_row(set.active)))
    {
        return QuestionKeyAction::Reformulate;
    }

    // `r` = toggle "remember my answers across sessions" (available where `c`
    // is). The tool persists answers keyed by each question's `memory_key`.
    if code == KeyCode::Char('r')
        && mods.is_empty()
        && (set.active >= nq
            || (set.questions[set.active].is_choice()
                && set.selections[set.active].cursor != set.other_row(set.active)))
    {
        set.remember = !set.remember;
        return QuestionKeyAction::Stay;
    }

    // Tab-strip navigation between questions / the review screen. Tab + Right
    // move forward; BackTab + Left move back (no in-field cursor in Stage 1).
    let go_next = code == KeyCode::Tab || (mods.is_empty() && code == KeyCode::Right);
    let go_prev = code == KeyCode::BackTab || (mods.is_empty() && code == KeyCode::Left);
    if go_next {
        set.active = (set.active + 1).min(nq);
        return QuestionKeyAction::Stay;
    }
    if go_prev {
        set.active = set.active.saturating_sub(1);
        return QuestionKeyAction::Stay;
    }

    // Review screen: 0 = Submit answers, 1 = Cancel.
    if set.active >= nq {
        match code {
            KeyCode::Up => set.review_cursor = 0,
            KeyCode::Down => set.review_cursor = 1,
            KeyCode::Char('1') => return QuestionKeyAction::Submit,
            KeyCode::Char('2') => return QuestionKeyAction::Dismiss,
            KeyCode::Enter => {
                return if set.review_cursor == 0 {
                    QuestionKeyAction::Submit
                } else {
                    QuestionKeyAction::Dismiss
                };
            },
            _ => {},
        }
        return QuestionKeyAction::Stay;
    }

    // A question tab.
    let q_idx = set.active;
    if set.questions[q_idx].is_input() {
        return apply_input_key(set, q_idx, code, mods);
    }
    if set.questions[q_idx].is_rank() {
        return apply_rank_key(set, q_idx, code, mods);
    }
    // Select / MultiSelect.
    let n = set.questions[q_idx].options.len();
    let other_row = set.other_row(q_idx);
    let row_count = set.row_count(q_idx);
    let cursor = set.selections[q_idx].cursor;

    // Text entry into the "Other" row: plain/shifted printables append,
    // Backspace deletes. Other keys fall through to navigation below.
    if cursor == other_row {
        match code {
            KeyCode::Char(c) if !mods.ctrl && !mods.alt => {
                set.selections[q_idx].other_text.push(c);
                return QuestionKeyAction::Stay;
            },
            KeyCode::Backspace => {
                set.selections[q_idx].other_text.pop();
                return QuestionKeyAction::Stay;
            },
            _ => {},
        }
    }

    match code {
        KeyCode::Up => {
            set.selections[q_idx].cursor = cursor.saturating_sub(1);
            QuestionKeyAction::Stay
        },
        KeyCode::Down => {
            set.selections[q_idx].cursor = (cursor + 1).min(row_count.saturating_sub(1));
            QuestionKeyAction::Stay
        },
        // Number keys jump to and act on an option directly (1-based).
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let idx = (c as usize) - ('1' as usize);
            if idx < n {
                set.selections[q_idx].cursor = idx;
                act_on_option(set, q_idx, idx)
            } else {
                QuestionKeyAction::Stay
            }
        },
        KeyCode::Enter | KeyCode::Char(' ') => act_on_row(set, q_idx, cursor),
        _ => QuestionKeyAction::Stay,
    }
}

/// Key handling for an input-kind question (Text/Number/Date/Path): typing
/// edits the value, Number steps with Up/Down, Enter submits when valid.
fn apply_input_key(
    set: &mut super::question::PendingQuestionSet,
    q_idx: usize,
    code: KeyCode,
    mods: KeyMods,
) -> QuestionKeyAction {
    let is_number = matches!(
        set.questions[q_idx].kind,
        crate::domain::QuestionKind::Number { .. }
    );
    match code {
        KeyCode::Char(c) if !mods.ctrl && !mods.alt => {
            set.selections[q_idx].value.push(c);
            QuestionKeyAction::Stay
        },
        KeyCode::Backspace => {
            set.selections[q_idx].value.pop();
            QuestionKeyAction::Stay
        },
        KeyCode::Up if is_number => {
            step_number(set, q_idx, 1.0);
            QuestionKeyAction::Stay
        },
        KeyCode::Down if is_number => {
            step_number(set, q_idx, -1.0);
            QuestionKeyAction::Stay
        },
        KeyCode::Enter => {
            let kind = set.questions[q_idx].kind.clone();
            if crate::domain::validate_input(&kind, &set.selections[q_idx].value).is_ok() {
                advance_question(set)
            } else {
                QuestionKeyAction::Stay
            }
        },
        _ => QuestionKeyAction::Stay,
    }
}

/// Step a Number question's value by `dir * step`, clamped to min/max.
fn step_number(set: &mut super::question::PendingQuestionSet, q_idx: usize, dir: f64) {
    let (min, max, step) = match &set.questions[q_idx].kind {
        crate::domain::QuestionKind::Number { min, max, step, .. } => (*min, *max, *step),
        _ => return,
    };
    let step = step.unwrap_or(1.0);
    let cur: f64 = set.selections[q_idx]
        .value
        .trim()
        .parse()
        .unwrap_or(min.unwrap_or(0.0));
    let mut next = cur + dir * step;
    if let Some(lo) = min {
        next = next.max(lo);
    }
    if let Some(hi) = max {
        next = next.min(hi);
    }
    set.selections[q_idx].value = format_number(next);
}

/// Format a number without a trailing `.0` for whole values.
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Key handling for a Rank question: Up/Down move the cursor; Space grabs the
/// item under the cursor so Up/Down then moves it; Enter submits the order.
fn apply_rank_key(
    set: &mut super::question::PendingQuestionSet,
    q_idx: usize,
    code: KeyCode,
    _mods: KeyMods,
) -> QuestionKeyAction {
    let n = set.questions[q_idx].options.len();
    if set.selections[q_idx].order.is_empty() {
        set.selections[q_idx].order = (0..n).collect();
    }
    let sel = &mut set.selections[q_idx];
    match code {
        KeyCode::Char(' ') => {
            sel.grabbed = !sel.grabbed;
            QuestionKeyAction::Stay
        },
        KeyCode::Up => {
            if sel.grabbed && sel.cursor > 0 {
                sel.order.swap(sel.cursor, sel.cursor - 1);
                sel.cursor -= 1;
            } else {
                sel.cursor = sel.cursor.saturating_sub(1);
            }
            QuestionKeyAction::Stay
        },
        KeyCode::Down => {
            if sel.grabbed && sel.cursor + 1 < n {
                sel.order.swap(sel.cursor, sel.cursor + 1);
                sel.cursor += 1;
            } else {
                sel.cursor = (sel.cursor + 1).min(n.saturating_sub(1));
            }
            QuestionKeyAction::Stay
        },
        KeyCode::Enter => advance_question(set),
        _ => QuestionKeyAction::Stay,
    }
}

/// Route a keypress to the front question modal, resolving it into
/// `Cmd::ResolveQuestion` when the user submits or dismisses. Exclusive while a
/// question set is pending.
fn handle_question_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode, mods: KeyMods) {
    let action = {
        let Some(set) = state.pending_question.front_mut() else {
            return;
        };
        apply_question_key(set, code, mods)
    };
    let resolution = match action {
        QuestionKeyAction::Stay => return,
        QuestionKeyAction::Submit => {
            let Some(set) = state.pending_question.front() else {
                return;
            };
            crate::domain::QuestionResolution::Answered {
                answers: set.build_answers(),
                remember: set.remember,
            }
        },
        QuestionKeyAction::Dismiss => crate::domain::QuestionResolution::Dismissed,
        QuestionKeyAction::Reformulate => crate::domain::QuestionResolution::Reformulate,
    };
    if let Some(front) = state.pending_question.front() {
        let call_id = front.call_id;
        state.pending_question.pop_front();
        cmds.push(Cmd::ResolveQuestion {
            call_id,
            resolution,
        });
    }
}

fn handle_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode, mods: KeyMods) {
    // Ctrl+C: press twice to exit. The first press does the useful thing —
    // interrupts a running turn (like Esc) or clears typed input — and arms a
    // short confirm window; a second press inside the window exits. This also
    // makes a stray copy-chord harmless: on terminals without the kitty
    // protocol Ctrl+Shift+C arrives byte-identical to Ctrl+C, and with the
    // protocol it arrives with the SHIFT bit — blocked here by `!mods.shift`
    // and intercepted earlier in the run loop as the copy action. Ctrl+D on
    // empty input and `/quit` remain immediate exits.
    if mods.ctrl && !mods.shift && code == KeyCode::Char('c') {
        if state
            .ui
            .exit_armed_until
            .is_some_and(|deadline| state.now <= deadline)
        {
            request_exit(state, cmds);
            return;
        }
        if state.is_busy() {
            handle_cancel_turn(state, cmds);
        } else if !state.ui.input_buffer.is_empty() {
            state.ui.input_buffer.clear();
            state.ui.input_cursor = 0;
            state.ui.palette_cursor = None;
            state.ui.input_history_cursor = None;
            state.ui.history_draft.clear();
        }
        state.ui.exit_armed_until = Some(
            state.now + chrono::Duration::seconds(crate::constants::UI_EXIT_CONFIRM_WINDOW_SECS),
        );
        return;
    }
    // Any other key disarms a pending exit confirmation — the user moved on.
    state.ui.exit_armed_until = None;
    // Same for the double-Esc rewind arming: only Esc keeps it alive.
    if code != KeyCode::Escape {
        state.ui.esc_armed_at = None;
    }

    // Ctrl+B: send a running foreground command to the background (it keeps
    // running as a `/processes` entry) instead of waiting on it. Only
    // meaningful while tools are executing; a swallowed no-op otherwise.
    if mods.ctrl && code == KeyCode::Char('b') {
        if let TurnState::ExecutingTools { id, .. } = &state.turn {
            cmds.push(Cmd::BackgroundScope(*id));
        }
        return;
    }

    // Ctrl+L: force a full repaint (the universal readline "redraw screen"
    // chord). Recovers from anything that scribbled on the terminal behind
    // ratatui's back buffer. Meta-level like Ctrl+C/Ctrl+B — deliberately
    // above the modal handlers so a repaint works with a modal open too.
    if mods.ctrl && code == KeyCode::Char('l') {
        state.ui.full_redraw_seq = state.ui.full_redraw_seq.wrapping_add(1);
        return;
    }

    // Ctrl+T: toggle the task checklist between its expanded rows and the
    // one-line collapsed form. Meta-level like Ctrl+L — works everywhere,
    // no-op in effect when no checklist is showing.
    if mods.ctrl && code == KeyCode::Char('t') {
        state.ui.tasks_collapsed = !state.ui.tasks_collapsed;
        return;
    }

    // Ctrl+O: compose the input draft in $VISUAL/$EDITOR. Allowed while a
    // turn is busy (it only edits the draft), but only from the plain input
    // surface — never over a picker or a pending approval/question modal,
    // whose keyboard focus it would steal.
    if mods.ctrl
        && code == KeyCode::Char('o')
        && matches!(state.ui.mode, UiMode::EditingInput)
        && state.pending_approval.is_empty()
        && state.pending_question.is_empty()
        && state.confirm.is_none()
    {
        cmds.push(Cmd::ComposeInEditor {
            text: state.ui.input_buffer.clone(),
        });
        return;
    }

    // Transcript scrolling (keyboard): PageUp/PageDown by a page, Shift+Up/Down
    // by a line, End to jump back to the newest message. Reuses the pure
    // publish-then-diff scroll pipeline — the render layer applies the delta,
    // so the reducer only bumps a counter. Positive accum scrolls toward older
    // messages (matches `Msg::MouseScroll`).
    const SCROLL_PAGE: i32 = 10;
    match code {
        KeyCode::PageUp => {
            state.ui.mouse_scroll_accum = state.ui.mouse_scroll_accum.saturating_add(SCROLL_PAGE);
            return;
        },
        KeyCode::PageDown => {
            state.ui.mouse_scroll_accum = state.ui.mouse_scroll_accum.saturating_sub(SCROLL_PAGE);
            return;
        },
        KeyCode::Up if mods.shift => {
            state.ui.mouse_scroll_accum = state.ui.mouse_scroll_accum.saturating_add(1);
            return;
        },
        KeyCode::Down if mods.shift => {
            state.ui.mouse_scroll_accum = state.ui.mouse_scroll_accum.saturating_sub(1);
            return;
        },
        KeyCode::End => {
            state.ui.scroll_to_bottom_seq = state.ui.scroll_to_bottom_seq.wrapping_add(1);
            return;
        },
        _ => {},
    }

    // Inline approval modal: while a tool awaits approval the prompt is
    // exclusive. Direct keys resolve immediately — 1/y approve · 2/a approve +
    // don't-ask-again · 3/n/Esc deny. Or move the highlight with ↑/↓ and press
    // Enter on it. Sits ABOVE the Esc-cancel guard so Esc denies just this tool
    // (keeping the turn alive) rather than cancelling the whole turn. Any other
    // key is swallowed. Resolving emits `Cmd::ResolveApproval`, which unblocks
    // the parked tool task via the broker.
    if !state.pending_approval.is_empty() {
        use crate::domain::ApprovalChoice;
        // Content-bearing external tools are non-allowlistable: the gate signals
        // this with an empty allowlist scope, and the modal then omits the
        // middle "approve always" option (#6, #31). Layout:
        //   allowlistable:     0 = Yes, 1 = Yes-always, 2 = No
        //   non-allowlistable: 0 = Yes,                 1 = No
        let allowlistable = state
            .pending_approval
            .front()
            .map(|i| !i.allowlist_scope.is_empty())
            .unwrap_or(false);
        let option_count = if allowlistable { 3 } else { 2 };
        let choice_for = |idx: usize| match (allowlistable, idx) {
            (true, 0) | (false, 0) => ApprovalChoice::Approve,
            (true, 1) => ApprovalChoice::ApproveAlways,
            _ => ApprovalChoice::Deny,
        };
        // Copy the current highlight out so the ↑/↓ arms can take a fresh
        // mutable borrow without conflicting.
        let selected = state
            .pending_approval
            .front()
            .map(|i| i.selected_option)
            .unwrap_or(0);
        let choice = match code {
            KeyCode::Char('1') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                Some(ApprovalChoice::Approve)
            },
            // 'a'/'A' and '2' select approve-always only when allowlistable;
            // when not, '2' is the (second, final) "No" option.
            KeyCode::Char('a') | KeyCode::Char('A') if allowlistable => {
                Some(ApprovalChoice::ApproveAlways)
            },
            KeyCode::Char('2') => Some(if allowlistable {
                ApprovalChoice::ApproveAlways
            } else {
                ApprovalChoice::Deny
            }),
            KeyCode::Char('3') | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Escape => {
                Some(ApprovalChoice::Deny)
            },
            KeyCode::Enter => Some(choice_for(selected)),
            KeyCode::Up => {
                if let Some(front) = state.pending_approval.front_mut() {
                    front.selected_option = selected.saturating_sub(1);
                }
                None
            },
            KeyCode::Down => {
                if let Some(front) = state.pending_approval.front_mut() {
                    front.selected_option = (selected + 1).min(option_count - 1);
                }
                None
            },
            _ => None,
        };
        if let Some(decision) = choice
            && let Some(call_id) = state.pending_approval.front().map(|i| i.call_id)
        {
            state.pending_approval.pop_front();
            cmds.push(Cmd::ResolveApproval { call_id, decision });
        }
        return;
    }

    // Inline question modal (ask_user_question): exclusive while a question set
    // awaits answers. Sits ABOVE the Esc-cancel guard so Esc dismisses just the
    // question and keeps the turn alive, mirroring the approval modal.
    if !state.pending_question.is_empty() {
        handle_question_key(state, cmds, code, mods);
        return;
    }

    // Pending confirmation modal (e.g. `/clear`): y/Enter accepts, n/Esc
    // declines. (This handler — and the render side — were missing, so the
    // confirmation was previously inert.)
    if state.confirm.is_some() {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                handle_confirm_accepted(state, cmds);
            },
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Escape => {
                state.confirm = None;
            },
            _ => {},
        }
        return;
    }

    // Esc interrupts active work by cancelling the current turn. It must NEVER
    // exit mermaid — only Ctrl+C (or `/quit`) does that. A second Esc while the
    // turn is already cancelling is a no-op: the cancellation is underway, and
    // Ctrl+C is the escalation path if it ever wedges. (Previously a second Esc
    // mid-cancel force-exited, which booted users out unexpectedly and could
    // leave a backgrounded process holding the terminal.) When idle, Esc falls
    // through to the palette/input/focus handlers below.
    if mods.is_empty() && code == KeyCode::Escape && state.is_busy() {
        if !matches!(state.turn, TurnState::Cancelling { .. }) {
            handle_cancel_turn(state, cmds);
        }
        return;
    }

    // Ctrl+D on empty input quits.
    if mods.ctrl && code == KeyCode::Char('d') && state.ui.input_buffer.is_empty() {
        request_exit(state, cmds);
        return;
    }

    // Ctrl+V: read the system clipboard and paste its contents. Gate
    // on `EditingInput` + no confirmation modal so the palette and
    // conversation-list picker don't swallow the keystroke. The
    // actual clipboard read happens off-thread in the effect runner
    // (xclip / wl-paste / pngpaste / PowerShell can block for
    // hundreds of ms on macOS); result comes back asynchronously as
    // `Msg::ClipboardRead(Image|Text|Empty|Error)`.
    if mods.ctrl
        && code == KeyCode::Char('v')
        && matches!(state.ui.mode, UiMode::EditingInput)
        && state.confirm.is_none()
    {
        // Mark a read in flight so a fast Enter waits for it (paste-race guard).
        state.ui.clipboard_reads_pending += 1;
        cmds.push(Cmd::ReadClipboard);
        return;
    }

    // Ctrl+J: insert a newline at the cursor (multi-line input). Works on
    // legacy terminals too — raw-mode crossterm parses the LF byte Ctrl+J
    // sends as `Char('j') + CONTROL` while Enter arrives as CR — so the
    // chord needs no kitty disambiguation. Gated like Ctrl+V so pickers
    // and modals never receive a stray newline.
    if mods.ctrl
        && code == KeyCode::Char('j')
        && matches!(state.ui.mode, UiMode::EditingInput)
        && state.confirm.is_none()
    {
        insert_text_at_cursor(state, "\n");
        return;
    }

    // Alt+T cycles reasoning depth. Persists per-model so cycling on
    // Sonnet doesn't bleed into the next session with Ollama.
    if mods.alt && code == KeyCode::Char('t') {
        let next = cycle_reasoning(state.session.reasoning);
        state.session.reasoning = next;
        cmds.push(Cmd::PersistReasoningFor {
            model_id: state.session.model_id.clone(),
            level: next,
        });
        // The bottom status bar already shows the new reasoning level — no banner.
        return;
    }

    // Alt+P toggles plan mode. A dedicated chord rather than a fifth
    // Shift+Tab cycle entry: plan mode restores the safety mode the user was
    // in, which a flat cycle can't express, and a cycle entry would put plan
    // on the path to full_access.
    if mods.alt && code == KeyCode::Char('p') {
        if state.session.plan.is_some() {
            exit_plan_mode(state, cmds);
        } else {
            enter_plan_mode(state, cmds);
        }
        return;
    }

    // Shift+Tab cycles the safety mode (read-only → ask → auto → full-access).
    // Session-scoped: the `[safety]` config value stays the persistent default,
    // so a session never silently inherits a more-permissive mode from a
    // previous run. Mirrors the Alt+T reasoning cycle above.
    if code == KeyCode::BackTab {
        let previous = state.session.safety_mode;
        let next = cycle_safety(previous);
        state.session.safety_mode = next;
        // Leaving read_only past a stale denial nudges the model to re-attempt
        // (hidden from the transcript); `build_chat_request` additionally
        // rewrites the stale denials themselves.
        note_safety_mode_change(state, cmds, previous, next);
        // Persist now so `--resume`/`--continue` restore this mode even if the
        // user changes it and quits without sending another message.
        cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        // The bottom status bar already shows the new safety mode — no banner.
        return;
    }

    // Conversation-list picker (UiMode::ConversationList): ↑/↓
    // navigate, Enter loads the highlighted session, Esc dismisses.
    if matches!(state.ui.mode, UiMode::ConversationList { .. }) {
        handle_conversation_list_key(state, cmds, code);
        return;
    }

    // Rewind picker (double-Esc): ↑/↓ navigate the user messages, Enter
    // forks the session at the highlighted one, Esc dismisses untouched.
    if matches!(state.ui.mode, UiMode::RewindPicker { .. }) {
        handle_rewind_picker_key(state, cmds, code);
        return;
    }

    // Plan-mode settings picker (/plan config): ↑/↓ navigate, Enter/←/→
    // cycle the highlighted value, Esc closes.
    if matches!(state.ui.mode, UiMode::PlanConfig { .. }) {
        handle_plan_config_key(state, cmds, code);
        return;
    }

    // Slash-palette navigation — intercepts ↑/↓/Tab/Esc while the
    // input buffer opens with `/`. Enter falls through to the normal
    // handler below so the command actually dispatches.
    if state.ui.input_buffer.starts_with('/') {
        use crate::domain::slash_commands::filter_entries;
        let typed = state
            .ui
            .input_buffer
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        let candidates = filter_entries(&typed, &state.plugin_commands);
        match code {
            KeyCode::Up => {
                let cur = state.ui.palette_cursor.unwrap_or(0);
                state.ui.palette_cursor = Some(cur.saturating_sub(1));
                return;
            },
            KeyCode::Down => {
                let max = candidates.len().saturating_sub(1);
                let cur = state.ui.palette_cursor.unwrap_or(0);
                state.ui.palette_cursor = Some((cur + 1).min(max));
                return;
            },
            KeyCode::Tab => {
                let sel = state.ui.palette_cursor.unwrap_or(0);
                if let Some(entry) = candidates.get(sel) {
                    let completed = format!("/{} ", entry.name());
                    drop(candidates);
                    state.ui.input_buffer = completed;
                    state.ui.input_cursor = state.ui.input_buffer.len();
                    state.ui.palette_cursor = Some(0);
                }
                return;
            },
            KeyCode::Escape => {
                state.ui.input_buffer.clear();
                state.ui.input_cursor = 0;
                state.ui.palette_cursor = None;
                return;
            },
            KeyCode::Enter if !mods.shift => {
                // Complete-then-execute: replace the command word with
                // the highlighted candidate (preserving any args the
                // user already typed), then fall through to the Enter
                // handler below so the command actually dispatches.
                let sel = state.ui.palette_cursor.unwrap_or(0);
                if let Some(entry) = candidates.get(sel) {
                    let name = entry.name().to_string();
                    drop(candidates);
                    let raw = state.ui.input_buffer.clone();
                    let after_slash = raw.trim_start_matches('/');
                    let rest = match after_slash.find(char::is_whitespace) {
                        Some(idx) => &after_slash[idx..],
                        None => "",
                    };
                    state.ui.input_buffer = format!("/{}{}", name, rest);
                    state.ui.input_cursor = state.ui.input_buffer.len();
                }
                // Fall through to the Enter handler below.
            },
            _ => {
                // Fall through to normal key handling (char/Backspace
                // update the filter; palette_cursor gets reset below).
            },
        }
    }

    // @-mention file picker — intercepts ↑/↓/Tab/Enter/Esc while an
    // @-token is under the cursor (never on a slash command; the palette
    // above owns that surface). Enter COMPLETES here instead of submitting:
    // picking a file and firing the prompt with one keypress would send a
    // half-written message.
    if state.ui.file_picker_open() {
        match code {
            KeyCode::Up => {
                let cur = state.ui.file_picker_cursor.unwrap_or(0);
                state.ui.file_picker_cursor = Some(cur.saturating_sub(1));
                return;
            },
            KeyCode::Down => {
                let max = state.ui.file_picker_matches.len().saturating_sub(1);
                let cur = state.ui.file_picker_cursor.unwrap_or(0);
                state.ui.file_picker_cursor = Some((cur + 1).min(max));
                return;
            },
            KeyCode::Tab => {
                complete_file_mention(state);
                return;
            },
            KeyCode::Enter if !mods.shift && !state.ui.file_picker_matches.is_empty() => {
                complete_file_mention(state);
                return;
            },
            KeyCode::Escape => {
                // Dismiss for THIS token only; the input stays untouched
                // (deliberate divergence from the slash palette's clear-all —
                // the @-text is prose the user typed, not a command filter).
                state.ui.file_picker_dismissed = true;
                state.ui.file_picker_matches.clear();
                state.ui.file_picker_cursor = None;
                return;
            },
            _ => {
                // Fall through: chars/Backspace edit the query below and the
                // trailing refresh re-ranks the matches.
            },
        }
    }

    // Enter submits the current input (or triggers the slash palette
    // pick) regardless of shift — Ctrl+J is the newline chord for
    // multi-line input. This arm enqueues a synthetic `Msg` on
    // `pending_msgs` rather than invoking the dispatch directly — the
    // outer `update()` drain will run the follow-up with stale-filter +
    // pending-msgs guarantees intact.
    if code == KeyCode::Enter {
        // Paste-race guard: if a Ctrl+V clipboard read is still in flight, hold
        // the submit until it lands. `handle_clipboard_read` re-runs
        // `submit_current_input` once the last pending read drains, re-deriving
        // the text + attachments so a just-pasted image is included rather than
        // dropped (and no stray `[Image #N]` leaks into the next prompt).
        if state.ui.clipboard_reads_pending > 0 {
            if !state.ui.input_buffer.trim().is_empty() {
                state.ui.submit_after_clipboard = true;
            }
            return;
        }
        submit_current_input(state);
        return;
    }

    if mods.is_empty() || mods.shift {
        match code {
            KeyCode::Char(c) => {
                // Any text mutation resets history nav — the user's
                // typing wins over whatever historical entry was
                // on-screen. It also un-dismisses the @-mention picker:
                // Esc dismisses per-token, typing reopens.
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
                state.ui.file_picker_dismissed = false;
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                state.ui.input_buffer.insert(pos, c);
                state.ui.input_cursor = clamp_cursor(&state.ui.input_buffer, pos + c.len_utf8());
                // Opening the palette, or editing its filter, resets
                // the cursor to the first candidate — stops stale
                // indices from pointing past the end of a shrinking
                // filter result.
                if state.ui.input_buffer.starts_with('/') {
                    state.ui.palette_cursor = Some(0);
                }
            },
            KeyCode::Backspace => {
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
                state.ui.file_picker_dismissed = false;
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                // If a whole `[Image #N]` pill ends at the cursor, delete it and
                // drop its attachment together; otherwise one codepoint.
                if let Some((start, number)) =
                    crate::domain::image_token::token_ending_at(&state.ui.input_buffer, pos)
                {
                    state.ui.input_buffer.drain(start..pos);
                    state.ui.input_cursor = start;
                    state.ui.attachments.retain(|a| a.number != number);
                } else if pos > 0 {
                    let new_pos = state.ui.input_buffer.floor_char_boundary(pos - 1);
                    state.ui.input_buffer.drain(new_pos..pos);
                    state.ui.input_cursor = new_pos;
                }
                if state.ui.input_buffer.starts_with('/') {
                    state.ui.palette_cursor = Some(0);
                } else {
                    state.ui.palette_cursor = None;
                }
            },
            KeyCode::Delete => {
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
                state.ui.file_picker_dismissed = false;
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                // Symmetric to Backspace: a pill starting at the cursor deletes
                // whole, taking its attachment with it.
                if let Some((end, number)) =
                    crate::domain::image_token::token_starting_at(&state.ui.input_buffer, pos)
                {
                    state.ui.input_buffer.drain(pos..end);
                    state.ui.attachments.retain(|a| a.number != number);
                } else if pos < state.ui.input_buffer.len() {
                    let next = state.ui.input_buffer.ceil_char_boundary(pos + 1);
                    state.ui.input_buffer.drain(pos..next);
                }
                if state.ui.input_buffer.starts_with('/') {
                    state.ui.palette_cursor = Some(0);
                } else {
                    state.ui.palette_cursor = None;
                }
            },
            KeyCode::Left => {
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                if pos > 0 {
                    state.ui.input_cursor = state.ui.input_buffer.floor_char_boundary(pos - 1);
                }
            },
            KeyCode::Right => {
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                if pos < state.ui.input_buffer.len() {
                    state.ui.input_cursor = state.ui.input_buffer.ceil_char_boundary(pos + 1);
                }
            },
            KeyCode::Home => state.ui.input_cursor = 0,
            KeyCode::End => state.ui.input_cursor = state.ui.input_buffer.len(),
            KeyCode::Up => {
                // Images are inline `[Image #N]` tokens now, so Up always steps
                // back through input history — no attachment-bar contention.
                history_nav_back(state);
            },
            KeyCode::Down => {
                history_nav_forward(state);
            },
            KeyCode::Escape => {
                // Clear any in-progress history nav, then run the double-Esc
                // rewind arming: first idle Esc arms, a second within the
                // window opens the rewind picker. Busy Esc never reaches here
                // (it cancelled and returned above).
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
                handle_rewind_esc(state);
            },
            _ => {},
        }
        // The buffer or cursor may have moved: re-evaluate the @-mention
        // token, re-rank matches, and fire the project walk on open.
        refresh_file_picker(state, cmds);
    }
}

/// Double-Esc window: a second idle Esc within this many ms of the first
/// opens the rewind picker.
const ESC_REWIND_WINDOW_MS: i64 = 1000;

/// Idle-Esc arming: first press arms, a second within the window fires the
/// rewind picker; past the window the press re-arms. Compared against the
/// injected `state.now` (pure; replay-exact).
fn handle_rewind_esc(state: &mut State) {
    let fired = state.ui.esc_armed_at.is_some_and(|armed| {
        (state.now - armed) <= chrono::Duration::milliseconds(ESC_REWIND_WINDOW_MS)
    });
    if !fired {
        state.ui.esc_armed_at = Some(state.now);
        return;
    }
    state.ui.esc_armed_at = None;
    let candidates = rewind_candidates(state.session.messages());
    if candidates.is_empty() {
        // Nothing to rewind to — silently no-op.
        return;
    }
    state.ui.mode = UiMode::RewindPicker {
        candidates,
        cursor: 0,
    };
}

/// The rewind targets: user-role `Normal` messages, NEWEST FIRST (the most
/// recent exchange is the most likely rewind point). Excerpts are the first
/// non-empty line, clipped.
fn rewind_candidates(messages: &[ChatMessage]) -> Vec<crate::domain::RewindCandidate> {
    const EXCERPT_MAX_CHARS: usize = 80;
    messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, m)| {
            m.role == crate::models::MessageRole::User
                && m.kind == crate::models::ChatMessageKind::Normal
        })
        .map(|(message_index, m)| {
            let line = m
                .content
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("(empty message)");
            let excerpt = if line.chars().count() > EXCERPT_MAX_CHARS {
                let mut clipped: String = line.chars().take(EXCERPT_MAX_CHARS).collect();
                clipped.push('…');
                clipped
            } else {
                line.to_string()
            };
            crate::domain::RewindCandidate {
                message_index,
                excerpt,
            }
        })
        .collect()
}

/// Handle keyboard input while the rewind picker is open. Up/Down walk the
/// candidate list; Enter forks the session at the highlighted user message;
/// Esc dismisses without touching the conversation.
fn handle_rewind_picker_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode) {
    let UiMode::RewindPicker {
        ref candidates,
        ref mut cursor,
    } = state.ui.mode
    else {
        return;
    };
    match code {
        KeyCode::Up => {
            *cursor = cursor.saturating_sub(1);
        },
        KeyCode::Down => {
            let max = candidates.len().saturating_sub(1);
            if *cursor < max {
                *cursor += 1;
            }
        },
        KeyCode::Enter => {
            if let Some(candidate) = candidates.get(*cursor) {
                let message_index = candidate.message_index;
                fork_conversation_at(state, cmds, message_index);
            }
        },
        KeyCode::Escape => {
            state.ui.mode = UiMode::EditingInput;
        },
        _ => {},
    }
}

/// Fork the session at `message_index` (a user message): the ORIGINAL
/// session is saved and preserved; a NEW session (new id, lineage stamped)
/// takes over with `messages[..message_index]` and the composer pre-filled
/// with the selected message — edit and resend to branch the timeline.
///
/// Idle-only by construction (the picker opens from an idle double-Esc), so
/// there is no in-flight TurnId or scope to reconcile when the id swaps.
/// Replace the render/persist copy of the checklist with the broker's
/// snapshot, firing `Cmd::NotifyTaskCompleted` for each task that flipped to
/// completed relative to the previous copy (a re-sent identical snapshot
/// fires nothing — the diff is the dedupe).
fn handle_tasks_updated(state: &mut State, cmds: &mut Vec<Cmd>, store: crate::domain::TaskStore) {
    let (completed, total) = store.counts();
    let fresh: Vec<crate::domain::TaskItem> = store
        .newly_completed(&state.session.conversation.tasks)
        .into_iter()
        .cloned()
        .collect();
    for task in fresh {
        cmds.push(Cmd::NotifyTaskCompleted {
            task,
            completed: completed as u32,
            total: total as u32,
        });
    }
    state.session.conversation.tasks = store;
    state.runtime.calls_since_task_update = 0;
}

/// `/todos` — the user's side of the checklist. Bare `/todos` prints the
/// list; edits route through `Cmd::UserTaskEdit` to the TaskBroker (single
/// writer), whose publish updates the band and whose notice tells the model.
fn handle_todos_command(state: &mut State, cmds: &mut Vec<Cmd>, arg: Option<&str>) {
    use crate::domain::UserTaskEdit;
    let arg = arg.unwrap_or("").trim();
    if arg.is_empty() {
        state
            .session
            .append(ChatMessage::system(todos_text(state)), state.now);
        return;
    }
    let (verb, rest) = arg.split_once(' ').unwrap_or((arg, ""));
    let rest = rest.trim();
    let parse_id =
        |rest: &str| -> Option<u32> { rest.strip_prefix('#').unwrap_or(rest).parse().ok() };
    let edit = match verb {
        "add" if !rest.is_empty() => Some(UserTaskEdit::Add {
            subject: rest.to_string(),
        }),
        "rm" | "remove" => parse_id(rest).map(|id| UserTaskEdit::Remove { id }),
        "done" => parse_id(rest).map(|id| UserTaskEdit::Done { id }),
        "clear" => Some(UserTaskEdit::Clear),
        _ => None,
    };
    match edit {
        Some(edit) => cmds.push(Cmd::UserTaskEdit(edit)),
        None => state.session.append(
            ChatMessage::system(
                "usage: /todos [add <subject> | rm <id> | done <id> | clear]".to_string(),
            ),
            state.now,
        ),
    }
}

/// The `/todos` listing: the full checklist with descriptions, cost stamps,
/// and recent evidence — the detail view the compact band doesn't show.
fn todos_text(state: &State) -> String {
    let store = &state.session.conversation.tasks;
    if store.is_empty() {
        return "No tasks. The model creates them for multi-step work, or add your own: \
                /todos add <subject>"
            .to_string();
    }
    let mut out = String::from("Task checklist\n");
    for task in store.visible() {
        out.push_str(&format!(
            "  #{} [{}] {}{}\n",
            task.id,
            task.status.as_str(),
            task.subject,
            if task.origin == crate::domain::TaskOrigin::User {
                " (you)"
            } else {
                ""
            }
        ));
        if let Some(desc) = &task.description {
            out.push_str(&format!("      {desc}\n"));
        }
        let mut cost = Vec::new();
        if let Some(secs) = task.elapsed_secs() {
            cost.push(format!("{secs}s"));
        }
        if let Some(tok) = task.tokens_spent {
            cost.push(format!("{tok} tokens"));
        }
        if !cost.is_empty() {
            out.push_str(&format!("      cost: {}\n", cost.join(" · ")));
        }
        for entry in task.evidence.iter().rev().take(3).rev() {
            out.push_str(&format!(
                "      evidence: {} {} ({})\n",
                entry.tool, entry.target, entry.status
            ));
        }
    }
    out.push_str(&format!("  {}", store.progress_string()));
    out
}

/// Cap on buffered checklist notices — same spirit as the hook-context cap;
/// notices are one-liners, so a small count bound suffices.
const MAX_TASK_NOTICES: usize = 8;

/// Buffer a checklist notice for the model's next request. NOT turn-gated:
/// `/todos` edits and vetoed completions arrive between turns and must
/// survive until the next real dispatch.
fn push_task_notice(state: &mut State, text: String) {
    if state.pending_task_notices.len() >= MAX_TASK_NOTICES {
        tracing::warn!("dropping task notice over the {MAX_TASK_NOTICES}-entry cap");
        return;
    }
    state.pending_task_notices.push(text);
}

/// How many model-call cycles an in_progress task may sit untouched before
/// the reducer injects a staleness nudge (then re-arms for another window).
const TASK_STALENESS_CALLS: u32 = 5;

fn fork_conversation_at(state: &mut State, cmds: &mut Vec<Cmd>, message_index: usize) {
    // 1. Persist the original FIRST — different id, different file, so this
    //    can never clobber the fork's save below.
    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));

    // File checkpoints anchored past the cut belong to the timeline being
    // discarded; ask the runtime store which exist (async — the reply arm
    // emits a /restore hint). Uses the ORIGINAL id: the fork's history is a
    // strict prefix, so anchors always reference the original session.
    cmds.push(Cmd::ListForkCheckpoints {
        session_id: state.session.conversation.id.clone(),
        message_index,
    });

    let original = &state.session.conversation;
    let original_id = original.id.clone();
    // 2. Mint the fork from the injected clock — the new id is a pure
    //    function of `state.now` (replay-exact; the `/clear` handler is the
    //    precedent).
    let mut fork = crate::session::ConversationHistory::new(
        original.project_path.clone(),
        original.model_name.clone(),
        state.now,
    );
    // Ids are millisecond-derived; a fork minted in the SAME millisecond as
    // the original would share its id — and its save file. Deterministic
    // 1ms bump keeps the fork pure while guaranteeing a distinct file.
    if fork.id == original.id {
        fork = crate::session::ConversationHistory::new(
            original.project_path.clone(),
            original.model_name.clone(),
            state.now + chrono::Duration::milliseconds(1),
        );
    }
    fork.title = original.title.clone();
    // The cut lands on a user message, which always starts a run, so the
    // prefix can't split a tool_use/tool_result pair — normalize anyway as
    // defense in depth.
    let mut messages: Vec<ChatMessage> = original.messages[..message_index].to_vec();
    super::compaction::normalize_history(&mut messages);
    fork.messages = messages;
    fork.input_history = original.input_history.clone();
    fork.git_branch = original.git_branch.clone();
    fork.git_sha = original.git_sha.clone();
    fork.cli_version = original.cli_version.clone();
    // Carried for provenance: the records' archive paths point at the
    // ORIGINAL session's archives (read-only history, never rewritten).
    fork.compactions = original.compactions.clone();
    // The payoff: the resume picker's "forked" chip and the `.meta` sidecar
    // light up from these two fields with zero render changes.
    fork.forked_from = Some(original_id.clone());
    fork.parent_session = Some(original_id);
    let selected = original.messages.get(message_index).cloned();

    // 3. Swap. Cumulative token meters continue (same spend, same session of
    //    work); context/last-usage described the dropped suffix — reset.
    // The fork starts with an empty checklist (a rewound plan describes
    // dropped work); the broker must forget it too or the next task tool
    // call would republish the stale list.
    state.session.conversation = fork;
    state.session.context_usage = None;
    state.session.last_token_usage = None;
    cmds.push(Cmd::SyncTaskStore(crate::domain::TaskStore::default()));
    // The fork minted a fresh conversation id, so it gets its own scratch
    // dir too — the original session's scratch contents describe work on
    // the timeline being discarded.
    refresh_scratchpad(state, cmds);

    // 4. `state.ids.image` is NOT re-based: the allocator stays monotonic, so
    //    image numbers remain unique across the fork (seed_conversation's
    //    re-base is for fresh processes only).

    // 5. Pre-fill the composer with the selected message. The pre-rewind
    //    draft and its staged attachments are deliberately dropped; the
    //    selected message's own images are RE-STAGED so its `[Image #N]`
    //    tokens resolve at submit.
    state.ui.attachments.clear();
    if let Some(msg) = selected {
        state.ui.input_buffer = msg.content.clone();
        state.ui.input_cursor = state.ui.input_buffer.len();
        if let (Some(images), Some(numbers)) = (&msg.images, &msg.image_numbers) {
            for (b64, number) in images.iter().zip(numbers) {
                restage_image(state, cmds, b64, *number);
            }
        }
    }

    // 6. Close the picker and save the fork. Edge: rewinding to the FIRST
    //    user message yields an empty prefix — `save_conversation` skips
    //    empty conversations, so the fork file materializes on first resend.
    state.ui.mode = UiMode::EditingInput;
    state.ui.esc_armed_at = None;
    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
    emit_title_if_changed(state, cmds);
}

/// Re-stage one of the selected message's images as a live attachment so its
/// inline `[Image #N]` token resolves at submit. Keeps the ORIGINAL number
/// (the token in the pre-filled text references it). A base64 decode failure
/// skips the attachment — the token degrades to literal text, matching how
/// orphan tokens behave today. The stored format isn't recorded per-image;
/// "png" is the decode-side fallback (viewers sniff the real magic bytes).
fn restage_image(state: &mut State, cmds: &mut Vec<Cmd>, base64_data: &str, number: u64) {
    let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64_data)
    else {
        return;
    };
    let id = state.ids.tool_call.next();
    let format = "png".to_string();
    let temp_path = state.temp_dir.join(format!("mermaid-img-{id}.{format}"));
    state.ui.attachments.push(super::state::Attachment {
        id,
        number,
        base64_data: base64_data.to_string(),
        temp_path: temp_path.clone(),
        size_bytes: bytes.len(),
        format: format.clone(),
    });
    cmds.push(Cmd::WriteImageToTemp {
        path: temp_path,
        bytes,
        format,
    });
}

/// The conversation id just changed (`/clear`, `/load`, rewind fork): drop
/// the old session's scratch dir handle and ask the effect layer to
/// materialize one for the new id. Pure — the directory creation happens in
/// the `EnsureScratchpad` handler, and `Msg::ScratchpadReady` stamps the
/// path back onto the session.
fn refresh_scratchpad(state: &mut State, cmds: &mut Vec<Cmd>) {
    state.session.scratchpad = None;
    cmds.push(Cmd::EnsureScratchpad {
        session_id: state.session.conversation.id.clone(),
    });
}

/// Handle keyboard input while the conversation-list picker is open.
/// Up/Down walk the cursor within the candidate list; Enter loads the
/// highlighted session; Esc dismisses.
fn handle_conversation_list_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode) {
    let UiMode::ConversationList {
        ref candidates,
        ref mut cursor,
    } = state.ui.mode
    else {
        return;
    };
    match code {
        KeyCode::Up => {
            *cursor = cursor.saturating_sub(1);
        },
        KeyCode::Down => {
            let max = candidates.len().saturating_sub(1);
            if *cursor < max {
                *cursor += 1;
            }
        },
        KeyCode::Enter => {
            if let Some(summary) = candidates.get(*cursor) {
                cmds.push(Cmd::LoadConversation(summary.id.clone()));
            }
            // Mode flips on `Msg::ConversationLoaded` — leave as-is
            // until then so the user sees the list until the load
            // completes.
        },
        KeyCode::Escape => {
            state.ui.mode = UiMode::EditingInput;
        },
        _ => {},
    }
}

/// Cap on ranked matches held in state (and shown by the widget's window).
const FILE_PICKER_MAX_MATCHES: usize = 50;

/// Re-rank `file_picker_matches` for the active @-token against the cached
/// project file list. Pure recompute — never fires a walk.
fn recompute_file_matches(state: &mut State) {
    let Some(token) = state.ui.active_file_token() else {
        state.ui.file_picker_matches.clear();
        state.ui.file_picker_cursor = None;
        return;
    };
    let query = state.ui.input_buffer[token.query_start..token.query_end].to_string();
    let files: &[String] = state.ui.project_files.as_deref().unwrap_or(&[]);
    state.ui.file_picker_matches =
        crate::domain::file_mention::fuzzy_rank(files, &query, FILE_PICKER_MAX_MATCHES);
    // Clamp (don't reset) the cursor so ↑/↓ position survives narrowing.
    let max = state.ui.file_picker_matches.len().saturating_sub(1);
    let cur = state.ui.file_picker_cursor.unwrap_or(0);
    state.ui.file_picker_cursor = Some(cur.min(max));
}

/// Token re-evaluation after a text mutation or cursor move: rank matches,
/// and on the CLOSED → OPEN transition fire a fresh project walk
/// (stale-while-revalidate — the user filters the cached list instantly and
/// the fresh list swaps in via `Msg::ProjectFilesListed`). The in-flight
/// flag dedupes: reopening while a walk runs never spawns a second one.
fn refresh_file_picker(state: &mut State, cmds: &mut Vec<Cmd>) {
    let was_open = state.ui.file_picker_cursor.is_some();
    recompute_file_matches(state);
    let is_open = state.ui.file_picker_cursor.is_some();
    if is_open && !was_open && !state.ui.project_files_loading {
        state.ui.project_files_loading = true;
        cmds.push(Cmd::ListProjectFiles);
    }
}

/// Complete the active @-token with the highlighted match: splice
/// `@<path> ` over `@<query>` and land the cursor after the space. The
/// trailing space closes the token, so the picker drops on its own.
fn complete_file_mention(state: &mut State) {
    let Some(token) = state.ui.active_file_token() else {
        return;
    };
    let sel = state.ui.file_picker_cursor.unwrap_or(0);
    let Some(path) = state.ui.file_picker_matches.get(sel) else {
        return;
    };
    let mention = format!("@{path} ");
    state
        .ui
        .input_buffer
        .replace_range(token.start..token.query_end, &mention);
    state.ui.input_cursor = token.start + mention.len();
    state.ui.file_picker_matches.clear();
    state.ui.file_picker_cursor = None;
}

/// Clamp a raw byte offset onto the nearest preceding char boundary
/// in `s`. Callers that trust their cursor is already valid can skip
/// this; paste + multi-step transformations should use it.
fn clamp_cursor(s: &str, pos: usize) -> usize {
    let capped = pos.min(s.len());
    s.floor_char_boundary(capped)
}

/// Step BACK through input history (Up arrow). The first press saves
/// the user's in-progress draft and replaces the buffer with the
/// newest history entry; subsequent presses step older.
fn history_nav_back(state: &mut State) {
    let history = &state.session.conversation.input_history;
    if history.is_empty() {
        return;
    }
    let next_cursor = match state.ui.input_history_cursor {
        None => {
            // First Up press — snapshot the current draft.
            state.ui.history_draft = state.ui.input_buffer.clone();
            0
        },
        Some(i) => (i + 1).min(history.len() - 1),
    };
    state.ui.input_history_cursor = Some(next_cursor);
    // `input_history` is a VecDeque with newest at the back. Index
    // 0 from the end = newest, 1 = one older, etc.
    let historical = history
        .iter()
        .rev()
        .nth(next_cursor)
        .cloned()
        .unwrap_or_default();
    state.ui.input_buffer = historical;
    state.ui.input_cursor = state.ui.input_buffer.len();
}

/// Step FORWARD through input history (Down arrow). Stepping past
/// the newest entry restores the user's original draft.
fn history_nav_forward(state: &mut State) {
    let Some(cursor) = state.ui.input_history_cursor else {
        return;
    };
    if cursor == 0 {
        // Back to the live draft.
        state.ui.input_buffer = std::mem::take(&mut state.ui.history_draft);
        state.ui.input_cursor = state.ui.input_buffer.len();
        state.ui.input_history_cursor = None;
        return;
    }
    let new_cursor = cursor - 1;
    state.ui.input_history_cursor = Some(new_cursor);
    let historical = state
        .session
        .conversation
        .input_history
        .iter()
        .rev()
        .nth(new_cursor)
        .cloned()
        .unwrap_or_default();
    state.ui.input_buffer = historical;
    state.ui.input_cursor = state.ui.input_buffer.len();
}

/// Cycle ReasoningLevel through every variant, wrapping around. Used
/// by Alt+T. Order matches the `Ord` impl so the cycle walks from
/// lowest to highest and back to None.
fn cycle_reasoning(current: crate::models::ReasoningLevel) -> crate::models::ReasoningLevel {
    use crate::models::ReasoningLevel as R;
    match current {
        R::None => R::Minimal,
        R::Minimal => R::Low,
        R::Low => R::Medium,
        R::Medium => R::High,
        R::High => R::XHigh,
        R::XHigh => R::Max,
        R::Max => R::None,
    }
}

/// Cycle SafetyMode by increasing permissiveness, wrapping around. Used by
/// Shift+Tab: ReadOnly → Ask → Auto → FullAccess → ReadOnly.
fn cycle_safety(current: crate::runtime::SafetyMode) -> crate::runtime::SafetyMode {
    use crate::runtime::SafetyMode as S;
    match current {
        S::ReadOnly => S::Ask,
        S::Ask => S::Auto,
        S::Auto => S::FullAccess,
        S::FullAccess => S::ReadOnly,
    }
}

/// Build and enqueue the submit for whatever is in the input buffer *right now*
/// — a slash command, or a prompt plus its staged attachments. Extracted from
/// the Enter handler so the paste-race guard can replay it verbatim once a
/// deferred clipboard read drains, re-deriving text + attachments (and thus
/// picking up a freshly-pasted image). No-op on empty/whitespace input.
fn submit_current_input(state: &mut State) {
    let buf = state.ui.input_buffer.trim().to_string();
    if buf.is_empty() {
        return;
    }
    if let Some(rest) = buf.strip_prefix('/') {
        // Plugin prompt commands: an enabled plugin's `/name args` expands
        // into a normal user prompt — the transcript shows the EXPANSION, so
        // recordings replay without the plugin installed. Built-ins always
        // win (the loader already refuses shadowing names; this order makes
        // it structural).
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((n, a)) => (n.to_lowercase(), a),
            None => (rest.to_lowercase(), ""),
        };
        let builtin = crate::domain::slash_commands::COMMAND_REGISTRY
            .iter()
            .any(|c| c.name == name || c.aliases.contains(&name.as_str()));
        if !builtin && let Some(cmd) = state.plugin_commands.iter().find(|c| c.name == name) {
            let text = cmd.expand(args);
            state.ui.input_buffer.clear();
            state.ui.input_cursor = 0;
            state.ui.palette_cursor = None;
            let attachment_ids: Vec<u64> = state.ui.attachments.iter().map(|a| a.id).collect();
            state.ui.pending_msgs.push_back(Msg::SubmitPrompt {
                text,
                attachment_ids,
            });
            return;
        }
        let slash = crate::app::event_source::parse_slash_command(rest);
        state.ui.input_buffer.clear();
        state.ui.input_cursor = 0;
        state.ui.palette_cursor = None;
        state.ui.pending_msgs.push_back(Msg::Slash(slash));
    } else {
        let text = std::mem::take(&mut state.ui.input_buffer);
        state.ui.input_cursor = 0;
        let attachment_ids: Vec<u64> = state.ui.attachments.iter().map(|a| a.id).collect();
        state.ui.pending_msgs.push_back(Msg::SubmitPrompt {
            text,
            attachment_ids,
        });
    }
}

/// Insert `text` at the input cursor and advance past it, resetting history-nav
/// and opening the slash palette if the buffer now starts with `/`. Shared by
/// terminal bracketed paste (`handle_paste`) and Ctrl+V text
/// (`handle_clipboard_read`) so the two agree on cursor handling.
fn insert_text_at_cursor(state: &mut State, text: &str) {
    // Insert at the cursor (not the end): on the Windows console a paste arrives
    // as a mix of coalesced `Paste` chunks and stray `Char` key events, and
    // appending here while keys insert at the cursor scrambled the result
    // (uppercase letters piled at the front).
    state.ui.input_history_cursor = None;
    state.ui.history_draft.clear();
    let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
    state.ui.input_buffer.insert_str(pos, text);
    state.ui.input_cursor = clamp_cursor(&state.ui.input_buffer, pos + text.len());
    if state.ui.input_buffer.starts_with('/') {
        state.ui.palette_cursor = Some(0);
    }
}

fn handle_paste(state: &mut State, paste: Paste) {
    // Terminal bracketed paste (and the Windows key-burst coalescer) is always
    // text; Ctrl+V clipboard reads — which can be images — arrive separately as
    // `Msg::ClipboardRead`.
    let Paste::Text(t) = paste;
    insert_text_at_cursor(state, &t);
}

/// A `Cmd::ReadClipboard` (Ctrl+V) has resolved. Release the pending-read
/// counter first — even on empty/error, so a submit held by the paste-race
/// guard is never wedged — then apply the outcome, and finally fire any held
/// submit once the last in-flight read has drained.
fn handle_clipboard_read(state: &mut State, cmds: &mut Vec<Cmd>, read: ClipboardRead) {
    state.ui.clipboard_reads_pending = state.ui.clipboard_reads_pending.saturating_sub(1);
    match read {
        ClipboardRead::Image { bytes, format } => {
            let id = state.ids.tool_call.next();
            let number = state.ids.fresh_image();
            let temp_path = state
                .temp_dir
                .join(format!("mermaid-img-{}.{}", id, format));
            // Splice the inline `[Image #N] ` token into the buffer at the
            // cursor — the token IS how the image lives in the message now, so
            // reset history-nav and advance past it.
            state.ui.input_history_cursor = None;
            state.ui.history_draft.clear();
            let token = crate::domain::image_token::render_token(number);
            let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
            state.ui.input_buffer.insert_str(pos, &token);
            state.ui.input_cursor = clamp_cursor(&state.ui.input_buffer, pos + token.len());
            state.ui.attachments.push(super::state::Attachment {
                id,
                number,
                base64_data: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &bytes,
                ),
                temp_path: temp_path.clone(),
                size_bytes: bytes.len(),
                format: format.clone(),
            });
            cmds.push(Cmd::WriteImageToTemp {
                path: temp_path,
                bytes,
                format,
            });
            // Proactively probe whether the current model can even see this
            // image, so a no-vision warning appears now — before you send —
            // rather than after a wasted turn.
            cmds.push(Cmd::ProbeVision {
                model_id: state.session.model_id.clone(),
                warn: true,
            });
        },
        ClipboardRead::Text(t) => {
            insert_text_at_cursor(state, &t);
        },
        ClipboardRead::Empty => {
            push_system(state, cmds, "Clipboard is empty");
        },
        ClipboardRead::Error(text) => {
            push_system(state, cmds, text);
        },
    }
    // Release a submit held by the paste-race guard once the last pending read
    // drains — re-deriving text + attachments so the freshly-pasted image is
    // included.
    if state.ui.clipboard_reads_pending == 0 && state.ui.submit_after_clipboard {
        state.ui.submit_after_clipboard = false;
        submit_current_input(state);
    }
}

/// Commit one user message to the conversation: resolve its `[Image #N]`
/// tokens against the attachments it owns, drop those attachments from the
/// staging area, append the `ChatMessage`, and record input history. Shared
/// by the idle submit path and the mid-run steering drain (tool-boundary
/// delivery of queued messages) so both commit with identical semantics.
fn commit_user_message(state: &mut State, text: String, attachment_ids: &[u64]) {
    if text.trim().is_empty() {
        return;
    }
    // Select images by the `[Image #N]` tokens present in the submitted text, in
    // first-appearance order — the inline tokens are the source of truth. Scope
    // by `attachment_ids` (the attachments this message owns) so the busy/queued
    // path can never grab a later message's image. `images[i]` and
    // `image_numbers[i]` stay parallel so the model correlates each image block
    // with its `[Image #N]` reference.
    let numbers = crate::domain::image_token::numbers_in_order(&text);
    let mut images: Vec<String> = Vec::new();
    let mut image_numbers: Vec<u64> = Vec::new();
    for n in &numbers {
        if let Some(a) = state
            .ui
            .attachments
            .iter()
            .find(|a| a.number == *n && attachment_ids.contains(&a.id))
        {
            images.push(a.base64_data.clone());
            image_numbers.push(*n);
        }
        // A token with no owned attachment (typed literal / mangled pill) stays
        // as plain text and simply sends no image.
    }
    // Drop every attachment this message owns — sent or orphaned — while keeping
    // any that belong to still-queued messages.
    state
        .ui
        .attachments
        .retain(|a| !attachment_ids.contains(&a.id));

    let mut user_msg = ChatMessage::user(text.clone());
    if !images.is_empty() {
        user_msg = user_msg
            .with_images(images)
            .with_image_numbers(image_numbers);
    }
    state.session.append(user_msg, state.now);
    state.session.conversation.add_to_input_history(text);
}

fn handle_submit_prompt(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    text: String,
    attachment_ids: &[u64],
) {
    if text.trim().is_empty() {
        return;
    }
    // If a turn is already in flight, queue this message. The
    // reducer's StreamDone arm pops the oldest queued message and
    // auto-submits it.
    if !matches!(state.turn, TurnState::Idle) {
        // Bound the queue: a user holding Enter during a long turn would
        // otherwise grow it without limit. Past the cap, drop the oldest queued
        // prompt (mirrors the `pending_msgs` drain cap).
        if state.ui.queued_messages.len() >= MAX_QUEUED_MESSAGES {
            state.ui.queued_messages.pop_front();
            tracing::warn!(
                max = MAX_QUEUED_MESSAGES,
                "reducer: queued_messages cap hit — dropped the oldest queued prompt"
            );
        }
        state
            .ui
            .queued_messages
            .push_back(super::state::QueuedMessage {
                text,
                attachment_ids: attachment_ids.to_vec(),
            });
        return;
    }

    commit_user_message(state, text, attachment_ids);
    state.ui.input_buffer.clear();

    // The first user message derives the conversation title; every
    // subsequent message keeps it. Either way, emit SetTerminalTitle
    // only on actual change.
    emit_title_if_changed(state, cmds);

    // Instructions/memory are kept fresh by the background config watcher (#45),
    // which stamps `state.instructions`/`state.memory` via
    // `Msg::InstructionsChanged`/`MemoryChanged`. The reducer reads them here as
    // injected data — no inline I/O — so `update()` stays pure and a recorded
    // session replays without re-statting the live filesystem.
    let turn = state.ids.fresh_turn();
    // Anchor the whole user interaction here. The agentic loop will mint fresh
    // `TurnId`s for each tool follow-up, but the spinner's elapsed + token
    // counters track this run start so they don't reset at every step.
    state.runtime.run_started = Some(std::time::SystemTime::from(state.now));
    state.runtime.run_tokens = super::runtime::RunTokenCounter::default();
    state.runtime.run_line_changes = super::runtime::RunLineChanges::default();
    // Fresh run — clear the truncation-recovery, empty-turn, and output-cap
    // continuation guards from any prior run so this intent gets a full retry
    // budget. (A run that *ended* at the continuation cap never hits the
    // in-stream reset, so without this the next run would start with a zero
    // continuation budget.)
    state.runtime.truncation_recoveries = 0;
    state.runtime.empty_continuations = 0;
    state.runtime.continue_recoveries = 0;
    state.turn = start_generating(turn, std::time::SystemTime::from(state.now));
    push_call_model(state, cmds, turn);
}

fn handle_slash(state: &mut State, cmds: &mut Vec<Cmd>, cmd: SlashCmd) {
    match cmd {
        SlashCmd::Model(None) => {
            push_system(
                state,
                cmds,
                format!("Current model: {}", state.session.model_id),
            );
        },
        SlashCmd::Model(Some(new_model)) => {
            let pull_target = ollama_pull_target(&new_model);
            state.session.model_id = new_model.clone();
            state.runtime.set_model(&new_model);
            // Refresh vision capability for the newly-selected model (set_model
            // reset the snapshot to a static default). Nag only if an image is
            // already staged — i.e. you switched TO a no-vision model with a
            // pending paste; otherwise this just keeps `/doctor` honest.
            cmds.push(Cmd::ProbeVision {
                model_id: state.session.model_id.clone(),
                warn: !state.ui.attachments.is_empty(),
            });
            // The bottom status bar shows the new model — no banner.
            cmds.push(Cmd::PersistLastModel(new_model));
            if let Some(model) = pull_target {
                cmds.push(Cmd::PullOllamaModel { model });
            }
        },
        SlashCmd::Reasoning(None) => {
            push_system(
                state,
                cmds,
                format!("Reasoning: {}", state.session.reasoning.as_str()),
            );
        },
        SlashCmd::Reasoning(Some(level)) => {
            state.session.reasoning = level;
            cmds.push(Cmd::PersistReasoningFor {
                model_id: state.session.model_id.clone(),
                level,
            });
        },
        SlashCmd::Safety(None) => {
            push_system(
                state,
                cmds,
                format!(
                    "Safety: {} — options: read_only, ask, auto, full_access (Shift+Tab cycles)",
                    state.session.safety_mode.as_str()
                ),
            );
        },
        SlashCmd::Safety(Some(mode)) => {
            // Session-scoped (mirrors Shift+Tab) — not written to the config.
            let previous = state.session.safety_mode;
            state.session.safety_mode = mode;
            // Leaving read_only past a stale denial nudges the model to
            // re-attempt (hidden). See the Shift+Tab handler.
            note_safety_mode_change(state, cmds, previous, mode);
            // Persist so `--resume`/`--continue` restore this mode (see the
            // Shift+Tab handler).
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
            // The bottom status bar shows the new mode — no banner.
        },
        SlashCmd::Plan(arg) => match arg.as_deref().map(str::trim) {
            None | Some("") | Some("on") => enter_plan_mode(state, cmds),
            Some("off") => exit_plan_mode(state, cmds),
            Some("show") => match &state.session.plan {
                Some(plan) => {
                    let path = plan_path_display(state, &plan.plan_path.clone());
                    push_system(state, cmds, format!("Plan file (drafting): {path}"));
                },
                None => push_system(state, cmds, "Not in plan mode (Alt+P or /plan enters it)"),
            },
            Some("config") => {
                state.ui.mode = UiMode::PlanConfig { cursor: 0 };
            },
            Some(_) => push_system(state, cmds, "Usage: /plan [off|show|config]"),
        },
        SlashCmd::Config => {
            state.ui.mode = UiMode::PlanConfig { cursor: 0 };
        },
        SlashCmd::VisibleReasoning(arg) => {
            match visible_reasoning_value(arg.as_deref(), state.ui.show_reasoning) {
                Ok(next) => {
                    state.ui.show_reasoning = next;
                    push_system(
                        state,
                        cmds,
                        if next {
                            "Visible reasoning: on"
                        } else {
                            "Visible reasoning: off"
                        },
                    );
                },
                Err(usage) => {
                    push_system(state, cmds, usage);
                },
            }
        },
        SlashCmd::Clear => {
            // Guard with a confirmation modal.
            state.confirm = Some(super::state::Confirmation {
                prompt: "Clear conversation history?".to_string(),
                accept_msg_token: super::state::ConfirmationTarget::ClearConversation,
            });
        },
        SlashCmd::Save(_name) => {
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        SlashCmd::Load(Some(id)) => {
            cmds.push(Cmd::LoadConversation(id));
        },
        SlashCmd::Load(None) | SlashCmd::List => {
            // Transition to the picker. Effect handler scans the
            // conversations directory; the reducer fills in
            // candidates when `Msg::ConversationsListed` arrives.
            state.ui.mode = UiMode::ConversationList {
                candidates: Vec::new(),
                cursor: 0,
            };
            cmds.push(Cmd::ListConversations);
        },
        SlashCmd::Usage => {
            state
                .session
                .append(ChatMessage::system(usage_text(state)), state.now);
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        SlashCmd::Todos(arg) => {
            handle_todos_command(state, cmds, arg.as_deref());
        },
        SlashCmd::Scratchpad => {
            // Listing needs the filesystem, so it runs as an effect; the
            // reducer only answers when there is no directory to list.
            match &state.session.scratchpad {
                Some(path) => cmds.push(Cmd::ListScratchpad { path: path.clone() }),
                None => {
                    state.session.append(
                        ChatMessage::system(
                            "No scratchpad yet for this session — it is created at startup \
                             and stamped shortly after; try again in a moment."
                                .to_string(),
                        ),
                        state.now,
                    );
                },
            }
        },
        SlashCmd::Context(cmd) => {
            use crate::domain::ContextCmd;
            let model_id = state.session.model_id.clone();
            let is_ollama = model_id.starts_with("ollama/");
            match cmd {
                ContextCmd::Show => {
                    state
                        .session
                        .append(ChatMessage::system(context_text(state)), state.now);
                    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
                },
                // The sizing knobs only affect Ollama's num_ctx.
                _ if !is_ollama => {
                    push_system(
                        state,
                        cmds,
                        format!(
                            "/context sizing applies to Ollama models; the active model is {model_id}."
                        ),
                    );
                },
                ContextCmd::Set(n) => {
                    state
                        .settings
                        .ollama_num_ctx_per_model
                        .insert(model_id.clone(), n);
                    cmds.push(Cmd::PersistOllamaNumCtxFor {
                        model_id,
                        num_ctx: Some(n),
                    });
                    push_system(
                        state,
                        cmds,
                        format!("Context window set to {n} tokens — applies to the next message."),
                    );
                },
                ContextCmd::Auto => {
                    state.settings.ollama_num_ctx_per_model.remove(&model_id);
                    // Also drop any auto-converged value so it re-fits from scratch.
                    state.runtime.ollama_converged_num_ctx.remove(&model_id);
                    cmds.push(Cmd::PersistOllamaNumCtxFor {
                        model_id,
                        num_ctx: None,
                    });
                    push_system(
                        state,
                        cmds,
                        "Context window back to auto-fit (sized to your GPU's VRAM) — applies to the next message.",
                    );
                },
                ContextCmd::Max => {
                    match state
                        .runtime
                        .ollama_context
                        .as_ref()
                        .and_then(|c| c.model_max)
                    {
                        Some(max) => {
                            let max_u32 = max.min(u32::MAX as usize) as u32;
                            state
                                .settings
                                .ollama_num_ctx_per_model
                                .insert(model_id.clone(), max_u32);
                            cmds.push(Cmd::PersistOllamaNumCtxFor {
                                model_id,
                                num_ctx: Some(max_u32),
                            });
                            push_system(
                                state,
                                cmds,
                                format!(
                                    "Context window set to the model's max ({max} tokens) — applies to the next message. \
                                     This may exceed VRAM; if it gets slow, enable `/context offload on`."
                                ),
                            );
                        },
                        None => {
                            push_system(
                                state,
                                cmds,
                                "Model's max window isn't known yet — send a message first, then `/context max`.",
                            );
                        },
                    }
                },
                ContextCmd::Offload(on) => {
                    state.settings.ollama.allow_ram_offload = on;
                    cmds.push(Cmd::PersistOllamaOffload(on));
                    push_system(
                        state,
                        cmds,
                        format!(
                            "RAM offload {} — applies to the next message. {}",
                            if on { "enabled" } else { "disabled" },
                            if on {
                                "Larger context windows are allowed, but inference may be much slower."
                            } else {
                                "Context auto-fits to VRAM to stay fast."
                            }
                        ),
                    );
                },
            }
        },
        SlashCmd::Compact(instructions) => {
            handle_manual_compact(state, cmds, instructions);
        },
        SlashCmd::Memory => {
            cmds.push(Cmd::ListMemory);
        },
        SlashCmd::Remember(Some(text)) => {
            cmds.push(Cmd::RememberMemory { text });
        },
        SlashCmd::Remember(None) => {
            push_system(state, cmds, "Usage: /remember <fact to remember>");
        },
        SlashCmd::Forget(Some(id)) => {
            cmds.push(Cmd::ForgetMemory { id });
        },
        SlashCmd::Forget(None) => {
            push_system(
                state,
                cmds,
                "Usage: /forget <memory name> (see /memory for names)",
            );
        },
        SlashCmd::ConsolidateMemory => {
            cmds.push(Cmd::ConsolidateMemory {
                model_id: state.session.model_id.clone(),
            });
        },
        SlashCmd::Doctor => {
            state
                .session
                .append(ChatMessage::system(doctor_text(state)), state.now);
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        SlashCmd::Tasks => {
            cmds.push(Cmd::ListRuntimeTasks { limit: 10 });
        },
        SlashCmd::Task(Some(id)) => {
            cmds.push(Cmd::LoadRuntimeTask { id });
        },
        SlashCmd::Task(None) => {
            push_system(state, cmds, "Usage: /task <id>");
        },
        SlashCmd::Pause(Some(id)) => {
            cmds.push(Cmd::UpdateRuntimeTaskStatus {
                id,
                status: TaskStatus::Blocked,
                final_report: Some("Paused from TUI".to_string()),
            });
        },
        SlashCmd::Pause(None) => {
            push_system(state, cmds, "Usage: /pause <task-id>");
        },
        SlashCmd::Resume(Some(id)) => {
            cmds.push(Cmd::UpdateRuntimeTaskStatus {
                id,
                status: TaskStatus::Running,
                final_report: None,
            });
        },
        SlashCmd::Resume(None) => {
            push_system(state, cmds, "Usage: /resume <task-id>");
        },
        SlashCmd::Cancel(Some(id)) => {
            cmds.push(Cmd::UpdateRuntimeTaskStatus {
                id,
                status: TaskStatus::Cancelled,
                final_report: Some("Cancelled from TUI".to_string()),
            });
        },
        SlashCmd::Cancel(None) => {
            if matches!(state.turn, TurnState::Idle) {
                push_system(state, cmds, "No active turn to cancel.");
            } else {
                handle_cancel_turn(state, cmds);
            }
        },
        SlashCmd::Handoff(Some(id)) | SlashCmd::Report(Some(id)) => {
            cmds.push(Cmd::LoadRuntimeTask { id });
        },
        SlashCmd::Handoff(None) => {
            let text = format!(
                "Handoff report\n\n{}\n\n{}",
                context_text(state),
                usage_text(state)
            );
            state.session.append(ChatMessage::system(text), state.now);
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        SlashCmd::Report(None) => {
            let text = format!(
                "Runtime report\n\n{}\n\n{}",
                context_text(state),
                usage_text(state)
            );
            state.session.append(ChatMessage::system(text), state.now);
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        SlashCmd::Processes => {
            cmds.push(Cmd::ListRuntimeProcesses { limit: 10 });
        },
        SlashCmd::Agents(arg) => {
            handle_slash_agents(state, cmds, arg.as_deref());
        },
        SlashCmd::Logs(Some(id)) => {
            cmds.push(Cmd::ShowRuntimeProcessLogs { id });
        },
        SlashCmd::Logs(None) => {
            push_system(state, cmds, "Usage: /logs <process-id>");
        },
        SlashCmd::Stop(Some(id)) => {
            cmds.push(Cmd::StopRuntimeProcess { id });
        },
        SlashCmd::Stop(None) => {
            push_system(state, cmds, "Usage: /stop <process-id>");
        },
        SlashCmd::Restart(Some(id)) => {
            cmds.push(Cmd::RestartRuntimeProcess { id });
        },
        SlashCmd::Restart(None) => {
            push_system(state, cmds, "Usage: /restart <process-id>");
        },
        SlashCmd::Open(Some(target)) => {
            cmds.push(Cmd::OpenRuntimeTarget { target });
        },
        SlashCmd::Open(None) => {
            push_system(state, cmds, "Usage: /open <url|path|process-id>");
        },
        SlashCmd::Ports => {
            cmds.push(Cmd::ShowRuntimePorts);
        },
        SlashCmd::Approvals => {
            cmds.push(Cmd::ListRuntimeApprovals);
        },
        SlashCmd::Approve(Some(id)) => {
            cmds.push(Cmd::DecideRuntimeApproval {
                id,
                decision: "approved".to_string(),
            });
        },
        SlashCmd::Approve(None) => {
            push_system(state, cmds, "Usage: /approve <approval-id>");
        },
        SlashCmd::Deny(Some(id)) => {
            cmds.push(Cmd::DecideRuntimeApproval {
                id,
                decision: "denied".to_string(),
            });
        },
        SlashCmd::Deny(None) => {
            push_system(state, cmds, "Usage: /deny <approval-id>");
        },
        SlashCmd::Checkpoint(Some(paths)) => {
            let paths = paths
                .split_whitespace()
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>();
            cmds.push(Cmd::CreateRuntimeCheckpoint { paths });
        },
        SlashCmd::Checkpoint(None) => {
            push_system(state, cmds, "Usage: /checkpoint <path...>");
        },
        SlashCmd::Checkpoints => {
            cmds.push(Cmd::ListRuntimeCheckpoints { limit: 10 });
        },
        SlashCmd::Restore(Some(id)) => {
            cmds.push(Cmd::RestoreRuntimeCheckpoint { id });
        },
        SlashCmd::Restore(None) => {
            push_system(state, cmds, "Usage: /restore <checkpoint-id>");
        },
        SlashCmd::Plugins => {
            cmds.push(Cmd::ListRuntimePlugins);
        },
        SlashCmd::ModelInfo(Some(model)) => {
            cmds.push(Cmd::ShowRuntimeModelInfo { model });
        },
        SlashCmd::ModelInfo(None) => {
            push_system(state, cmds, "Usage: /model-info <model>");
        },
        SlashCmd::CloudSetup => {
            // Cloud setup needs interactive stdin (rpassword) which
            // fights with ratatui's raw mode. The in-TUI command
            // points users at the `mermaid cloud-setup` subcommand
            // instead — clean separation of modes.
            push_system(
                state,
                cmds,
                "Run `mermaid cloud-setup` from your shell, then restart mermaid.",
            );
        },
        SlashCmd::Theme(arg) => {
            use crate::app::ThemeChoice;
            // The trailing NO_COLOR note keeps a persisted-but-invisible
            // switch from reading as a broken command.
            let no_color_note = if state.ui.no_color {
                " NO_COLOR is set, so colors stay disabled until it is unset."
            } else {
                ""
            };
            let choice = match arg.as_deref().map(str::trim) {
                None | Some("") => {
                    push_system(
                        state,
                        cmds,
                        format!(
                            "Theme: {}. Usage: /theme <dark|light>.{}",
                            state.ui.theme.as_str(),
                            no_color_note
                        ),
                    );
                    return;
                },
                Some("dark") => ThemeChoice::Dark,
                Some("light") => ThemeChoice::Light,
                Some(other) => {
                    push_system(
                        state,
                        cmds,
                        format!("Unknown theme '{}'. Usage: /theme <dark|light>", other),
                    );
                    return;
                },
            };
            state.ui.theme = choice;
            cmds.push(Cmd::PersistUiTheme(choice));
            push_system(
                state,
                cmds,
                format!(
                    "Theme set to {} (persisted).{}",
                    choice.as_str(),
                    no_color_note
                ),
            );
        },
        SlashCmd::Editor => {
            // `/editor` opens on whatever draft remains after the command
            // itself was consumed (usually empty); Ctrl+O is the
            // draft-preserving path.
            cmds.push(Cmd::ComposeInEditor {
                text: state.ui.input_buffer.clone(),
            });
        },
        SlashCmd::Help => {
            state.session.append(
                ChatMessage::system(help_text(&state.plugin_commands)),
                state.now,
            );
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
        },
        SlashCmd::Quit => {
            request_exit(state, cmds);
        },
        SlashCmd::Unknown(name) => {
            push_system(state, cmds, format!("Unknown command: /{}", name));
        },
    }
}

/// `/agents` — list detached background agents, or kill them
/// (`kill <id>` / `kill all`). Kills validate against the registry here
/// (immediate feedback for a bad id), mark the row "cancelling…", and hand
/// the actual token fire to the effect layer via `Cmd::KillBackgroundAgent`;
/// the dying child's `Msg::BackgroundAgentFinished { cancelled: true, .. }`
/// posts the closing note and clears the row.
fn handle_slash_agents(state: &mut State, cmds: &mut Vec<Cmd>, arg: Option<&str>) {
    let arg = arg.map(str::trim).filter(|s| !s.is_empty());
    match arg {
        None => {
            if state.runtime.background_agents.is_empty() {
                push_system(
                    state,
                    cmds,
                    "No background agents. (ctrl+b detaches a running agent)",
                );
                return;
            }
            let now_sys = std::time::SystemTime::from(state.now);
            let mut lines = vec![format!(
                "Background agents ({}) — /agents kill <id> cancels one, /agents kill all cancels every one",
                state.runtime.background_agents.len()
            )];
            for agent in &state.runtime.background_agents {
                let elapsed = now_sys
                    .duration_since(agent.started)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                lines.push(format!(
                    "  {}  {} — {} · {}s · ~{} tokens",
                    agent.agent_id,
                    agent.description,
                    agent.activity,
                    elapsed,
                    super::compaction::format_compact_count(agent.tokens),
                ));
            }
            push_system(state, cmds, lines.join("\n"));
        },
        Some("kill all") => {
            if state.runtime.background_agents.is_empty() {
                push_system(state, cmds, "No background agents to kill.");
                return;
            }
            for agent in &mut state.runtime.background_agents {
                agent.activity = "cancelling…".to_string();
            }
            cmds.push(Cmd::KillBackgroundAgent { agent_id: None });
        },
        Some(rest) if rest.starts_with("kill ") || rest == "kill" => {
            let id = rest.strip_prefix("kill").unwrap_or_default().trim();
            if id.is_empty() {
                push_system(
                    state,
                    cmds,
                    "Usage: /agents kill <id> (or: /agents kill all)",
                );
                return;
            }
            let Some(agent) = state
                .runtime
                .background_agents
                .iter_mut()
                .find(|a| a.agent_id == id)
            else {
                push_system(state, cmds, format!("No background agent '{id}'."));
                return;
            };
            agent.activity = "cancelling…".to_string();
            cmds.push(Cmd::KillBackgroundAgent {
                agent_id: Some(id.to_string()),
            });
        },
        Some(_) => {
            push_system(
                state,
                cmds,
                "Usage: /agents (list), /agents kill <id>, /agents kill all",
            );
        },
    }
}

fn visible_reasoning_value(arg: Option<&str>, current: bool) -> Result<bool, &'static str> {
    match arg.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("toggle") => Ok(!current),
        Some("on") | Some("true") | Some("yes") | Some("show") => Ok(true),
        Some("off") | Some("false") | Some("no") | Some("hide") => Ok(false),
        Some(_) => Err("Usage: /visible-reasoning [on|off|toggle]"),
    }
}

/// Append a one-off system note to the chat transcript (and persist it).
///
/// This is where command feedback, errors, and query answers go now that the
/// transient status banner above the input is gone — they live in the
/// scrollable transcript instead of flashing in the spinner's row. The zone
/// above the input is reserved for the generation spinner alone.
fn push_system(state: &mut State, cmds: &mut Vec<Cmd>, text: impl Into<String>) {
    push_system_kind(state, cmds, text, crate::models::ChatMessageKind::Normal);
}

/// `push_system` with an explicit message kind. The recovery tails use it to
/// stamp their one-shot nudges `RecoveryNudge` so the transcript hides them
/// and `sweep_spent_nudges` retires them at the next turn-end.
fn push_system_kind(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    text: impl Into<String>,
    kind: crate::models::ChatMessageKind,
) {
    // While tools are mid-flight the trailing message is the committed
    // `assistant(tool_calls)` whose `tool` results haven't landed yet. Appending
    // a system note *after* it wedges a message between the `tool_use` and its
    // `tool_result` — which OpenAI- and Ollama-shaped providers reject on the
    // next request (Anthropic and Gemini happen to drop mid-history system
    // messages, but we can't lean on that for every backend). Insert the note
    // just *before* that assistant message so the pair stays adjacent; as a
    // bonus the assistant message stays last, so in-flight tool actions/images
    // still attach to it. Anywhere else, plain append.
    let messages = &state.session.conversation.messages;
    // Also guard `Compacting`: a `ContextLimitRetry`/`TruncationRecovery`
    // compaction keeps a trailing unpaired `tool_use` (see `preserve_pending_tail`),
    // so a mid-compaction `push_system` (e.g. `McpServerErrored`) must insert
    // before it too — otherwise the next request wedges a system note between the
    // `tool_use` and its `tool_result`.
    let would_split = matches!(
        state.turn,
        TurnState::ExecutingTools { .. } | TurnState::Compacting { .. }
    ) && messages
        .last()
        .is_some_and(|m| m.role == MessageRole::Assistant && m.tool_calls.is_some());
    let mut msg = ChatMessage::system(text.into());
    msg.kind = kind;
    if would_split {
        let pos = messages.len() - 1;
        state.session.conversation.messages.insert(pos, msg);
    } else {
        state.session.append(msg, state.now);
    }
    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
}

/// Retire spent recovery nudges. A `RecoveryNudge` steers exactly one request
/// (auto-continue resume, stalled-turn retry); by the time any turn-end
/// arrives that request has already gone out, so the note is dead weight —
/// worse, if it lingered it would keep instructing the model on the user's
/// *next, unrelated* turn while the transcript hides it. Returns whether
/// anything was removed so callers on paths without a save can persist.
fn sweep_spent_nudges(state: &mut State) -> bool {
    let messages = &mut state.session.conversation.messages;
    let before = messages.len();
    messages.retain(|m| m.kind != crate::models::ChatMessageKind::RecoveryNudge);
    messages.len() != before
}

fn ollama_pull_target(model_id: &str) -> Option<String> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }
    let (provider, model) = match model_id.split_once('/') {
        Some((provider, model)) => (provider, model),
        None => ("ollama", model_id),
    };
    if !provider.eq_ignore_ascii_case("ollama") {
        return None;
    }
    let model = model.trim();
    if model.is_empty() || model.ends_with(":cloud") {
        None
    } else {
        Some(model.to_string())
    }
}

fn handle_manual_compact(state: &mut State, cmds: &mut Vec<Cmd>, instructions: Option<String>) {
    if !matches!(state.turn, TurnState::Idle) {
        push_system(state, cmds, "Cannot compact while a turn is active.");
        return;
    }

    if state.session.messages().len() < 3 {
        push_system(state, cmds, "Not enough conversation history to compact.");
        return;
    }

    // Instructions/memory are kept fresh by the config watcher (#45); read as
    // injected data so the reducer does no I/O before building the request.
    let turn = state.ids.fresh_turn();
    state.turn = TurnState::Compacting {
        id: turn,
        started: std::time::SystemTime::from(state.now),
        trigger: CompactionTrigger::Manual,
        resume_continuation: false,
    };
    // The live "Compacting…" status comes from the TurnState::Compacting status
    // line (the blue indicator); no separate gray status message — it was a
    // redundant duplicate. The completion receipt is set on CompactionFinished.
    // An explicit /compact is the user's retry lever: un-pause auto-compaction
    // so this attempt (and later turns) get a fresh shot.
    state.runtime.auto_compact_suppressed = false;
    cmds.push(Cmd::CompactConversation {
        turn,
        request: CompactionRequest::manual(build_chat_request(state), instructions),
    });
}

fn help_text(plugin_commands: &[crate::domain::PluginCommand]) -> String {
    let mut lines = Vec::with_capacity(COMMAND_REGISTRY.len() + COMMAND_GROUPS.len() + 2);
    lines.push("Mermaid commands".to_string());
    lines.push(
        "Everyday commands are first; daemon/task/process commands are advanced runtime."
            .to_string(),
    );
    for group in COMMAND_GROUPS {
        lines.push(String::new());
        lines.push(format!("{}:", group.title()));
        for command in COMMAND_REGISTRY
            .iter()
            .filter(|command| command.group == *group)
        {
            let hint = command.arg_hint.unwrap_or("");
            let aliases = if command.aliases.is_empty() {
                String::new()
            } else {
                format!(" ({})", command.aliases.join(", "))
            };
            let suffix = if hint.is_empty() {
                String::new()
            } else {
                format!(" {}", hint)
            };
            lines.push(format!(
                "  /{}{}{} - {}",
                command.name, suffix, aliases, command.description
            ));
        }
    }
    if !plugin_commands.is_empty() {
        lines.push(String::new());
        lines.push("Plugin commands:".to_string());
        for cmd in plugin_commands {
            lines.push(format!(
                "  /{} - {} (plugin:{})",
                cmd.name,
                if cmd.description.is_empty() {
                    "prompt"
                } else {
                    &cmd.description
                },
                cmd.plugin
            ));
        }
    }
    lines.push(String::new());
    lines.push("Keyboard shortcuts:".to_string());
    for (keys, desc) in crate::domain::slash_commands::KEYBINDINGS {
        lines.push(format!("  {keys} - {desc}"));
    }
    lines.join("\n")
}

fn doctor_text(state: &State) -> String {
    let mut lines = Vec::new();
    lines.push("Mermaid Doctor".to_string());
    lines.push(format!("Project: {}", state.cwd.display()));
    lines.push(format!("Active model: {}", state.session.model_id));
    lines.push(format!("Reasoning: {}", state.session.reasoning.as_str()));
    lines.push(format!(
        "Provider: {} / {}",
        state.runtime.provider_capabilities.provider, state.runtime.provider_capabilities.model
    ));
    lines.push(format!(
        "Model capabilities: tools={}, vision={}, reasoning={}, context={}",
        state.runtime.provider_capabilities.supports_tools,
        state.runtime.provider_capabilities.supports_vision,
        state.runtime.provider_capabilities.reasoning,
        state
            .runtime
            .provider_capabilities
            .max_context_tokens
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    ));
    lines.push(format!(
        "Safety: mode={}, checkpoint_on_mutation={}",
        state.settings.safety.mode.as_str(),
        state.settings.safety.checkpoint_on_mutation
    ));
    lines.push(format!(
        "Scratchpad: {}",
        state
            .session
            .scratchpad
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not ready".to_string())
    ));
    lines.push(format!(
        "Prompt: {}",
        if state.settings.prompt.is_customized() {
            "customized for this invocation"
        } else {
            "default"
        }
    ));
    match &state.instructions {
        Some(instructions) => lines.push(format!(
            "Project instructions: {} bytes from {} source(s){}",
            instructions.byte_len,
            instructions.sources.len(),
            if instructions.truncated {
                " (truncated)"
            } else {
                ""
            }
        )),
        None => lines.push("Project instructions: none loaded (AGENTS.md, MERMAID.md)".to_string()),
    }
    lines.push(format!(
        "MCP servers: {} configured, {} ready",
        state.mcp.servers.len(),
        state
            .mcp
            .servers
            .values()
            .filter(|entry| matches!(entry.status, crate::domain::McpServerStatus::Ready))
            .count()
    ));
    lines.push(
        "Useful next commands: /help, /context, /model-info <model>, /compact [focus]".to_string(),
    );
    lines.join("\n")
}

/// The most recent user message, trimmed and length-capped — used as the
/// Auto-mode classifier's "what is the user trying to do" context. `None`
/// when the session has no user message yet.
fn latest_user_intent(session: &super::state::Session) -> Option<String> {
    const MAX: usize = 2000;
    session
        .messages()
        .iter()
        .rev()
        .find(|m| matches!(m.role, crate::models::MessageRole::User))
        .map(|m| {
            let c = m.content.trim();
            if c.len() > MAX {
                format!("{}…", &c[..c.floor_char_boundary(MAX)])
            } else {
                c.to_string()
            }
        })
}

fn usage_text(state: &State) -> String {
    let mut lines = Vec::new();
    lines.push("Usage".to_string());
    lines.push(format!("Model: {}", state.session.model_id));
    lines.push(String::new());

    match &state.session.context_usage {
        Some(context) => {
            let source = if context.is_estimate() {
                "estimated"
            } else {
                "provider-reported"
            };
            lines.push(format!(
                "Current context: {}{}{}",
                format_compact_count(context.used_tokens),
                context
                    .max_tokens
                    .map(|max| format!(" / {}", format_compact_count(max)))
                    .unwrap_or_else(|| " / unknown".to_string()),
                context
                    .used_percent
                    .map(|p| format!(" ({}%, {})", p, source))
                    .unwrap_or_else(|| format!(" ({})", source))
            ));
        },
        None => lines.push("Current context: n/a".to_string()),
    }

    match state.session.last_token_usage {
        Some(last) => lines.push(format!("Last API request: {}", usage_totals_line(last))),
        None => lines.push("Last API request: n/a".to_string()),
    }
    // Cumulative is a cost-accounting sum: every API call re-sends the
    // growing conversation, so input dwarfs output and the total grows much
    // faster than the context gauge — label it so that reads as intended.
    lines.push(format!(
        "Session cumulative (all API calls, subagents included): {}",
        usage_totals_line(state.session.cumulative_token_usage)
    ));

    lines.join("\n")
}

fn context_text(state: &State) -> String {
    let mut lines = Vec::new();
    lines.push("Context".to_string());
    lines.push(format!("Model: {}", state.session.model_id));
    lines.push(format!(
        "Provider: {}",
        state.runtime.provider_capabilities.provider
    ));
    lines.push(String::new());

    let request = build_chat_request(state);
    let max_context = state
        .session
        .context_usage
        .as_ref()
        .and_then(|snapshot| snapshot.max_tokens)
        .or(state.runtime.provider_capabilities.max_context_tokens);
    // The request here carries MCP tools only; the effect runner appends the
    // built-in tool schemas during dispatch. Fold their estimated cost in so
    // the verdict/hard-limit lines agree with what dispatch actually decides.
    let next_snapshot = super::state::estimate_context_usage_for_request(&request, max_context)
        .with_additional_tokens(state.runtime.builtin_tool_schema_tokens);

    // Ollama auto-sizing detail (probed on the first turn; `source` is `Some`
    // only for Ollama). Shows the real window, what we send as num_ctx, the
    // output budget, and the offload mode — so users can see + tune sizing.
    if let Some(ctx) = state
        .runtime
        .ollama_context
        .as_ref()
        .filter(|c| c.source.is_some())
    {
        if let Some(model_max) = ctx.model_max {
            lines.push(format!(
                "Model max window: {}",
                format_compact_count(model_max)
            ));
        }
        if let Some(eff) = ctx.effective {
            // An auto-converged value rides the override path internally, but it's
            // Mermaid's choice (not the user's) — label it honestly.
            let model = &state.session.model_id;
            let src = if state.runtime.ollama_converged_num_ctx.contains_key(model)
                && !state.settings.ollama_num_ctx_per_model.contains_key(model)
            {
                "auto (GPU-fit)"
            } else {
                ctx.source.map(|s| s.label()).unwrap_or("auto")
            };
            lines.push(format!(
                "Active num_ctx: {} ({src})",
                format_compact_count(eff)
            ));
        }
        let num_predict = crate::models::adapters::ollama_sizing::default_ollama_num_predict(
            request.max_tokens,
            ctx.effective,
            next_snapshot.used_tokens,
            state.runtime.provider_capabilities.max_output_tokens,
        );
        lines.push(format!(
            "Output budget (num_predict): {}",
            match num_predict {
                Some(n) => format_compact_count(n as usize),
                None => "auto (provider default)".to_string(),
            }
        ));
        lines.push(format!(
            "RAM offload: {} (toggle with /context offload on|off)",
            if state.settings.ollama.allow_ram_offload {
                "on"
            } else {
                "off"
            }
        ));
        // If auto-fit capped well below the model's max, point to the override.
        if let (Some(model_max), Some(eff), Some(src)) = (ctx.model_max, ctx.effective, ctx.source)
            && src.is_auto()
            && model_max > eff
        {
            lines.push(format!(
                "Tip: this model supports up to {} — `/context max` for the full window, or `/context <n>`.",
                format_compact_count(model_max)
            ));
        }
        // Real memory placement once a turn has probed `/api/ps`.
        if let Some(p) = state.runtime.ollama_placement.as_ref() {
            if p.offloaded() {
                lines.push(format!(
                    "GPU placement: ~{}% on CPU/RAM (slower) — `/context <n>` to shrink or `/context offload on` to accept",
                    p.percent_on_cpu()
                ));
            } else {
                lines.push("GPU placement: fully on GPU".to_string());
            }
        }
        lines.push(String::new());
    }

    let policy = CompactionPolicy::default();
    let response_reserve = policy.response_reserve(&request);
    let usage_summary = match (next_snapshot.used_percent, next_snapshot.max_tokens) {
        (Some(percent), Some(_)) if percent >= policy.auto_threshold_percent => {
            format!("high ({percent}% used)")
        },
        (Some(percent), Some(_)) if percent >= 70 => format!("getting full ({percent}% used)"),
        (Some(percent), Some(_)) => format!("comfortable ({percent}% used)"),
        _ => "unknown because provider context limit is unknown".to_string(),
    };

    lines.push(format!("Context fullness: {usage_summary}"));
    lines.push(format!(
        "Next request: {}{} (estimated)",
        format_compact_count(next_snapshot.used_tokens),
        next_snapshot
            .max_tokens
            .map(|max| format!(" / {}", format_compact_count(max)))
            .unwrap_or_else(|| " / unknown".to_string())
    ));
    if let Some(remaining) = next_snapshot.remaining_tokens {
        lines.push(format!(
            "Remaining after request: {}",
            format_compact_count(remaining)
        ));
    }
    lines.push(format!(
        "Response reserve: {}",
        format_compact_count(response_reserve)
    ));
    lines.push(format!(
        "Auto compact threshold: {}%",
        policy.auto_threshold_percent
    ));
    let auto_skip = should_auto_compact(&next_snapshot, &request, policy);
    let auto_status = match &auto_skip {
        Ok(()) => "would run before the next model call".to_string(),
        // Paused is not "not needed" — the threshold may well be exceeded;
        // the pause is the reason it won't run.
        Err(reason @ crate::domain::compaction::CompactionSkip::Suppressed) => reason.to_string(),
        Err(reason) => format!("not needed ({reason})"),
    };
    lines.push(format!("Auto compact: {auto_status}"));
    match &auto_skip {
        Ok(()) => lines.push(
            "Suggested action: continue normally; Mermaid will compact before the next model call."
                .to_string(),
        ),
        Err(crate::domain::compaction::CompactionSkip::Suppressed) => lines.push(
            "Suggested action: run /compact to checkpoint now and re-enable automatic compaction."
                .to_string(),
        ),
        Err(_) => lines.push("Suggested action: no manual compaction needed unless you want a handoff checkpoint now.".to_string()),
    }
    lines.push(format!(
        "Hard limit risk: {}",
        if context_exceeds_hard_limit(&next_snapshot, &request, policy) {
            "yes"
        } else {
            "no"
        }
    ));

    if let Some(context) = &state.session.context_usage {
        let source = if context.is_estimate() {
            "estimated"
        } else {
            "provider-reported"
        };
        lines.push(format!(
            "Last reported context: {}{} ({})",
            format_compact_count(context.used_tokens),
            context
                .max_tokens
                .map(|max| format!(" / {}", format_compact_count(max)))
                .unwrap_or_else(|| " / unknown".to_string()),
            source
        ));
    }

    if let Some(breakdown) = &next_snapshot.breakdown {
        lines.push(String::new());
        lines.push("Prompt budget estimate:".to_string());
        lines.push(format!(
            "- system prompt: {}",
            format_compact_count(breakdown.system_tokens)
        ));
        lines.push(format!(
            "- instructions: {}",
            format_compact_count(breakdown.instructions_tokens)
        ));
        lines.push(format!(
            "- messages ({}): {}",
            breakdown.message_count,
            format_compact_count(breakdown.message_tokens)
        ));
        lines.push(format!(
            "- MCP tool schemas ({}): {}",
            breakdown.tool_count,
            format_compact_count(breakdown.tool_schema_tokens)
        ));
        if state.runtime.builtin_tool_schema_tokens > 0 {
            lines.push(format!(
                "- built-in tool schemas: {}",
                format_compact_count(state.runtime.builtin_tool_schema_tokens)
            ));
        } else {
            lines.push("- built-in tool schemas: measured on the first model call".to_string());
        }
        if breakdown.image_count > 0 {
            lines.push(format!("- images: {}", breakdown.image_count));
        }
    }

    if let Some(last) = state.session.conversation.compactions.last() {
        lines.push(String::new());
        lines.push("Last compaction:".to_string());
        lines.push(format!("- trigger: {}", last.trigger.label()));
        lines.push(format!(
            "- context: {} -> {} tokens",
            format_compact_count(last.before_tokens),
            format_compact_count(last.after_tokens)
        ));
        lines.push(format!(
            "- archived: {} messages",
            last.archived_message_count
        ));
        lines.push(format!(
            "- preserved: {} messages",
            last.preserved_message_count
        ));
        lines.push(format!(
            "- review: {}",
            match last.review_status {
                crate::domain::CompactionReviewStatus::Reviewed => "reviewed".to_string(),
                crate::domain::CompactionReviewStatus::DraftValidated => last
                    .review_error
                    .as_ref()
                    .map(|err| format!("validated draft ({err})"))
                    .unwrap_or_else(|| "validated draft".to_string()),
            }
        ));
        if let Some(path) = &last.archive_path {
            lines.push(format!("- archive: {}", path));
        }
        lines.push("- inspect: use the archive path above to review the raw messages Mermaid removed from context.".to_string());
    } else {
        lines.push(String::new());
        lines.push("Last compaction: none yet.".to_string());
    }

    lines.join("\n")
}

fn tasks_text(tasks: &[crate::runtime::TaskRecord]) -> String {
    let mut lines = vec!["Tasks".to_string()];
    if tasks.is_empty() {
        lines.push("No tasks recorded yet.".to_string());
        return lines.join("\n");
    }
    for task in tasks {
        lines.push(format!(
            "- {} [{}] {} - {}",
            task.id, task.status, task.priority, task.title
        ));
        lines.push(format!("  project: {}", task.project_path));
        lines.push(format!("  updated: {}", task.updated_at));
    }
    lines.join("\n")
}

fn task_detail_text(
    task: Option<&crate::runtime::TaskRecord>,
    events: &[crate::runtime::TaskTimelineEvent],
) -> String {
    let Some(task) = task else {
        return "Task not found.".to_string();
    };
    let mut lines = vec![
        format!("Task {}", task.id),
        format!("Title: {}", task.title),
        format!("Status: {}", task.status),
        format!("Priority: {}", task.priority),
        format!("Project: {}", task.project_path),
        format!("Model: {}", task.model_id),
        format!("Created: {}", task.created_at),
        format!("Updated: {}", task.updated_at),
    ];
    if let Some(report) = &task.final_report {
        lines.push(String::new());
        lines.push("Final report:".to_string());
        lines.push(report.clone());
    }
    if !events.is_empty() {
        lines.push(String::new());
        lines.push("Timeline:".to_string());
        for event in events {
            lines.push(format!(
                "- {} {}: {}",
                event.created_at, event.kind, event.message
            ));
        }
    }
    lines.join("\n")
}

fn processes_text(processes: &[crate::runtime::ProcessRecord]) -> String {
    let mut lines = vec!["Processes".to_string()];
    if processes.is_empty() {
        lines.push("No processes recorded yet.".to_string());
        return lines.join("\n");
    }
    for process in processes {
        lines.push(format!(
            "- {} pid={} [{}] {}",
            process.id,
            process.pid,
            process.status.as_str(),
            process.command
        ));
        if let Some(task_id) = &process.task_id {
            lines.push(format!("  task: {}", task_id));
        }
        if let Some(url) = &process.detected_url {
            lines.push(format!("  url: {}", url));
        }
        if let Some(log_path) = &process.log_path {
            lines.push(format!("  log: {}", log_path));
        }
    }
    lines.join("\n")
}

fn approvals_text(approvals: &[crate::runtime::ApprovalRecord]) -> String {
    let mut lines = vec!["Approvals".to_string()];
    if approvals.is_empty() {
        lines.push("No pending approvals.".to_string());
        return lines.join("\n");
    }
    for approval in approvals {
        lines.push(format!(
            "- {} [{}] {}",
            approval.id, approval.risk_classification, approval.proposed_action
        ));
        if let Some(checkpoint_id) = &approval.checkpoint_id {
            lines.push(format!("  checkpoint: {}", checkpoint_id));
        }
        if approval.pending_action_json.is_some() {
            lines.push("  pending action: recorded".to_string());
        }
    }
    lines.join("\n")
}

fn checkpoints_text(checkpoints: &[crate::runtime::CheckpointRecord]) -> String {
    let mut lines = vec!["Checkpoints".to_string()];
    if checkpoints.is_empty() {
        lines.push("No checkpoints recorded yet.".to_string());
        return lines.join("\n");
    }
    for checkpoint in checkpoints {
        lines.push(format!(
            "- {} {} {}",
            checkpoint.id, checkpoint.created_at, checkpoint.project_path
        ));
    }
    lines.join("\n")
}

fn plugins_text(plugins: &[crate::runtime::PluginInstallRecord]) -> String {
    let mut lines = vec!["Plugins".to_string()];
    if plugins.is_empty() {
        lines.push("No plugins installed.".to_string());
        return lines.join("\n");
    }
    for plugin in plugins {
        lines.push(format!(
            "- {} [{}] {}",
            plugin.id,
            if plugin.enabled {
                "enabled"
            } else {
                "disabled"
            },
            plugin.source
        ));
    }
    lines.join("\n")
}

fn usage_totals_line(usage: TokenUsageTotals) -> String {
    let mut parts = vec![
        format!("total {}", format_compact_count(usage.total_tokens())),
        format!("input {}", format_compact_count(usage.input_total_tokens())),
        format!(
            "output {}",
            format_compact_count(usage.output_total_tokens())
        ),
    ];
    if usage.cached_input_tokens > 0 {
        parts.push(format!(
            "cache read {}",
            format_compact_count(usage.cached_input_tokens)
        ));
    }
    if usage.cache_creation_input_tokens > 0 {
        parts.push(format!(
            "cache write {}",
            format_compact_count(usage.cache_creation_input_tokens)
        ));
    }
    if usage.reasoning_output_tokens > 0 {
        parts.push(format!(
            "reasoning {}",
            format_compact_count(usage.reasoning_output_tokens)
        ));
    }
    parts.join(", ")
}

/// When a turn is aborted while its tools are mid-flight, the `assistant`
/// message carrying the `tool_calls` is already committed to history but the
/// matching `tool` result messages never will be. Left as-is, the next request
/// sends `tool_use` blocks with no `tool_result` and providers (Anthropic in
/// particular) reject it with a 400. Commit a `cancelled` placeholder result
/// for every outstanding call so history stays well-formed — the same repair
/// compaction performs for orphaned tool calls (#71), applied to the live
/// cancel/quit paths. Leaves `state.turn` untouched for any non-`ExecutingTools`
/// state; the caller sets the real target state afterwards.
fn seal_orphaned_tool_calls(state: &mut State) {
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
fn clear_parked_tool_requests(state: &mut State) {
    state.pending_approval.clear();
    state.pending_question.clear();
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

fn request_exit(state: &mut State, cmds: &mut Vec<Cmd>) {
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
    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
    cmds.push(Cmd::Exit);
}

fn handle_confirm_accepted(state: &mut State, cmds: &mut Vec<Cmd>) {
    let Some(confirm) = state.confirm.take() else {
        return;
    };
    match confirm.accept_msg_token {
        super::state::ConfirmationTarget::ClearConversation => {
            // If a turn was still in flight when the user cleared, cancel its
            // scope first and reset to `Idle` — mirroring `Msg::ConversationLoaded`
            // (#2, F34). Without this the orphaned model/tool tasks keep running
            // (tools keep mutating files after a "clear"), and the still-active
            // turn's same-id `StreamDone`/`ToolFinished` would pass the stale
            // filter and commit a stray message into the freshly-cleared
            // conversation. The cancelled turn's parked approval requests can
            // never be answered, so drop them too.
            if let Some(id) = state.turn.id() {
                cmds.push(Cmd::CancelScope(id));
                // Drop the cancelled turn's parked approval/question modals and
                // stale running-tool indicators before wiping the conversation.
                clear_parked_tool_requests(state);
                state.ui.live_tool_status.clear();
            }
            // A message queued mid-turn belonged to the conversation being
            // wiped — don't let it auto-submit into the fresh one.
            state.ui.queued_messages.clear();
            // Clear = start a fresh conversation: new ID, new default
            // title, empty history, zero cumulative tokens. Matches
            // user mental model ("wipe everything").
            let project_path = state.session.conversation.project_path.clone();
            let model_name = state.session.conversation.model_name.clone();
            // Carry the git branch forward: the impure startup can't re-detect
            // it inside the pure reducer, and the cleared session is still the
            // same working tree.
            let git_branch = state.session.conversation.git_branch.clone();
            state.session.conversation =
                crate::session::ConversationHistory::new(project_path, model_name, state.now);
            state.session.conversation.git_branch = git_branch;
            state.session.last_token_usage = None;
            state.session.cumulative_token_usage = TokenUsageTotals::default();
            state.session.context_usage = None;
            state.runtime.auto_compact_suppressed = false;
            state.turn = TurnState::Idle;
            // The fresh conversation starts with an empty checklist; the
            // broker must forget the old one too (single-writer sync).
            cmds.push(Cmd::SyncTaskStore(crate::domain::TaskStore::default()));
            // New conversation id -> new scratch dir. The old one stays on
            // disk until the sweep reaps it (its pid lock expires with us).
            refresh_scratchpad(state, cmds);
            emit_title_if_changed(state, cmds);
        },
    }
}

/// How one API request's usage folds into the session/run counters.
enum UsageFold {
    /// The session's own model request: also becomes `last_token_usage`
    /// (which feeds the context gauge) and banks its output into the run
    /// counter.
    OwnRequest,
    /// A subagent's child-session delta: cumulative + run counter only —
    /// never `last_token_usage`, which is the parent's most recent request
    /// and feeds the PARENT's context-size estimate (the child's context
    /// is a separate window).
    Subagent,
    /// A compaction summarizer call: charged like an own request, but its
    /// output counts toward the run only when the compaction happened
    /// inside one (auto/recovery) — a manual `/compact` is not run spend.
    /// The caller rebuilds the context gauge from the compaction snapshot.
    Compaction { mid_run: bool },
    /// A detached background agent's final usage: cumulative only — it is
    /// not part of whichever run may be active when it lands, so it never
    /// touches `last_token_usage` or the run counter.
    Detached,
}

/// The single accumulation point for provider-reported usage. Every path
/// that bills tokens goes through here so the meters cannot drift apart.
/// Takes the two sub-states it touches (not `&mut State`) so callers
/// holding a `&mut state.turn` borrow can still fold.
fn fold_token_usage(
    session: &mut super::state::Session,
    runtime: &mut super::runtime::RuntimeState,
    usage: &TokenUsage,
    fold: UsageFold,
) {
    let totals = TokenUsageTotals::from_usage(usage);
    session.cumulative_token_usage.add_assign(totals);
    let bank_run_output = match fold {
        UsageFold::OwnRequest => {
            session.last_token_usage = Some(totals);
            true
        },
        UsageFold::Subagent => true,
        UsageFold::Compaction { mid_run } => {
            session.last_token_usage = Some(totals);
            mid_run
        },
        UsageFold::Detached => false,
    };
    if bank_run_output {
        runtime.run_tokens.add_provider(usage.output_total_tokens());
    }
}

fn handle_compaction_finished(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    turn: TurnId,
    result: CompactionResult,
) {
    // Manual `/compact` ends the turn; a truncation recovery or context-limit
    // retry resumes the run; a pre-turn auto-compaction (still `Generating`)
    // just swaps in the compacted messages and lets the in-flight stream
    // continue in the effect.
    enum Outcome {
        Manual,
        /// Resume the run; `resume_continuation` carries the interrupted turn's
        /// auto-continue flag so a chain survives a mid-chain compaction.
        Recovery {
            resume_continuation: bool,
        },
        AutoMidTurn,
    }
    let outcome = match state.turn {
        TurnState::Compacting {
            id,
            trigger,
            resume_continuation,
            ..
        } if id == turn => match trigger {
            // A context-limit compaction (the provider rejected the request
            // mid-stream for length; emitted from effect/mod.rs via
            // `is_context_limit_error`) must RESUME the interrupted request, just
            // like a truncation recovery — not silently end the turn as the old
            // `_ => Manual` arm did.
            CompactionTrigger::ContextLimitRetry | CompactionTrigger::TruncationRecovery => {
                Outcome::Recovery {
                    resume_continuation,
                }
            },
            _ => Outcome::Manual,
        },
        TurnState::Generating { id, .. } if id == turn => Outcome::AutoMidTurn,
        _ => return,
    };

    // Compaction runs asynchronously from a request snapshot. Preserve every
    // message that arrived after dispatch (MCP/runtime notices, run summaries,
    // and similar non-turn-scoped events) instead of letting the replacement
    // clobber them. Match the request-visible source as an ordered subsequence;
    // anything else in the live history is intervening state.
    let intervening =
        compaction_intervening_messages(state.session.messages(), &result.source_boundaries);
    let intervening_tokens = super::compaction::estimate_messages_tokens(&intervening);

    let conversation_id = state.session.conversation.id.clone();
    let mut record = result.record;
    record.archive_path = Some(format!(
        ".mermaid/compactions/{}/{}.json",
        conversation_id, record.id
    ));
    let archive = CompactionArchive {
        id: record.id.clone(),
        conversation_id,
        created_at: record.created_at,
        messages: result.archived_messages,
    };

    state
        .session
        .conversation
        .replace_messages(result.replacement_messages, state.now);
    if !intervening.is_empty() {
        let messages = &mut state.session.conversation.messages;
        let before_pending_tail = messages.last().is_some_and(|message| {
            message.role == MessageRole::Assistant
                && message
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty())
        });
        if before_pending_tail {
            // A tool result that answers the pending tail must land AFTER its
            // call — a `tool_result` preceding its `tool_use` 400s providers
            // and gets scrubbed as an orphan. Split at the FIRST such result:
            // everything before it goes ahead of the tail, and everything from
            // it onward follows the tail, so a notice that arrived after (and
            // may reference) the result keeps its arrival order too.
            let tail_ids: std::collections::HashSet<String> = messages
                .last()
                .and_then(|message| message.tool_calls.as_ref())
                .into_iter()
                .flatten()
                .filter_map(|call| call.id.clone())
                .collect();
            let split = intervening.iter().position(|message| {
                message.role == MessageRole::Tool
                    && message
                        .tool_call_id
                        .as_ref()
                        .is_some_and(|id| tail_ids.contains(id))
            });
            let mut before_tail = intervening;
            let after_tail = match split {
                Some(index) => before_tail.split_off(index),
                None => Vec::new(),
            };
            let position = messages.len() - 1;
            messages.splice(position..position, before_tail);
            messages.extend(after_tail);
        } else {
            messages.extend(intervening);
        }
    }
    state
        .session
        .conversation
        .add_compaction(record.clone(), state.now);
    state.session.context_usage = Some(
        result
            .after_snapshot
            .with_additional_tokens(intervening_tokens),
    );
    // A successful compaction un-pauses auto-compaction regardless of trigger.
    state.runtime.auto_compact_suppressed = false;

    if let Some(usage) = result.usage {
        fold_token_usage(
            &mut state.session,
            &mut state.runtime,
            &usage,
            UsageFold::Compaction {
                mid_run: !matches!(outcome, Outcome::Manual),
            },
        );
    }

    match outcome {
        Outcome::Manual => {
            state.turn = TurnState::Idle;
            // Drain one queued message on the way out, same as the no-tool-calls
            // tail of `handle_stream_done` / `handle_turn_cancelled` (#73). A
            // message the user typed during `/compact` would otherwise sit in the
            // FIFO until some later turn happened to end.
            drain_next_queued_message(state);
        },
        Outcome::Recovery {
            resume_continuation,
        } => {
            // Resume the run with the compacted context so the model can finish the
            // work the truncation cut off (mirrors `handle_tool_finished`'s
            // follow-up dispatch).
            let next_turn = state.ids.fresh_turn();
            state.turn = super::transition::start_generating_with(
                next_turn,
                std::time::SystemTime::from(state.now),
                resume_continuation,
            );
            push_call_model(state, cmds, next_turn);
        },
        // Pre-turn auto-compaction: the stream is still live in the effect, which
        // already retried with the compacted messages — nothing to do here.
        Outcome::AutoMidTurn => {},
    }

    // The compaction's replacement message already carries the receipt text, so
    // the old transient banner that repeated it is simply gone.
    // SaveCompactionArchive persists the stripped conversation.
    cmds.push(Cmd::SaveCompactionArchive {
        archive,
        record,
        conversation: state.session.conversation.clone(),
    });
}

fn compaction_intervening_messages(
    current: &[ChatMessage],
    source: &[crate::domain::CompactionBoundary],
) -> Vec<ChatMessage> {
    if source.is_empty() {
        return Vec::new();
    }
    let mut source_index = 0usize;
    let mut intervening = Vec::new();
    for message in current {
        // One hash per live message; the lookahead below compares strings.
        let fingerprint = crate::domain::CompactionBoundary::fingerprint_of(message);
        if source_index < source.len() && source[source_index].fingerprint == fingerprint {
            source_index += 1;
            continue;
        }
        if let Some(offset) = source[source_index..]
            .iter()
            .position(|boundary| boundary.fingerprint == fingerprint)
        {
            source_index += offset + 1;
        } else {
            intervening.push(message.clone());
        }
    }
    intervening
}

fn handle_compaction_failed(
    state: &mut State,
    turn: TurnId,
    trigger: CompactionTrigger,
    message: String,
    kind: StatusKind,
) {
    match state.turn {
        TurnState::Compacting { id, .. } if id == turn => {
            state.turn = TurnState::Idle;
            // This is the one arm that ends the turn; drain a queued message so
            // it isn't stranded (the Generating arm leaves the stream live).
            drain_next_queued_message(state);
        },
        TurnState::Generating { id, .. } if id == turn => {},
        _ => return,
    }

    let prefix = match trigger {
        // A benign no-op (`Info`) — e.g. too little history to summarize — is not a
        // failure. The user ran `/compact` explicitly, so say plainly there's
        // nothing to do rather than printing "Compaction failed: Invalid request".
        CompactionTrigger::Manual if matches!(kind, StatusKind::Info) => {
            state.session.append(
                ChatMessage::system(format!("Nothing to compact — {message}.")),
                state.now,
            );
            return;
        },
        CompactionTrigger::Manual => "Compaction failed",
        // Auto-compaction is best-effort preflight: Mermaid proceeds with the
        // un-compacted request (the provider's own context limit is the real
        // gate). But a failing summarizer must not silently retry — and pay
        // for — a draft + review model call on every later turn: pause it
        // until a compaction succeeds, `/compact` runs, or the conversation
        // switches, and tell the user once.
        CompactionTrigger::AutoThreshold => {
            if !state.runtime.auto_compact_suppressed {
                state.runtime.auto_compact_suppressed = true;
                state.session.append(
                    ChatMessage::system(format!(
                        "Auto-compaction paused after a failed attempt ({message}) — run /compact to retry."
                    )),
                    state.now,
                );
            }
            return;
        },
        CompactionTrigger::ContextLimitRetry => "Context-limit compaction failed",
        // The response truncated and recovery couldn't reduce the context (e.g. the
        // preserved tail already fills the window). Stop the run cleanly with the
        // manual levers instead of the raw "did not reduce" error or a retry loop.
        CompactionTrigger::TruncationRecovery => {
            let hint = truncation_hint(state);
            state.session.append(ChatMessage::system(hint), state.now);
            return;
        },
    };
    state.session.append(
        ChatMessage::system(format!("{}: {}", prefix, message)),
        state.now,
    );
}

fn handle_stream_tool_call(
    state: &mut State,
    turn: TurnId,
    call: crate::models::tool_call::ToolCall,
) {
    if let TurnState::Generating {
        id,
        pending_tool_calls,
        ..
    } = &mut state.turn
        && *id == turn
    {
        pending_tool_calls.push(call);
        return;
    }
    // The stale filter at update_step's top guarantees this event's turn
    // matches the active turn, so reaching here means the turn already
    // advanced past Generating (e.g. into ExecutingTools) before this tool
    // call arrived. Single-channel relay ordering should prevent it; log so
    // a provider that ever emits a ToolCall after Done is diagnosable.
    tracing::warn!(
        event_turn = %turn,
        active_turn = ?state.turn.id(),
        "reducer: dropped StreamToolCall — turn not in Generating state",
    );
}

/// The "response truncated, here's how to get more room" message shown when a
/// length-truncation can't be (or has stopped being) auto-recovered. Shared by
/// `handle_stream_done` (cap reached / nothing to compact) and
/// `handle_compaction_failed` (recovery compaction couldn't reduce the context).
fn truncation_hint(state: &State) -> String {
    let mut msg = "Response truncated — reached the model's max output-token limit.".to_string();
    // Ollama quick-fix: if auto-fit capped the window below the model's max, tell
    // the user exactly how to raise it.
    if let Some(ctx) = state.runtime.ollama_context.as_ref()
        && let (Some(model_max), Some(eff), Some(src)) = (ctx.model_max, ctx.effective, ctx.source)
        && src.is_auto()
        && model_max > eff
    {
        msg.push_str(&format!(
            " This model supports up to {} but the window is auto-fit to {} for your GPU — \
             raise it with `/context max`, or allow RAM with `/context offload on`.",
            format_compact_count(model_max),
            format_compact_count(eff)
        ));
    }
    msg
}

/// Hint for an output-cap length stop (the window had room). With AUTO
/// budgeting mermaid no longer imposes its own cap, so the stop was either the
/// user's explicit `max_tokens` hard cap or the model/provider's own
/// per-response ceiling.
fn output_cap_hint(state: &State) -> String {
    let cap = state.settings.default_model.max_tokens;
    if cap > 0 {
        format!(
            "Response truncated — hit your configured max_tokens cap ({}). Raise it, or set \
             `default_model.max_tokens = 0` (auto) to lift it.",
            format_compact_count(cap)
        )
    } else {
        "Response truncated — the model's per-response output limit was reached. Ask it to \
         continue from where it stopped."
            .to_string()
    }
}

/// Per-run cap on automatic retries of a turn that produced no visible output.
/// One nudged re-attempt recovers the common case (a reasoning-heavy model that
/// stalled without replying) without letting a persistently-empty model loop and
/// burn tokens; past the cap the run stops with a hint.
const MAX_EMPTY_CONTINUATIONS: u32 = 1;

fn handle_stream_done(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    turn: TurnId,
    usage: Option<crate::models::TokenUsage>,
    provider_continuation: Option<ProviderContinuation>,
    stop_reason: Option<crate::models::FinishReason>,
) {
    // Unpack the Generating state, drop it into Idle temporarily;
    // the branch below decides whether to stay Idle (no tool calls)
    // or transition to ExecutingTools (calls buffered).
    let generating = match std::mem::replace(&mut state.turn, TurnState::Idle) {
        TurnState::Generating {
            id,
            partial_text,
            partial_reasoning,
            provider_continuation: accumulated_continuation,
            pending_tool_calls,
            continuation,
            ..
        } if id == turn => (
            partial_text,
            partial_reasoning,
            accumulated_continuation,
            pending_tool_calls,
            continuation,
        ),
        other => {
            // #F40: a StreamDone can arrive for a turn that is already Cancelling
            // (the provider completed a moment before the user's cancel was
            // processed). Honor the cancel — the turn is NOT committed — but still
            // fold the already-billed token usage into the running totals so the
            // /context accounting stays accurate. Everything else about the late
            // Done is dropped.
            if let TurnState::Cancelling { id, .. } = &other
                && *id == turn
                && let Some(u) = usage
            {
                fold_token_usage(
                    &mut state.session,
                    &mut state.runtime,
                    &u,
                    UsageFold::OwnRequest,
                );
            }
            state.turn = other;
            return;
        },
    };

    let (partial_text, partial_reasoning, accumulated_continuation, tool_calls, continuation) =
        generating;
    // Bank this phase's generated tokens into the run total so the spinner's
    // counter carries across the tool step into the next model call. When the
    // provider reported usage, the real output lands via `fold_token_usage`
    // below; only a usage-less phase (common on tool follow-ups from some
    // providers, or a stream cut early) falls back to the chars/4 estimate,
    // which marks the whole run counter `~`.
    if usage.is_none() {
        state
            .runtime
            .run_tokens
            .add_estimate((partial_text.len() + partial_reasoning.len()) / 4);
    }

    // The turn that any live recovery nudge was steering has now ended — retire
    // it before committing, so the partial and its continuation sit adjacent in
    // history (and the one-shot instruction never leaks into a later request).
    // Placed after the Generating unpack so a stale/Cancelling Done can't sweep.
    sweep_spent_nudges(state);

    // A turn that produced no assistant *text* and no tool calls is a dead end for
    // the user — even when the model spent the turn "thinking" (reasoning is
    // hidden and non-actionable). This is a real failure mode at high reasoning
    // effort / with small local models: the model reasons at length, then stops
    // without a reply or any action, and the run goes silent. (The old guard also
    // required reasoning to be empty, so a reasoning-heavy stall slipped through.)
    let no_visible_output = partial_text.trim().is_empty() && tool_calls.is_empty();
    // A normal stop — not a window-full truncation (compaction-recovered below)
    // or a content-filter block (terminal); those have their own handling.
    let normal_stop = !matches!(
        stop_reason,
        Some(crate::models::FinishReason::Length)
            | Some(crate::models::FinishReason::ContentFilter)
    );
    // Recover a stalled turn by re-issuing the model call (tail below) so it
    // actually produces its reply/actions, bounded per-run so a persistently-empty
    // model can't loop forever. Decided up front so the empty assistant turn is
    // left uncommitted — keeping history clean for a faithful re-attempt.
    let auto_retry_empty = no_visible_output
        && normal_stop
        && state.runtime.empty_continuations < MAX_EMPTY_CONTINUATIONS;
    // The empty-turn guard counts only *consecutive* no-output turns: any turn
    // that makes progress (text or tool calls) resets it.
    if !no_visible_output {
        state.runtime.empty_continuations = 0;
    }

    let final_continuation = provider_continuation.or(accumulated_continuation);

    // Commit the assistant message (with any tool calls attached — the adapter
    // serializes them into the next conversation turn), unless it's an empty turn
    // we're about to retry: leaving it out keeps history clean so the re-attempt
    // isn't seeded with an empty assistant message.
    if !auto_retry_empty {
        let msg = commit_assistant_message(
            partial_text,
            partial_reasoning,
            tool_calls.clone(),
            final_continuation,
            state.now,
            continuation,
        );
        state.session.append(msg, state.now);
    }

    // A bare length-truncation (no tool calls) is the recoverable case below; any
    // other ending means the run made progress, so reset the recovery guard — it
    // should count only *consecutive* no-progress truncations.
    let dry_truncation =
        tool_calls.is_empty() && matches!(stop_reason, Some(crate::models::FinishReason::Length));
    if !dry_truncation {
        state.runtime.truncation_recoveries = 0;
        state.runtime.continue_recoveries = 0;
    }

    // Set when a length-truncation is recoverable: instead of ending the run with
    // a hint, compact the conversation and resume (handled after the save below).
    let mut recovering = false;
    // Set when an OUTPUT-cap truncation left visible content: instead of ending
    // the run, continue the reply in a fresh turn (handled after the save below).
    let mut continuing = false;

    // Surface a terminal stop reason that would otherwise leave the response
    // silently incomplete. (A refusal with no content is turned into an error
    // upstream in the adapter; here we only see reasons that still produced
    // output.) Skip it when tool calls are pending: a system message inserted
    // between the assistant's `tool_calls` and their results breaks provider
    // pairing → 400 (#72). A Length/ContentFilter stop *with* tool calls is
    // contradictory anyway, so dropping the note in that case is safe.
    if tool_calls.is_empty() && !auto_retry_empty {
        match stop_reason {
            Some(crate::models::FinishReason::Length) => {
                // Classify before deciding: a length-stop is either the window
                // filling mid-turn (compaction helps) or the per-response
                // OUTPUT cap (it can't — compacting the input is futile; GLM-5.2
                // at deep reasoning hit this with a 1M window at 2% used and
                // looped through pointless compactions).
                let window = state
                    .session
                    .context_usage
                    .as_ref()
                    .and_then(|s| s.max_tokens)
                    .or(state.runtime.provider_capabilities.max_context_tokens);
                let reserve =
                    CompactionPolicy::default().response_reserve(&build_chat_request(state));
                match crate::domain::compaction::classify_length_stop(
                    usage.as_ref(),
                    window,
                    reserve,
                ) {
                    crate::domain::compaction::LengthCause::OutputCapped => {
                        // Never compact for an output-cap stop — the input
                        // isn't the problem. If the cut left visible content
                        // and the model wasn't cut off mid-reasoning, continue
                        // the reply in a fresh turn (bounded per run);
                        // otherwise stop with the accurate hint.
                        let mid_reasoning = usage.as_ref().is_some_and(|u| {
                            u.completion_tokens > 0
                                && u.reasoning_output_tokens.saturating_mul(10)
                                    >= u.completion_tokens.saturating_mul(9)
                        });
                        let under_cap = state.runtime.continue_recoveries
                            < crate::constants::MAX_OUTPUT_CONTINUATIONS;
                        if !no_visible_output && !mid_reasoning && under_cap {
                            continuing = true;
                        } else {
                            let hint = output_cap_hint(state);
                            push_system(state, cmds, hint);
                        }
                    },
                    crate::domain::compaction::LengthCause::ContextFull
                    | crate::domain::compaction::LengthCause::Unknown => {
                        // Visible assistant text is real forward progress even if
                        // the context filled again. The guard bounds only repeated
                        // no-output thrashing, as the config promises.
                        if !no_visible_output {
                            state.runtime.truncation_recoveries = 0;
                        }
                        // The window filled mid-turn (or usage was absent and we
                        // assume so). If there's history to compact and we're
                        // under the per-run cap, recover (compact + continue)
                        // rather than stopping; else the manual-levers hint.
                        let cap = state.settings.compaction.max_truncation_recoveries;
                        let under_cap =
                            cap == 0 || state.runtime.truncation_recoveries < cap as u32;
                        if under_cap && state.session.messages().len() >= 3 {
                            recovering = true;
                            push_system(
                                state,
                                cmds,
                                "Context window full — compacting the conversation to continue.",
                            );
                        } else {
                            let hint = truncation_hint(state);
                            push_system(state, cmds, hint);
                        }
                    },
                }
            },
            Some(crate::models::FinishReason::ContentFilter) => push_system(
                state,
                cmds,
                "Response was flagged by the provider's content filter.",
            ),
            _ if no_visible_output => push_system(
                state,
                cmds,
                "The model ended its turn with no reply or action (it produced only \
                 internal reasoning). Send a message to continue, or rephrase your \
                 request.",
            ),
            _ => {},
        }
    }

    // Provider token usage is per API request. Track both the last
    // reported request and the session total so the footer can label
    // the number honestly instead of presenting a giant raw counter.
    if let Some(u) = usage {
        fold_token_usage(
            &mut state.session,
            &mut state.runtime,
            &u,
            UsageFold::OwnRequest,
        );
        let max_context = state
            .session
            .context_usage
            .as_ref()
            .and_then(|snapshot| snapshot.max_tokens)
            .or(state.runtime.provider_capabilities.max_context_tokens);
        let mut context = super::state::ContextUsageSnapshot::from_usage(&u, max_context);
        if let Some(prev) = state.session.context_usage.as_ref()
            && context.breakdown.is_none()
        {
            context.breakdown = prev.breakdown.clone();
        }
        state.session.context_usage = Some(context);
    }
    // When a turn reports no usage (common on tool follow-ups), leave the
    // prior request's `last_token_usage` intact rather than nulling it —
    // the footer's "Last API request" should reflect the last request that
    // actually reported usage, not flip to "n/a".

    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));

    // If the model asked for any tools, transition to ExecutingTools
    // and dispatch one ExecuteTool per call. The Vec<Option<ToolOutcome>>
    // invariant now has a real producer — ToolFinished messages
    // populate the slots, and try_complete_outcomes gates the
    // transition to the follow-up Generating turn.
    if !tool_calls.is_empty() {
        let pending: Vec<super::state::PendingToolCall> = tool_calls
            .into_iter()
            .map(|source| super::state::PendingToolCall {
                call_id: state.ids.fresh_tool_call(),
                source,
            })
            .collect();
        // `tool_search` is DOMAIN-INTERCEPTED: it is a pure function of
        // state (deferred MCP specs + the promoted set), so it never gets a
        // `Cmd::ExecuteTool`. Its outcome is computed inline AFTER the
        // ExecutingTools transition (so the outcome slots exist) and fed
        // through the normal `handle_tool_finished` slot-fill machinery —
        // deterministic under --replay with zero new Msg/Cmd variants.
        let intercepted: Vec<(super::ids::ToolCallId, ToolOutcome)> = pending
            .iter()
            .filter(|call| call.source.function.name == super::tool_search::TOOL_SEARCH_NAME)
            .map(|call| {
                let result =
                    super::tool_search::run_tool_search(state, &call.source.function.arguments);
                super::tool_search::apply_promotions(&mut state.mcp.promoted, result.promote);
                (
                    call.call_id,
                    ToolOutcome::success(result.text, result.summary, 0.0),
                )
            })
            .collect();
        // Captured once for the whole batch: the live safety mode + the
        // turn's intent (for the Auto-mode classifier).
        let intent = latest_user_intent(&state.session);
        // Plan mode floors the effective safety mode to read-only at the one
        // dispatch chokepoint — the policy gate, shell classifier, and
        // subagent inheritance then all apply unchanged. The plan carve-outs
        // (plan file, memory, known-safe builds) key on `plan_file` inside
        // the gate.
        let effective_safety = if state.session.plan.is_some() {
            crate::runtime::SafetyMode::ReadOnly
        } else {
            state.session.safety_mode
        };
        let plan_file = state
            .session
            .plan
            .as_ref()
            .map(|plan| plan.plan_path.clone());
        for call in &pending {
            if call.source.function.name == super::tool_search::TOOL_SEARCH_NAME {
                continue;
            }
            cmds.push(Cmd::ExecuteTool {
                turn,
                call_id: call.call_id,
                source: call.source.clone(),
                // F7: pass the session's current model id so subagent
                // tools can spawn children against the same provider.
                model_id: state.session.model_id.clone(),
                safety_mode: effective_safety,
                plan_file: plan_file.clone(),
                plan_permissions: state.settings.plan.permissions,
                context_percent: state
                    .session
                    .context_usage
                    .as_ref()
                    .and_then(|c| c.used_percent),
                intent: intent.clone(),
                // Checkpoint anchoring: conversation id + length at DISPATCH.
                // History here is [..., user@k, assistant(tool_use)], so any
                // checkpoint this run takes has message_index >= k+1 and a
                // fork at k discards it iff message_index > k (strict).
                session_id: state.session.conversation.id.clone(),
                message_index: state.session.messages().len(),
                // The session's scratch dir, when already materialized —
                // `None` before `Msg::ScratchpadReady` lands (tools fall
                // back to workdir-relative temp space).
                scratchpad: state.session.scratchpad.clone(),
            });
        }
        state.turn = super::transition::start_executing_tools(
            turn,
            pending,
            std::time::SystemTime::from(state.now),
        );
        for (call_id, outcome) in intercepted {
            handle_tool_finished(state, cmds, turn, call_id, outcome);
        }
        return;
    }

    // Length-truncation recovery: compact the conversation, then resume the run.
    // `Cmd::CompactConversation` force-runs compaction (no threshold gate); the
    // `CompactionFinished` handler re-dispatches the model call with the compacted
    // context. Returning here skips the queue-drain so the run doesn't end.
    if recovering {
        state.runtime.truncation_recoveries += 1;
        let comp_turn = state.ids.fresh_turn();
        state.turn = TurnState::Compacting {
            id: comp_turn,
            started: std::time::SystemTime::from(state.now),
            trigger: CompactionTrigger::TruncationRecovery,
            // Carry the interrupted turn's auto-continue flag through the
            // compaction so the resumed text keeps its Continuation stamp.
            resume_continuation: continuation,
        };
        cmds.push(Cmd::CompactConversation {
            turn: comp_turn,
            request: CompactionRequest::auto(
                build_chat_request(state),
                CompactionTrigger::TruncationRecovery,
            ),
        });
        return;
    }

    // Output-cap continuation: the response hit the provider's per-response
    // output ceiling with window room to spare, so compaction can't help —
    // continue the reply in a fresh turn instead. The committed partial rides
    // in history, so the model sees exactly where it stopped; the system note
    // (which rides in the next request) nudges it to resume rather than
    // restart. Bounded by MAX_OUTPUT_CONTINUATIONS per run so a model that
    // restarts or re-truncates can't loop; portable across providers (no
    // assistant-prefill dependency). Returning keeps the run alive.
    if continuing {
        state.runtime.continue_recoveries += 1;
        push_system_kind(
            state,
            cmds,
            "The response hit the model's per-response output limit — continuing. Resume \
             exactly where the previous message stopped; do not repeat text already sent.",
            crate::models::ChatMessageKind::RecoveryNudge,
        );
        let next_turn = state.ids.fresh_turn();
        state.turn = super::transition::start_generating_with(
            next_turn,
            std::time::SystemTime::from(state.now),
            true,
        );
        push_call_model(state, cmds, next_turn);
        return;
    }

    // Stalled-turn recovery: the model produced no reply and no action on a normal
    // stop. Rather than ending the run silently, re-issue the model call so it
    // completes the work it skipped (bounded by MAX_EMPTY_CONTINUATIONS, decided
    // above). The system note both tells the user what's happening and, since it
    // rides in the next request, nudges the model to actually respond. Returning
    // here keeps the run alive instead of dropping to the run-summary tail.
    if auto_retry_empty {
        state.runtime.empty_continuations += 1;
        push_system_kind(
            state,
            cmds,
            "The last turn produced no reply or action — continuing. Provide your \
             response or take the next step.",
            crate::models::ChatMessageKind::RecoveryNudge,
        );
        let next_turn = state.ids.fresh_turn();
        // Propagate the continuation flag: an empty retry *inside* an
        // auto-continue chain must not strip the marker from the eventual
        // real continuation text.
        state.turn = super::transition::start_generating_with(
            next_turn,
            std::time::SystemTime::from(state.now),
            continuation,
        );
        push_call_model(state, cmds, next_turn);
        return;
    }

    // The run is fully done (no tool calls, not recovering). If it began with a
    // user submit, emit a one-line "Worked for … · used … tokens" summary where
    // the spinner was, then clear `run_started` so it fires exactly once per run.
    // It's a display-only line — `build_chat_request` keeps it out of the model
    // context.
    if let Some(started) = state.runtime.run_started.take() {
        let elapsed = std::time::SystemTime::from(state.now)
            .duration_since(started)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let run_tokens = state.runtime.run_tokens;
        let mut summary = format!(
            "Worked for {} · used {}{} tokens",
            super::transition::format_run_duration(elapsed),
            if run_tokens.contains_estimate {
                "~"
            } else {
                ""
            },
            format_compact_count(run_tokens.output_tokens),
        );
        // Total line changes across the run's file mutations, so the user
        // doesn't have to sum each tool call's diff by hand. Omitted when the
        // run changed nothing — a read-only run stays a two-part summary.
        let changes = state.runtime.run_line_changes;
        if !changes.is_empty() {
            summary.push_str(&format!(" · +{}/-{}", changes.added, changes.removed));
        }
        state
            .session
            .append(ChatMessage::run_summary(summary), state.now);
        cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
    }

    // No tool calls — turn ends here. Drain the queued-message FIFO.
    drain_next_queued_message(state);
}

/// Handle `Msg::OpenImageAt`. Resolves the base64 payload from the committed
/// message history, writes it to a temp file, and dispatches
/// `Cmd::OpenInSystem` so the user's default image viewer opens it. F13.
///
/// Resolution prefers the stable global image number: the click map indexes
/// the DISPLAY transcript, which the continuation stitch can shift away from
/// committed history (hidden nudges, merged bubbles). The positional pair is
/// the fallback for images without a number (pre-numbering sessions — which
/// predate stitching, so their indices still align).
fn handle_open_image_at(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    message_index: usize,
    image_index: usize,
    image_number: Option<u64>,
) {
    let by_number = image_number.and_then(|n| {
        state.session.messages().iter().find_map(|m| {
            let pos = m.image_numbers.as_ref()?.iter().position(|&x| x == n)?;
            m.images.as_ref()?.get(pos)
        })
    });
    let b64 = match by_number {
        Some(b64) => b64,
        None => {
            let msg = match state.session.messages().get(message_index) {
                Some(m) => m,
                None => return,
            };
            let Some(images) = msg.images.as_ref() else {
                return;
            };
            let Some(b64) = images.get(image_index) else {
                return;
            };
            b64
        },
    };
    use base64::{Engine, engine::general_purpose};
    let Ok(bytes) = general_purpose::STANDARD.decode(b64) else {
        return;
    };
    let id = state.ids.tool_call.next();
    let temp_path = state.temp_dir.join(format!("mermaid-img-{}.png", id));
    cmds.push(Cmd::WriteImageToTemp {
        path: temp_path.clone(),
        bytes,
        format: "png".to_string(),
    });
    cmds.push(Cmd::OpenInSystem(temp_path));
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
fn handle_turn_cancelled(state: &mut State, cmds: &mut Vec<Cmd>, turn: TurnId) {
    match state.turn {
        TurnState::Cancelling { id, .. } if id == turn => {
            state.turn = TurnState::Idle;
            state.ui.live_tool_status.clear();
            // The cancelled turn abandoned whatever recovery its nudge was
            // steering — retire it so the hidden instruction can't leak into
            // the user's next, unrelated request.
            if sweep_spent_nudges(state) {
                cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
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

/// Drain one message from the queued-message FIFO when a turn ends. The
/// follow-up is re-injected through `pending_msgs` so the outer `update()`
/// re-enters cleanly (preserving stale-filter semantics) rather than
/// inline-invoking a new turn. Shared by the stream-done, cancelled, and
/// upstream-error turn-end paths so a queued message is never stranded.
fn drain_next_queued_message(state: &mut State) {
    if let Some(next) = state.ui.queued_messages.pop_front() {
        state.ui.pending_msgs.push_back(Msg::SubmitPrompt {
            text: next.text,
            attachment_ids: next.attachment_ids,
        });
    }
}

fn handle_upstream_error(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    turn: TurnId,
    error: crate::models::UserFacingError,
) {
    // Defense in depth (F4): even though the stale-filter at the top of
    // `update_step` gates on `turn_id()`, re-check here so a future
    // refactor that weakens the filter can't silently wipe the active
    // turn with an error message that belongs to a superseded one.
    if state.turn.id() != Some(turn) {
        return;
    }

    // F35: if the turn is already being cancelled (the user hit Ctrl+C / Esc),
    // a late `UpstreamError` from the cancelled provider call is the cancel's
    // own side-channel, not a real failure. The stale-filter lets a same-id
    // `Cancelling` turn through, so guard the state explicitly here — mirroring
    // the `ApprovalRequested` guard (#74). Painting a spurious error line for
    // the user's own cancel and draining a queued message here would race the
    // terminal `TurnCancelled` that `drop_scope` emits.
    if matches!(state.turn, TurnState::Cancelling { .. }) {
        return;
    }

    // End the current turn. Surface the error through a single
    // channel — the ActionDisplay attached to an empty assistant
    // message. The chat widget paints ActionDisplays as colored
    // error blocks, so committing to both `content` and `actions`
    // would paint the same error twice.
    let now = state.now;
    state.turn = TurnState::Idle;
    // The errored turn abandoned whatever recovery its nudge was steering —
    // retire it so the hidden instruction can't leak into a later request.
    if sweep_spent_nudges(state) {
        cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
    }
    let msg = ChatMessage {
        role: MessageRole::Assistant,
        content: String::new(),
        timestamp: now,
        kind: crate::models::ChatMessageKind::Normal,
        metadata: None,
        actions: vec![super::action::ActionDisplay {
            action_type: "Error".to_string(),
            target: error.summary.clone(),
            result: super::action::ActionResult::Error {
                error: error.message.clone(),
            },
            details: super::action::ActionDetails::Simple,
            duration_seconds: None,
            metadata: None,
        }],
        thinking: None,
        images: None,
        image_numbers: None,
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
        provider_continuation: None,
    };
    state.session.append(msg, state.now);

    // A provider error ends the turn just like a normal completion — persist
    // the session too, so an errored headless run's emitted session id points
    // at a real file (`mermaid run --resume <id>` after a failed run).
    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));

    // Drain the queued-message FIFO so a message the user typed mid-turn isn't
    // stranded until their next manual submit (it would otherwise run out of
    // order).
    drain_next_queued_message(state);
}

/// Route a typed `ProgressEvent`.
///
/// Tool stdout / status / byte-progress and subagent chatter are intentionally
/// dropped: surfacing each line to the status banner flickered a fresh line
/// above the input every few milliseconds (build output, pids, streamed file
/// contents) which read as noise. The status *line* already names the in-flight
/// tool, and a tool's full output lands in the chat transcript when it
/// finishes. Only image artifacts are handled here — they attach to the
/// in-flight assistant message for inline display.
fn handle_tool_progress(
    state: &mut State,
    _cmds: &mut Vec<Cmd>,
    turn: TurnId,
    call_id: super::ids::ToolCallId,
    event: crate::providers::ProgressEvent,
) {
    use crate::providers::{ProgressEvent, SubagentPhase};
    use base64::{Engine as _, engine::general_purpose};

    match event {
        ProgressEvent::Artifact { mime, data, .. }
            if mime.starts_with("image/")
                && matches!(
                    state.turn,
                    TurnState::ExecutingTools { .. } | TurnState::Generating { .. }
                ) =>
        {
            if let Some(last) = state.session.conversation.messages.last_mut()
                && last.role == MessageRole::Assistant
            {
                let encoded = general_purpose::STANDARD.encode(&data);
                last.images.get_or_insert_with(Vec::new).push(encoded);
            }
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
                if let Some(last) = state.session.conversation.messages.last_mut()
                    && last.role == MessageRole::Assistant
                {
                    last.actions.push(action);
                }
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
            cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
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
const MAX_HOOK_CONTEXT_BYTES: usize = 16 * 1024;

/// Buffer `additionalContext` strings from `before_tool_use` hooks for the
/// next dispatched model request. Turn-gated (the stale filter already drops
/// mismatched turns; re-check here for defense in depth, like
/// `handle_upstream_error`).
fn handle_hook_context(state: &mut State, turn: TurnId, texts: Vec<String>) {
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
fn push_call_model(state: &mut State, cmds: &mut Vec<Cmd>, turn: TurnId) {
    // Structural plan-rot guard: count model-call cycles while a task sits
    // in_progress with no checklist update (`handle_tasks_updated` resets the
    // counter). At the threshold, inject a targeted nudge into THIS request
    // and re-arm — prompt discipline alone demonstrably decays mid-run.
    match state.session.conversation.tasks.active() {
        Some(active) => {
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
        None => state.runtime.calls_since_task_update = 0,
    }
    let request = build_chat_request(state);
    state.pending_hook_context.clear();
    state.pending_task_notices.clear();
    cmds.push(Cmd::CallModel { turn, request });
}

pub fn build_chat_request(state: &State) -> ChatRequest {
    // Project instructions + the always-loaded memory index + any pending
    // hook context compose into the single dynamic suffix. Each block carries
    // its own header (`# Memory`, `# Hook Context`), so the parts stay clearly
    // separated from AGENTS.md/MERMAID.md and the model adapters need no
    // changes.
    let mut instruction_parts: Vec<String> = Vec::new();
    if let Some(i) = state.instructions.as_ref() {
        instruction_parts.push(i.content.clone());
    }
    if let Some(m) = state.memory.as_ref() {
        instruction_parts.push(m.index.clone());
    }
    if let Some(s) = state.skills.as_ref() {
        instruction_parts.push(s.index.clone());
    }
    if !state.pending_hook_context.is_empty() {
        instruction_parts.push(format!(
            "# Hook Context\n\n{}",
            state.pending_hook_context.join("\n\n")
        ));
    }
    if !state.pending_task_notices.is_empty() {
        instruction_parts.push(format!(
            "# Task Checklist Notices\n\n{}",
            state.pending_task_notices.join("\n")
        ));
    }
    let instructions = if instruction_parts.is_empty() {
        None
    } else {
        Some(instruction_parts.join("\n\n"))
    };

    // Pass the user's temperature verbatim — including an explicit `0.0`
    // (deterministic / greedy decoding). `ModelSettings::default()` supplies
    // `DEFAULT_TEMPERATURE`, so a `0.0` reaching here is always a deliberate
    // choice, never "unset"; the old `> 0.0` guard silently clobbered it to
    // `0.7`.
    let settings = &state.settings.default_model;
    let temperature = settings.temperature;
    // `max_tokens == 0` is AUTO: pass it through so each adapter applies the
    // model-scaled output budget — OpenAI-compat/Gemini omit the field (the
    // provider uses its own per-response max), Ollama sizes to `num_ctx`, and
    // Anthropic resolves its documented per-model ceiling. A positive value is
    // the user's explicit hard cap.
    let max_tokens = settings.max_tokens;

    // MCP tools the model should see — advertised names arrive pre-sanitized
    // (`mcp__<server>__<tool>`) from ingestion. With deferral on (the
    // default), most MCP tools are replaced by one `tool_search` definition;
    // see `domain::tool_search`. The effect runner prepends built-in tools
    // before dispatching, so this vector is the MCP-only portion. Ordering
    // is byte-stable across runs for prompt-cache warmth (#F68).
    let mut mcp_tools = super::tool_search::mcp_tool_definitions(state);
    // Plan-mode tools are registered `is_internal` (never in the effect
    // layer's `describe_all`), so which one the model sees is decided HERE,
    // where the plan state lives: `exit_plan_mode` only while planning,
    // `enter_plan_mode` only while not (and never for subagents — children
    // explore, they don't plan).
    if state.session.plan.is_some() {
        mcp_tools.push(super::plan::exit_plan_mode_definition());
    } else if !state.session.is_subagent {
        mcp_tools.push(super::plan::enter_plan_mode_definition());
    }

    // Run-summary lines ("Worked for …") are display-only UI — never send them
    // to the model. Then repair tool_use/tool_result pairing as the FINAL pass
    // over the CLONED request messages (never state.session): a session persisted
    // or hand-edited mid-tool would otherwise send a dangling tool_use and hit an
    // unrecoverable 400.
    let mut messages = evict_stale_screenshots(
        state
            .session
            .messages()
            .iter()
            .filter(|m| m.kind != crate::models::ChatMessageKind::RunSummary)
            .cloned()
            .collect(),
    );
    // The user loosening the safety mode (read_only → …) leaves the model's own
    // earlier read-only denials in history, contradicting the now-current mode;
    // rewrite them so the wire history matches the live mode (else the model
    // keeps refusing edits / claims "still read-only" after a switch up).
    // While a plan is being drafted the EFFECTIVE mode is the read-only floor,
    // so pre-plan read-only denials still describe reality — pass the floor,
    // not the (possibly looser) restore target, so they stay untouched.
    let effective_safety = if state.session.plan.is_some() {
        crate::runtime::SafetyMode::ReadOnly
    } else {
        state.session.safety_mode
    };
    neutralize_superseded_policy_denials(&mut messages, effective_safety);
    // Same contract for plan-mode denials: they stop applying the moment plan
    // mode ends (approve or cancel).
    neutralize_superseded_plan_denials(&mut messages, state.session.plan.is_some());
    super::compaction::normalize_history(&mut messages);

    ChatRequest {
        model_id: state.session.model_id.clone(),
        messages,
        system_prompt: system_prompt_for_state(state),
        instructions,
        reasoning: state.session.reasoning,
        temperature,
        max_tokens,
        // A formatting turn (`--output-schema`) advertises NO tools: several
        // providers reject or degrade schema-constrained output when tools
        // are present, and the turn's only job is reshaping the final answer.
        tools: if state.output_schema.is_some() {
            Vec::new()
        } else {
            mcp_tools
        },
        // Per-model `/context` override (set via /context <n>/max) wins; else the
        // auto-converged value the `/api/ps` check found fits; else None = auto-fit.
        ollama_num_ctx: state
            .settings
            .ollama_num_ctx_per_model
            .get(&state.session.model_id)
            .copied()
            .or_else(|| {
                state
                    .runtime
                    .ollama_converged_num_ctx
                    .get(&state.session.model_id)
                    .copied()
            }),
        // Live offload toggle — carry the current setting so `/context offload`
        // applies next turn without rebuilding the (startup-frozen) provider.
        ollama_allow_ram_offload: Some(state.settings.ollama.allow_ram_offload),
        // Filled by the effect layer from cache-first live discovery just
        // before dispatch — the reducer never awaits a probe.
        resolved_context_window: None,
        resolved_max_output: None,
        output_schema: state.output_schema.clone(),
        // Pause auto-compaction after a failed attempt (cleared by a successful
        // compaction, manual /compact, or a conversation switch). Rides on the
        // request because the effect preflight never sees RuntimeState.
        suppress_auto_compact: state.runtime.auto_compact_suppressed,
    }
}

fn system_prompt_for_state(state: &State) -> String {
    let base = state
        .settings
        .prompt
        .render_system_prompt(&get_system_prompt());
    // While a plan is being drafted the live-mode line would mislead ("attempt
    // gated actions") — the effective policy is the plan-mode read-only floor.
    let safety_line = match &state.session.plan {
        Some(_) => format!(
            "Safety mode: plan (a plan is being drafted; the plan-mode read-only floor is in \
             effect; leaving plan mode restores {}).",
            state.session.safety_mode.as_str()
        ),
        None => format!(
            "Safety mode: {} (live — the user can switch it anytime with Shift+Tab or /safety; \
             trust this over any earlier tool error, and attempt gated actions rather than \
             assuming they will fail).",
            state.session.safety_mode.as_str()
        ),
    };
    let mut prompt = format!(
        "{}\n\n## Current Session\nCurrent working directory: {}\n{}\nTreat this as the project root unless the user specifies a different path.",
        base,
        state.cwd.display(),
        safety_line
    );
    // The concrete path (the static prompt only describes the mechanism);
    // absent before `Msg::ScratchpadReady` lands or when creation failed.
    if let Some(scratch) = &state.session.scratchpad {
        prompt.push_str(&format!(
            "\nScratchpad directory: {}\nUse it for ALL temporary files instead of /tmp or the system temp dir.",
            scratch.display()
        ));
    }
    if state.session.is_subagent {
        prompt.push_str("\n\n");
        prompt.push_str(crate::prompts::SUBAGENT_CONTRACT);
    }
    if let Some(preamble) = &state.session.agent_preamble {
        prompt.push_str("\n\n");
        prompt.push_str(preamble);
    }
    if let Some(plan) = &state.session.plan {
        prompt.push_str("\n\n");
        prompt.push_str(
            &crate::prompts::PLAN_MODE_PROMPT
                .replace("{plan_path}", &plan.plan_path.display().to_string())
                .replace(
                    "{plan_capabilities}",
                    &plan_capabilities_line(&state.settings.plan.permissions),
                ),
        );
    }
    prompt
}

/// Compose the "what runs while planning" sentence from the LIVE permission
/// profile, so the prompt never promises a capability the gate will deny
/// (`/plan config` can retune the profile mid-session).
fn plan_capabilities_line(perms: &crate::app::PlanPermissions) -> String {
    use crate::app::PlanPermLevel as L;
    // Read-only subagent fan-out is always allowed under the plan-mode floor
    // (policy_gate leaves the Subagent Allow untouched) — without naming it,
    // "everything else is blocked" suppresses legitimate parallel exploration.
    let mut parts =
        vec!["reads and inspection (including spawning read-only subagents)".to_string()];
    let mut push = |label: &str, level: L| match level {
        L::Allow => parts.push(label.to_string()),
        L::Auto | L::Ask => parts.push(format!("{label} (each use is reviewed first)")),
        L::Deny => {},
    };
    push(
        "known-safe build and test commands (cargo check/build/test/clippy, go build/test/vet, npm test, make test, and similar)",
        perms.builds,
    );
    push("web search/fetch", perms.web);
    push("memory writes", perms.memory);
    let mut line = parts.join(", ");
    line.push_str(", and edits to the plan file ONLY.");
    line
}

/// Walk the message log and retain only the `MAX_RETAINED_SCREENSHOTS`
/// most recent images across the whole conversation. Older messages
/// that had images get `images: None` AND an appended
/// `[Image elided — superseded by newer screenshot]` marker in
/// `content`, so the model still knows something visual was there.
///
/// Why: an agentic GUI loop can generate 10+ screenshots in a single
/// session. At 2MB/PNG that's 20MB uncompressed in every outgoing
/// request body — request bloat compounds across turns and slows the
/// model. The on-screen chat history still shows all images (this
/// transformation is on the CLONED Vec passed to the provider); only
/// the wire payload is slimmed.
fn evict_stale_screenshots(mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    use crate::constants::MAX_RETAINED_SCREENSHOTS;
    let mut seen = 0usize;
    for msg in messages.iter_mut().rev() {
        let Some(imgs) = msg.images.as_ref() else {
            continue;
        };
        if imgs.is_empty() {
            continue;
        }
        if seen < MAX_RETAINED_SCREENSHOTS {
            seen += imgs.len();
            continue;
        }
        // Beyond the cap — elide.
        let elided_count = imgs.len();
        msg.images = None;
        let marker = if elided_count == 1 {
            "\n[Image elided — superseded by newer screenshot]"
        } else {
            "\n[Images elided — superseded by newer screenshots]"
        };
        if !msg.content.ends_with(marker) {
            msg.content.push_str(marker);
        }
    }
    messages
}

/// The user loosening the safety mode (e.g. read_only → full_access) leaves the
/// *old* read-only denials sitting in the conversation history, still asserting
/// verbatim that mutations are blocked. A model trusts those concrete
/// tool-results over the (correct, live) system-prompt line and so refuses to
/// act or claims the runtime "is still read-only". Rewrite each superseded
/// read-only denial to a past-tense, mode-aware note so the wire history stops
/// contradicting the current mode.
///
/// Scope guards keep this surgical:
/// - only tool-result messages (`MessageRole::Tool`) are considered, so a user
///   message or model turn that merely quotes the phrase is untouched;
/// - the match is the *contiguous* denial signature (`blocked by policy:` +
///   [`crate::runtime::READ_ONLY_DENIAL_MARKER`]), so a `grep` hit that happens
///   to contain the marker text is not rewritten;
/// - it is a no-op in read_only (the denials still apply) and self-corrects if
///   the user toggles back down.
///
/// Runs on the CLONED request vec (like [`evict_stale_screenshots`]); the
/// on-screen transcript is untouched, and only `content` changes so the
/// tool_use/tool_result pairing is preserved.
fn neutralize_superseded_policy_denials(
    messages: &mut [ChatMessage],
    mode: crate::runtime::SafetyMode,
) {
    use crate::runtime::SafetyMode;
    if mode == SafetyMode::ReadOnly {
        return;
    }
    let signature = readonly_denial_signature();
    for msg in messages.iter_mut() {
        if msg.role != MessageRole::Tool || !msg.content.contains(&signature) {
            continue;
        }
        // Keep the action summary (everything before " blocked by policy: ") so
        // the model still knows WHAT was blocked; drop the standing-rule reason.
        let summary = msg
            .content
            .split_once(" blocked by policy: ")
            .map(|(head, _)| head.trim_end())
            .filter(|head| !head.is_empty())
            .unwrap_or("The action");
        msg.content = format!(
            "{summary} was blocked earlier while safety mode was read_only. \
             Safety mode is now {} — that restriction no longer applies; \
             re-run it if it is still needed.",
            mode.as_str()
        );
    }
}

/// The `content` infix that marks a persisted **read-only** policy denial: the
/// gate wraps a `PolicyDecision::Deny` as `"{summary} blocked by policy:
/// {reason}"`, and every read-only reason starts with
/// [`crate::runtime::READ_ONLY_DENIAL_MARKER`]. Matching the *contiguous* phrase
/// (not the bare marker) avoids rewriting a `grep` hit that merely contains the
/// marker text.
fn readonly_denial_signature() -> String {
    format!(
        "blocked by policy: {}",
        crate::runtime::READ_ONLY_DENIAL_MARKER
    )
}

/// True if the conversation still carries a read-only policy denial (a tool
/// result matching [`readonly_denial_signature`]) — used to decide whether a
/// loosening mode-switch is worth announcing.
fn history_has_readonly_denial(messages: &[ChatMessage]) -> bool {
    let signature = readonly_denial_signature();
    messages
        .iter()
        .any(|m| m.role == MessageRole::Tool && m.content.contains(&signature))
}

/// Content prefix of a [`safety_loosened_note`] — how
/// [`note_safety_mode_change`] recognizes its own pending nudge to retract it.
const SAFETY_NUDGE_PREFIX: &str = "Safety mode is now ";

/// One-line note injected for the model when the user leaves read_only while
/// stale read-only denials are in history, so it re-attempts gated actions
/// instead of trusting the old blocks. Stamped `RecoveryNudge`: hidden from the
/// transcript (the status bar already shows the mode) and swept once the
/// request it steers has gone out. Pairs with
/// `neutralize_superseded_policy_denials`, which rewrites the denials
/// themselves on every request.
fn safety_loosened_note(mode: crate::runtime::SafetyMode) -> String {
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
///   tighten back to read_only, would contradict the standing denials;
/// - (re-)injects one only while a leave-read_only event is pending: either
///   this switch leaves read_only, or a pending nudge proves an unsent earlier
///   leave (the user is still cycling, e.g. read_only → ask → auto).
///
/// A loosening long after read_only (no pending nudge) stays silent — the
/// per-request denial rewrite already covers it, and re-announcing on every
/// loosening step was the old bug.
fn note_safety_mode_change(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    previous: crate::runtime::SafetyMode,
    next: crate::runtime::SafetyMode,
) {
    use crate::models::ChatMessageKind;
    use crate::runtime::SafetyMode;
    let messages = &mut state.session.conversation.messages;
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

/// Word pools for the plan-file slug suffix. Indexed by a deterministic hash
/// of (conversation id, message count) — never the wall clock or an RNG — so
/// `--replay` allocates the identical path.
const PLAN_SLUG_ADJECTIVES: &[&str] = &[
    "amber", "bright", "calm", "deft", "eager", "fresh", "keen", "lucid", "mellow", "neat",
    "quiet", "sharp", "steady", "swift", "tidy", "vivid",
];
const PLAN_SLUG_NOUNS: &[&str] = &[
    "anchor", "beacon", "compass", "current", "delta", "harbor", "inlet", "lagoon", "pearl",
    "reef", "ripple", "shoal", "spring", "strand", "tide", "wake",
];

/// FNV-1a — tiny, dependency-free, deterministic across runs (unlike
/// `DefaultHasher`, whose seed is unspecified).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Allocate the plan-file path for a new plan: `.mermaid/plans/<topic>-<adj>-
/// <noun>.md` under the project root. The topic comes from the latest user
/// message (what the plan is about); the word-pair suffix keeps concurrent
/// sessions in one repo from colliding.
fn plan_path_for(state: &State) -> std::path::PathBuf {
    let topic_src = latest_user_intent(&state.session).unwrap_or_default();
    let words: Vec<String> = topic_src
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .map(|c| c.to_ascii_lowercase())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .take(4)
        .collect();
    let mut topic = if words.is_empty() {
        "plan".to_string()
    } else {
        words.join("-")
    };
    topic.truncate(topic.floor_char_boundary(40));
    let topic = topic.trim_end_matches('-');
    let seed = format!(
        "{}:{}",
        state.session.conversation.id,
        state.session.messages().len()
    );
    let h = fnv1a(seed.as_bytes());
    let adj = PLAN_SLUG_ADJECTIVES[(h % PLAN_SLUG_ADJECTIVES.len() as u64) as usize];
    let noun = PLAN_SLUG_NOUNS[((h >> 8) % PLAN_SLUG_NOUNS.len() as u64) as usize];
    state
        .cwd
        .join(".mermaid")
        .join("plans")
        .join(format!("{topic}-{adj}-{noun}.md"))
}

/// The plan-file path shown to the user: relative to the project root when it
/// is inside it (it always is today), absolute otherwise.
fn plan_path_display(state: &State, path: &std::path::Path) -> String {
    path.strip_prefix(&state.cwd)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Rows in the plan-config picker. Cross-checked against the widget's
/// `plan_config_rows` by a test in `render::widgets::plan_config`.
const PLAN_CONFIG_ROW_COUNT: usize = 9;

/// `/plan config` picker keys: ↑/↓ navigate, Enter/←/→ cycle the highlighted
/// value (persisting the `[plan]` table on every change), Esc closes.
fn handle_plan_config_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode) {
    let UiMode::PlanConfig { ref mut cursor } = state.ui.mode else {
        return;
    };
    match code {
        KeyCode::Up => {
            *cursor = cursor.saturating_sub(1);
        },
        KeyCode::Down => {
            *cursor = (*cursor + 1).min(PLAN_CONFIG_ROW_COUNT - 1);
        },
        KeyCode::Enter | KeyCode::Right => {
            let row = *cursor;
            cycle_plan_config_row(state, row, true);
            cmds.push(Cmd::PersistPlanConfig(state.settings.plan.clone()));
        },
        KeyCode::Left => {
            let row = *cursor;
            cycle_plan_config_row(state, row, false);
            cmds.push(Cmd::PersistPlanConfig(state.settings.plan.clone()));
        },
        KeyCode::Escape => {
            state.ui.mode = UiMode::EditingInput;
        },
        _ => {},
    }
}

/// Advance one picker row's value. Every change takes effect immediately —
/// the permission profile rides each tool dispatch, and the model/reasoning
/// overrides are read at the next plan-mode entry.
fn cycle_plan_config_row(state: &mut State, row: usize, forward: bool) {
    use crate::app::{PlanPermLevel as L, PlanPermissions, PlanPostApprove};
    fn cycle<T: Copy + PartialEq>(order: &[T], current: T, forward: bool) -> T {
        let idx = order.iter().position(|v| *v == current).unwrap_or(0);
        let len = order.len();
        let next = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        order[next]
    }
    const LEVELS: [L; 4] = [L::Allow, L::Auto, L::Ask, L::Deny];
    let session_model = state.session.model_id.clone();
    let plan = &mut state.settings.plan;
    match row {
        0 => {
            // Preset cycle; a custom profile snaps to the nearest preset
            // boundary (default going forward, open going back).
            let presets = [
                PlanPermissions::default(),
                PlanPermissions::strict(),
                PlanPermissions::open(),
            ];
            let idx = presets.iter().position(|p| *p == plan.permissions);
            plan.permissions = match (idx, forward) {
                (Some(i), true) => presets[(i + 1) % 3],
                (Some(i), false) => presets[(i + 2) % 3],
                (None, true) => presets[0],
                (None, false) => presets[2],
            };
        },
        1 => plan.permissions.builds = cycle(&LEVELS, plan.permissions.builds, forward),
        2 => plan.permissions.web = cycle(&LEVELS, plan.permissions.web, forward),
        3 => plan.permissions.memory = cycle(&LEVELS, plan.permissions.memory, forward),
        // Task tools are ungated (no approval path): allow/deny only.
        4 => {
            plan.permissions.tasks = if plan.permissions.tasks == L::Allow {
                L::Deny
            } else {
                L::Allow
            };
        },
        // Unset <-> pin to the current session model. Other models are set
        // by editing `[plan] model` in config.toml (the handoff picker is
        // the searchable surface).
        5 => {
            plan.model = match plan.model {
                Some(_) => None,
                None => Some(session_model),
            };
        },
        6 => {
            use crate::models::ReasoningLevel as R;
            const REASONING: [Option<R>; 8] = [
                None,
                Some(R::None),
                Some(R::Minimal),
                Some(R::Low),
                Some(R::Medium),
                Some(R::High),
                Some(R::XHigh),
                Some(R::Max),
            ];
            plan.reasoning = cycle(&REASONING, plan.reasoning, forward);
        },
        7 => plan.auto_approve = !plan.auto_approve,
        8 => {
            const POST: [Option<PlanPostApprove>; 3] = [
                None,
                Some(PlanPostApprove::Start),
                Some(PlanPostApprove::Wait),
            ];
            plan.post_approve = cycle(&POST, plan.post_approve, forward);
        },
        _ => {},
    }
}

/// The pure state flip of entering plan mode: allocate the plan file path,
/// set `session.plan`, retract stale nudges, persist. Shared by the
/// interactive entry (Alt+P / `/plan`) and the `enter_plan_mode` tool — the
/// tool path runs mid-batch, where appending a system message would
/// interleave between an assistant `tool_use` and its tool results, so THIS
/// helper never touches the message log. Returns the allocated path; `None`
/// when already planning or a subagent.
///
/// `session.safety_mode` is deliberately untouched — it is the restore target
/// (shown as "restores: <mode>"); tool dispatch floors the effective mode
/// while `session.plan` is `Some`.
fn enter_plan_mode_state(state: &mut State, cmds: &mut Vec<Cmd>) -> Option<std::path::PathBuf> {
    // Children explore, they don't plan (and have no user to approve).
    if state.session.is_subagent || state.session.plan.is_some() {
        return None;
    }
    let plan_path = plan_path_for(state);
    // The `[plan]` model/reasoning overrides: swap on entry, stash what to
    // restore. Per-request routing makes this safe — the very next dispatch
    // resolves the new model lazily.
    let mut prev_model_id = None;
    let mut prev_reasoning = None;
    if let Some(plan_model) = state.settings.plan.model.clone()
        && plan_model != state.session.model_id
    {
        prev_model_id = Some(std::mem::replace(
            &mut state.session.model_id,
            plan_model.clone(),
        ));
        state.runtime.set_model(&plan_model);
    }
    if let Some(plan_reasoning) = state.settings.plan.reasoning
        && plan_reasoning != state.session.reasoning
    {
        prev_reasoning = Some(std::mem::replace(
            &mut state.session.reasoning,
            plan_reasoning,
        ));
    }
    state.session.plan = Some(super::state::PlanState {
        plan_path: plan_path.clone(),
        prev_model_id,
        prev_reasoning,
    });
    // Retract any pending mode-change nudge: "re-attempt gated actions" would
    // steer the model wrong now that the read-only floor applies.
    state.session.conversation.messages.retain(|m| {
        m.kind != crate::models::ChatMessageKind::RecoveryNudge
            || (!m.content.starts_with(SAFETY_NUDGE_PREFIX)
                && !m.content.starts_with(PLAN_NUDGE_PREFIX))
    });
    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
    Some(plan_path)
}

/// Interactive plan-mode entry (Alt+P / `/plan`): the state flip only — the
/// status band announces the mode (it shows `plan mode on … restores: <mode>`
/// while `session.plan` is set), so no transcript row is added. The model
/// learns the mode + plan path from the system prompt.
fn enter_plan_mode(state: &mut State, cmds: &mut Vec<Cmd>) {
    if let Some(plan) = &state.session.plan {
        let path = plan_path_display(state, &plan.plan_path.clone());
        push_system(
            state,
            cmds,
            format!("Already in plan mode — plan file: {path} (Alt+P or /plan off leaves)"),
        );
        return;
    }
    enter_plan_mode_state(state, cmds);
}

/// Undo the `[plan]` model/reasoning overrides stashed at entry.
fn restore_plan_overrides(state: &mut State, plan: &super::state::PlanState) {
    if let Some(prev) = &plan.prev_model_id {
        state.session.model_id = prev.clone();
        state.runtime.set_model(prev);
    }
    if let Some(prev) = plan.prev_reasoning {
        state.session.reasoning = prev;
    }
}

/// Leave plan mode without an approval flow (Alt+P toggle / `/plan off`).
/// The effective safety mode reverts to `session.safety_mode` simply because
/// the floor stops applying.
fn exit_plan_mode(state: &mut State, cmds: &mut Vec<Cmd>) {
    let Some(plan) = state.session.plan.take() else {
        push_system(state, cmds, "Not in plan mode (Alt+P or /plan enters it)");
        return;
    };
    restore_plan_overrides(state, &plan);
    note_plan_mode_exit(state, cmds);
    // No transcript row: the status band reverting to the plain safety mode
    // is the announcement.
    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
}

/// The `content` infix that marks a persisted **plan-mode** policy denial.
/// Sibling of [`readonly_denial_signature`], keyed on
/// [`crate::runtime::PLAN_DENIAL_MARKER`].
fn plan_denial_signature() -> String {
    format!("blocked by policy: {}", crate::runtime::PLAN_DENIAL_MARKER)
}

/// True if the conversation still carries a plan-mode policy denial.
fn history_has_plan_denial(messages: &[ChatMessage]) -> bool {
    let signature = plan_denial_signature();
    messages
        .iter()
        .any(|m| m.role == MessageRole::Tool && m.content.contains(&signature))
}

/// Plan-mode sibling of [`neutralize_superseded_policy_denials`]: once plan
/// mode ends, its denials stop describing the live policy — rewrite them to a
/// past-tense note so the wire history stops contradicting the current mode.
/// No-op while planning (the denials still apply).
fn neutralize_superseded_plan_denials(messages: &mut [ChatMessage], plan_active: bool) {
    if plan_active {
        return;
    }
    let signature = plan_denial_signature();
    for msg in messages.iter_mut() {
        if msg.role != MessageRole::Tool || !msg.content.contains(&signature) {
            continue;
        }
        let summary = msg
            .content
            .split_once(" blocked by policy: ")
            .map(|(head, _)| head.trim_end())
            .filter(|head| !head.is_empty())
            .unwrap_or("The action");
        msg.content = format!(
            "{summary} was blocked earlier while a plan was being drafted. Plan \
             mode is now off — that restriction no longer applies; re-run it if \
             the plan calls for it."
        );
    }
}

/// Seed the checklist from the plan's Tasks section. Re-plan reconcile:
/// completed items survive (their subjects aren't re-seeded), everything
/// still open is replaced by the new plan's steps. `Stamp::default()` keeps
/// it replay-deterministic; the wholesale `SyncTaskStore` mirrors the
/// fork/clear reset path (the broker's `seed` doesn't publish — the reducer
/// already holds the truth).
fn seed_plan_tasks(state: &mut State, cmds: &mut Vec<Cmd>, body: &str) {
    let specs = super::plan::parse_plan_tasks(body);
    if specs.is_empty() {
        return;
    }
    use super::tasks::{Stamp, TaskEdit, TaskOrigin, TaskStatus};
    let mut store = state.session.conversation.tasks.clone();
    let completed: std::collections::HashSet<String> = store
        .visible()
        .filter(|t| t.status == TaskStatus::Completed)
        .map(|t| t.subject.trim().to_ascii_lowercase())
        .collect();
    let stale: Vec<TaskEdit> = store
        .visible()
        .filter(|t| t.status != TaskStatus::Completed)
        .map(|t| TaskEdit {
            id: t.id,
            status: Some(TaskStatus::Deleted),
            subject: None,
            active_form: None,
            description: None,
        })
        .collect();
    if !stale.is_empty() {
        store.apply(&stale, Stamp::default());
    }
    let fresh: Vec<_> = specs
        .into_iter()
        .filter(|s| !completed.contains(&s.subject.trim().to_ascii_lowercase()))
        .collect();
    if !fresh.is_empty() {
        store.create(fresh, TaskOrigin::Model, Stamp::default());
    }
    state.session.conversation.tasks = store.clone();
    cmds.push(Cmd::SyncTaskStore(store));
}

/// The user approved the plan into a NEW conversation (clear-context
/// execute, or an explicit handoff): persist the exploration transcript,
/// mint the next conversation (fork carries the transcript + checklist;
/// fresh starts from the plan alone), optionally switch models, seed the
/// checklist, and drive the kickoff turn.
fn handoff_plan_mode(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    body: &str,
    fresh: bool,
    fork: bool,
    model: Option<String>,
) {
    let Some(plan) = state.session.plan.take() else {
        return;
    };
    restore_plan_overrides(state, &plan);
    // The exploration conversation is history now — save it under its own id
    // BEFORE swapping (the fork_conversation_at ordering).
    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
    let original = &state.session.conversation;
    let mut next = crate::session::ConversationHistory::new(
        original.project_path.clone(),
        original.model_name.clone(),
        state.now,
    );
    // Millisecond-derived ids: bump deterministically on collision (the
    // rewind-fork precedent) so the two sessions never share a save file.
    if next.id == original.id {
        next = crate::session::ConversationHistory::new(
            original.project_path.clone(),
            original.model_name.clone(),
            state.now + chrono::Duration::milliseconds(1),
        );
    }
    if fork {
        next.title = original.title.clone();
        next.messages = original.messages.clone();
        next.input_history = original.input_history.clone();
        next.git_branch = original.git_branch.clone();
        next.tasks = original.tasks.clone();
        next.forked_from = Some(original.id.clone());
    }
    state.session.conversation = next;
    if let Some(model) = model {
        state.session.model_id = model.clone();
        state.runtime.set_model(&model);
    }
    seed_plan_tasks(state, cmds, body);
    // Fresh contexts get the handoff preamble + the plan as their opening
    // user message (the plan IS the brief); a fork already carries the plan
    // in its transcript, so a bare kickoff suffices.
    let kickoff = if fresh {
        format!(
            "{}

{}",
            crate::prompts::PLAN_HANDOFF_PREAMBLE,
            body
        )
    } else {
        "Implement the plan.".to_string()
    };
    state
        .ui
        .queued_messages
        .push_back(super::state::QueuedMessage {
            text: kickoff,
            attachment_ids: vec![],
        });
    drain_next_queued_message(state);
    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
}

/// Content prefix of the plan-mode-exit nudge — how [`enter_plan_mode`] and
/// [`note_plan_mode_exit`] recognize a pending one to retract it.
const PLAN_NUDGE_PREFIX: &str = "Plan mode is now off";

/// One-shot nudge injected when plan mode ends while plan-mode denials are in
/// history, so the model re-attempts gated actions instead of trusting the
/// old blocks. Pairs with [`neutralize_superseded_plan_denials`], which
/// rewrites the denials themselves on every request.
fn note_plan_mode_exit(state: &mut State, cmds: &mut Vec<Cmd>) {
    use crate::models::ChatMessageKind;
    state.session.conversation.messages.retain(|m| {
        m.kind != ChatMessageKind::RecoveryNudge || !m.content.starts_with(PLAN_NUDGE_PREFIX)
    });
    if history_has_plan_denial(state.session.messages()) {
        push_system_kind(
            state,
            cmds,
            format!(
                "{PLAN_NUDGE_PREFIX}; earlier plan-mode policy blocks no longer \
                 apply. Safety mode is {} — re-attempt gated actions instead of \
                 assuming they'll fail.",
                state.session.safety_mode.as_str()
            ),
            ChatMessageKind::RecoveryNudge,
        );
    }
}

/// Plan-tool post-processing at the tool boundary (`handle_tool_finished`):
/// flip the plan state so everything the follow-up model call derives —
/// system prompt, tool advertisement, dispatch flooring — sees the new mode.
/// Message-log appends are deliberately avoided here (a system message now
/// would interleave between the assistant `tool_use` and its tool results);
/// the transcript record is the rendered `ToolMetadata::Plan` block, and the
/// per-request denial neutralizer handles history hygiene.
/// Returns `true` when the outcome triggered a conversation handoff (fresh
/// or fork) — the caller must then abandon the executing turn instead of
/// appending its tool results into the NEW conversation.
fn plan_tool_transition(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    call_id: super::ids::ToolCallId,
    outcome: &ToolOutcome,
) -> bool {
    use super::plan::{ENTER_PLAN_MODE_TOOL, EXIT_PLAN_MODE_TOOL};
    let tool_name = match &state.turn {
        TurnState::ExecutingTools { calls, .. } => calls
            .iter()
            .find(|c| c.call_id == call_id)
            .map(|c| c.source.function.name.clone()),
        _ => None,
    };
    match tool_name.as_deref() {
        Some(name)
            if name == ENTER_PLAN_MODE_TOOL
                && outcome.status == crate::domain::ToolStatus::Success =>
        {
            enter_plan_mode_state(state, cmds);
        },
        Some(name) if name == EXIT_PLAN_MODE_TOOL => {
            if let crate::domain::ToolMetadata::Plan {
                body,
                start,
                fresh,
                fork,
                model,
                ..
            } = &outcome.metadata.detail
            {
                if *fresh || *fork || model.is_some() {
                    let (body, fresh, fork, model) = (body.clone(), *fresh, *fork, model.clone());
                    handoff_plan_mode(state, cmds, &body, fresh, fork, model);
                    return true;
                }
                let (body, start) = (body.clone(), *start);
                finish_plan_mode(state, cmds, &body, start);
            }
        },
        _ => {},
    }
    false
}

/// The user approved the plan (`exit_plan_mode` returned `ToolMetadata::
/// Plan`): leave plan mode, seed the checklist from the plan's Tasks section,
/// and optionally queue the implementation kickoff.
fn finish_plan_mode(state: &mut State, cmds: &mut Vec<Cmd>, body: &str, start: bool) {
    let Some(plan) = state.session.plan.take() else {
        // Stale or duplicate approval — nothing to transition.
        return;
    };
    restore_plan_overrides(state, &plan);
    // Retract any pending plan nudge; the per-request neutralizer rewrites
    // the denials themselves, and the tool result already tells the model
    // plan mode is off — no extra message here (see `plan_tool_transition`).
    state.session.conversation.messages.retain(|m| {
        m.kind != crate::models::ChatMessageKind::RecoveryNudge
            || !m.content.starts_with(PLAN_NUDGE_PREFIX)
    });
    seed_plan_tasks(state, cmds, body);
    cmds.push(Cmd::SaveConversation(state.session.snapshot_conversation()));
    if start {
        // Auto-submit implementation through the queued-message path (the
        // background-report precedent): the tool-boundary drain in
        // `handle_tool_finished` commits it before the follow-up model call,
        // so approval flows straight into implementation in one turn.
        state
            .ui
            .queued_messages
            .push_back(super::state::QueuedMessage {
                text: "Implement the plan.".to_string(),
                attachment_ids: vec![],
            });
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
            chrono::Local::now(),
        )
    }

    #[test]
    fn focus_changed_toggles_terminal_unfocused() {
        let state = fresh_state();
        assert!(!state.ui.terminal_unfocused); // default: assume attended
        let (state, _) = update(state, Msg::FocusChanged(false));
        assert!(state.ui.terminal_unfocused);
        let (state, _) = update(state, Msg::FocusChanged(true));
        assert!(!state.ui.terminal_unfocused);
    }

    #[test]
    fn keyboard_scroll_publishes_deltas_and_end_jumps() {
        let key = |code, modifiers| Msg::Key(Key { code, modifiers });
        let (state, _) = update(fresh_state(), key(KeyCode::PageUp, KeyMods::default()));
        assert_eq!(state.ui.mouse_scroll_accum, 10);
        let (state, _) = update(state, key(KeyCode::PageDown, KeyMods::default()));
        assert_eq!(state.ui.mouse_scroll_accum, 0);
        let shift = KeyMods {
            shift: true,
            ..Default::default()
        };
        let (state, _) = update(state, key(KeyCode::Up, shift));
        assert_eq!(state.ui.mouse_scroll_accum, 1);
        let before = state.ui.scroll_to_bottom_seq;
        let (state, _) = update(state, key(KeyCode::End, KeyMods::default()));
        assert_eq!(state.ui.scroll_to_bottom_seq, before + 1);
    }

    /// Type each char of `text` through the real reducer.
    fn type_text(mut state: State, text: &str) -> (State, Vec<Cmd>) {
        let mut all_cmds = Vec::new();
        for c in text.chars() {
            let (next, cmds) = update(
                state,
                Msg::Key(Key {
                    code: KeyCode::Char(c),
                    modifiers: KeyMods::default(),
                }),
            );
            state = next;
            all_cmds.extend(cmds);
        }
        (state, all_cmds)
    }

    fn plain_key(code: KeyCode) -> Msg {
        Msg::Key(Key {
            code,
            modifiers: KeyMods::default(),
        })
    }

    #[test]
    fn file_picker_opens_on_at_and_walks_once() {
        let (state, cmds) = type_text(fresh_state(), "look at @");
        assert!(state.ui.file_picker_open(), "@ opens the picker");
        assert!(state.ui.project_files_loading);
        let walks = cmds
            .iter()
            .filter(|c| matches!(c, Cmd::ListProjectFiles))
            .count();
        assert_eq!(walks, 1, "open fires exactly one walk");
        // Further filtering while the walk is in flight never re-fires.
        let (state, cmds) = type_text(state, "src");
        assert!(state.ui.file_picker_open());
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::ListProjectFiles)),
            "in-flight walk dedupes"
        );
        let _ = state;
    }

    #[test]
    fn file_picker_ranks_listed_files_and_completes_with_tab() {
        let (state, _) = type_text(fresh_state(), "@ma");
        let (state, _) = update(
            state,
            Msg::ProjectFilesListed(vec!["docs/notes.md".to_string(), "src/main.rs".to_string()]),
        );
        assert_eq!(state.ui.file_picker_matches, vec!["src/main.rs"]);
        let (state, _) = update(state, plain_key(KeyCode::Tab));
        assert_eq!(state.ui.input_buffer, "@src/main.rs ");
        assert_eq!(state.ui.input_cursor, state.ui.input_buffer.len());
        assert!(
            !state.ui.file_picker_open(),
            "the trailing space closes the token"
        );
    }

    #[test]
    fn ctrl_j_inserts_newline_at_cursor_without_submitting() {
        let (mut state, _) = type_text(fresh_state(), "line one");
        // Move the cursor mid-buffer to prove insertion happens at the
        // cursor, not the end.
        state.ui.input_cursor = 4;
        let (state, cmds) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char('j'),
                modifiers: KeyMods::ctrl(),
            }),
        );
        assert_eq!(state.ui.input_buffer, "line\n one");
        assert_eq!(state.ui.input_cursor, 5, "cursor lands after the newline");
        assert!(
            state.session.messages().is_empty(),
            "Ctrl+J must never submit"
        );
        assert!(cmds.is_empty(), "newline insert is reducer-only");
    }

    #[test]
    fn ctrl_j_outside_editing_input_is_ignored() {
        let (mut state, _) = type_text(fresh_state(), "draft");
        state.ui.mode = UiMode::ModelList;
        let (state, _) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char('j'),
                modifiers: KeyMods::ctrl(),
            }),
        );
        assert_eq!(
            state.ui.input_buffer, "draft",
            "pickers must not receive a stray newline"
        );
    }

    #[test]
    fn shift_enter_submits_like_plain_enter() {
        let (state, _) = type_text(fresh_state(), "hello there");
        let (state, _) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Enter,
                modifiers: KeyMods {
                    shift: true,
                    ..KeyMods::NONE
                },
            }),
        );
        assert!(
            state.ui.input_buffer.is_empty(),
            "Shift+Enter submits — Ctrl+J is the only newline chord"
        );
        assert_eq!(state.session.messages().len(), 1);
    }

    #[test]
    fn file_picker_enter_completes_instead_of_submitting() {
        let (state, _) = type_text(fresh_state(), "@ma");
        let (state, _) = update(
            state,
            Msg::ProjectFilesListed(vec!["src/main.rs".to_string()]),
        );
        let (state, _) = update(state, plain_key(KeyCode::Enter));
        assert_eq!(
            state.ui.input_buffer, "@src/main.rs ",
            "Enter completes the mention"
        );
        assert!(
            state.session.messages().is_empty(),
            "Enter with the picker open must NOT submit the prompt"
        );
    }

    #[test]
    fn file_picker_esc_dismisses_and_typing_reopens() {
        let (state, _) = type_text(fresh_state(), "@ma");
        let (state, _) = update(
            state,
            Msg::ProjectFilesListed(vec!["src/main.rs".to_string()]),
        );
        let buffer_before = state.ui.input_buffer.clone();
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        assert!(!state.ui.file_picker_open(), "Esc dismisses");
        assert_eq!(
            state.ui.input_buffer, buffer_before,
            "Esc leaves the typed text untouched"
        );
        let (state, _) = type_text(state, "i");
        assert!(state.ui.file_picker_open(), "typing reopens the picker");
    }

    #[test]
    fn file_picker_never_opens_on_slash_commands_or_emails() {
        let (state, cmds) = type_text(fresh_state(), "/load @x");
        assert!(!state.ui.file_picker_open(), "slash palette owns `/` input");
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::ListProjectFiles)));
        let (state, cmds) = type_text(fresh_state(), "mail user@host");
        assert!(!state.ui.file_picker_open(), "user@host is not a mention");
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::ListProjectFiles)));
        let _ = state;
    }

    #[test]
    fn file_picker_arrow_keys_move_the_cursor_without_editing() {
        let (state, _) = type_text(fresh_state(), "@s");
        let (state, _) = update(
            state,
            Msg::ProjectFilesListed(vec![
                "src/a.rs".to_string(),
                "src/b.rs".to_string(),
                "src/c.rs".to_string(),
            ]),
        );
        let (state, _) = update(state, plain_key(KeyCode::Down));
        assert_eq!(state.ui.file_picker_cursor, Some(1));
        assert_eq!(state.ui.input_buffer, "@s", "Down never edits the buffer");
        let (state, _) = update(state, plain_key(KeyCode::Up));
        assert_eq!(state.ui.file_picker_cursor, Some(0));
    }

    /// A conversation with two full user/assistant exchanges.
    fn state_with_two_exchanges() -> State {
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("first prompt"), state.now);
        state
            .session
            .append(ChatMessage::assistant("first reply"), state.now);
        state
            .session
            .append(ChatMessage::user("second prompt"), state.now);
        state
            .session
            .append(ChatMessage::assistant("second reply"), state.now);
        state
    }

    #[test]
    fn double_esc_opens_rewind_picker_single_esc_only_arms() {
        let mut state = state_with_two_exchanges();
        let (state2, _) = update(state.clone(), plain_key(KeyCode::Escape));
        assert!(state2.ui.esc_armed_at.is_some(), "first Esc arms");
        assert!(matches!(state2.ui.mode, UiMode::EditingInput));
        let (state3, _) = update(state2, plain_key(KeyCode::Escape));
        let UiMode::RewindPicker { candidates, cursor } = &state3.ui.mode else {
            panic!("second Esc within the window opens the picker");
        };
        assert_eq!(cursor, &0);
        assert_eq!(candidates.len(), 2, "only user messages are candidates");
        assert_eq!(candidates[0].excerpt, "second prompt", "newest first");
        assert_eq!(candidates[0].message_index, 2);
        assert_eq!(candidates[1].message_index, 0);
        // Past the window: the press RE-ARMS instead of firing.
        state.ui.esc_armed_at = Some(state.now - chrono::Duration::milliseconds(1500));
        let (state4, _) = update(state, plain_key(KeyCode::Escape));
        assert!(matches!(state4.ui.mode, UiMode::EditingInput));
        assert_eq!(state4.ui.esc_armed_at, Some(state4.now), "re-armed");
    }

    #[test]
    fn busy_esc_cancels_and_never_arms() {
        let mut state = state_with_two_exchanges();
        state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
        let (state, cmds) = update(state, plain_key(KeyCode::Escape));
        assert!(
            matches!(state.turn, TurnState::Cancelling { .. }),
            "busy Esc stays the cancel gesture"
        );
        assert!(state.ui.esc_armed_at.is_none(), "busy Esc never arms");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::CancelScope(_))));
    }

    #[test]
    fn any_other_key_disarms_rewind() {
        let state = state_with_two_exchanges();
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        assert!(state.ui.esc_armed_at.is_some());
        let (state, _) = update(state, plain_key(KeyCode::Char('x')));
        assert!(state.ui.esc_armed_at.is_none(), "typing disarms");
    }

    #[test]
    fn double_esc_with_no_user_messages_is_a_noop() {
        let state = fresh_state();
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        assert!(
            matches!(state.ui.mode, UiMode::EditingInput),
            "nothing to rewind to"
        );
    }

    #[test]
    fn rewind_picker_esc_dismisses_without_mutation() {
        let state = state_with_two_exchanges();
        let original_id = state.session.conversation.id.clone();
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        assert!(matches!(state.ui.mode, UiMode::RewindPicker { .. }));
        let (state, cmds) = update(state, plain_key(KeyCode::Escape));
        assert!(matches!(state.ui.mode, UiMode::EditingInput));
        assert_eq!(state.session.conversation.id, original_id);
        assert_eq!(state.session.messages().len(), 4, "history untouched");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))),
            "dismiss saves nothing"
        );
    }

    #[test]
    fn rewind_enter_forks_with_lineage_and_prefilled_composer() {
        let state = state_with_two_exchanges();
        let original_id = state.session.conversation.id.clone();
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        // Cursor 0 = "second prompt" (message_index 2).
        let (state, cmds) = update(state, plain_key(KeyCode::Enter));
        let fork = &state.session.conversation;
        assert_ne!(fork.id, original_id, "the fork gets a NEW session id");
        assert_eq!(fork.forked_from.as_deref(), Some(original_id.as_str()));
        assert_eq!(fork.parent_session.as_deref(), Some(original_id.as_str()));
        assert_eq!(
            fork.messages.len(),
            2,
            "fork keeps only the prefix before the selected message"
        );
        assert_eq!(fork.messages[0].content, "first prompt");
        assert_eq!(fork.messages[1].content, "first reply");
        assert_eq!(
            state.ui.input_buffer, "second prompt",
            "composer pre-filled with the selected message"
        );
        assert_eq!(state.ui.input_cursor, state.ui.input_buffer.len());
        assert!(matches!(state.ui.mode, UiMode::EditingInput));
        let saves = cmds
            .iter()
            .filter(|c| matches!(c, Cmd::SaveConversation(_)))
            .count();
        assert_eq!(saves, 2, "original saved first, then the fork");
        // The FIRST save carries the ORIGINAL (full history, its own id).
        let Some(Cmd::SaveConversation(first_saved)) =
            cmds.iter().find(|c| matches!(c, Cmd::SaveConversation(_)))
        else {
            unreachable!()
        };
        assert_eq!(first_saved.id, original_id);
        assert_eq!(first_saved.messages.len(), 4);
    }

    #[test]
    fn rewind_to_first_message_yields_empty_fork_prefix() {
        let state = state_with_two_exchanges();
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        // Move to the OLDEST candidate ("first prompt", message_index 0).
        let (state, _) = update(state, plain_key(KeyCode::Down));
        let (state, _) = update(state, plain_key(KeyCode::Enter));
        assert!(state.session.messages().is_empty());
        assert_eq!(state.ui.input_buffer, "first prompt");
    }

    #[test]
    fn fork_id_is_a_pure_function_of_the_injected_clock() {
        let build = || {
            let mut s = state_with_two_exchanges();
            s.now = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05+00:00")
                .unwrap()
                .with_timezone(&chrono::Local);
            let (s, _) = update(s, plain_key(KeyCode::Escape));
            let (s, _) = update(s, plain_key(KeyCode::Escape));
            let (s, _) = update(s, plain_key(KeyCode::Enter));
            s.session.conversation.id.clone()
        };
        assert_eq!(build(), build(), "replay-exact fork ids");
    }

    #[test]
    fn rewind_restages_the_selected_messages_images() {
        let mut state = state_with_two_exchanges();
        let mut msg = ChatMessage::user("look at [Image #1]");
        // 1x1 PNG-ish payload; content only needs to round-trip base64.
        msg.images = Some(vec![base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"fake-image-bytes",
        )]);
        msg.image_numbers = Some(vec![1]);
        state.session.append(msg, state.now);
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        let (state, _) = update(state, plain_key(KeyCode::Escape));
        // Cursor 0 = the newest user message (the image one).
        let (state, cmds) = update(state, plain_key(KeyCode::Enter));
        assert_eq!(state.ui.input_buffer, "look at [Image #1]");
        assert_eq!(state.ui.attachments.len(), 1, "image re-staged");
        assert_eq!(state.ui.attachments[0].number, 1, "original number kept");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::WriteImageToTemp { .. })),
            "temp preview rewritten for the re-staged image"
        );
    }

    #[test]
    fn rewind_candidates_skip_non_user_and_non_normal_messages() {
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("real"), state.now);
        state
            .session
            .append(ChatMessage::system("system note"), state.now);
        let mut summary = ChatMessage::user("summary-ish");
        summary.kind = crate::models::ChatMessageKind::RunSummary;
        state.session.append(summary, state.now);
        let candidates = rewind_candidates(state.session.messages());
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].excerpt, "real");
    }

    #[test]
    fn desired_title_reflects_run_state() {
        let mut state = fresh_state();
        // Idle: a mermaid title, but not the "working" status.
        assert!(desired_title(&state).starts_with("mermaid"));
        assert!(!desired_title(&state).contains("working"));
        // Active: the working status.
        state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
        assert_eq!(desired_title(&state), "mermaid · working");
    }

    #[test]
    fn help_text_lists_keyboard_shortcuts() {
        let help = help_text(&[]);
        assert!(help.contains("Keyboard shortcuts:"));
        assert!(help.contains("PageUp"));
    }

    #[test]
    fn evict_stale_screenshots_retains_most_recent_and_elides_rest() {
        use crate::constants::MAX_RETAINED_SCREENSHOTS;
        let mut msgs = Vec::new();
        for i in 0..(MAX_RETAINED_SCREENSHOTS + 3) {
            msgs.push(ChatMessage {
                role: MessageRole::Assistant,
                content: format!("turn {}", i),
                timestamp: chrono::Local::now(),
                kind: crate::models::ChatMessageKind::Normal,
                metadata: None,
                actions: vec![],
                thinking: None,
                images: Some(vec![format!("png-base64-{}", i)]),
                image_numbers: None,
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                provider_continuation: None,
            });
        }
        let out = super::evict_stale_screenshots(msgs);
        // Last MAX_RETAINED_SCREENSHOTS entries still carry images.
        for m in out.iter().rev().take(MAX_RETAINED_SCREENSHOTS) {
            assert!(m.images.is_some(), "most-recent images must survive");
        }
        // Everything before the cap is elided.
        for m in out.iter().rev().skip(MAX_RETAINED_SCREENSHOTS) {
            assert!(m.images.is_none(), "older images must be elided");
            assert!(
                m.content.contains("elided"),
                "elision marker must land in content"
            );
        }
    }

    #[test]
    fn evict_stale_screenshots_preserves_messages_without_images() {
        use crate::constants::MAX_RETAINED_SCREENSHOTS;
        // 5 text-only + 2 with images (under the cap) — nothing should
        // be elided.
        let mut msgs = Vec::new();
        for i in 0..5 {
            msgs.push(ChatMessage {
                role: MessageRole::User,
                content: format!("text only {}", i),
                timestamp: chrono::Local::now(),
                kind: crate::models::ChatMessageKind::Normal,
                metadata: None,
                actions: vec![],
                thinking: None,
                images: None,
                image_numbers: None,
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                provider_continuation: None,
            });
        }
        for i in 0..2 {
            msgs.push(ChatMessage {
                role: MessageRole::Assistant,
                content: format!("with image {}", i),
                timestamp: chrono::Local::now(),
                kind: crate::models::ChatMessageKind::Normal,
                metadata: None,
                actions: vec![],
                thinking: None,
                images: Some(vec![format!("png-{}", i)]),
                image_numbers: None,
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                provider_continuation: None,
            });
        }
        const { assert!(2 < MAX_RETAINED_SCREENSHOTS, "test premise") };
        let out = super::evict_stale_screenshots(msgs);
        // All 7 messages unchanged.
        let with_images = out.iter().filter(|m| m.images.is_some()).count();
        assert_eq!(with_images, 2);
        assert!(!out.iter().any(|m| m.content.contains("elided")));
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

    fn ctrl_c() -> Msg {
        Msg::Key(Key {
            code: KeyCode::Char('c'),
            modifiers: KeyMods::ctrl(),
        })
    }

    /// Press-twice-to-exit: the first Ctrl+C arms the confirm window, the
    /// second press inside it exits.
    #[test]
    fn ctrl_c_on_idle_empty_input_arms_then_second_press_exits() {
        let state = fresh_state();
        let (state, cmds) = update(state, ctrl_c());
        assert!(!state.should_exit, "first press must not exit");
        assert!(cmds.is_empty(), "first press on idle is reducer-only");
        assert!(state.ui.exit_armed_until.is_some(), "first press arms");

        let (state, cmds) = update(state, ctrl_c());
        assert!(state.should_exit, "second press inside the window exits");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
    }

    /// First Ctrl+C with typed input clears the input (Claude Code parity)
    /// instead of exiting; the second press exits.
    #[test]
    fn ctrl_c_on_idle_with_input_clears_then_second_press_exits() {
        let mut state = fresh_state();
        state.ui.input_buffer = "partial".to_string();
        state.ui.input_cursor = 7;
        let (state, cmds) = update(state, ctrl_c());
        assert!(!state.should_exit);
        assert!(state.ui.input_buffer.is_empty(), "first press clears input");
        assert_eq!(state.ui.input_cursor, 0);
        assert!(cmds.is_empty());

        let (state, cmds) = update(state, ctrl_c());
        assert!(state.should_exit);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
    }

    /// A second Ctrl+C after the confirm window expired re-arms instead of
    /// exiting (lazy expiry against `state.now`).
    #[test]
    fn armed_exit_expires_after_window() {
        let state = fresh_state();
        let (mut state, _) = update(state, ctrl_c());
        assert!(state.ui.exit_armed_until.is_some());
        // Advance the injected clock past the deadline.
        state.now += chrono::Duration::seconds(crate::constants::UI_EXIT_CONFIRM_WINDOW_SECS + 1);
        let (state, cmds) = update(state, ctrl_c());
        assert!(!state.should_exit, "expired arm must re-arm, not exit");
        assert!(cmds.is_empty());
        assert!(state.ui.exit_armed_until.is_some(), "re-armed");
    }

    /// Any other key while armed disarms the exit confirmation.
    #[test]
    fn any_other_key_disarms_exit_confirmation() {
        let state = fresh_state();
        let (state, _) = update(state, ctrl_c());
        assert!(state.ui.exit_armed_until.is_some());
        let (state, _) = update(state, key(KeyCode::Char('x')));
        assert!(state.ui.exit_armed_until.is_none(), "typing disarms");
        let (state, _) = update(state, ctrl_c());
        assert!(
            !state.should_exit,
            "post-disarm Ctrl+C is a fresh first press"
        );
    }

    /// Ctrl+Shift+C is the copy chord (kitty-protocol terminals deliver the
    /// SHIFT bit) — it must never reach the quit/arm path.
    #[test]
    fn ctrl_shift_c_does_not_exit_or_arm() {
        let state = fresh_state();
        let msg = Msg::Key(Key {
            code: KeyCode::Char('c'),
            modifiers: KeyMods {
                ctrl: true,
                shift: true,
                ..KeyMods::NONE
            },
        });
        let (state, cmds) = update(state, msg);
        assert!(!state.should_exit);
        assert!(
            state.ui.exit_armed_until.is_none(),
            "copy chord must not arm"
        );
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::Exit)));
    }

    /// Uppercase 'C' (however delivered) must not match the quit path either.
    #[test]
    fn ctrl_c_uppercase_never_exits() {
        let state = fresh_state();
        let msg = Msg::Key(Key {
            code: KeyCode::Char('C'),
            modifiers: KeyMods::ctrl(),
        });
        let (state, cmds) = update(state, msg);
        assert!(!state.should_exit);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::Exit)));
    }

    /// Tool stdout progress lines must NOT append a chat message. Surfacing
    /// every progress line (build output, pids, streamed file contents) as UI
    /// would be noise; the status line names the running tool and the full
    /// output lands in chat only when the tool finishes.
    #[test]
    fn tool_progress_output_does_not_append_message() {
        use crate::providers::ProgressEvent;
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
        let turn = state.current_turn_id().unwrap();

        let (state, _cmds) = update(
            state,
            Msg::ToolProgress {
                turn,
                call_id: super::super::ids::ToolCallId(1),
                event: ProgressEvent::Output(
                    "drwxrwxr-x  3 nsabaj nsabaj 4096 Mar 30 14:02 .mermaid".to_string(),
                ),
            },
        );
        assert!(
            state.session.messages().is_empty(),
            "tool stdout must not append a chat message"
        );
    }

    /// F14: Ctrl+V in the chat input emits `Cmd::ReadClipboard`. The
    /// reducer stays pure — the actual clipboard read runs off-thread
    /// in the effect runner.
    #[test]
    fn ctrl_v_in_editing_input_emits_read_clipboard() {
        let state = fresh_state();
        assert!(matches!(state.ui.mode, UiMode::EditingInput));
        let (_, cmds) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char('v'),
                modifiers: KeyMods::ctrl(),
            }),
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::ReadClipboard)),
            "Ctrl+V should dispatch Cmd::ReadClipboard; got tags: {:?}",
            cmds.iter().map(|c| c.tag()).collect::<Vec<_>>(),
        );
    }

    /// F14: Ctrl+V while a confirmation modal is open should NOT
    /// hijack the keystroke — the user might be mid-confirmation and
    /// accidentally paste into dismissed UI. Gated out.
    #[test]
    fn ctrl_v_with_confirm_modal_open_is_noop() {
        let mut state = fresh_state();
        state.confirm = Some(super::super::state::Confirmation {
            prompt: "Clear conversation history?".to_string(),
            accept_msg_token: super::super::state::ConfirmationTarget::ClearConversation,
        });
        let (_, cmds) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char('v'),
                modifiers: KeyMods::ctrl(),
            }),
        );
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::ReadClipboard)));
    }

    /// F14: Ctrl+V in the conversation-list picker must not trigger
    /// a clipboard read. The picker has its own key handling.
    #[test]
    fn ctrl_v_in_conversation_list_mode_is_noop() {
        let mut state = fresh_state();
        state.ui.mode = UiMode::ConversationList {
            candidates: Vec::new(),
            cursor: 0,
        };
        let (_, cmds) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char('v'),
                modifiers: KeyMods::ctrl(),
            }),
        );
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::ReadClipboard)));
    }

    // ── Paste-race guard: Ctrl+V clipboard read vs. a fast Enter ────────

    /// Ctrl+V marks a clipboard read in flight so a racing Enter can wait for it.
    #[test]
    fn ctrl_v_marks_a_clipboard_read_pending() {
        let (state, _) = update(
            fresh_state(),
            Msg::Key(Key {
                code: KeyCode::Char('v'),
                modifiers: KeyMods::ctrl(),
            }),
        );
        assert_eq!(state.ui.clipboard_reads_pending, 1);
    }

    /// Enter while a clipboard read is still in flight must NOT submit: it holds
    /// the submit (so the racing paste isn't dropped) and leaves the buffer intact.
    #[test]
    fn enter_while_clipboard_read_pending_holds_the_submit() {
        let mut state = fresh_state();
        for c in "hi".chars() {
            let (s, _) = update(state, key(KeyCode::Char(c)));
            state = s;
        }
        // Ctrl+V: a read is now pending.
        let (state, _) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char('v'),
                modifiers: KeyMods::ctrl(),
            }),
        );
        // Enter: held, not submitted.
        let (state, cmds) = update(state, key(KeyCode::Enter));
        assert!(state.ui.submit_after_clipboard, "submit is held");
        assert_eq!(
            state.ui.input_buffer, "hi",
            "buffer not consumed while held"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
            "no turn dispatched while a read is pending"
        );
        assert!(
            state
                .session
                .messages()
                .iter()
                .all(|m| m.role != MessageRole::User),
            "no user message sent while the read is pending"
        );
    }

    /// The full race: paste (read in flight) → Enter → the image lands. The held
    /// submit fires exactly once, includes the pasted image, and leaves no stray
    /// `[Image #N]` behind in the input.
    #[test]
    fn held_submit_fires_with_the_pasted_image_once_the_read_lands() {
        let mut state = fresh_state();
        for c in "look".chars() {
            let (s, _) = update(state, key(KeyCode::Char(c)));
            state = s;
        }
        let (state, _) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char('v'),
                modifiers: KeyMods::ctrl(),
            }),
        );
        let (state, _) = update(state, key(KeyCode::Enter));
        // The async clipboard read resolves with an image.
        let (state, cmds) = update(
            state,
            Msg::ClipboardRead(super::super::msg::ClipboardRead::Image {
                bytes: vec![0x89, 0x50, 0x4E, 0x47],
                format: "png".to_string(),
            }),
        );
        assert_eq!(state.ui.clipboard_reads_pending, 0);
        assert!(!state.ui.submit_after_clipboard, "held submit released");
        let msg = state
            .session
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .expect("the held submit fires once the image lands");
        assert_eq!(
            msg.images.as_ref().map(Vec::len),
            Some(1),
            "the pasted image is included, not dropped"
        );
        assert_eq!(msg.image_numbers, Some(vec![1]));
        assert!(msg.content.contains("look") && msg.content.contains("[Image #1]"));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
        assert!(
            state.ui.attachments.is_empty(),
            "attachment consumed by submit"
        );
        assert!(
            state.ui.input_buffer.is_empty(),
            "no stray token left in the input"
        );
    }

    /// An empty/failed clipboard read must still release a held submit (never
    /// wedge it): the typed text goes out, just without an image.
    #[test]
    fn empty_clipboard_read_releases_held_submit_without_an_image() {
        let mut state = fresh_state();
        for c in "just text".chars() {
            let (s, _) = update(state, key(KeyCode::Char(c)));
            state = s;
        }
        let (state, _) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char('v'),
                modifiers: KeyMods::ctrl(),
            }),
        );
        let (state, _) = update(state, key(KeyCode::Enter));
        let (state, _) = update(
            state,
            Msg::ClipboardRead(super::super::msg::ClipboardRead::Empty),
        );
        assert_eq!(state.ui.clipboard_reads_pending, 0);
        assert!(!state.ui.submit_after_clipboard);
        let msg = state
            .session
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .expect("held submit still fires on an empty read");
        assert!(msg.images.is_none(), "no image on an empty read");
        assert!(msg.content.contains("just text"));
    }

    /// A terminal bracketed paste (`Msg::Paste`) is NOT a Ctrl+V clipboard read:
    /// it must not touch the pending counter, which would otherwise let a stray
    /// paste prematurely release a held submit.
    #[test]
    fn bracketed_text_paste_does_not_touch_the_clipboard_counter() {
        let (state, _) = update(
            fresh_state(),
            Msg::Paste(super::super::msg::Paste::Text("pasted".to_string())),
        );
        assert_eq!(state.ui.clipboard_reads_pending, 0);
        assert_eq!(state.ui.input_buffer, "pasted");
    }

    /// Generic async feedback (`Msg::TransientStatus`, e.g. clipboard results)
    /// posts a system message into the chat transcript — there is no banner.
    #[test]
    fn transient_status_posts_to_chat_transcript() {
        let state = fresh_state();
        let (state, cmds) = update(
            state,
            Msg::TransientStatus {
                text: "Clipboard is empty".to_string(),
            },
        );
        let last = state
            .session
            .messages()
            .last()
            .expect("a transcript message was appended");
        assert!(last.content.contains("Clipboard is empty"));
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))),
            "the transcript message is persisted"
        );
    }

    // ── No-vision-model warning (Msg::ProviderVisionResolved) ───────────

    fn vision_resolved(model_id: &str, supports_vision: Option<bool>, warn: bool) -> Msg {
        Msg::ProviderVisionResolved {
            model_id: model_id.to_string(),
            supports_vision,
            warn,
        }
    }

    fn count_no_vision_notices(state: &State) -> usize {
        state
            .session
            .messages()
            .iter()
            .filter(|m| m.content.contains("no vision capability"))
            .count()
    }

    /// A no-vision model with an image in play warns exactly once per session.
    #[test]
    fn no_vision_model_warns_once() {
        // fresh_state's model is "ollama/test".
        let (state, cmds) = update(
            fresh_state(),
            vision_resolved("ollama/test", Some(false), true),
        );
        assert_eq!(
            count_no_vision_notices(&state),
            1,
            "one warning on first probe"
        );
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
        // A second probe for the same model must not nag again.
        let (state, _) = update(state, vision_resolved("ollama/test", Some(false), true));
        assert_eq!(
            count_no_vision_notices(&state),
            1,
            "warning is once-per-session"
        );
    }

    /// A vision-capable model refreshes the display snapshot but never warns.
    #[test]
    fn vision_capable_model_updates_snapshot_without_warning() {
        let state = fresh_state();
        assert!(
            !state.runtime.provider_capabilities.supports_vision,
            "ollama's static default is false"
        );
        let (state, _) = update(state, vision_resolved("ollama/test", Some(true), true));
        assert!(
            state.runtime.provider_capabilities.supports_vision,
            "snapshot refreshed to the probed value"
        );
        assert_eq!(count_no_vision_notices(&state), 0);
    }

    /// Unknown vision (`None` — non-Ollama or a failed probe) is ignored: no
    /// warning and no snapshot change.
    #[test]
    fn unknown_vision_is_ignored() {
        let (state, _) = update(fresh_state(), vision_resolved("ollama/test", None, true));
        assert!(
            !state.runtime.provider_capabilities.supports_vision,
            "snapshot untouched on unknown"
        );
        assert_eq!(count_no_vision_notices(&state), 0);
    }

    /// `warn: false` (no image in play) suppresses the nag even for a no-vision
    /// model — the probe is only keeping the snapshot honest.
    #[test]
    fn no_warn_flag_suppresses_the_nag() {
        let (state, _) = update(
            fresh_state(),
            vision_resolved("ollama/test", Some(false), false),
        );
        assert_eq!(count_no_vision_notices(&state), 0);
    }

    /// A probe that lands after a `/model` switch (model_id no longer matches the
    /// active model) is dropped — no warning for the model now in use.
    #[test]
    fn stale_vision_probe_is_dropped() {
        let (state, _) = update(
            fresh_state(),
            vision_resolved("ollama/previous", Some(false), true),
        );
        assert_eq!(count_no_vision_notices(&state), 0);
    }

    /// Staging a pasted image proactively probes vision so the warning can appear
    /// before the user sends.
    #[test]
    fn staging_an_image_probes_vision() {
        let (_, cmds) = update(
            fresh_state(),
            Msg::ClipboardRead(super::super::msg::ClipboardRead::Image {
                bytes: vec![1, 2, 3],
                format: "png".to_string(),
            }),
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::ProbeVision { warn: true, .. })),
            "pasting an image probes vision with warn=true"
        );
    }

    /// Switching models probes the new model's vision; it only arms the warning
    /// (`warn: true`) when an image is already staged.
    #[test]
    fn model_switch_probes_vision_and_arms_warning_only_with_staged_image() {
        // No image staged → probe with warn=false (snapshot refresh only).
        let (_, cmds) = update(
            fresh_state(),
            Msg::Slash(SlashCmd::Model(Some("ollama/other".to_string()))),
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::ProbeVision { warn: false, .. })),
            "switching with no staged image probes with warn=false"
        );
        // Stage an image, then switch → probe with warn=true.
        let (state, _) = update(
            fresh_state(),
            Msg::ClipboardRead(super::super::msg::ClipboardRead::Image {
                bytes: vec![9],
                format: "png".to_string(),
            }),
        );
        let (_, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Model(Some("ollama/other".to_string()))),
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::ProbeVision { warn: true, .. })),
            "switching with a staged image arms the warning"
        );
    }

    /// F14: a `Msg::ClipboardRead(Image)` (the Ctrl+V clipboard read result)
    /// creates an Attachment entry and emits Cmd::WriteImageToTemp. This is the
    /// existing contract; the test pins it so the Ctrl+V wiring has a
    /// known-good downstream to rely on.
    #[test]
    fn paste_image_creates_attachment_and_writes_temp() {
        let state = fresh_state();
        let (state, cmds) = update(
            state,
            Msg::ClipboardRead(super::super::msg::ClipboardRead::Image {
                bytes: vec![0x89, 0x50, 0x4E, 0x47], // PNG magic bytes
                format: "png".to_string(),
            }),
        );
        assert_eq!(state.ui.attachments.len(), 1);
        let att = &state.ui.attachments[0];
        assert_eq!(att.format, "png");
        assert_eq!(att.size_bytes, 4);
        // First paste mints global image #1, splices the inline pill into the
        // buffer, and advances the cursor past it.
        assert_eq!(att.number, 1);
        assert_eq!(state.ui.input_buffer, "[Image #1] ");
        assert_eq!(state.ui.input_cursor, "[Image #1] ".len());
        assert!(cmds.iter().any(|c| {
            matches!(c, Cmd::WriteImageToTemp { path, .. } if path == &att.temp_path)
        }));
    }

    #[test]
    fn atomic_backspace_deletes_whole_pill_and_its_attachment() {
        let (state, _) = update(
            fresh_state(),
            Msg::ClipboardRead(super::super::msg::ClipboardRead::Image {
                bytes: vec![1, 2, 3, 4],
                format: "png".to_string(),
            }),
        );
        assert_eq!(state.ui.input_buffer, "[Image #1] ");
        // Backspace #1: normal delete of the trailing space; pill intact.
        let (state, _) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Backspace,
                modifiers: KeyMods::default(),
            }),
        );
        assert_eq!(state.ui.input_buffer, "[Image #1]");
        assert_eq!(state.ui.attachments.len(), 1, "pill intact → image intact");
        // Backspace #2: cursor now abuts the pill → whole pill + image removed.
        let (state, _) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Backspace,
                modifiers: KeyMods::default(),
            }),
        );
        assert_eq!(state.ui.input_buffer, "");
        assert!(state.ui.attachments.is_empty(), "pill gone → image gone");
    }

    #[test]
    fn submit_sends_images_in_token_order_and_drops_orphans() {
        let mut state = fresh_state();
        state.ui.attachments.push(test_attachment(2)); // id 2, number 2
        state.ui.attachments.push(test_attachment(1)); // id 1, number 1
        // References #2 before #1, plus a phantom #9 that owns no attachment.
        let (state, _) = update(
            state,
            Msg::SubmitPrompt {
                text: "[Image #2] a [Image #1] b [Image #9]".to_string(),
                attachment_ids: vec![1, 2],
            },
        );
        let msg = state
            .session
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .expect("submitted user message");
        // First-appearance order (#2 then #1); the phantom #9 sends no image.
        assert_eq!(msg.image_numbers, Some(vec![2, 1]));
        assert_eq!(msg.images.as_ref().map(Vec::len), Some(2));
        assert!(
            state.ui.attachments.is_empty(),
            "owned attachments consumed / GC'd"
        );
    }

    #[test]
    fn submit_with_typed_literal_and_no_attachment_sends_no_image() {
        let (state, _) = update(
            fresh_state(),
            Msg::SubmitPrompt {
                text: "compare [Image #99] please".to_string(),
                attachment_ids: vec![],
            },
        );
        let msg = state
            .session
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .expect("submitted user message");
        assert!(msg.images.is_none());
        assert!(
            msg.content.contains("[Image #99]"),
            "the literal stays in the text"
        );
    }

    #[test]
    fn image_numbering_is_global_and_monotonic_across_messages() {
        // Message 1: paste (→ #1) and submit.
        let (state, _) = update(
            fresh_state(),
            Msg::ClipboardRead(super::super::msg::ClipboardRead::Image {
                bytes: vec![1],
                format: "png".to_string(),
            }),
        );
        assert_eq!(state.ui.attachments[0].number, 1);
        let text1 = state.ui.input_buffer.clone();
        let ids1: Vec<u64> = state.ui.attachments.iter().map(|a| a.id).collect();
        let (state, _) = update(
            state,
            Msg::SubmitPrompt {
                text: text1,
                attachment_ids: ids1,
            },
        );
        assert_eq!(
            state
                .session
                .messages()
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::User)
                .unwrap()
                .image_numbers,
            Some(vec![1])
        );
        // Message 2: the next paste keeps climbing to #2 (global, not per-message).
        let (state, _) = update(
            state,
            Msg::ClipboardRead(super::super::msg::ClipboardRead::Image {
                bytes: vec![2],
                format: "png".to_string(),
            }),
        );
        assert_eq!(state.ui.attachments[0].number, 2);
        assert_eq!(state.ui.input_buffer, "[Image #2] ");
    }

    #[test]
    fn resume_continues_image_numbering_past_transcript_max() {
        let mut state = fresh_state();
        let mut history = crate::session::ConversationHistory::new(
            "proj".to_string(),
            "model".to_string(),
            state.now,
        );
        history
            .messages
            .push(ChatMessage::user("look [Image #16]").with_image_numbers(vec![16]));
        state.seed_conversation(history);
        // A paste after resume continues past the transcript's #16 → #17, not #1.
        let (state, _) = update(
            state,
            Msg::ClipboardRead(super::super::msg::ClipboardRead::Image {
                bytes: vec![1],
                format: "png".to_string(),
            }),
        );
        assert_eq!(state.ui.attachments[0].number, 17);
        assert_eq!(state.ui.input_buffer, "[Image #17] ");
    }

    #[test]
    fn open_image_writes_and_opens_the_same_temp_path() {
        let mut state = fresh_state();
        let image =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"image bytes");
        state.session.append(
            ChatMessage::assistant("image").with_images(vec![image]),
            state.now,
        );

        let (_, cmds) = update(
            state,
            Msg::OpenImageAt {
                message_index: 0,
                image_index: 0,
                image_number: None,
            },
        );

        let write_path = cmds.iter().find_map(|cmd| match cmd {
            Cmd::WriteImageToTemp { path, .. } => Some(path.clone()),
            _ => None,
        });
        let open_path = cmds.iter().find_map(|cmd| match cmd {
            Cmd::OpenInSystem(path) => Some(path.clone()),
            _ => None,
        });
        assert_eq!(write_path, open_path);
    }

    #[test]
    fn open_image_resolves_by_global_number_over_stale_position() {
        // The click map indexes the DISPLAY transcript, which the continuation
        // stitch can shift away from committed history. The stable [Image #N]
        // number must win over a stale positional pair.
        use base64::Engine as _;
        let mut state = fresh_state();
        let first = base64::engine::general_purpose::STANDARD.encode(b"first image");
        let second = base64::engine::general_purpose::STANDARD.encode(b"second image");
        state.session.append(
            ChatMessage::user("a [Image #7]")
                .with_images(vec![first])
                .with_image_numbers(vec![7]),
            state.now,
        );
        state.session.append(
            ChatMessage::user("b [Image #9]")
                .with_images(vec![second.clone()])
                .with_image_numbers(vec![9]),
            state.now,
        );

        // Positional pair deliberately points at the FIRST message.
        let (_, cmds) = update(
            state,
            Msg::OpenImageAt {
                message_index: 0,
                image_index: 0,
                image_number: Some(9),
            },
        );

        let expected = base64::engine::general_purpose::STANDARD
            .decode(second)
            .unwrap();
        let written = cmds.iter().find_map(|cmd| match cmd {
            Cmd::WriteImageToTemp { bytes, .. } => Some(bytes.clone()),
            _ => None,
        });
        assert_eq!(
            written.as_deref(),
            Some(expected.as_slice()),
            "resolution follows the global number, not the display position"
        );
    }

    #[test]
    fn ctrl_c_during_turn_cancels_then_second_press_exits() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
        // First press: interrupt the running turn (like Esc), don't exit.
        let (state, cmds) = update(state, ctrl_c());
        assert!(!state.should_exit, "first press interrupts, not exits");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::CancelScope(TurnId(5))))
        );
        assert!(matches!(state.turn, TurnState::Cancelling { .. }));
        // Second press inside the window: exit for real.
        let (state, cmds) = update(state, ctrl_c());
        assert!(state.should_exit);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
    }

    #[test]
    fn cancel_and_reset_paths_clear_pending_question() {
        // RC-1 (D2/D3/D4): a parked `ask_user_question` modal must not survive a
        // turn cancel/reset — the tool task behind it is torn down, so the modal
        // would be permanently unanswerable. Every cancel/reset path clears it.
        use super::super::ids::ToolCallId;

        let parked = || {
            let mut state = fresh_state();
            state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
            let (state, _) = update(
                state,
                Msg::QuestionAsked {
                    turn: TurnId(5),
                    call_id: ToolCallId(1),
                    questions: vec![],
                },
            );
            assert_eq!(
                state.pending_question.len(),
                1,
                "precondition: a question is parked mid-turn"
            );
            state
        };

        // Esc / CancelTurn.
        let (state, _) = update(parked(), Msg::CancelTurn);
        assert!(
            state.pending_question.is_empty(),
            "CancelTurn must clear the parked question"
        );

        // Ctrl+C quit (request_exit).
        let (state, _) = update(
            parked(),
            Msg::Key(Key {
                code: KeyCode::Char('c'),
                modifiers: KeyMods::ctrl(),
            }),
        );
        assert!(
            state.pending_question.is_empty(),
            "Ctrl+C quit must clear the parked question"
        );

        // `/load` a conversation mid-turn.
        let history = fresh_state().session.conversation.clone();
        let (state, _) = update(parked(), Msg::ConversationLoaded(history));
        assert!(
            state.pending_question.is_empty(),
            "ConversationLoaded must clear the parked question"
        );

        // `/clear` (confirmed) mid-turn.
        let mut state = parked();
        state.confirm = Some(super::super::state::Confirmation {
            prompt: "Clear conversation history?".to_string(),
            accept_msg_token: super::super::state::ConfirmationTarget::ClearConversation,
        });
        let (state, _) = update(state, Msg::ConfirmAccepted);
        assert!(
            state.pending_question.is_empty(),
            "ClearConversation must clear the parked question"
        );
    }

    #[test]
    fn load_conversation_mid_turn_cancels_orphaned_scope() {
        // `/load` while a turn is generating must cancel the in-flight scope,
        // not silently overwrite `state.turn` and orphan the running tasks (#2).
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
        let history = fresh_state().session.conversation.clone();
        let (state, cmds) = update(state, Msg::ConversationLoaded(history));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::CancelScope(TurnId(5)))),
            "loading a conversation mid-turn must cancel the in-flight scope"
        );
        assert!(matches!(state.turn, TurnState::Idle));
    }

    #[test]
    fn load_conversation_when_idle_does_not_cancel() {
        // No in-flight turn → nothing to cancel; `/load` just swaps state.
        let state = fresh_state();
        let history = fresh_state().session.conversation.clone();
        let (state, cmds) = update(state, Msg::ConversationLoaded(history));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::CancelScope(_))));
        assert!(matches!(state.turn, TurnState::Idle));
    }

    #[test]
    fn scratchpad_ready_stamps_the_matching_session() {
        let state = fresh_state();
        let id = state.session.conversation.id.clone();
        let path = std::path::PathBuf::from("/data/tmp/scratchpad/-proj/x");
        let (state, cmds) = update(
            state,
            Msg::ScratchpadReady {
                session_id: id,
                path: path.clone(),
            },
        );
        assert_eq!(state.session.scratchpad.as_deref(), Some(path.as_path()));
        assert!(cmds.is_empty(), "stamping is silent");
    }

    #[test]
    fn scratchpad_ready_for_a_stale_session_is_dropped() {
        // A `/clear` or `/load` racing the effect's mkdir leaves a ready for
        // a discarded conversation id — it must not attach to the new one.
        let state = fresh_state();
        let (state, _) = update(
            state,
            Msg::ScratchpadReady {
                session_id: "some_other_session".to_string(),
                path: std::path::PathBuf::from("/data/tmp/scratchpad/-proj/stale"),
            },
        );
        assert_eq!(state.session.scratchpad, None);
    }

    #[test]
    fn clear_recomputes_the_scratchpad_for_the_new_conversation_id() {
        let mut state = fresh_state();
        let old_id = state.session.conversation.id.clone();
        state.session.scratchpad = Some(std::path::PathBuf::from("/data/tmp/scratchpad/-proj/old"));
        // Ids are minted from `state.now`; advance it so the cleared
        // conversation provably gets a different id than the original.
        state.now += chrono::Duration::seconds(1);
        state.confirm = Some(super::super::state::Confirmation {
            prompt: "Clear conversation history?".to_string(),
            accept_msg_token: super::super::state::ConfirmationTarget::ClearConversation,
        });
        let (state, cmds) = update(state, Msg::ConfirmAccepted);
        let new_id = state.session.conversation.id.clone();
        assert_ne!(new_id, old_id);
        assert_eq!(
            state.session.scratchpad, None,
            "the old session's scratch dir must not leak into the new one"
        );
        assert!(
            cmds.iter().any(
                |c| matches!(c, Cmd::EnsureScratchpad { session_id } if *session_id == new_id)
            ),
            "clear must request a scratch dir keyed by the NEW conversation id"
        );
    }

    #[test]
    fn slash_scratchpad_lists_the_stamped_dir_or_explains_its_absence() {
        // Stamped: the listing runs as an effect against the stamped path.
        let mut state = fresh_state();
        let path = std::path::PathBuf::from("/data/tmp/scratchpad/-proj/s");
        state.session.scratchpad = Some(path.clone());
        let (_, cmds) = update(state, Msg::Slash(SlashCmd::Scratchpad));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::ListScratchpad { path: p } if *p == path)),
            "expected ListScratchpad carrying the stamped path, got {cmds:?}"
        );
        // Not stamped (`fresh_state` starts with `scratchpad: None`): a
        // system message instead of a doomed effect.
        let state = fresh_state();
        let (state, cmds) = update(state, Msg::Slash(SlashCmd::Scratchpad));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::ListScratchpad { .. })));
        let msg = state.session.messages().last().expect("system message");
        assert!(msg.content.contains("No scratchpad yet"), "{}", msg.content);
    }

    #[test]
    fn doctor_reports_the_scratchpad_path() {
        let mut state = fresh_state();
        state.session.scratchpad = Some(std::path::PathBuf::from("/data/tmp/scratchpad/-proj/s"));
        let (state, _) = update(state, Msg::Slash(SlashCmd::Doctor));
        let report = &state.session.messages().last().expect("report").content;
        assert!(
            report.contains("Scratchpad: /data/tmp/scratchpad/-proj/s"),
            "{report}"
        );
        // Before the ready lands, /doctor says so instead of omitting the line.
        let state = fresh_state();
        let (state, _) = update(state, Msg::Slash(SlashCmd::Doctor));
        let report = &state.session.messages().last().expect("report").content;
        assert!(report.contains("Scratchpad: not ready"), "{report}");
    }

    #[test]
    fn load_conversation_recomputes_the_scratchpad() {
        let mut state = fresh_state();
        state.session.scratchpad = Some(std::path::PathBuf::from("/data/tmp/scratchpad/-proj/old"));
        let mut history = fresh_state().session.conversation.clone();
        history.id = "loaded_session".to_string();
        let (state, cmds) = update(state, Msg::ConversationLoaded(history));
        assert_eq!(state.session.scratchpad, None);
        assert!(cmds.iter().any(|c| matches!(
            c,
            Cmd::EnsureScratchpad { session_id } if session_id == "loaded_session"
        )));
    }

    #[test]
    fn clear_conversation_mid_turn_cancels_scope_and_resets_turn() {
        // F34: confirming `/clear` while a turn is generating must cancel the
        // in-flight scope and reset to Idle (mirroring `ConversationLoaded`), so
        // the orphaned model/tool tasks stop and a stray same-id
        // `StreamDone`/`ToolFinished` can't commit into the cleared conversation.
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("scratch history"), state.now);
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
        state.confirm = Some(super::super::state::Confirmation {
            prompt: "Clear conversation history?".to_string(),
            accept_msg_token: super::super::state::ConfirmationTarget::ClearConversation,
        });

        let (state, cmds) = update(state, Msg::ConfirmAccepted);

        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::CancelScope(TurnId(5)))),
            "clearing mid-turn must cancel the in-flight scope (F34)"
        );
        assert!(
            matches!(state.turn, TurnState::Idle),
            "turn must reset to Idle after clear"
        );
        assert!(
            state.session.messages().is_empty(),
            "clear must wipe to a fresh, empty conversation"
        );
    }

    #[test]
    fn clear_conversation_when_idle_does_not_cancel() {
        // No in-flight turn → nothing to cancel; clear just wipes the history.
        let mut state = fresh_state();
        state.confirm = Some(super::super::state::Confirmation {
            prompt: "Clear conversation history?".to_string(),
            accept_msg_token: super::super::state::ConfirmationTarget::ClearConversation,
        });
        let (state, cmds) = update(state, Msg::ConfirmAccepted);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::CancelScope(_))));
        assert!(matches!(state.turn, TurnState::Idle));
    }

    /// Build an `ExecutingTools` state with one outstanding call plus the
    /// committed `assistant(tool_calls)` that a real turn leaves as the trailing
    /// message before the tool results land.
    fn executing_tools_with_committed_call(turn: TurnId) -> State {
        let mut state = fresh_state();
        let source = crate::models::tool_call::ToolCall {
            id: Some("call-1".to_string()),
            function: crate::models::tool_call::FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "foo"}),
            },
        };
        let call = PendingToolCall {
            call_id: super::super::ids::ToolCallId(1),
            source: source.clone(),
        };
        state.session.append(
            ChatMessage::assistant("running a tool").with_tool_calls(vec![source]),
            state.now,
        );
        state.turn = start_executing_tools(turn, vec![call], std::time::SystemTime::now());
        state
    }

    #[test]
    fn cancel_mid_tools_seals_orphaned_tool_calls() {
        // Cancelling while tools run left the committed `assistant(tool_calls)`
        // without matching `tool` results — the next request then 400s on
        // Anthropic ("tool_use without tool_result"). The cancel path must now
        // seal every outstanding call with a cancelled placeholder.
        let state = executing_tools_with_committed_call(TurnId(7));
        let (state, _cmds) = update(state, Msg::CancelTurn);
        // History is well-formed the moment we leave ExecutingTools.
        let last = state.session.messages().last().expect("a message");
        assert_eq!(
            last.role,
            MessageRole::Tool,
            "the orphaned tool_call must be sealed with a tool result"
        );
        assert_eq!(last.tool_call_id.as_deref(), Some("call-1"));
        assert!(matches!(state.turn, TurnState::Cancelling { .. }));

        // The terminal TurnCancelled still closes the turn out to Idle.
        let (state, _cmds) = update(state, Msg::TurnCancelled(TurnId(7)));
        assert!(matches!(state.turn, TurnState::Idle));
    }

    #[test]
    fn quit_mid_tools_seals_orphaned_tool_calls_before_saving() {
        // Same hazard on the quit path: the saved history a later `--continue`
        // reloads must not be a dangling `assistant(tool_calls)`.
        let state = executing_tools_with_committed_call(TurnId(7));
        let (state, cmds) = update(state, Msg::Quit);
        assert!(state.should_exit);
        let last = state.session.messages().last().expect("a message");
        assert_eq!(last.role, MessageRole::Tool);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
    }

    #[test]
    fn system_note_during_tools_does_not_split_tool_pair() {
        // A system note appended between `assistant(tool_calls)` and its results
        // wedges a message into the tool_use/tool_result pair, which OpenAI- and
        // Ollama-shaped providers reject. `push_system` must insert it *before*
        // the trailing assistant message instead, keeping the pair adjacent and
        // the assistant message last (so tool actions still attach to it).
        let mut state = executing_tools_with_committed_call(TurnId(7));
        let mut cmds = Vec::new();
        push_system(&mut state, &mut cmds, "an MCP server errored mid-turn");

        let msgs = state.session.messages();
        let last = msgs.last().expect("a message");
        assert_eq!(
            last.role,
            MessageRole::Assistant,
            "the assistant(tool_calls) must stay last so results follow it directly"
        );
        assert!(last.tool_calls.is_some());
        let prev = &msgs[msgs.len() - 2];
        assert_eq!(prev.role, MessageRole::System);
        assert!(prev.content.contains("MCP server errored"));
    }

    #[test]
    fn system_note_when_idle_appends_normally() {
        // Outside ExecutingTools the note is a plain append (no trailing
        // tool-call message to protect).
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("hi"), state.now);
        let mut cmds = Vec::new();
        push_system(&mut state, &mut cmds, "just a note");
        let last = state.session.messages().last().expect("a message");
        assert_eq!(last.role, MessageRole::System);
        assert!(last.content.contains("just a note"));
    }

    #[test]
    fn load_conversation_drops_queued_messages() {
        // A message queued against the previous conversation must not survive a
        // `/load` and auto-submit into the loaded one.
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
        state
            .ui
            .queued_messages
            .push_back(super::super::state::QueuedMessage {
                text: "stale queued prompt".to_string(),
                attachment_ids: Vec::new(),
            });
        let history = fresh_state().session.conversation.clone();
        let (state, _cmds) = update(state, Msg::ConversationLoaded(history));
        assert!(
            state.ui.queued_messages.is_empty(),
            "queued messages must be dropped on /load"
        );
    }

    #[test]
    fn clear_conversation_drops_queued_messages() {
        // Same for `/clear`: a mid-turn queued message belonged to the wiped
        // conversation and must not auto-submit into the fresh one.
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
        state
            .ui
            .queued_messages
            .push_back(super::super::state::QueuedMessage {
                text: "stale queued prompt".to_string(),
                attachment_ids: Vec::new(),
            });
        state.confirm = Some(super::super::state::Confirmation {
            prompt: "Clear conversation history?".to_string(),
            accept_msg_token: super::super::state::ConfirmationTarget::ClearConversation,
        });
        let (state, _cmds) = update(state, Msg::ConfirmAccepted);
        assert!(
            state.ui.queued_messages.is_empty(),
            "queued messages must be dropped on /clear"
        );
    }

    #[test]
    fn upstream_error_during_cancelling_is_dropped() {
        // F35: a late `UpstreamError` from a cancelled provider call (same turn
        // id, state already `Cancelling`) must be a no-op — not paint a spurious
        // error line for the user's own cancel, and not drain a queued message
        // early (which would race the terminal `TurnCancelled` from `drop_scope`).
        let mut state = fresh_state();
        state.turn = TurnState::Cancelling {
            id: TurnId(5),
            since: std::time::SystemTime::now(),
        };
        let before_len = state.session.messages().len();

        let (state, cmds) = update(
            state,
            Msg::UpstreamError {
                turn: TurnId(5),
                error: crate::models::UserFacingError {
                    summary: "Backend error".to_string(),
                    message: "connection reset".to_string(),
                    suggestion: String::new(),
                    category: crate::models::ErrorCategory::Connection,
                    recoverable: true,
                },
            },
        );

        assert!(
            matches!(state.turn, TurnState::Cancelling { id: TurnId(5), .. }),
            "the turn must stay Cancelling until TurnCancelled lands"
        );
        assert_eq!(
            state.session.messages().len(),
            before_len,
            "no error message should be committed for the user's own cancel"
        );
        assert!(
            cmds.is_empty(),
            "a dropped cancel-side-channel error emits no commands"
        );
    }

    #[test]
    fn reducer_reads_injected_now_not_wall_clock() {
        // Cause 3 determinism: the reducer stamps turn timestamps from
        // `state.now`, never `SystemTime::now()` / `Local::now()`. Folding the
        // same `(State, Msg)` with the same injected `now` must produce a
        // byte-identical `since`, and it must equal the injected value — proving
        // the reducer is a pure function of its inputs and a replay fold
        // reproduces State exactly.
        let now = chrono::Local::now();
        let cancel_since = |injected: chrono::DateTime<chrono::Local>| {
            let mut state = fresh_state();
            state.now = injected;
            state.turn = start_generating(TurnId(1), std::time::SystemTime::from(injected));
            let (state, cmds) = update(state, Msg::CancelTurn);
            assert!(
                cmds.iter()
                    .any(|c| matches!(c, Cmd::CancelScope(TurnId(1))))
            );
            match state.turn {
                TurnState::Cancelling { since, .. } => since,
                other => panic!("expected Cancelling, got {other:?}"),
            }
        };
        // Same injected clock ⇒ identical result, regardless of real wall time.
        assert_eq!(cancel_since(now), cancel_since(now));
        // And the stamp is exactly the injected value, not "roughly now".
        assert_eq!(cancel_since(now), std::time::SystemTime::from(now));
        // A different injected clock yields a correspondingly different stamp.
        let earlier = now - chrono::Duration::seconds(3600);
        assert_eq!(cancel_since(earlier), std::time::SystemTime::from(earlier));
        assert_ne!(cancel_since(earlier), cancel_since(now));
    }

    #[test]
    fn runtime_signal_exits_and_records_timeline() {
        let state = fresh_state();
        let (state, cmds) = update(
            state,
            Msg::RuntimeSignal(super::super::runtime::RuntimeSignal::Terminate),
        );
        assert!(state.should_exit);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
        assert!(
            state
                .runtime
                .timeline
                .iter()
                .any(|event| event.message.contains("terminate"))
        );
    }

    #[test]
    fn model_switch_updates_provider_capability_snapshot() {
        let state = fresh_state();
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Model(Some(
                "anthropic/claude-opus-4-7".to_string(),
            ))),
        );
        assert_eq!(state.runtime.provider_capabilities.provider, "anthropic");
        assert!(state.runtime.provider_capabilities.supports_vision);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::PersistLastModel(_))));
    }

    #[test]
    fn build_chat_request_preserves_explicit_zero_temperature() {
        // D5: an explicit temperature of 0.0 (deterministic decoding) must reach
        // the request as 0.0, not be silently clobbered to DEFAULT_TEMPERATURE.
        let mut state = fresh_state();
        state.settings.default_model.temperature = 0.0;
        assert_eq!(build_chat_request(&state).temperature, 0.0);
    }

    #[test]
    fn hook_context_buffers_caps_and_clears_on_dispatch() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
        // Buffer context for the CURRENT turn…
        let (state, _) = update(
            state,
            Msg::HookContext {
                turn: TurnId(1),
                texts: vec!["remember: staging only".to_string()],
            },
        );
        assert_eq!(state.pending_hook_context.len(), 1);
        // …a stale turn's context is dropped (defense in depth + stale filter)…
        let (mut state2, _) = update(
            state,
            Msg::HookContext {
                turn: TurnId(999),
                texts: vec!["stale".to_string()],
            },
        );
        assert_eq!(state2.pending_hook_context.len(), 1);
        // …the byte cap bounds the buffer…
        let big = "x".repeat(super::MAX_HOOK_CONTEXT_BYTES);
        super::handle_hook_context(&mut state2, TurnId(1), vec![big]);
        assert_eq!(
            state2.pending_hook_context.len(),
            1,
            "over-cap context must be dropped"
        );
        // …the request carries the hook block and dispatch consumes it once.
        let mut cmds = Vec::new();
        super::push_call_model(&mut state2, &mut cmds, TurnId(2));
        let Some(Cmd::CallModel { request, .. }) =
            cmds.iter().find(|c| matches!(c, Cmd::CallModel { .. }))
        else {
            panic!("expected a CallModel");
        };
        let instr = request.instructions.clone().expect("instructions present");
        assert!(instr.contains("# Hook Context"));
        assert!(instr.contains("remember: staging only"));
        assert!(
            state2.pending_hook_context.is_empty(),
            "dispatch must consume the buffer"
        );
        // A second dispatch has no hook block.
        let mut cmds = Vec::new();
        super::push_call_model(&mut state2, &mut cmds, TurnId(3));
        let Some(Cmd::CallModel { request, .. }) =
            cmds.iter().find(|c| matches!(c, Cmd::CallModel { .. }))
        else {
            panic!("expected a CallModel");
        };
        assert!(
            !request
                .instructions
                .clone()
                .unwrap_or_default()
                .contains("# Hook Context")
        );
    }

    #[test]
    fn build_chat_request_injects_memory_index() {
        let mut state = fresh_state();
        // No memory loaded → no memory block in the dynamic suffix.
        assert!(
            !build_chat_request(&state)
                .instructions
                .map(|i| i.contains("# Memory"))
                .unwrap_or(false)
        );
        // With memory → the auto-derived index is composed into the suffix.
        state.memory = Some(crate::app::memory::LoadedMemory {
            entries: Vec::new(),
            index: "# Memory\n\n## Global (all projects)\n- [pnpm] use pnpm — /m/pnpm.md\n"
                .to_string(),
            truncated: false,
        });
        let instr = build_chat_request(&state)
            .instructions
            .expect("memory index should populate the instructions suffix");
        assert!(instr.contains("# Memory"));
        assert!(instr.contains("[pnpm] use pnpm"));
    }

    #[test]
    fn build_chat_request_injects_skill_index() {
        let mut state = fresh_state();
        // No skills discovered → no skills block in the dynamic suffix.
        assert!(
            !build_chat_request(&state)
                .instructions
                .map(|i| i.contains("# Skills"))
                .unwrap_or(false)
        );
        // With skills → the pre-rendered index is composed into the suffix.
        state.skills = Some(crate::app::skills::LoadedSkills {
            entries: Vec::new(),
            index: "# Skills\n\n- [deploy] Ship a release — /s/deploy/SKILL.md (project)\n"
                .to_string(),
        });
        let instr = build_chat_request(&state)
            .instructions
            .expect("skill index should populate the instructions suffix");
        assert!(instr.contains("# Skills"));
        assert!(instr.contains("[deploy] Ship a release"));
    }

    #[test]
    fn build_chat_request_includes_current_working_directory() {
        let state = fresh_state();
        let request = build_chat_request(&state);
        assert!(request.system_prompt.contains("Current Session"));
        assert!(
            request
                .system_prompt
                .contains("Current working directory: /tmp/project")
        );
        assert!(
            request
                .system_prompt
                .contains("Treat this as the project root")
        );
        // The live safety mode must be surfaced so the model knows the current
        // policy instead of inferring it from a stale tool error.
        assert!(
            request.system_prompt.contains("Safety mode: "),
            "system prompt must surface the live safety mode"
        );
    }

    #[test]
    fn esc_during_turn_transitions_to_cancelling() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
        let msg = Msg::Key(Key {
            code: KeyCode::Escape,
            modifiers: KeyMods::default(),
        });
        let (state, cmds) = update(state, msg);
        assert!(matches!(
            state.turn,
            TurnState::Cancelling { id: TurnId(5), .. }
        ));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::CancelScope(TurnId(5))))
        );
    }

    #[test]
    fn esc_while_already_cancelling_does_not_exit() {
        // Esc must NEVER quit mermaid — a second Esc mid-cancel is a no-op,
        // not a force-exit. (Regression: it used to call request_exit, which
        // booted the user out and could leave a background process holding the
        // terminal. Only Ctrl+C / `/quit` exit.)
        let mut state = fresh_state();
        state.turn = TurnState::Cancelling {
            id: TurnId(5),
            since: std::time::SystemTime::now(),
        };
        let msg = Msg::Key(Key {
            code: KeyCode::Escape,
            modifiers: KeyMods::default(),
        });
        let (state, cmds) = update(state, msg);
        assert!(!state.should_exit, "Esc must not exit mermaid");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Exit)),
            "Esc must not emit Cmd::Exit"
        );
        assert!(
            matches!(state.turn, TurnState::Cancelling { id: TurnId(5), .. }),
            "a second Esc mid-cancel leaves the turn cancelling, unchanged"
        );
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
        // CallModel only — instructions/memory freshness comes from the config
        // watcher (#45) in the TUI and a synchronous load in the one-shot paths,
        // so submit never refreshes inline.
        assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
        // user message committed
        assert_eq!(state.session.messages().len(), 1);
        assert_eq!(state.session.messages()[0].content, "hi there");
    }

    #[test]
    fn submit_prompt_when_busy_is_queued() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
        let msg = Msg::SubmitPrompt {
            text: "queue me".to_string(),
            attachment_ids: vec![],
        };
        let (state, cmds) = update(state, msg);
        assert!(matches!(
            state.turn,
            TurnState::Generating { id: TurnId(1), .. }
        ));
        assert!(cmds.is_empty());
        // Not committed to the session — but it IS queued (the old name
        // `..._is_dropped` was misleading: the message is held, not discarded).
        assert!(state.session.messages().is_empty());
        assert_eq!(state.ui.queued_messages.len(), 1);
        assert_eq!(state.ui.queued_messages[0].text, "queue me");
    }

    #[test]
    fn queued_messages_are_capped_dropping_oldest() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
        for i in 0..(MAX_QUEUED_MESSAGES + 5) {
            let (s, _) = update(
                state,
                Msg::SubmitPrompt {
                    text: format!("msg {i}"),
                    attachment_ids: vec![],
                },
            );
            state = s;
        }
        assert_eq!(state.ui.queued_messages.len(), MAX_QUEUED_MESSAGES);
        // The oldest were dropped: the queue window is the last MAX_QUEUED_MESSAGES.
        assert_eq!(state.ui.queued_messages.front().unwrap().text, "msg 5");
        assert_eq!(
            state.ui.queued_messages.back().unwrap().text,
            format!("msg {}", MAX_QUEUED_MESSAGES + 4)
        );
    }

    #[test]
    fn cancelled_turn_submits_oldest_queued_message() {
        let mut state = fresh_state();
        state.turn = TurnState::Cancelling {
            id: TurnId(1),
            since: std::time::SystemTime::now(),
        };
        state
            .ui
            .queued_messages
            .push_back(super::super::state::QueuedMessage {
                text: "first queued".to_string(),
                attachment_ids: Vec::new(),
            });
        state
            .ui
            .queued_messages
            .push_back(super::super::state::QueuedMessage {
                text: "second queued".to_string(),
                attachment_ids: Vec::new(),
            });

        let (state, cmds) = update(state, Msg::TurnCancelled(TurnId(1)));

        assert!(matches!(state.turn, TurnState::Generating { .. }));
        assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::CallModel { .. })));
        assert_eq!(state.session.messages()[0].content, "first queued");
        assert_eq!(state.ui.queued_messages.len(), 1);
        assert_eq!(
            state.ui.queued_messages.front().map(|q| q.text.as_str()),
            Some("second queued")
        );
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
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
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
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
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
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
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
            tokens,
            ..
        } = &state.turn
        {
            assert_eq!(*phase, GenPhase::Thinking);
            assert_eq!(partial_reasoning, "weighing...");
            // The live token counter must climb during thinking, not sit at 0
            // until answer text arrives.
            assert!(*tokens > 0, "reasoning must advance the live token counter");
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
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        let (state, cmds) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        assert!(matches!(state.turn, TurnState::Idle));
        assert_eq!(state.session.messages().len(), 1);
        assert_eq!(state.session.messages()[0].content, "final answer");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
    }

    #[test]
    fn submit_anchors_run_and_resets_token_counter() {
        let mut state = fresh_state();
        // Stale values from a previous run must not leak into the new one.
        state.runtime.run_tokens.add_estimate(999);
        state.runtime.run_started = None;
        let (state, _) = update(
            state,
            Msg::SubmitPrompt {
                text: "hi".to_string(),
                attachment_ids: vec![],
            },
        );
        assert!(
            state.runtime.run_started.is_some(),
            "run anchor set on submit"
        );
        assert_eq!(
            state.runtime.run_tokens,
            crate::domain::runtime::RunTokenCounter::default(),
            "token counter reset on submit"
        );
    }

    #[test]
    fn run_end_appends_a_display_only_summary_once() {
        let mut state = fresh_state();
        // Run started 72s ago with some generated tokens.
        state.runtime.run_started =
            Some(std::time::SystemTime::from(state.now) - std::time::Duration::from_secs(72));
        state.runtime.run_tokens.add_provider(1500);
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: "final answer".to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        let summary = state
            .session
            .messages()
            .iter()
            .find(|m| m.kind == crate::models::ChatMessageKind::RunSummary)
            .expect("a run summary should be appended at run end");
        assert!(summary.content.contains("Worked for"));
        assert!(
            summary.content.contains("1m 12s"),
            "72s should format as 1m 12s, got {:?}",
            summary.content
        );
        assert!(
            state.runtime.run_started.is_none(),
            "run_started is cleared so the summary fires exactly once per run"
        );
        assert!(
            summary.content.contains("used ~"),
            "a usage-less final phase falls back to a chars/4 estimate and \
             must mark the total `~`, got {:?}",
            summary.content
        );
    }

    #[test]
    fn run_summary_shows_line_change_totals() {
        let mut state = fresh_state();
        state.runtime.run_started =
            Some(std::time::SystemTime::from(state.now) - std::time::Duration::from_secs(10));
        state.runtime.run_line_changes.add(4, 4);
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: "final answer".to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        let summary = state
            .session
            .messages()
            .iter()
            .find(|m| m.kind == crate::models::ChatMessageKind::RunSummary)
            .expect("a run summary should be appended at run end");
        assert!(
            summary.content.contains("· +4/-4"),
            "a run that mutated files totals its line changes, got {:?}",
            summary.content
        );
    }

    #[test]
    fn run_summary_omits_line_changes_when_nothing_changed() {
        let mut state = fresh_state();
        state.runtime.run_started =
            Some(std::time::SystemTime::from(state.now) - std::time::Duration::from_secs(10));
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: "final answer".to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        let summary = state
            .session
            .messages()
            .iter()
            .find(|m| m.kind == crate::models::ChatMessageKind::RunSummary)
            .expect("a run summary should be appended at run end");
        assert!(
            !summary.content.contains('+'),
            "a read-only run keeps the two-part summary, got {:?}",
            summary.content
        );
    }

    #[test]
    fn tool_finished_folds_line_changes_into_run_totals() {
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::assistant("editing a file"), state.now);
        state.turn = super::super::transition::start_executing_tools(
            TurnId(1),
            vec![pending_read_file_call()],
            std::time::SystemTime::now(),
        );
        let outcome = ToolOutcome::success("Wrote foo.rs (3 lines)", "3 lines written", 0.1)
            .with_metadata(crate::domain::runtime::ToolRunMetadata {
                lines_added: 3,
                lines_removed: 1,
                ..Default::default()
            });
        let (state, _) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(1),
                call_id: crate::domain::ToolCallId(1),
                outcome,
            },
        );
        assert_eq!(
            state.runtime.run_line_changes,
            crate::domain::runtime::RunLineChanges {
                added: 3,
                removed: 1
            },
            "exact metadata counts accumulate across the run"
        );
    }

    #[test]
    fn run_summary_uses_real_provider_output_unmarked() {
        let mut state = fresh_state();
        state.runtime.run_started =
            Some(std::time::SystemTime::from(state.now) - std::time::Duration::from_secs(10));
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: "final answer".to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: Some(
                    crate::models::TokenUsage::provider(1_000, 30).with_reasoning_output(20),
                ),
                provider_continuation: None,
                stop_reason: None,
            },
        );
        let summary = state
            .session
            .messages()
            .iter()
            .find(|m| m.kind == crate::models::ChatMessageKind::RunSummary)
            .expect("a run summary should be appended at run end");
        assert!(
            summary.content.contains("used 50 tokens"),
            "provider-reported output (completion 30 + reasoning 20) with no \
             `~`, got {:?}",
            summary.content
        );
    }

    #[test]
    fn build_chat_request_excludes_run_summaries() {
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("hello"), state.now);
        state.session.append(
            ChatMessage::run_summary("Worked for 5s · used 100 tokens"),
            state.now,
        );
        let req = build_chat_request(&state);
        assert!(
            !req.messages
                .iter()
                .any(|m| m.kind == crate::models::ChatMessageKind::RunSummary),
            "run summaries are display-only and must not reach the model"
        );
        assert!(
            req.messages.iter().any(|m| m.content == "hello"),
            "real conversation messages are still sent"
        );
    }

    #[test]
    fn run_token_counter_banks_each_phase_across_tool_steps() {
        // The counter must accumulate, not reset, as each model call completes —
        // so a multi-step agentic run shows one growing total.
        let mut state = fresh_state();
        state.runtime.run_tokens.add_provider(100); // earlier phases this run
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: "x".repeat(400),      // ~100 tokens
            partial_reasoning: "y".repeat(400), // ~100 tokens
            tokens: 200,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        // 100 prior + (400 + 400)/4 = 200 this phase — an estimate, because
        // this Done carried no provider usage, so the counter is tainted `~`.
        assert_eq!(state.runtime.run_tokens.output_tokens, 300);
        assert!(state.runtime.run_tokens.contains_estimate);
    }

    #[test]
    fn stream_done_completely_empty_turn_auto_retries() {
        // No text, no reasoning, no tool calls → previously a silent dead-end (or a
        // bare hint). Now it auto-retries the model call (bounded), same as a
        // reasoning-heavy stall — the "no visible output" test doesn't hinge on
        // whether the model happened to emit hidden reasoning.
        let mut state = fresh_state();
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        let (state, cmds) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
            "a completely empty turn must re-issue the model call, not dead-end"
        );
        assert!(matches!(state.turn, TurnState::Generating { .. }));
        assert_eq!(state.runtime.empty_continuations, 1);
    }

    #[test]
    fn stream_done_does_not_flag_reasoning_only_turn() {
        // Reasoning-only (hidden) is not "empty" — it renders as "Reasoning
        // hidden", so the empty-output note must NOT fire.
        let mut state = fresh_state();
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: "thinking it through".to_string(),
            tokens: 0,
            phase: GenPhase::Thinking,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        assert!(
            !state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("no output")),
            "reasoning-only turn is not empty"
        );
    }

    // ── Length-truncation recovery (compact + continue) ──────────────────

    fn truncating_turn(partial: &str) -> TurnState {
        TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: partial.to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        }
    }

    fn length_done() -> Msg {
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            provider_continuation: None,
            stop_reason: Some(crate::models::FinishReason::Length),
        }
    }

    #[test]
    fn length_truncation_recovers_by_compacting_and_continuing() {
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("build a site"), state.now);
        state
            .session
            .append(ChatMessage::assistant("ok, writing files"), state.now);
        state.turn = truncating_turn("let me fix the");
        let (state, cmds) = update(state, length_done());

        assert!(
            matches!(
                state.turn,
                TurnState::Compacting {
                    trigger: CompactionTrigger::TruncationRecovery,
                    ..
                }
            ),
            "a recoverable truncation compacts instead of ending the run"
        );
        assert_eq!(state.runtime.truncation_recoveries, 1, "recovery counted");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::CompactConversation { request, .. }
                if request.trigger == CompactionTrigger::TruncationRecovery)),
            "emits a truncation-recovery compaction"
        );
        assert!(
            state.session.messages().iter().any(|m| m
                .content
                .contains("compacting the conversation to continue")),
            "tells the user it's recovering"
        );
        assert!(
            !state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("Response truncated")),
            "no terminal hint while recovering"
        );
    }

    #[test]
    fn length_truncation_without_progress_at_cap_stops_with_hint() {
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("build a site"), state.now);
        state
            .session
            .append(ChatMessage::assistant("ok"), state.now);
        // Already at the default cap of consecutive recoveries.
        state.runtime.truncation_recoveries =
            state.settings.compaction.max_truncation_recoveries as u32;
        state.turn = truncating_turn("");
        let (state, cmds) = update(state, length_done());

        assert!(matches!(state.turn, TurnState::Idle), "run ends at the cap");
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::CompactConversation { .. })),
            "no further compaction once capped"
        );
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("Response truncated")),
            "shows the manual-levers hint at the cap"
        );
    }

    #[test]
    fn length_truncation_uncapped_keeps_recovering() {
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("x"), state.now);
        state.session.append(ChatMessage::assistant("y"), state.now);
        state.settings.compaction.max_truncation_recoveries = 0; // uncapped
        state.runtime.truncation_recoveries = 99; // would exceed any finite cap
        state.turn = truncating_turn("z");
        let (state, cmds) = update(state, length_done());

        assert!(
            matches!(
                state.turn,
                TurnState::Compacting {
                    trigger: CompactionTrigger::TruncationRecovery,
                    ..
                }
            ),
            "cap 0 means recover regardless of the count"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::CompactConversation { .. }))
        );
    }

    #[test]
    fn length_truncation_without_history_stops_with_hint() {
        // Only the truncated message exists — nothing to compact, so just inform.
        let mut state = fresh_state();
        state.turn = truncating_turn("partial");
        let (state, cmds) = update(state, length_done());

        assert!(matches!(state.turn, TurnState::Idle));
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::CompactConversation { .. }))
        );
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("Response truncated"))
        );
    }

    #[test]
    fn truncation_recoveries_reset_when_run_makes_progress() {
        // A normal (non-truncated) completion is progress and clears the guard, so
        // the cap counts only *consecutive* no-progress truncations.
        let mut state = fresh_state();
        state.runtime.truncation_recoveries = 2;
        state.turn = truncating_turn("a clean final answer");
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None, // not a length truncation
            },
        );
        assert_eq!(state.runtime.truncation_recoveries, 0);
    }

    fn length_done_with_usage(prompt: usize, completion: usize) -> Msg {
        Msg::StreamDone {
            turn: TurnId(5),
            usage: Some(crate::models::TokenUsage::provider(prompt, completion)),
            provider_continuation: None,
            stop_reason: Some(crate::models::FinishReason::Length),
        }
    }

    #[test]
    fn length_output_cap_continues_in_fresh_turn_not_compaction() {
        // The GLM-5.2 case: a length-stop with usage present and the window
        // unknown (the normal remote-provider state) is the per-response
        // OUTPUT cap — compacting the input can't help. With visible content
        // committed, the run now CONTINUES the reply in a fresh turn.
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("audit the widget"), state.now);
        state
            .session
            .append(ChatMessage::assistant("exploring the code"), state.now);
        state.turn = truncating_turn("here is the audit so f");
        let (state, cmds) = update(state, length_done_with_usage(16_600, 4_000));

        assert!(
            matches!(state.turn, TurnState::Generating { .. }),
            "output-cap with content continues in a fresh turn"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
            "the continuation model call is dispatched"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::CompactConversation { .. })),
            "output-cap truncation must never dispatch a compaction"
        );
        assert_eq!(state.runtime.continue_recoveries, 1);
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("continuing")),
            "the continuation note rides in history to nudge the model"
        );
        assert!(
            !state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("Context window full")),
            "the misdiagnosed window-full message is gone"
        );
    }

    #[test]
    fn length_output_cap_mid_reasoning_stops_with_hint() {
        // Cut off mid-think (nearly all completion tokens were reasoning): a
        // continuation can't stitch a hidden trace, so stop with the accurate
        // hint instead.
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("audit the widget"), state.now);
        state
            .session
            .append(ChatMessage::assistant("exploring the code"), state.now);
        state.turn = truncating_turn("here is");
        let usage = crate::models::TokenUsage::provider(16_600, 4_000).with_reasoning_output(3_900);
        let (state, cmds) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: Some(usage),
                provider_continuation: None,
                stop_reason: Some(crate::models::FinishReason::Length),
            },
        );

        assert!(matches!(state.turn, TurnState::Idle), "no continuation");
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
        assert_eq!(state.runtime.continue_recoveries, 0);
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("per-response output limit")),
            "the accurate output-cap hint is shown"
        );
    }

    #[test]
    fn length_output_cap_respects_continuation_cap() {
        // At the per-run continuation cap the run stops with the hint instead
        // of looping forever on a model that keeps re-truncating.
        let mut state = fresh_state();
        state.runtime.continue_recoveries = crate::constants::MAX_OUTPUT_CONTINUATIONS;
        state
            .session
            .append(ChatMessage::user("audit the widget"), state.now);
        state
            .session
            .append(ChatMessage::assistant("exploring the code"), state.now);
        state.turn = truncating_turn("here is the audit so f");
        let (state, cmds) = update(state, length_done_with_usage(16_600, 4_000));

        assert!(matches!(state.turn, TurnState::Idle));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("per-response output limit")),
        );
    }

    #[test]
    fn continue_recoveries_reset_when_run_makes_progress() {
        // Any non-truncation ending is progress — the continuation guard only
        // counts consecutive output-cap truncations.
        let mut state = fresh_state();
        state.runtime.continue_recoveries = 3;
        state.turn = truncating_turn("a clean final answer");
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        assert_eq!(state.runtime.continue_recoveries, 0);
    }

    /// A live continuation turn (mid auto-continue) with accumulated text.
    fn continuation_turn(id: TurnId, partial: &str) -> TurnState {
        TurnState::Generating {
            id,
            started: std::time::SystemTime::now(),
            partial_text: partial.to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: true,
        }
    }

    #[test]
    fn continuation_full_cycle_stamps_kind_and_sweeps_spent_nudge() {
        // The whole seamless-stitch contract at the domain layer: the nudge is
        // stamped RecoveryNudge (hidden + retirable), the continuation turn
        // carries the flag, its commit is stamped Continuation, and the spent
        // nudge is swept at the continuation's stream-done — leaving partial
        // and continuation ADJACENT in history so the transcript can merge
        // them. An unrelated system note must survive the sweep.
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("audit the widget"), state.now);
        state
            .session
            .append(ChatMessage::system("unrelated note"), state.now);
        state.turn = truncating_turn("part one of the reply");
        let (mut state, _) = update(state, length_done_with_usage(16_600, 4_000));

        let nudge = state
            .session
            .messages()
            .iter()
            .find(|m| m.content.contains("continuing"))
            .expect("nudge pushed");
        assert_eq!(
            nudge.kind,
            crate::models::ChatMessageKind::RecoveryNudge,
            "the nudge is stamped for hiding + retirement"
        );
        let cont_id = state.turn.id().expect("continuation turn live");
        assert!(
            matches!(
                state.turn,
                TurnState::Generating {
                    continuation: true,
                    ..
                }
            ),
            "the fresh turn carries the continuation flag"
        );

        // The continuation streams its half and finishes normally.
        state.turn = continuation_turn(cont_id, "and part two");
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: cont_id,
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );

        let messages = state.session.messages();
        assert!(
            !messages
                .iter()
                .any(|m| m.kind == crate::models::ChatMessageKind::RecoveryNudge),
            "the spent nudge is retired from history"
        );
        assert!(
            messages.iter().any(|m| m.content == "unrelated note"),
            "the sweep only removes recovery nudges"
        );
        let part_one = messages
            .iter()
            .position(|m| m.content == "part one of the reply")
            .expect("partial committed");
        let part_two = &messages[part_one + 1];
        assert_eq!(
            part_two.content, "and part two",
            "partial and continuation sit adjacent once the nudge is gone"
        );
        assert_eq!(
            part_two.kind,
            crate::models::ChatMessageKind::Continuation,
            "the continuation commit is stamped for the display stitch"
        );
        assert_eq!(state.runtime.continue_recoveries, 0, "progress resets");
    }

    #[test]
    fn empty_retry_inside_continuation_chain_keeps_the_flag() {
        // A continuation turn that comes back empty auto-retries; the retry
        // turn must still be a continuation or the eventual real text commits
        // unstamped and the transcript shows a seam mid-chain.
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("audit the widget"), state.now);
        state.turn = continuation_turn(TurnId(5), "");
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        assert!(
            matches!(
                state.turn,
                TurnState::Generating {
                    continuation: true,
                    ..
                }
            ),
            "the empty-retry turn inherits the continuation flag"
        );
        let nudge = state
            .session
            .messages()
            .iter()
            .find(|m| m.content.contains("no reply or action"))
            .expect("empty-retry nudge pushed");
        assert_eq!(
            nudge.kind,
            crate::models::ChatMessageKind::RecoveryNudge,
            "the stalled-turn nudge gets the same retirement treatment"
        );
    }

    #[test]
    fn truncation_recovery_resume_keeps_continuation_flag() {
        // A continuation turn that hits a genuine context-full stop compacts;
        // the resume after compaction must re-enter Generating with the flag.
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("original prompt"), state.now);
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::TruncationRecovery,
            resume_continuation: true,
        };
        let result = fake_recovery_result(vec![ChatMessage::user("compacted context")]);
        let (state, _) = update(
            state,
            Msg::CompactionFinished {
                turn: TurnId(7),
                result,
            },
        );
        assert!(
            matches!(
                state.turn,
                TurnState::Generating {
                    continuation: true,
                    ..
                }
            ),
            "the compaction resume carries the chain marker through"
        );
    }

    #[test]
    fn quit_mid_continuation_stamps_interrupted_commit_and_sweeps() {
        // Ctrl+C mid-continuation: the interrupted partial keeps the
        // Continuation stamp (a `--continue` reload still stitches) and the
        // live nudge doesn't get persisted into the saved session.
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("audit the widget"), state.now);
        state
            .session
            .append(ChatMessage::assistant("part one of the reply"), state.now);
        let mut nudge = ChatMessage::system("resume nudge");
        nudge.kind = crate::models::ChatMessageKind::RecoveryNudge;
        state.session.append(nudge, state.now);
        state.turn = continuation_turn(TurnId(9), "and part tw");
        let (state, cmds) = update(state, Msg::Quit);

        assert!(state.should_exit);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
        let messages = state.session.messages();
        assert!(
            !messages
                .iter()
                .any(|m| m.kind == crate::models::ChatMessageKind::RecoveryNudge),
            "the live nudge is not persisted on quit"
        );
        let last = messages.last().expect("interrupted partial committed");
        assert!(last.content.contains("and part tw"));
        assert_eq!(
            last.kind,
            crate::models::ChatMessageKind::Continuation,
            "the interrupted commit keeps the stitch marker"
        );
    }

    #[test]
    fn cancel_and_upstream_error_sweep_spent_nudges() {
        // Both non-stream-done turn ends retire a live nudge: leaving it in
        // history would keep steering later requests while the transcript
        // hides it.
        for is_error in [false, true] {
            let mut state = fresh_state();
            let mut nudge = ChatMessage::system("resume nudge");
            nudge.kind = crate::models::ChatMessageKind::RecoveryNudge;
            state.session.append(nudge, state.now);
            let msg = if is_error {
                state.turn = continuation_turn(TurnId(9), "half");
                Msg::UpstreamError {
                    turn: TurnId(9),
                    error: crate::models::UserFacingError {
                        summary: "boom".to_string(),
                        message: "provider died".to_string(),
                        suggestion: String::new(),
                        category: crate::models::ErrorCategory::Temporary,
                        recoverable: true,
                    },
                }
            } else {
                state.turn = TurnState::Cancelling {
                    id: TurnId(9),
                    since: std::time::SystemTime::now(),
                };
                Msg::TurnCancelled(TurnId(9))
            };
            let (state, cmds) = update(state, msg);
            assert!(
                !state
                    .session
                    .messages()
                    .iter()
                    .any(|m| m.kind == crate::models::ChatMessageKind::RecoveryNudge),
                "nudge swept (is_error={is_error})"
            );
            assert!(
                cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))),
                "sweep persisted (is_error={is_error})"
            );
        }
    }

    #[test]
    fn submit_resets_continue_recoveries() {
        // A run that ENDED at the continuation cap never hits the in-stream
        // reset; the next submit must restore the full budget.
        let mut state = fresh_state();
        state.runtime.continue_recoveries = crate::constants::MAX_OUTPUT_CONTINUATIONS;
        let (state, _) = update(
            state,
            Msg::SubmitPrompt {
                text: "next task".to_string(),
                attachment_ids: Vec::new(),
            },
        );
        assert_eq!(state.runtime.continue_recoveries, 0);
    }

    #[test]
    fn length_with_usage_near_known_window_still_compacts() {
        // With usage AND a known window that prompt+completion+reserve reaches,
        // the window genuinely filled — the legacy compact-and-continue
        // recovery is still correct.
        let mut state = fresh_state();
        state.runtime.provider_capabilities.max_context_tokens = Some(20_000);
        state
            .session
            .append(ChatMessage::user("build a site"), state.now);
        state
            .session
            .append(ChatMessage::assistant("ok, writing files"), state.now);
        state.turn = truncating_turn("let me fix the");
        let (state, cmds) = update(state, length_done_with_usage(18_000, 1_500));

        assert!(
            matches!(
                state.turn,
                TurnState::Compacting {
                    trigger: CompactionTrigger::TruncationRecovery,
                    ..
                }
            ),
            "a genuinely full window still recovers via compaction"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::CompactConversation { .. })),
            "compaction dispatched"
        );
    }

    fn fake_recovery_result(replacement: Vec<ChatMessage>) -> CompactionResult {
        let snap = crate::domain::state::ContextUsageSnapshot::from_estimate(
            crate::domain::state::PromptTokenBreakdown::default(),
            Some(12_000),
        );
        CompactionResult {
            record: crate::domain::CompactionRecord {
                id: "rec1".to_string(),
                trigger: CompactionTrigger::TruncationRecovery,
                created_at: chrono::Local::now(),
                before_tokens: 100,
                after_tokens: 40,
                archived_message_count: 2,
                preserved_message_count: replacement.len(),
                preserved_turn_count: replacement
                    .iter()
                    .filter(|message| message.role == MessageRole::User)
                    .count(),
                summary_tokens: 10,
                duration_secs: 0.0,
                review_status: crate::domain::CompactionReviewStatus::Reviewed,
                review_error: None,
                focus: None,
                archive_path: None,
            },
            replacement_messages: replacement,
            archived_messages: vec![ChatMessage::user("archived")],
            before_snapshot: snap.clone(),
            after_snapshot: snap,
            usage: None,
            source_boundaries: Vec::new(),
        }
    }

    #[test]
    fn compaction_finish_preserves_messages_appended_after_dispatch() {
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("old"), state.now);
        state
            .session
            .append(ChatMessage::assistant("old answer"), state.now);
        state.session.append(ChatMessage::user("latest"), state.now);
        let boundaries = state
            .session
            .messages()
            .iter()
            .map(crate::domain::CompactionBoundary::from_message)
            .collect();
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::Manual,
            resume_continuation: false,
        };
        state.session.append(
            ChatMessage::system("MCP server failed during compaction"),
            state.now,
        );

        let mut result = fake_recovery_result(vec![ChatMessage::user("compacted context")]);
        result.source_boundaries = boundaries;
        let (state, _) = update(
            state,
            Msg::CompactionFinished {
                turn: TurnId(7),
                result,
            },
        );
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|message| { message.content == "MCP server failed during compaction" })
        );
    }

    #[test]
    fn compaction_finish_places_late_tool_results_after_the_pending_tail() {
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("old"), state.now);
        state
            .session
            .append(ChatMessage::assistant("old answer"), state.now);
        state.session.append(ChatMessage::user("latest"), state.now);
        let boundaries = state
            .session
            .messages()
            .iter()
            .map(crate::domain::CompactionBoundary::from_message)
            .collect();
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::TruncationRecovery,
            resume_continuation: false,
        };
        // All three arrive while compaction runs: a notice, a tool result that
        // answers the pending assistant tail the checkpoint preserves, and a
        // report that (chronologically and textually) follows the result.
        state
            .session
            .append(ChatMessage::system("notice during compaction"), state.now);
        state.session.append(
            ChatMessage::tool("call_9", "execute_command", "late result"),
            state.now,
        );
        state
            .session
            .append(ChatMessage::system("report about the result"), state.now);

        let mut pending_tail = ChatMessage::assistant("");
        pending_tail.tool_calls = Some(vec![tool_call_fixture("call_9", "execute_command")]);
        let mut result =
            fake_recovery_result(vec![ChatMessage::user("compacted context"), pending_tail]);
        result.source_boundaries = boundaries;
        let (state, _) = update(
            state,
            Msg::CompactionFinished {
                turn: TurnId(7),
                result,
            },
        );

        let contents: Vec<&str> = state
            .session
            .messages()
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        let tail_call = contents
            .iter()
            .position(|content| content.is_empty())
            .expect("pending tail kept");
        let tail_result = contents
            .iter()
            .position(|content| *content == "late result")
            .expect("late tool result kept");
        let notice = contents
            .iter()
            .position(|content| *content == "notice during compaction")
            .expect("notice kept");
        let report = contents
            .iter()
            .position(|content| *content == "report about the result")
            .expect("report kept");
        assert!(
            notice < tail_call,
            "pre-result intervening messages go before the pending tail"
        );
        assert!(
            tail_call < tail_result,
            "a tool result must follow its tool call"
        );
        assert!(
            tail_result < report,
            "a message that arrived after the result must stay after it"
        );
    }

    fn tool_call_fixture(id: &str, name: &str) -> crate::models::tool_call::ToolCall {
        crate::models::tool_call::ToolCall {
            id: Some(id.to_string()),
            function: crate::models::tool_call::FunctionCall {
                name: name.to_string(),
                arguments: serde_json::json!({}),
            },
        }
    }

    #[test]
    fn auto_compaction_failure_pauses_retries_until_reset() {
        let mut state = fresh_state();
        state.turn = truncating_turn("");
        let turn = state.turn.id().expect("generating turn");

        let (mut state, _) = update(
            state,
            Msg::CompactionFailed {
                turn,
                trigger: CompactionTrigger::AutoThreshold,
                message: "checkpoint invalid".to_string(),
                kind: StatusKind::Warn,
            },
        );
        assert!(state.runtime.auto_compact_suppressed);
        assert!(build_chat_request(&state).suppress_auto_compact);
        let notices = |state: &State| {
            state
                .session
                .messages()
                .iter()
                .filter(|message| message.content.contains("Auto-compaction paused"))
                .count()
        };
        assert_eq!(notices(&state), 1);

        // A second failure stays silent — the pause was already announced.
        state.turn = truncating_turn("");
        let turn = state.turn.id().expect("generating turn");
        let (state, _) = update(
            state,
            Msg::CompactionFailed {
                turn,
                trigger: CompactionTrigger::AutoThreshold,
                message: "checkpoint invalid".to_string(),
                kind: StatusKind::Warn,
            },
        );
        assert!(state.runtime.auto_compact_suppressed);
        assert_eq!(notices(&state), 1);

        // A successful compaction (any trigger) lifts the pause.
        let mut state = state;
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::Manual,
            resume_continuation: false,
        };
        let result = fake_recovery_result(vec![ChatMessage::user("compacted context")]);
        let (state, _) = update(
            state,
            Msg::CompactionFinished {
                turn: TurnId(7),
                result,
            },
        );
        assert!(!state.runtime.auto_compact_suppressed);
        assert!(!build_chat_request(&state).suppress_auto_compact);
    }

    #[test]
    fn manual_compact_lifts_the_auto_compaction_pause() {
        let mut state = fresh_state();
        state.runtime.auto_compact_suppressed = true;
        state.session.append(ChatMessage::user("one"), state.now);
        state
            .session
            .append(ChatMessage::assistant("two"), state.now);
        state.session.append(ChatMessage::user("three"), state.now);

        let (state, cmds) = update(state, Msg::Slash(SlashCmd::Compact(None)));
        assert!(!state.runtime.auto_compact_suppressed);
        let Some(Cmd::CompactConversation { request, .. }) = cmds
            .iter()
            .find(|command| matches!(command, Cmd::CompactConversation { .. }))
        else {
            panic!("expected a CompactConversation command");
        };
        assert!(!request.chat.suppress_auto_compact);
    }

    #[test]
    fn model_switch_lifts_the_auto_compaction_pause() {
        let mut state = fresh_state();
        state.runtime.auto_compact_suppressed = true;

        // The pause is model-scoped: switching models is the natural reaction
        // to a summarizer that can't produce the checkpoint structure, and the
        // new model deserves a fresh shot.
        let (state, _) = update(
            state,
            Msg::Slash(SlashCmd::Model(Some("ollama/other".to_string()))),
        );
        assert!(!state.runtime.auto_compact_suppressed);
    }

    #[test]
    fn visible_context_full_progress_resets_recovery_guard() {
        let mut state = fresh_state();
        state.runtime.provider_capabilities.max_context_tokens = Some(20_000);
        state.settings.compaction.max_truncation_recoveries = 3;
        state.runtime.truncation_recoveries = 2;
        state.session.append(ChatMessage::user("one"), state.now);
        state
            .session
            .append(ChatMessage::assistant("two"), state.now);
        state.session.append(ChatMessage::user("three"), state.now);
        state.turn = truncating_turn("visible progress");

        let (state, cmds) = update(state, length_done_with_usage(18_000, 1_500));
        assert!(matches!(state.turn, TurnState::Compacting { .. }));
        assert_eq!(state.runtime.truncation_recoveries, 1);
        assert!(
            cmds.iter()
                .any(|command| matches!(command, Cmd::CompactConversation { .. }))
        );
    }

    #[test]
    fn finished_truncation_recovery_resumes_the_run() {
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("original prompt"), state.now);
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::TruncationRecovery,
            resume_continuation: false,
        };
        let result = fake_recovery_result(vec![ChatMessage::user("compacted context")]);
        let (state, cmds) = update(
            state,
            Msg::CompactionFinished {
                turn: TurnId(7),
                result,
            },
        );

        assert!(
            matches!(state.turn, TurnState::Generating { .. }),
            "recovery resumes generating with the compacted context"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
            "re-dispatches the model call to finish the work"
        );
    }

    #[test]
    fn finished_context_limit_retry_resumes_the_run() {
        // D6: a context-limit compaction must resume the interrupted request,
        // exactly like a truncation recovery — not silently drop the turn to Idle.
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("original prompt"), state.now);
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::ContextLimitRetry,
            resume_continuation: false,
        };
        let result = fake_recovery_result(vec![ChatMessage::user("compacted context")]);
        let (state, cmds) = update(
            state,
            Msg::CompactionFinished {
                turn: TurnId(7),
                result,
            },
        );

        assert!(
            matches!(state.turn, TurnState::Generating { .. }),
            "context-limit retry resumes generating with the compacted context"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
            "re-dispatches the model call to finish the interrupted work"
        );
    }

    #[test]
    fn finished_manual_compaction_still_goes_idle() {
        // Regression guard: only TruncationRecovery resumes; manual /compact ends.
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("original prompt"), state.now);
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::Manual,
            resume_continuation: false,
        };
        let result = fake_recovery_result(vec![ChatMessage::user("compacted")]);
        let (state, cmds) = update(
            state,
            Msg::CompactionFinished {
                turn: TurnId(7),
                result,
            },
        );

        assert!(matches!(state.turn, TurnState::Idle));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
    }

    #[test]
    fn failed_truncation_recovery_stops_with_hint() {
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("x"), state.now);
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::TruncationRecovery,
            resume_continuation: false,
        };
        let (state, _) = update(
            state,
            Msg::CompactionFailed {
                turn: TurnId(7),
                trigger: CompactionTrigger::TruncationRecovery,
                message: "compaction did not reduce context".to_string(),
                kind: StatusKind::Error,
            },
        );
        assert!(matches!(state.turn, TurnState::Idle));
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("Response truncated")),
            "a failed recovery falls back to the manual-levers hint, not a raw error"
        );
    }

    #[test]
    fn manual_compaction_skip_shows_calm_note_not_failure() {
        // A manual /compact with nothing to compact (Info kind) is a benign no-op,
        // not a failure: show a calm note, never "Compaction failed: Invalid request".
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("x"), state.now);
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::Manual,
            resume_continuation: false,
        };
        let (state, _) = update(
            state,
            Msg::CompactionFailed {
                turn: TurnId(7),
                trigger: CompactionTrigger::Manual,
                message: "not enough conversation history to summarize".to_string(),
                kind: StatusKind::Info,
            },
        );
        assert!(matches!(state.turn, TurnState::Idle));
        let msgs = state.session.messages();
        assert!(
            msgs.iter()
                .any(|m| m.content.contains("Nothing to compact")),
            "benign skip should show a calm note"
        );
        assert!(
            !msgs.iter().any(|m| m.content.contains("Compaction failed")
                || m.content.contains("Invalid request")),
            "benign skip must not read as a failure"
        );
    }

    #[test]
    fn manual_compaction_real_failure_still_says_failed() {
        // Regression guard: a genuine manual-compaction error (Error kind) still
        // surfaces as "Compaction failed: …".
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("x"), state.now);
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::Manual,
            resume_continuation: false,
        };
        let (state, _) = update(
            state,
            Msg::CompactionFailed {
                turn: TurnId(7),
                trigger: CompactionTrigger::Manual,
                message: "compaction produced an empty summary".to_string(),
                kind: StatusKind::Error,
            },
        );
        assert!(matches!(state.turn, TurnState::Idle));
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("Compaction failed")),
            "a real failure should still say so"
        );
    }

    #[test]
    fn compaction_config_defaults_to_three() {
        let cfg = crate::app::CompactionConfig::default();
        assert_eq!(cfg.max_truncation_recoveries, 3);
        // An absent [compaction] section deserializes to the default.
        let parsed: crate::app::Config = toml::from_str("").unwrap();
        assert_eq!(parsed.compaction.max_truncation_recoveries, 3);
    }

    #[test]
    fn fold_token_usage_variants_route_to_the_right_meters() {
        let mut state = fresh_state();
        let usage = crate::models::TokenUsage::provider(100, 20).with_reasoning_output(5);

        // OwnRequest: last + cumulative, no run banking (the stream-done
        // path banks its own output separately).
        fold_token_usage(
            &mut state.session,
            &mut state.runtime,
            &usage,
            UsageFold::OwnRequest,
        );
        assert_eq!(state.session.last_token_usage.unwrap().total_tokens(), 125);
        assert_eq!(state.session.cumulative_token_usage.total_tokens(), 125);
        assert_eq!(state.runtime.run_tokens.output_tokens, 25);
        assert!(!state.runtime.run_tokens.contains_estimate);

        // Subagent: cumulative + run counter (output only), never last —
        // the child's context is a separate window.
        state.session.last_token_usage = None;
        fold_token_usage(
            &mut state.session,
            &mut state.runtime,
            &usage,
            UsageFold::Subagent,
        );
        assert!(state.session.last_token_usage.is_none());
        assert_eq!(state.session.cumulative_token_usage.total_tokens(), 250);
        assert_eq!(state.runtime.run_tokens.output_tokens, 50);

        // Manual compaction: charged like an own request, but not run spend.
        fold_token_usage(
            &mut state.session,
            &mut state.runtime,
            &usage,
            UsageFold::Compaction { mid_run: false },
        );
        assert_eq!(state.session.last_token_usage.unwrap().total_tokens(), 125);
        assert_eq!(state.session.cumulative_token_usage.total_tokens(), 375);
        assert_eq!(state.runtime.run_tokens.output_tokens, 50);

        // Mid-run (auto/recovery) compaction output IS run spend.
        fold_token_usage(
            &mut state.session,
            &mut state.runtime,
            &usage,
            UsageFold::Compaction { mid_run: true },
        );
        assert_eq!(state.session.cumulative_token_usage.total_tokens(), 500);
        assert_eq!(state.runtime.run_tokens.output_tokens, 75);
    }

    #[test]
    fn stream_done_tracks_last_and_cumulative_token_usage() {
        let mut state = fresh_state();
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: "final answer".to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };

        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: Some(crate::models::TokenUsage::provider(120, 30)),
                provider_continuation: None,
                stop_reason: None,
            },
        );

        assert_eq!(state.session.last_token_usage.unwrap().prompt_tokens, 120);
        assert_eq!(state.session.cumulative_token_usage.total_tokens(), 150);
        assert_eq!(
            state.session.context_usage.as_ref().unwrap().used_tokens,
            150
        );
    }

    #[test]
    fn stream_done_empty_output_with_reasoning_auto_retries() {
        // The reported bug: a reasoning-heavy turn that produced no text and no
        // tool calls must NOT end the run silently — it auto-retries the model
        // call (bounded), without committing an empty assistant message.
        let mut state = fresh_state();
        state.runtime.run_started = Some(std::time::SystemTime::now());
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: "internal thinking ".repeat(50),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };

        let (state, cmds) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: Some(crate::models::TokenUsage::provider(100, 0)),
                provider_continuation: None,
                stop_reason: None,
            },
        );

        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
            "a stalled (no-output) turn must re-issue the model call"
        );
        assert!(
            matches!(state.turn, TurnState::Generating { .. }),
            "run should continue in a fresh Generating turn"
        );
        assert_eq!(state.runtime.empty_continuations, 1);
        assert!(
            !state
                .session
                .messages()
                .iter()
                .any(|m| m.role == MessageRole::Assistant && m.content.trim().is_empty()),
            "must not commit an empty assistant message"
        );
        // The tokens the stalled turn spent are still accounted for.
        assert_eq!(state.session.cumulative_token_usage.total_tokens(), 100);
    }

    #[test]
    fn stream_done_empty_output_past_cap_hints_and_ends() {
        // Once the per-run retry budget is spent, a still-empty turn stops the run
        // with a clear hint instead of looping forever.
        let mut state = fresh_state();
        state.runtime.run_started = Some(std::time::SystemTime::now());
        state.runtime.empty_continuations = super::MAX_EMPTY_CONTINUATIONS;
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: "thinking".to_string(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };

        let (state, cmds) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );

        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
            "past the cap the run must not keep retrying"
        );
        assert!(matches!(state.turn, TurnState::Idle), "run ends");
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("no reply or action")),
            "should surface the no-output hint"
        );
    }

    #[test]
    fn stream_done_with_output_resets_empty_continuation_guard() {
        // A turn that makes progress clears the guard so a later stall in the same
        // run gets a full retry budget again.
        let mut state = fresh_state();
        state.runtime.empty_continuations = 1;
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: "here is the answer".to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };

        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );

        assert_eq!(state.runtime.empty_continuations, 0);
    }

    #[test]
    fn context_usage_estimate_is_stored_during_generation() {
        let mut state = fresh_state();
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Thinking,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        let snapshot = crate::domain::state::ContextUsageSnapshot::from_estimate(
            crate::domain::state::PromptTokenBreakdown {
                system_tokens: 10,
                instructions_tokens: 0,
                message_tokens: 20,
                tool_schema_tokens: 30,
                image_count: 0,
                message_count: 1,
                tool_count: 2,
            },
            Some(1_000),
        );

        let (state, _) = update(
            state,
            Msg::ContextUsageEstimated {
                turn: TurnId(5),
                snapshot,
            },
        );

        let context = state.session.context_usage.expect("context usage");
        assert!(context.is_estimate());
        assert_eq!(context.used_tokens, 60);
        assert_eq!(context.used_percent, Some(6));
    }

    #[test]
    fn context_text_explains_auto_compaction_policy() {
        let mut state = fresh_state();
        state.runtime.provider_capabilities.max_context_tokens = Some(8_000);
        state.session.append(ChatMessage::user("hello"), state.now);

        let text = context_text(&state);

        assert!(text.contains("Next request:"));
        assert!(text.contains("Response reserve:"));
        assert!(text.contains("Auto compact threshold:"));
        assert!(text.contains("Auto compact:"));
        assert!(text.contains("Hard limit risk:"));
    }

    /// F4 defense-in-depth: if a later refactor weakens the stale
    /// filter at the top of `update_step`, `handle_upstream_error`
    /// still refuses to mutate state when the error's turn id doesn't
    /// match the active turn. Direct-call the helper to exercise the
    /// guard without relying on the outer filter.
    #[test]
    fn handle_upstream_error_refuses_mismatched_turn_id() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
        let err = crate::models::UserFacingError {
            summary: "Stale".to_string(),
            message: "wrong turn".to_string(),
            suggestion: String::new(),
            category: crate::models::ErrorCategory::Temporary,
            recoverable: true,
        };
        let mut cmds = Vec::new();
        super::handle_upstream_error(&mut state, &mut cmds, TurnId(999), err);
        // Active turn must be untouched and no error message committed.
        assert!(matches!(
            state.turn,
            TurnState::Generating { id: TurnId(5), .. }
        ));
        assert!(state.session.messages().is_empty());
    }

    #[test]
    fn upstream_error_ends_turn_and_records_line() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
        let err = crate::models::UserFacingError {
            summary: "Server error".to_string(),
            message: "500 internal".to_string(),
            suggestion: "retry".to_string(),
            category: crate::models::ErrorCategory::Temporary,
            recoverable: true,
        };
        let (state, cmds) = update(
            state,
            Msg::UpstreamError {
                turn: TurnId(1),
                error: err,
            },
        );
        assert!(matches!(state.turn, TurnState::Idle));
        // The errored turn persists the session — a headless run's emitted
        // session id must point at a real file even when the provider failed.
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))),
            "upstream error must save the conversation"
        );
        assert_eq!(state.session.messages().len(), 1);
        let m = &state.session.messages()[0];
        // Error surfaces through the ActionDisplay only — content is
        // intentionally empty so the chat widget doesn't paint the
        // error twice (once as a content line, once as an action).
        assert_eq!(m.content, "");
        assert_eq!(m.actions.len(), 1);
        assert_eq!(m.actions[0].target, "Server error");
    }

    #[test]
    fn upstream_error_drains_queued_message() {
        // A provider error ends the turn; a message the user queued mid-turn
        // must be submitted, not stranded until the next manual prompt (#121).
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
        state
            .ui
            .queued_messages
            .push_back(super::super::state::QueuedMessage {
                text: "queued during turn".to_string(),
                attachment_ids: Vec::new(),
            });
        let err = crate::models::UserFacingError {
            summary: "Server error".to_string(),
            message: "500 internal".to_string(),
            suggestion: "retry".to_string(),
            category: crate::models::ErrorCategory::Temporary,
            recoverable: true,
        };
        let (state, cmds) = update(
            state,
            Msg::UpstreamError {
                turn: TurnId(1),
                error: err,
            },
        );
        // The queued message was submitted: a fresh turn is generating with a
        // CallModel, and the FIFO is empty.
        assert!(matches!(state.turn, TurnState::Generating { .. }));
        assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::CallModel { .. })));
        assert!(state.ui.queued_messages.is_empty());
        // Both the error line and the now-submitted queued message are present.
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.actions.iter().any(|a| a.target == "Server error"))
        );
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.content == "queued during turn")
        );
    }

    #[test]
    fn slash_model_with_arg_persists_and_updates_session() {
        let state = fresh_state();
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Model(Some("anthropic/opus".to_string()))),
        );
        assert_eq!(state.session.model_id, "anthropic/opus");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::PersistLastModel(_))));
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::PullOllamaModel { .. }))
        );
    }

    #[test]
    fn slash_model_local_ollama_auto_pulls() {
        let state = fresh_state();
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Model(Some("ollama/qwen3:8b".to_string()))),
        );
        assert_eq!(state.session.model_id, "ollama/qwen3:8b");
        assert!(
            cmds.iter()
                .any(|c| { matches!(c, Cmd::PullOllamaModel { model } if model == "qwen3:8b") }),
            "local Ollama model should dispatch pull: {:?}",
            cmds
        );
    }

    #[test]
    fn slash_model_bare_name_auto_pulls_as_ollama() {
        let state = fresh_state();
        let (_, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Model(Some("qwen3-coder:30b".to_string()))),
        );
        assert!(
            cmds.iter().any(|c| {
                matches!(c, Cmd::PullOllamaModel { model } if model == "qwen3-coder:30b")
            }),
            "bare model names should dispatch an Ollama pull: {:?}",
            cmds
        );
    }

    #[test]
    fn slash_model_ollama_cloud_skips_local_pull() {
        let state = fresh_state();
        let (_, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Model(Some("ollama/gpt-oss:cloud".to_string()))),
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::PullOllamaModel { .. }))
        );
    }

    #[test]
    fn slash_help_appends_system_help_and_persists() {
        let state = fresh_state();
        let (state, cmds) = update(state, Msg::Slash(SlashCmd::Help));
        let msg = state.session.messages().last().expect("help message");
        assert_eq!(msg.role, MessageRole::System);
        assert!(msg.content.contains("Everyday:"));
        assert!(msg.content.contains("Advanced runtime:"));
        assert!(msg.content.contains("/model"));
        assert!(msg.content.contains("/help"));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
    }

    #[test]
    fn slash_doctor_appends_session_readiness_report() {
        let state = fresh_state();
        let (state, cmds) = update(state, Msg::Slash(SlashCmd::Doctor));
        let msg = state.session.messages().last().expect("doctor message");
        assert_eq!(msg.role, MessageRole::System);
        assert!(msg.content.contains("Mermaid Doctor"));
        assert!(msg.content.contains("Active model:"));
        assert!(msg.content.contains("Safety:"));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
    }

    #[test]
    fn slash_memory_commands_dispatch_effects() {
        // /memory lists; /remember <text> and /forget <id> route to effects.
        let (_s, cmds) = update(fresh_state(), Msg::Slash(SlashCmd::Memory));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::ListMemory)));

        let (_s, cmds) = update(
            fresh_state(),
            Msg::Slash(SlashCmd::Remember(Some("prefer ripgrep".to_string()))),
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::RememberMemory { text } if text == "prefer ripgrep"))
        );

        let (_s, cmds) = update(
            fresh_state(),
            Msg::Slash(SlashCmd::Forget(Some("prefer-rg".to_string()))),
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::ForgetMemory { id } if id == "prefer-rg"))
        );

        // No-arg /remember explains usage instead of dispatching.
        let (state, cmds) = update(fresh_state(), Msg::Slash(SlashCmd::Remember(None)));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::RememberMemory { .. })));
        assert!(
            state
                .session
                .messages()
                .last()
                .is_some_and(|m| m.content.contains("Usage: /remember")),
            "no-arg /remember posts a usage hint to the transcript"
        );

        // /consolidate-memory routes to the model-assisted prune effect.
        let (_s, cmds) = update(fresh_state(), Msg::Slash(SlashCmd::ConsolidateMemory));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::ConsolidateMemory { .. }))
        );
    }

    #[test]
    fn chat_request_uses_runtime_prompt_customization() {
        let mut state = fresh_state();
        state.settings.prompt.system_prompt = Some("replacement prompt".to_string());
        state
            .settings
            .prompt
            .append_system_prompt
            .push("extra runtime rule".to_string());

        let request = build_chat_request(&state);
        assert!(request.system_prompt.contains("replacement prompt"));
        assert!(request.system_prompt.contains("extra runtime rule"));
        assert!(!request.system_prompt.contains("Core Loop"));
        assert!(request.system_prompt.contains("Current working directory"));
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
                Cmd::PersistReasoningFor { model_id, level } => Some((model_id.clone(), *level)),
                _ => None,
            })
            .expect("persist cmd emitted");
        assert_eq!(emitted.0, "ollama/test");
        assert_eq!(emitted.1, crate::models::ReasoningLevel::High);
    }

    #[test]
    fn slash_context_set_persists_per_model() {
        use crate::domain::ContextCmd;
        let state = fresh_state();
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Context(ContextCmd::Set(65_536))),
        );
        assert_eq!(
            state.settings.ollama_num_ctx_per_model.get("ollama/test"),
            Some(&65_536)
        );
        assert!(cmds.iter().any(|c| matches!(
            c,
            Cmd::PersistOllamaNumCtxFor { model_id, num_ctx: Some(65_536) } if model_id == "ollama/test"
        )));
    }

    #[test]
    fn slash_context_auto_clears_override() {
        use crate::domain::ContextCmd;
        let mut state = fresh_state();
        state
            .settings
            .ollama_num_ctx_per_model
            .insert("ollama/test".to_string(), 65_536);
        let (state, cmds) = update(state, Msg::Slash(SlashCmd::Context(ContextCmd::Auto)));
        assert!(
            !state
                .settings
                .ollama_num_ctx_per_model
                .contains_key("ollama/test")
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::PersistOllamaNumCtxFor { num_ctx: None, .. }))
        );
    }

    #[test]
    fn slash_context_offload_toggles_and_persists() {
        use crate::domain::ContextCmd;
        let state = fresh_state();
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Context(ContextCmd::Offload(true))),
        );
        assert!(state.settings.ollama.allow_ram_offload);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::PersistOllamaOffload(true)))
        );
    }

    #[test]
    fn build_chat_request_carries_per_model_num_ctx() {
        let mut state = fresh_state();
        state
            .settings
            .ollama_num_ctx_per_model
            .insert("ollama/test".to_string(), 32_768);
        let req = build_chat_request(&state);
        assert_eq!(req.ollama_num_ctx, Some(32_768));
    }

    #[test]
    fn build_chat_request_carries_live_offload_setting() {
        // The provider's config is frozen at startup, so the live offload toggle
        // must ride on the request to take effect on the next turn.
        let mut state = fresh_state();
        assert_eq!(
            build_chat_request(&state).ollama_allow_ram_offload,
            Some(false)
        );
        state.settings.ollama.allow_ram_offload = true;
        assert_eq!(
            build_chat_request(&state).ollama_allow_ram_offload,
            Some(true)
        );
    }

    #[test]
    fn provider_context_resolved_stored_in_runtime() {
        use crate::models::adapters::ollama_sizing::NumCtxSource;
        let state = fresh_state();
        let (state, _) = update(
            state,
            Msg::ProviderContextResolved {
                model_id: "ollama/test".to_string(),
                model_max: Some(262_144),
                effective: Some(12_288),
                source: Some(NumCtxSource::Auto),
                max_output: Some(64_000),
            },
        );
        let ctx = state.runtime.ollama_context.expect("stored");
        assert_eq!(ctx.model_max, Some(262_144));
        assert_eq!(ctx.effective, Some(12_288));
        // The live values also refresh the capability snapshot (this is what
        // turns `Context: unknown` into a real number for remote providers).
        assert_eq!(
            state.runtime.provider_capabilities.max_context_tokens,
            Some(12_288)
        );
        assert_eq!(
            state.runtime.provider_capabilities.max_output_tokens,
            Some(64_000)
        );
    }

    #[test]
    fn provider_context_resolved_ignores_probe_for_other_model() {
        // A window probe that lands after a /model switch (model_id != session
        // model) must not overwrite the active model's context window.
        use crate::models::adapters::ollama_sizing::NumCtxSource;
        let state = fresh_state();
        let (state, _) = update(
            state,
            Msg::ProviderContextResolved {
                model_id: "ollama/other".to_string(),
                model_max: Some(262_144),
                effective: Some(12_288),
                source: Some(NumCtxSource::Auto),
                max_output: Some(64_000),
            },
        );
        assert!(state.runtime.ollama_context.is_none());
        // The stale probe must not leak into the capability snapshot either.
        assert_ne!(
            state.runtime.provider_capabilities.max_output_tokens,
            Some(64_000)
        );
    }

    // A spill with no fitting smaller window (weights-bound) → the warn path.
    fn placement_msg(model_id: &str, vram: u64, total: u64) -> Msg {
        Msg::OllamaPlacementResolved {
            model_id: model_id.to_string(),
            size_vram_bytes: vram,
            total_bytes: total,
            suggested_num_ctx: None,
        }
    }

    fn cpu_warn_count(state: &State) -> usize {
        state
            .session
            .messages()
            .iter()
            .filter(|m| m.role == MessageRole::System && m.content.contains("CPU/RAM"))
            .count()
    }

    #[test]
    fn ollama_placement_stored_and_warns_once_when_offloaded() {
        let state = fresh_state(); // session model is ollama/test; offload off by default
        assert!(!state.settings.ollama.allow_ram_offload);
        let (state, _) = update(
            state,
            placement_msg("ollama/test", 6_000_000_000, 8_000_000_000),
        );
        let p = state.runtime.ollama_placement.expect("stored");
        assert!(p.offloaded());
        assert_eq!(p.percent_on_cpu(), 25);
        assert_eq!(cpu_warn_count(&state), 1);
        assert!(state.runtime.offload_warned.contains("ollama/test"));
        // A second probe for the same model must not warn again.
        let (state, _) = update(
            state,
            placement_msg("ollama/test", 6_000_000_000, 8_000_000_000),
        );
        assert_eq!(cpu_warn_count(&state), 1);
    }

    #[test]
    fn ollama_placement_no_warn_when_offload_on() {
        let mut state = fresh_state();
        state.settings.ollama.allow_ram_offload = true;
        // Fully on CPU, but the user explicitly accepted RAM → silent.
        let (state, _) = update(state, placement_msg("ollama/test", 0, 8_000_000_000));
        assert!(state.runtime.ollama_placement.expect("stored").offloaded());
        assert_eq!(cpu_warn_count(&state), 0);
    }

    #[test]
    fn ollama_placement_no_warn_when_fully_on_gpu() {
        let state = fresh_state();
        let (state, _) = update(
            state,
            placement_msg("ollama/test", 8_000_000_000, 8_000_000_000),
        );
        assert!(!state.runtime.ollama_placement.expect("stored").offloaded());
        assert_eq!(cpu_warn_count(&state), 0);
    }

    #[test]
    fn ollama_placement_ignores_probe_for_other_model() {
        // A probe that lands after a /model switch (model_id != session model).
        let state = fresh_state();
        let (state, _) = update(state, placement_msg("ollama/other", 0, 8_000_000_000));
        assert!(state.runtime.ollama_placement.is_none());
        assert!(!state.runtime.offload_warned.contains("ollama/other"));
        assert_eq!(cpu_warn_count(&state), 0);
    }

    #[test]
    fn ollama_placement_offload_math_boundaries() {
        use crate::domain::runtime::OllamaPlacement;
        let p = |vram, total| OllamaPlacement {
            size_vram_bytes: vram,
            total_bytes: total,
        };
        assert!(!p(100, 100).offloaded());
        assert_eq!(p(100, 100).percent_on_cpu(), 0);
        assert!(p(0, 100).offloaded());
        assert_eq!(p(0, 100).percent_on_cpu(), 100);
        assert_eq!(p(75, 100).percent_on_cpu(), 25);
        // vram > total can't really happen, but must not panic / underflow.
        assert!(!p(200, 100).offloaded());
        assert_eq!(p(200, 100).percent_on_cpu(), 0);
        // Unknown footprint → 0, not a divide-by-zero.
        assert_eq!(p(0, 0).percent_on_cpu(), 0);
    }

    fn converge_msg(model_id: &str, vram: u64, total: u64, suggested: u32) -> Msg {
        Msg::OllamaPlacementResolved {
            model_id: model_id.to_string(),
            size_vram_bytes: vram,
            total_bytes: total,
            suggested_num_ctx: Some(suggested),
        }
    }

    #[test]
    fn ollama_placement_auto_converges_to_suggested_window() {
        // Spilled, but a smaller window fits → adopt it (no warning), and
        // build_chat_request should then send it.
        let state = fresh_state();
        let (state, _) = update(
            state,
            converge_msg("ollama/test", 6_000_000_000, 8_000_000_000, 8_192),
        );
        assert_eq!(
            state.runtime.ollama_converged_num_ctx.get("ollama/test"),
            Some(&8_192)
        );
        assert_eq!(cpu_warn_count(&state), 0); // converged, not warned
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("Reduced") && m.content.contains("fits your GPU"))
        );
        assert_eq!(build_chat_request(&state).ollama_num_ctx, Some(8_192));
    }

    #[test]
    fn ollama_placement_does_not_converge_when_user_pinned() {
        // The user explicitly set a window → don't auto-resize it; warn instead.
        let mut state = fresh_state();
        state
            .settings
            .ollama_num_ctx_per_model
            .insert("ollama/test".to_string(), 32_768);
        let (state, _) = update(
            state,
            converge_msg("ollama/test", 6_000_000_000, 8_000_000_000, 8_192),
        );
        assert!(
            !state
                .runtime
                .ollama_converged_num_ctx
                .contains_key("ollama/test")
        );
        assert_eq!(cpu_warn_count(&state), 1);
        // Their pinned value still wins.
        assert_eq!(build_chat_request(&state).ollama_num_ctx, Some(32_768));
    }

    #[test]
    fn ollama_placement_never_converges_below_conversation_size() {
        // A fitting window smaller than the live conversation would wedge the
        // session (every turn truncates) — keep the larger window and warn.
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("word ".repeat(8_000)), state.now); // ≫ 4096 tokens
        let (state, _) = update(
            state,
            converge_msg("ollama/test", 6_000_000_000, 8_000_000_000, 4_096),
        );
        assert!(
            !state
                .runtime
                .ollama_converged_num_ctx
                .contains_key("ollama/test"),
            "must not shrink below the conversation"
        );
        assert_eq!(cpu_warn_count(&state), 1);
        assert_eq!(build_chat_request(&state).ollama_num_ctx, None); // window stays auto-fit
    }

    #[test]
    fn slash_context_auto_clears_converged_value() {
        use crate::domain::ContextCmd;
        let mut state = fresh_state();
        state
            .runtime
            .ollama_converged_num_ctx
            .insert("ollama/test".to_string(), 8_192);
        let (state, _) = update(state, Msg::Slash(SlashCmd::Context(ContextCmd::Auto)));
        assert!(
            !state
                .runtime
                .ollama_converged_num_ctx
                .contains_key("ollama/test")
        );
        assert_eq!(build_chat_request(&state).ollama_num_ctx, None); // back to raw auto-fit
    }

    #[test]
    fn slash_visible_reasoning_toggles_runtime_ui_state() {
        let state = fresh_state();
        let (state, _) = update(state, Msg::Slash(SlashCmd::VisibleReasoning(None)));
        assert!(state.ui.show_reasoning);

        let (state, _) = update(
            state,
            Msg::Slash(SlashCmd::VisibleReasoning(Some("off".to_string()))),
        );
        assert!(!state.ui.show_reasoning);
    }

    #[test]
    fn cycle_safety_walks_by_permissiveness() {
        use crate::runtime::SafetyMode as S;
        assert_eq!(cycle_safety(S::ReadOnly), S::Ask);
        assert_eq!(cycle_safety(S::Ask), S::Auto);
        assert_eq!(cycle_safety(S::Auto), S::FullAccess);
        assert_eq!(cycle_safety(S::FullAccess), S::ReadOnly);
    }

    #[test]
    fn shift_tab_cycles_safety_mode() {
        let state = fresh_state();
        let start = state.session.safety_mode;
        let (state, _) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::BackTab,
                modifiers: KeyMods::NONE,
            }),
        );
        assert_eq!(state.session.safety_mode, cycle_safety(start));
    }

    #[test]
    fn slash_safety_sets_session_mode() {
        let state = fresh_state();
        let (state, _) = update(
            state,
            Msg::Slash(SlashCmd::Safety(Some(crate::runtime::SafetyMode::Auto))),
        );
        assert_eq!(state.session.safety_mode, crate::runtime::SafetyMode::Auto);
    }

    /// A tool-result shaped exactly like a real read-only policy denial
    /// (`{summary} blocked by policy: {marker} blocks …`), built from the shared
    /// marker so the test exercises the real detection path.
    fn readonly_denial_message(summary: &str) -> ChatMessage {
        ChatMessage::tool(
            "call-x",
            "execute_command",
            format!(
                "{summary} blocked by policy: {} blocks mutations and control actions",
                crate::runtime::READ_ONLY_DENIAL_MARKER
            ),
        )
    }

    #[test]
    fn superseded_readonly_denial_is_rewritten_when_mode_loosened() {
        use crate::runtime::SafetyMode;
        let mut msgs = vec![readonly_denial_message("write_file(main.qml)")];
        neutralize_superseded_policy_denials(&mut msgs, SafetyMode::FullAccess);
        let content = &msgs[0].content;
        assert!(
            !content.contains("blocked by policy"),
            "standing denial phrasing should be gone: {content:?}"
        );
        assert!(
            content.contains("write_file(main.qml)"),
            "action summary must be kept: {content:?}"
        );
        assert!(
            content.contains("no longer applies"),
            "should read as lifted: {content:?}"
        );
        assert!(
            content.contains("full_access"),
            "should name the now-current mode: {content:?}"
        );
    }

    #[test]
    fn readonly_denial_preserved_in_read_only_mode() {
        use crate::runtime::SafetyMode;
        let original = readonly_denial_message("write_file(x)");
        let mut msgs = vec![original.clone()];
        neutralize_superseded_policy_denials(&mut msgs, SafetyMode::ReadOnly);
        assert_eq!(
            msgs[0].content, original.content,
            "a still-valid denial must be untouched in read_only"
        );
    }

    #[test]
    fn neutralizer_ignores_non_tool_and_non_denial_messages() {
        use crate::runtime::SafetyMode;
        // Role gate: a USER message quoting the full signature is left alone.
        let quote = ChatMessage::user(format!(
            "it said: blocked by policy: {} blocks mutations and control actions",
            crate::runtime::READ_ONLY_DENIAL_MARKER
        ));
        // Contiguous-signature gate: a tool result that merely contains the
        // marker text (e.g. a grep of the source) is not a denial.
        let grep = ChatMessage::tool(
            "c",
            "execute_command",
            format!(
                "policy.rs: const MARKER = {:?};",
                crate::runtime::READ_ONLY_DENIAL_MARKER
            ),
        );
        let mut msgs = vec![quote.clone(), grep.clone()];
        neutralize_superseded_policy_denials(&mut msgs, SafetyMode::FullAccess);
        assert_eq!(msgs[0].content, quote.content, "user message untouched");
        assert_eq!(
            msgs[1].content, grep.content,
            "non-denial tool result untouched"
        );
    }

    #[test]
    fn leaving_read_only_past_a_stale_denial_injects_a_hidden_nudge() {
        use crate::runtime::SafetyMode;
        let mut state = fresh_state();
        state.session.safety_mode = SafetyMode::ReadOnly;
        state
            .session
            .append(readonly_denial_message("write_file(x)"), state.now);
        let (state, _) = update(state, key(KeyCode::BackTab));
        assert_eq!(state.session.safety_mode, SafetyMode::Ask);
        let nudge = state
            .session
            .messages()
            .iter()
            .find(|m| m.content.contains("Safety mode is now ask"))
            .expect("leaving read_only past a stale denial should inject the nudge");
        assert_eq!(nudge.role, MessageRole::System);
        assert_eq!(
            nudge.kind,
            crate::models::ChatMessageKind::RecoveryNudge,
            "the nudge is for the model only — RecoveryNudge hides it from the transcript",
        );
    }

    #[test]
    fn further_loosening_replaces_the_pending_nudge_instead_of_stacking() {
        use crate::runtime::SafetyMode;
        let mut state = fresh_state();
        state.session.safety_mode = SafetyMode::ReadOnly;
        state
            .session
            .append(readonly_denial_message("write_file(x)"), state.now);
        // The screenshot bug: cycling read_only → ask → auto → full_access
        // announced three times. Now the pending nudge is carried forward,
        // renamed to the current mode.
        let (state, _) = update(state, key(KeyCode::BackTab));
        let (state, _) = update(state, key(KeyCode::BackTab));
        let (state, _) = update(state, key(KeyCode::BackTab));
        assert_eq!(state.session.safety_mode, SafetyMode::FullAccess);
        let nudges: Vec<_> = state
            .session
            .messages()
            .iter()
            .filter(|m| m.content.starts_with(SAFETY_NUDGE_PREFIX))
            .collect();
        assert_eq!(nudges.len(), 1, "exactly one pending nudge, never a stack");
        assert!(
            nudges[0].content.contains("full_access"),
            "the surviving nudge names the current mode: {:?}",
            nudges[0].content
        );
    }

    #[test]
    fn loosening_after_the_nudge_was_spent_stays_silent() {
        use crate::runtime::SafetyMode;
        let mut state = fresh_state();
        state.session.safety_mode = SafetyMode::ReadOnly;
        state
            .session
            .append(readonly_denial_message("write_file(x)"), state.now);
        let (mut state, _) = update(state, key(KeyCode::BackTab)); // → ask, nudge pending
        sweep_spent_nudges(&mut state); // the request it steered went out
        let (state, _) = update(state, key(KeyCode::BackTab)); // ask → auto
        assert!(
            !state
                .session
                .messages()
                .iter()
                .any(|m| m.content.starts_with(SAFETY_NUDGE_PREFIX)),
            "a later loosening must not re-announce a spent nudge",
        );
    }

    #[test]
    fn tightening_back_to_read_only_retracts_the_pending_nudge() {
        use crate::runtime::SafetyMode;
        let mut state = fresh_state();
        state.session.safety_mode = SafetyMode::ReadOnly;
        state
            .session
            .append(readonly_denial_message("write_file(x)"), state.now);
        let (state, _) = update(state, key(KeyCode::BackTab)); // → ask, nudge pending
        let (state, _) = update(
            state,
            Msg::Slash(SlashCmd::Safety(Some(SafetyMode::ReadOnly))),
        );
        assert!(
            !state
                .session
                .messages()
                .iter()
                .any(|m| m.content.starts_with(SAFETY_NUDGE_PREFIX)),
            "back in read_only the denials stand again — the lifted-note must not ride the next request",
        );
    }

    #[test]
    fn clean_mode_cycle_stays_silent() {
        use crate::runtime::SafetyMode;
        let mut state = fresh_state();
        state.session.safety_mode = SafetyMode::ReadOnly;
        let (state, _) = update(state, key(KeyCode::BackTab));
        assert_eq!(state.session.safety_mode, SafetyMode::Ask);
        assert!(
            !state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("Safety mode is now")),
            "a clean mode cycle must not add a banner",
        );
    }

    #[test]
    fn slash_safety_loosening_announces_when_denial_present() {
        use crate::runtime::SafetyMode;
        let mut state = fresh_state();
        state.session.safety_mode = SafetyMode::ReadOnly;
        state
            .session
            .append(readonly_denial_message("edit(main.qml)"), state.now);
        let (state, _) = update(
            state,
            Msg::Slash(SlashCmd::Safety(Some(SafetyMode::FullAccess))),
        );
        assert_eq!(state.session.safety_mode, SafetyMode::FullAccess);
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.role == MessageRole::System
                    && m.content.contains("Safety mode is now full_access")),
        );
    }

    #[test]
    fn build_chat_request_neutralizes_a_superseded_denial() {
        use crate::runtime::SafetyMode;
        // Full production path: a read_only denial sits in history behind a valid
        // tool_use/tool_result pair (so `normalize_history` keeps it); once the
        // live mode is looser, `build_chat_request` must hand the model a
        // rewritten, non-standing note rather than the original block.
        let mut state = fresh_state();
        state.session.safety_mode = SafetyMode::FullAccess;
        let call = crate::models::tool_call::ToolCall {
            id: Some("call-1".to_string()),
            function: crate::models::tool_call::FunctionCall {
                name: "write_file".to_string(),
                arguments: serde_json::json!({ "path": "main.qml" }),
            },
        };
        state.session.append(
            ChatMessage::assistant("editing").with_tool_calls(vec![call]),
            state.now,
        );
        state.session.append(
            ChatMessage::tool(
                "call-1",
                "write_file",
                format!(
                    "write_file(main.qml) blocked by policy: {} blocks mutations and control actions",
                    crate::runtime::READ_ONLY_DENIAL_MARKER
                ),
            ),
            state.now,
        );

        let req = build_chat_request(&state);
        let tool_msg = req
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("the tool_result should survive into the request");
        assert!(
            !tool_msg.content.contains("blocked by policy"),
            "build_chat_request must neutralize the stale denial: {:?}",
            tool_msg.content
        );
        assert!(
            tool_msg.content.contains("no longer applies"),
            "rewritten note expected: {:?}",
            tool_msg.content
        );
    }

    /// State with one queued approval (turn must accept the message, so put
    /// the reducer in a live turn first).
    fn pending_approval_state() -> State {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
        let (state, _) = update(
            state,
            Msg::ApprovalRequested {
                turn: TurnId(1),
                call_id: super::super::ids::ToolCallId(5),
                tool: "execute_command".to_string(),
                risk: "shell_mutation".to_string(),
                kind: crate::domain::ApprovalKind::Shell,
                prompt: "$ npm test".to_string(),
                allowlist_scope: "execute_command:npm".to_string(),
            },
        );
        state
    }

    fn key(code: KeyCode) -> Msg {
        Msg::Key(Key {
            code,
            modifiers: KeyMods::NONE,
        })
    }

    #[test]
    fn ctrl_b_backgrounds_running_tool() {
        let ctrl_b = Msg::Key(Key {
            code: KeyCode::Char('b'),
            modifiers: KeyMods {
                ctrl: true,
                ..KeyMods::NONE
            },
        });
        // While tools are executing → emit BackgroundScope(turn).
        let mut state = fresh_state();
        state.turn = start_executing_tools(TurnId(9), Vec::new(), std::time::SystemTime::now());
        let (_s, cmds) = update(state, ctrl_b.clone());
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::BackgroundScope(t) if *t == TurnId(9))),
            "Ctrl+B during tool execution should background the scope"
        );
        // Idle → swallowed, no BackgroundScope.
        let (_s, cmds) = update(fresh_state(), ctrl_b);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::BackgroundScope(_))));
    }

    #[test]
    fn theme_command_switches_persists_and_reports() {
        use crate::app::ThemeChoice;
        // /theme light → state flips, persist emitted, confirmation appended.
        let (state, cmds) = update(
            fresh_state(),
            Msg::Slash(SlashCmd::Theme(Some("light".to_string()))),
        );
        assert_eq!(state.ui.theme, ThemeChoice::Light);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::PersistUiTheme(ThemeChoice::Light)))
        );
        // /theme (no arg) → reports current, persists nothing.
        let (state, cmds) = update(state, Msg::Slash(SlashCmd::Theme(None)));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::PersistUiTheme(_))));
        let last = state.session.messages().last().unwrap().content.clone();
        assert!(last.contains("light"), "shows current theme: {last}");
        // Bad arg → usage, no state change, no persist.
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Theme(Some("solarized".to_string()))),
        );
        assert_eq!(state.ui.theme, ThemeChoice::Light);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::PersistUiTheme(_))));
        let last = state.session.messages().last().unwrap().content.clone();
        assert!(last.contains("Usage: /theme"), "usage line: {last}");
    }

    #[test]
    fn theme_command_notes_no_color() {
        let mut state = fresh_state();
        state.ui.no_color = true;
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Theme(Some("light".to_string()))),
        );
        // Still persists (applies when NO_COLOR is unset) but says so.
        assert!(cmds.iter().any(|c| matches!(c, Cmd::PersistUiTheme(_))));
        let last = state.session.messages().last().unwrap().content.clone();
        assert!(last.contains("NO_COLOR"), "notes NO_COLOR: {last}");
    }

    #[test]
    fn ctrl_o_composes_draft_in_editor() {
        let ctrl_o = Msg::Key(Key {
            code: KeyCode::Char('o'),
            modifiers: KeyMods {
                ctrl: true,
                ..KeyMods::NONE
            },
        });
        // Idle with a draft → emits ComposeInEditor carrying it.
        let mut state = fresh_state();
        state.ui.input_buffer = "half-typed prompt".to_string();
        let (_s, cmds) = update(state, ctrl_o.clone());
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::ComposeInEditor { text } if text == "half-typed prompt"))
        );
        // Busy (generating) → still allowed; it only edits the draft.
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(3), std::time::SystemTime::now());
        let (_s, cmds) = update(state, ctrl_o.clone());
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::ComposeInEditor { .. }))
        );
        // Over a modal surface (model list) → swallowed.
        let mut state = fresh_state();
        state.ui.mode = UiMode::ModelList;
        let (_s, cmds) = update(state, ctrl_o);
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::ComposeInEditor { .. }))
        );
        // /editor also routes to the compose command.
        let (_s, cmds) = update(fresh_state(), Msg::Slash(SlashCmd::Editor));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::ComposeInEditor { .. }))
        );
    }

    #[test]
    fn editor_returned_replaces_draft() {
        let mut state = fresh_state();
        state.ui.input_buffer = "old draft".to_string();
        state.ui.input_cursor = 3;
        let (state, cmds) = update(
            state,
            Msg::EditorReturned {
                text: Some("new draft from vim".to_string()),
            },
        );
        assert_eq!(state.ui.input_buffer, "new draft from vim");
        assert_eq!(state.ui.input_cursor, state.ui.input_buffer.len());
        assert!(cmds.is_empty());
        // Empty Some = deliberate clear.
        let (state, _) = update(
            state,
            Msg::EditorReturned {
                text: Some(String::new()),
            },
        );
        assert!(state.ui.input_buffer.is_empty());
        // None = no-op.
        let mut state = fresh_state();
        state.ui.input_buffer = "kept".to_string();
        let (state, _) = update(state, Msg::EditorReturned { text: None });
        assert_eq!(state.ui.input_buffer, "kept");
    }

    fn plugin_cmd(name: &str, body: &str) -> crate::domain::PluginCommand {
        crate::domain::PluginCommand {
            name: name.to_string(),
            description: "does things".to_string(),
            body: body.to_string(),
            plugin: "demo".to_string(),
        }
    }

    #[test]
    fn plugin_command_expands_into_a_prompt_submit() {
        let mut state = fresh_state();
        state.plugin_commands = vec![plugin_cmd("deploy", "Deploy to $ARGUMENTS now.")];
        state.ui.input_buffer = "/deploy prod".to_string();
        let (mut state, _) = update(state, key(KeyCode::Enter));
        // The reducer re-enters pending_msgs itself; the expansion lands as a
        // committed user message (transcript shows the EXPANSION, so
        // recordings replay without the plugin installed).
        let last_user = state
            .session
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == crate::models::MessageRole::User)
            .map(|m| m.content.clone());
        assert_eq!(last_user.as_deref(), Some("Deploy to prod now."));
        assert!(state.ui.input_buffer.is_empty());
        // No-args + no token: body submits verbatim.
        state.turn = crate::domain::TurnState::Idle;
        state.plugin_commands = vec![plugin_cmd("ship", "Ship it.")];
        state.ui.input_buffer = "/ship".to_string();
        let (state, _) = update(state, key(KeyCode::Enter));
        let queued_or_committed = state
            .session
            .messages()
            .iter()
            .any(|m| m.content == "Ship it.")
            || state
                .ui
                .queued_messages
                .iter()
                .any(|q| q.text == "Ship it.");
        assert!(queued_or_committed, "plugin body submitted or queued");
    }

    #[test]
    fn unknown_slash_still_reports_unknown_not_plugin() {
        let mut state = fresh_state();
        state.plugin_commands = vec![plugin_cmd("deploy", "body")];
        state.ui.input_buffer = "/nosuch".to_string();
        let (state, _) = update(state, key(KeyCode::Enter));
        let last = state.session.messages().last().unwrap().content.clone();
        assert!(last.contains("Unknown command: /nosuch"), "{last}");
    }

    #[test]
    fn builtin_wins_over_same_named_plugin_command() {
        // Structural guarantee on top of the loader's shadowing filter.
        let mut state = fresh_state();
        state.plugin_commands = vec![plugin_cmd("help", "hijacked")];
        state.ui.input_buffer = "/help".to_string();
        let (state, _) = update(state, key(KeyCode::Enter));
        let last = state.session.messages().last().unwrap().content.clone();
        assert!(
            last.contains("Mermaid commands"),
            "built-in help ran: {last}"
        );
        assert!(!last.contains("hijacked"));
    }

    #[test]
    fn palette_filter_entries_appends_plugins_and_agrees_on_indices() {
        use crate::domain::slash_commands::{COMMAND_REGISTRY, filter_entries};
        let plugin = vec![plugin_cmd("deploy", "body")];
        let all = filter_entries("", &plugin);
        assert_eq!(all.len(), COMMAND_REGISTRY.len() + 1);
        assert_eq!(all.last().unwrap().name(), "deploy");
        assert!(all.last().unwrap().description().contains("(plugin:demo)"));
        // Prefix filtering reaches plugin rows too.
        let d = filter_entries("dep", &plugin);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name(), "deploy");
        // Tab-completion path: cursor over the plugin row completes it.
        let mut state = fresh_state();
        state.plugin_commands = plugin;
        state.ui.input_buffer = "/dep".to_string();
        state.ui.palette_cursor = Some(0);
        let (state, _) = update(state, key(KeyCode::Tab));
        assert_eq!(state.ui.input_buffer, "/deploy ");
    }

    #[test]
    fn help_lists_plugin_commands() {
        let mut state = fresh_state();
        state.plugin_commands = vec![plugin_cmd("deploy", "body")];
        let (state, _) = update(state, Msg::Slash(SlashCmd::Help));
        let last = state.session.messages().last().unwrap().content.clone();
        assert!(last.contains("Plugin commands:"), "{last}");
        assert!(
            last.contains("/deploy - does things (plugin:demo)"),
            "{last}"
        );
    }

    #[test]
    fn plugin_command_expand_cases() {
        let cmd = plugin_cmd("x", "Do $ARGUMENTS and $ARGUMENTS.");
        assert_eq!(cmd.expand("this"), "Do this and this.");
        assert_eq!(cmd.expand("  "), "Do  and .");
        let cmd = plugin_cmd("x", "Just do it.");
        assert_eq!(cmd.expand(""), "Just do it.");
        assert_eq!(cmd.expand("with args"), "Just do it.\n\nwith args");
    }

    #[test]
    fn paste_interleaved_with_keys_preserves_order() {
        // Reproduces the Windows paste scramble: a paste burst splits into
        // stray Char keys + coalesced Paste chunks. Both must insert at the
        // cursor and advance it, so the result stays in order regardless of
        // the split. (Before the fix, Paste appended to the end while keys
        // inserted at a never-advanced cursor, yielding "RDeview the Docs".)
        let mut state = fresh_state();
        for msg in [
            key(KeyCode::Char('R')),
            Msg::Paste(Paste::Text("eview the ".to_string())),
            key(KeyCode::Char('D')),
            Msg::Paste(Paste::Text("ocs".to_string())),
        ] {
            let (next, _) = update(state, msg);
            state = next;
        }
        assert_eq!(state.ui.input_buffer, "Review the Docs");
        assert_eq!(state.ui.input_cursor, state.ui.input_buffer.len());
    }

    #[test]
    fn paste_inserts_at_cursor_not_end() {
        // Type "ac", move left one, paste "b" → "abc" (not "acb").
        let mut state = fresh_state();
        for msg in [
            key(KeyCode::Char('a')),
            key(KeyCode::Char('c')),
            key(KeyCode::Left),
            Msg::Paste(Paste::Text("b".to_string())),
        ] {
            let (next, _) = update(state, msg);
            state = next;
        }
        assert_eq!(state.ui.input_buffer, "abc");
    }

    #[test]
    fn approval_requested_enqueues_modal() {
        let state = pending_approval_state();
        assert_eq!(state.pending_approval.len(), 1);
        assert_eq!(
            state.pending_approval.front().unwrap().tool,
            "execute_command"
        );
    }

    #[test]
    fn approval_requested_during_cancelling_is_dropped() {
        // #74: a tool task unwinding under cancellation can still emit an
        // ApprovalRequested; parking a modal for it would outlive the turn.
        let mut state = fresh_state();
        state.turn = TurnState::Cancelling {
            id: TurnId(1),
            since: std::time::SystemTime::now(),
        };
        let (state, _) = update(
            state,
            Msg::ApprovalRequested {
                turn: TurnId(1),
                call_id: super::super::ids::ToolCallId(5),
                tool: "execute_command".to_string(),
                risk: "shell_mutation".to_string(),
                kind: crate::domain::ApprovalKind::Shell,
                prompt: "$ rm -rf /".to_string(),
                allowlist_scope: "execute_command:rm".to_string(),
            },
        );
        assert!(
            state.pending_approval.is_empty(),
            "approval for a cancelling turn must not be queued (#74)"
        );
    }

    #[test]
    fn copy_selection_emits_clipboard_cmd_when_nonempty() {
        // #18: the copy side effect flows through the reducer as a Cmd.
        let (_s, cmds) = update(fresh_state(), Msg::CopySelection("hello".to_string()));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::CopyToClipboard(t) if t == "hello")),
            "non-empty selection should emit CopyToClipboard"
        );
        // An empty selection is a no-op — no clipboard Cmd.
        let (_s, cmds) = update(fresh_state(), Msg::CopySelection(String::new()));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::CopyToClipboard(_))));
    }

    #[test]
    fn approval_keys_emit_the_right_decision() {
        use crate::domain::ApprovalChoice as A;
        for (code, expected) in [
            (KeyCode::Char('1'), A::Approve),
            (KeyCode::Char('y'), A::Approve),
            (KeyCode::Enter, A::Approve),
            (KeyCode::Char('2'), A::ApproveAlways),
            (KeyCode::Char('a'), A::ApproveAlways),
            (KeyCode::Char('3'), A::Deny),
            (KeyCode::Char('n'), A::Deny),
            (KeyCode::Escape, A::Deny),
        ] {
            let (state, cmds) = update(pending_approval_state(), key(code));
            assert!(
                state.pending_approval.is_empty(),
                "{code:?} should pop the modal"
            );
            assert!(
                cmds.iter().any(
                    |c| matches!(c, Cmd::ResolveApproval { decision, .. } if *decision == expected)
                ),
                "{code:?} should resolve {expected:?}; got {cmds:?}",
            );
            // Esc on an approval denies the tool — it must NOT cancel the turn.
            if code == KeyCode::Escape {
                assert!(
                    !cmds.iter().any(|c| matches!(c, Cmd::CancelScope(_))),
                    "Esc on an approval must deny, not cancel the turn",
                );
            }
        }
    }

    #[test]
    fn approval_modal_swallows_unrelated_keys() {
        let (state, cmds) = update(pending_approval_state(), key(KeyCode::Char('x')));
        assert_eq!(
            state.pending_approval.len(),
            1,
            "unrelated key must not pop the modal"
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn approval_arrows_move_highlight_without_resolving() {
        // ↓ moves the highlight and clamps at the last option; ↑ moves back.
        // Neither resolves the modal.
        let (state, cmds) = update(pending_approval_state(), key(KeyCode::Down));
        assert_eq!(state.pending_approval.front().unwrap().selected_option, 1);
        assert!(cmds.is_empty() && state.pending_approval.len() == 1);

        let (state, _) = update(state, key(KeyCode::Down));
        assert_eq!(state.pending_approval.front().unwrap().selected_option, 2);
        let (state, _) = update(state, key(KeyCode::Down)); // clamps at 2
        assert_eq!(state.pending_approval.front().unwrap().selected_option, 2);
        let (state, _) = update(state, key(KeyCode::Up));
        assert_eq!(state.pending_approval.front().unwrap().selected_option, 1);
    }

    #[test]
    fn approval_enter_resolves_the_highlighted_option() {
        use crate::domain::ApprovalChoice as A;
        // Highlight option 3 (No) with two ↓, then Enter → Deny.
        let (state, _) = update(pending_approval_state(), key(KeyCode::Down));
        let (state, _) = update(state, key(KeyCode::Down));
        let (state, cmds) = update(state, key(KeyCode::Enter));
        assert!(
            state.pending_approval.is_empty(),
            "Enter should pop the modal"
        );
        assert!(
            cmds.iter().any(
                |c| matches!(c, Cmd::ResolveApproval { decision, .. } if *decision == A::Deny)
            ),
            "Enter on the highlighted 'No' must deny; got {cmds:?}"
        );
    }

    #[test]
    fn approval_fifo_shows_one_at_a_time() {
        let state = pending_approval_state();
        let (state, _) = update(
            state,
            Msg::ApprovalRequested {
                turn: TurnId(1),
                call_id: super::super::ids::ToolCallId(6),
                tool: "write_file".to_string(),
                risk: "file_mutation".to_string(),
                kind: crate::domain::ApprovalKind::FileMutation,
                prompt: "src/x.rs".to_string(),
                allowlist_scope: "write_file".to_string(),
            },
        );
        assert_eq!(state.pending_approval.len(), 2);
        let (state, _) = update(state, key(KeyCode::Char('1')));
        assert_eq!(state.pending_approval.len(), 1);
        assert_eq!(state.pending_approval.front().unwrap().tool, "write_file");
    }

    #[test]
    fn clear_confirm_now_accepts_via_keypress() {
        // Regression: the /clear confirmation was inert (never rendered, never
        // key-handled). It now resolves on a keypress.
        let mut state = fresh_state();
        state.confirm = Some(super::super::state::Confirmation {
            prompt: "Clear conversation history?".to_string(),
            accept_msg_token: super::super::state::ConfirmationTarget::ClearConversation,
        });
        let (state, _) = update(state, key(KeyCode::Char('y')));
        assert!(
            state.confirm.is_none(),
            "y should accept and clear the confirm modal"
        );
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
        state.session.append(ChatMessage::user("one"), state.now);
        state
            .session
            .append(ChatMessage::assistant("two"), state.now);
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
        state.session.append(ChatMessage::user("kept"), state.now);
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
                    ..Default::default()
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
    fn build_chat_request_orders_mcp_tools_by_server_name() {
        // #F68: `state.mcp.servers` is a HashMap with per-process randomized
        // iteration order. `build_chat_request` must sort servers by name so the
        // emitted `ChatRequest.tools` ordering is deterministic across runs
        // (byte-reproducible requests / prompt-cache stability).
        let mut state = fresh_state();
        // Deferral would collapse the list to `tool_search`; this test pins
        // the DIRECT advertisement ordering, so turn deferral off.
        state.settings.mcp_defer_tools = Some(false);
        state.mcp = McpState::default();
        for name in ["zeta", "alpha", "mike", "bravo", "delta"] {
            state.mcp.servers.insert(
                name.to_string(),
                McpServerEntry {
                    config: crate::app::McpServerConfig {
                        command: "echo".to_string(),
                        args: vec![],
                        env: std::collections::HashMap::new(),
                        ..Default::default()
                    },
                    status: McpServerStatus::Ready,
                    tools: vec![crate::domain::state::McpToolSpec {
                        name: format!("mcp__{name}__do"),
                        raw_name: "do".to_string(),
                        description: "d".to_string(),
                        input_schema: serde_json::json!({}),
                    }],
                },
            );
        }
        let request = build_chat_request(&state);
        // Scope to the MCP portion: the reducer also appends the mode-scoped
        // plan tool (enter/exit_plan_mode) after the MCP block.
        let names: Vec<&str> = request
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .filter(|n| n.starts_with("mcp__"))
            .collect();
        assert_eq!(
            names,
            vec![
                "mcp__alpha__do",
                "mcp__bravo__do",
                "mcp__delta__do",
                "mcp__mike__do",
                "mcp__zeta__do",
            ],
            "MCP tools must be ordered by server name regardless of HashMap layout"
        );
    }

    #[test]
    fn tool_search_call_is_intercepted_and_promotes_for_the_follow_up() {
        // A `tool_search` call never reaches the effect layer: the reducer
        // resolves it purely, promotes the matches, and the follow-up
        // CallModel already advertises the promoted tool directly.
        let mut state = fresh_state();
        state.mcp.servers.insert(
            "srv".to_string(),
            McpServerEntry {
                config: crate::app::McpServerConfig::default(),
                status: McpServerStatus::Ready,
                tools: vec![crate::domain::state::McpToolSpec {
                    name: "mcp__srv__alpha".to_string(),
                    raw_name: "alpha".to_string(),
                    description: "does alpha things".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }],
            },
        );
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: vec![crate::models::ToolCall {
                id: Some("call_ts".to_string()),
                function: crate::models::FunctionCall {
                    name: super::super::tool_search::TOOL_SEARCH_NAME.to_string(),
                    arguments: serde_json::json!({"query": "alpha"}),
                },
            }],
            continuation: false,
        };
        let (state, cmds) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::ExecuteTool { .. })),
            "tool_search must not dispatch to the effect layer"
        );
        assert!(state.mcp.promoted.contains("mcp__srv__alpha"));
        let follow_up = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::CallModel { request, .. } => Some(request),
                _ => None,
            })
            .expect("interception completes the batch and fires the follow-up");
        // Scope to the MCP portion: the reducer also appends the mode-scoped
        // plan tool after the MCP block.
        let names: Vec<&str> = follow_up
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .filter(|n| n.starts_with("mcp__") || *n == "tool_search")
            .collect();
        assert_eq!(
            names,
            vec!["mcp__srv__alpha"],
            "promoted tool advertised directly; tool_search drops out once nothing is deferred"
        );
        // The tool result round-trips through the normal pairing machinery.
        let has_tool_result = state
            .session
            .messages()
            .iter()
            .any(|m| m.role == crate::models::MessageRole::Tool && m.content.contains("alpha"));
        assert!(has_tool_result, "tool_search outcome committed to history");
    }

    #[test]
    fn tool_search_with_deferral_off_returns_clean_no_op_outcome() {
        // A hallucinated tool_search while nothing is deferred must not
        // reach the effect layer's unknown-tool arm.
        let mut state = fresh_state();
        state.settings.mcp_defer_tools = Some(false);
        state.turn = TurnState::Generating {
            id: TurnId(6),
            started: std::time::SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: vec![crate::models::ToolCall {
                id: Some("call_ts2".to_string()),
                function: crate::models::FunctionCall {
                    name: super::super::tool_search::TOOL_SEARCH_NAME.to_string(),
                    arguments: serde_json::json!({"query": "anything"}),
                },
            }],
            continuation: false,
        };
        let (state, cmds) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(6),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::ExecuteTool { .. })));
        assert!(state.mcp.promoted.is_empty());
        let has_no_tools_note = state
            .session
            .messages()
            .iter()
            .any(|m| m.content.contains("No deferred MCP tools"));
        assert!(has_no_tools_note, "clean informative outcome committed");
    }

    fn pending_read_file_call() -> super::super::state::PendingToolCall {
        super::super::state::PendingToolCall {
            call_id: crate::domain::ToolCallId(1),
            source: crate::models::ToolCall {
                id: Some("call_a".to_string()),
                function: crate::models::FunctionCall {
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        }
    }

    #[test]
    fn steering_delivers_all_queued_messages_at_the_tool_boundary() {
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::assistant("calling a tool"), state.now);
        state.turn = super::super::transition::start_executing_tools(
            TurnId(1),
            vec![pending_read_file_call()],
            std::time::SystemTime::now(),
        );
        for text in ["steer one", "steer two"] {
            state
                .ui
                .queued_messages
                .push_back(super::super::state::QueuedMessage {
                    text: text.to_string(),
                    attachment_ids: vec![],
                });
        }
        let (state, cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(1),
                call_id: crate::domain::ToolCallId(1),
                outcome: ToolOutcome::success("file body", "read it", 0.1),
            },
        );
        assert!(state.ui.queued_messages.is_empty(), "queue fully drained");
        // Wire order: assistant → tool result → steered user texts, FIFO.
        let contents: Vec<&str> = state
            .session
            .messages()
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        let tool_pos = contents.iter().position(|c| c.contains("file body"));
        let one_pos = contents.iter().position(|c| *c == "steer one");
        let two_pos = contents.iter().position(|c| *c == "steer two");
        assert!(tool_pos < one_pos && one_pos < two_pos, "{contents:?}");
        // The follow-up request already carries the steered messages.
        let request = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::CallModel { request, .. } => Some(request),
                _ => None,
            })
            .expect("follow-up CallModel");
        assert!(request.messages.iter().any(|m| m.content == "steer two"));
        // User-authored text persists at the boundary, not only at StreamDone.
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
    }

    #[test]
    fn steering_resolves_queued_image_tokens_against_owned_attachments() {
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::assistant("calling a tool"), state.now);
        state.turn = super::super::transition::start_executing_tools(
            TurnId(1),
            vec![pending_read_file_call()],
            std::time::SystemTime::now(),
        );
        state.ui.attachments.push(super::super::state::Attachment {
            id: 7,
            number: 1,
            base64_data: "aGk=".to_string(),
            temp_path: std::path::PathBuf::from("/tmp/x.png"),
            size_bytes: 2,
            format: "png".to_string(),
        });
        state
            .ui
            .queued_messages
            .push_back(super::super::state::QueuedMessage {
                text: "look at [Image #1]".to_string(),
                attachment_ids: vec![7],
            });
        let (state, _) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(1),
                call_id: crate::domain::ToolCallId(1),
                outcome: ToolOutcome::success("done", "done", 0.1),
            },
        );
        let steered = state
            .session
            .messages()
            .iter()
            .find(|m| m.content.contains("[Image #1]"))
            .expect("steered message committed");
        assert_eq!(steered.images.as_ref().map(Vec::len), Some(1));
        assert!(state.ui.attachments.is_empty(), "attachment consumed");
    }

    #[test]
    fn execute_tool_cmd_carries_the_session_anchor() {
        let mut state = state_with_two_exchanges();
        let expected_session = state.session.conversation.id.clone();
        let expected_scratchpad = std::path::PathBuf::from("/data/tmp/scratchpad/-proj/s");
        state.session.scratchpad = Some(expected_scratchpad.clone());
        state.turn = TurnState::Generating {
            id: TurnId(9),
            started: std::time::SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: vec![crate::models::ToolCall {
                id: Some("call_b".to_string()),
                function: crate::models::FunctionCall {
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({}),
                },
            }],
            continuation: false,
        };
        let (state, cmds) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(9),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        let (session_id, message_index, scratchpad) = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::ExecuteTool {
                    session_id,
                    message_index,
                    scratchpad,
                    ..
                } => Some((session_id.clone(), *message_index, scratchpad.clone())),
                _ => None,
            })
            .expect("ExecuteTool dispatched");
        assert_eq!(session_id, expected_session);
        assert_eq!(
            scratchpad.as_deref(),
            Some(expected_scratchpad.as_path()),
            "the materialized scratch dir rides on the dispatch"
        );
        assert_eq!(
            message_index,
            state.session.messages().len(),
            "stamped at dispatch, after the assistant tool_use commit"
        );
    }

    #[test]
    fn alt_p_toggles_plan_mode_and_allocates_a_plan_path() {
        let key = || {
            Msg::Key(Key {
                code: KeyCode::Char('p'),
                modifiers: KeyMods {
                    alt: true,
                    ..Default::default()
                },
            })
        };
        let (state, _) = update(fresh_state(), key());
        let plan = state.session.plan.clone().expect("Alt+P enters plan mode");
        assert!(
            plan.plan_path.starts_with("/tmp/project/.mermaid/plans"),
            "plan file is project-local: {:?}",
            plan.plan_path
        );
        assert!(plan.plan_path.extension().is_some_and(|e| e == "md"));
        // The status band announces the mode — toggling adds no transcript row.
        assert!(
            state.session.messages().is_empty(),
            "entry adds no transcript row (status band carries the mode): {:?}",
            state.session.messages()
        );
        // The restore target is untouched — plan mode is not a safety mode.
        assert_eq!(state.session.safety_mode, Config::default().safety.mode);
        let (state, _) = update(state, key());
        assert!(state.session.plan.is_none(), "Alt+P toggles back off");
        assert!(
            state.session.messages().is_empty(),
            "exit adds no transcript row either: {:?}",
            state.session.messages()
        );
    }

    #[test]
    fn slash_plan_enters_shows_and_leaves() {
        let (state, _) = update(fresh_state(), Msg::Slash(SlashCmd::Plan(None)));
        assert!(state.session.plan.is_some());
        let (state, _) = update(state, Msg::Slash(SlashCmd::Plan(Some("show".to_string()))));
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.content.contains("Plan file (drafting):")),
            "/plan show prints the path"
        );
        let (state, _) = update(state, Msg::Slash(SlashCmd::Plan(Some("off".to_string()))));
        assert!(state.session.plan.is_none());
    }

    #[test]
    fn plan_path_allocation_is_deterministic() {
        // `--replay` must allocate the identical path: no wall clock, no RNG.
        let state = fresh_state();
        assert_eq!(plan_path_for(&state), plan_path_for(&state));
    }

    #[test]
    fn plan_mode_floors_dispatch_to_read_only_and_stamps_the_plan_file() {
        use crate::runtime::SafetyMode;
        let mut state = state_with_two_exchanges();
        state.session.safety_mode = SafetyMode::FullAccess;
        let plan_path = PathBuf::from("/tmp/project/.mermaid/plans/x.md");
        state.session.plan = Some(crate::domain::PlanState {
            plan_path: plan_path.clone(),
            prev_model_id: None,
            prev_reasoning: None,
        });
        state.turn = TurnState::Generating {
            id: TurnId(9),
            started: std::time::SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: vec![crate::models::ToolCall {
                id: Some("call_b".to_string()),
                function: crate::models::FunctionCall {
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({}),
                },
            }],
            continuation: false,
        };
        let (_state, cmds) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(9),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        let (mode, plan_file) = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::ExecuteTool {
                    safety_mode,
                    plan_file,
                    ..
                } => Some((*safety_mode, plan_file.clone())),
                _ => None,
            })
            .expect("ExecuteTool dispatched");
        assert_eq!(
            mode,
            SafetyMode::ReadOnly,
            "plan mode floors the effective mode even from full_access"
        );
        assert_eq!(plan_file, Some(plan_path));
    }

    #[test]
    fn build_chat_request_neutralizes_a_superseded_plan_denial() {
        // Once plan mode ends its denials stop describing the live policy —
        // the wire history must stop asserting them.
        let mut state = fresh_state();
        let call = crate::models::tool_call::ToolCall {
            id: Some("call-1".to_string()),
            function: crate::models::tool_call::FunctionCall {
                name: "write_file".to_string(),
                arguments: serde_json::json!({ "path": "main.qml" }),
            },
        };
        state.session.append(
            ChatMessage::assistant("editing").with_tool_calls(vec![call]),
            state.now,
        );
        state.session.append(
            ChatMessage::tool(
                "call-1",
                "write_file",
                format!(
                    "write_file(main.qml) blocked by policy: {} is active — planning only",
                    crate::runtime::PLAN_DENIAL_MARKER
                ),
            ),
            state.now,
        );

        // While planning: the denial stands verbatim.
        state.session.plan = Some(crate::domain::PlanState {
            plan_path: PathBuf::from("/tmp/project/.mermaid/plans/x.md"),
            prev_model_id: None,
            prev_reasoning: None,
        });
        let req = build_chat_request(&state);
        let tool_msg = req
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("tool_result in request");
        assert!(
            tool_msg.content.contains("blocked by policy"),
            "denial must stand while planning: {:?}",
            tool_msg.content
        );

        // Plan mode off: rewritten to a past-tense note.
        state.session.plan = None;
        let req = build_chat_request(&state);
        let tool_msg = req
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("tool_result in request");
        assert!(
            !tool_msg.content.contains("blocked by policy"),
            "stale plan denial must be neutralized: {:?}",
            tool_msg.content
        );
        assert!(tool_msg.content.contains("no longer applies"));
    }

    #[test]
    fn plan_mode_keeps_read_only_denials_standing() {
        use crate::runtime::SafetyMode;
        // full_access + planning: the EFFECTIVE mode is the read-only floor,
        // so a pre-plan read-only denial still describes reality — the
        // loosened-mode rewrite must not fire.
        let mut state = fresh_state();
        state.session.safety_mode = SafetyMode::FullAccess;
        state.session.plan = Some(crate::domain::PlanState {
            plan_path: PathBuf::from("/tmp/project/.mermaid/plans/x.md"),
            prev_model_id: None,
            prev_reasoning: None,
        });
        let call = crate::models::tool_call::ToolCall {
            id: Some("call-1".to_string()),
            function: crate::models::tool_call::FunctionCall {
                name: "write_file".to_string(),
                arguments: serde_json::json!({ "path": "main.qml" }),
            },
        };
        state.session.append(
            ChatMessage::assistant("editing").with_tool_calls(vec![call]),
            state.now,
        );
        state.session.append(
            ChatMessage::tool(
                "call-1",
                "write_file",
                format!(
                    "write_file(main.qml) blocked by policy: {} blocks mutations and control actions",
                    crate::runtime::READ_ONLY_DENIAL_MARKER
                ),
            ),
            state.now,
        );
        let req = build_chat_request(&state);
        let tool_msg = req
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("tool_result in request");
        assert!(
            tool_msg.content.contains("blocked by policy"),
            "read-only denial stands while the plan floor applies: {:?}",
            tool_msg.content
        );
    }

    #[test]
    fn system_prompt_names_the_scratchpad_path_once_ready() {
        let mut state = fresh_state();
        assert!(
            !system_prompt_for_state(&state).contains("Scratchpad directory:"),
            "no path line before ScratchpadReady lands"
        );
        state.session.scratchpad = Some(PathBuf::from("/tmp/mermaid-1000/-proj/s/scratchpad"));
        let prompt = system_prompt_for_state(&state);
        assert!(
            prompt.contains("Scratchpad directory: /tmp/mermaid-1000/-proj/s/scratchpad"),
            "the session block names the concrete scratchpad: {prompt}"
        );
    }

    #[test]
    fn system_prompt_carries_the_plan_block_only_while_planning() {
        let mut state = fresh_state();
        let prompt = system_prompt_for_state(&state);
        assert!(!prompt.contains("## Plan Mode"));
        state.session.plan = Some(crate::domain::PlanState {
            plan_path: PathBuf::from("/tmp/project/.mermaid/plans/x.md"),
            prev_model_id: None,
            prev_reasoning: None,
        });
        let prompt = system_prompt_for_state(&state);
        assert!(prompt.contains("## Plan Mode"));
        assert!(
            prompt.contains("/tmp/project/.mermaid/plans/x.md"),
            "the plan block names the concrete plan file"
        );
        assert!(
            prompt.contains("Safety mode: plan"),
            "the session block must not invite gated actions while planning"
        );
    }

    #[test]
    fn build_chat_request_advertises_the_right_plan_tool() {
        let mut state = fresh_state();
        let names = |state: &State| -> Vec<String> {
            build_chat_request(state)
                .tools
                .iter()
                .map(|t| t.name.clone())
                .collect()
        };
        // Not planning: enter_plan_mode only.
        let n = names(&state);
        assert!(n.contains(&"enter_plan_mode".to_string()));
        assert!(!n.contains(&"exit_plan_mode".to_string()));
        // Planning: exit_plan_mode only.
        state.session.plan = Some(crate::domain::PlanState {
            plan_path: PathBuf::from("/tmp/project/.mermaid/plans/x.md"),
            prev_model_id: None,
            prev_reasoning: None,
        });
        let n = names(&state);
        assert!(n.contains(&"exit_plan_mode".to_string()));
        assert!(!n.contains(&"enter_plan_mode".to_string()));
        // Subagents get neither.
        state.session.plan = None;
        state.session.is_subagent = true;
        let n = names(&state);
        assert!(!n.contains(&"enter_plan_mode".to_string()));
        assert!(!n.contains(&"exit_plan_mode".to_string()));
    }

    /// Drive a single named tool call through StreamDone so the turn lands in
    /// `ExecutingTools`, returning the allocated call id.
    fn drive_single_tool_call(state: &mut State, tool: &str) -> crate::domain::ToolCallId {
        state.turn = TurnState::Generating {
            id: TurnId(9),
            started: std::time::SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: vec![crate::models::ToolCall {
                id: Some("call_p".to_string()),
                function: crate::models::FunctionCall {
                    name: tool.to_string(),
                    arguments: serde_json::json!({}),
                },
            }],
            continuation: false,
        };
        let (next, cmds) = update(
            std::mem::replace(state, fresh_state()),
            Msg::StreamDone {
                turn: TurnId(9),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        *state = next;
        cmds.iter()
            .find_map(|c| match c {
                Cmd::ExecuteTool { call_id, .. } => Some(*call_id),
                _ => None,
            })
            .expect("tool dispatched")
    }

    #[test]
    fn exit_plan_mode_approval_transitions_out_and_seeds_the_checklist() {
        let mut state = state_with_two_exchanges();
        state.session.plan = Some(crate::domain::PlanState {
            plan_path: PathBuf::from("/tmp/project/.mermaid/plans/x.md"),
            prev_model_id: None,
            prev_reasoning: None,
        });
        let call_id = drive_single_tool_call(&mut state, "exit_plan_mode");

        let body = "## Summary\nS\n\n## Tasks\n1. Add the flag\n2. Wire the broker\n";
        let outcome = ToolOutcome::success("The user approved the plan.", "plan approved", 0.1)
            .with_metadata(crate::domain::ToolRunMetadata {
                detail: crate::domain::ToolMetadata::Plan {
                    path: ".mermaid/plans/x.md".to_string(),
                    body: body.to_string(),
                    start: true,
                    fresh: false,
                    fork: false,
                    model: None,
                },
                ..Default::default()
            });
        let (state, cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(9),
                call_id,
                outcome,
            },
        );

        assert!(state.session.plan.is_none(), "approval leaves plan mode");
        let subjects: Vec<String> = state
            .session
            .conversation
            .tasks
            .visible()
            .map(|t| t.subject.clone())
            .collect();
        assert_eq!(subjects, ["Add the flag", "Wire the broker"]);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::SyncTaskStore(store) if store.visible().count() == 2)),
            "the effect-side broker must be synced with the seeded store"
        );
        // start=true: the kickoff was committed as a user message at the tool
        // boundary and the follow-up model call fired.
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.role == MessageRole::User && m.content == "Implement the plan."),
            "auto-submit rides the queued-message drain"
        );
        assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
    }

    #[test]
    fn replan_reconcile_preserves_completed_tasks() {
        use crate::domain::tasks::{Stamp, TaskEdit, TaskOrigin, TaskSpec, TaskStatus};
        let mut state = fresh_state();
        state.session.plan = Some(crate::domain::PlanState {
            plan_path: PathBuf::from("/tmp/project/.mermaid/plans/x.md"),
            prev_model_id: None,
            prev_reasoning: None,
        });
        // Prior round: one completed, one still open.
        let mut store = crate::domain::tasks::TaskStore::default();
        let ids = store.create(
            vec![
                TaskSpec {
                    subject: "Add the flag".into(),
                    active_form: "Add the flag".into(),
                    description: None,
                    in_progress: false,
                },
                TaskSpec {
                    subject: "Old open step".into(),
                    active_form: "Old open step".into(),
                    description: None,
                    in_progress: false,
                },
            ],
            TaskOrigin::Model,
            Stamp::default(),
        );
        store.apply(
            &[TaskEdit {
                id: ids[0],
                status: Some(TaskStatus::Completed),
                subject: None,
                active_form: None,
                description: None,
            }],
            Stamp::default(),
        );
        state.session.conversation.tasks = store;

        let mut cmds = Vec::new();
        let body = "## Tasks\n1. Add the flag\n2. New step\n";
        finish_plan_mode(&mut state, &mut cmds, body, false);

        let visible: Vec<(String, crate::domain::tasks::TaskStatus)> = state
            .session
            .conversation
            .tasks
            .visible()
            .map(|t| (t.subject.clone(), t.status))
            .collect();
        // Completed survives, is not duplicated; the open step is replaced.
        assert_eq!(
            visible,
            [
                ("Add the flag".to_string(), TaskStatus::Completed),
                ("New step".to_string(), TaskStatus::Pending),
            ]
        );
        // start=false: nothing queued.
        assert!(state.ui.queued_messages.is_empty());
    }

    #[test]
    fn enter_plan_mode_tool_success_flips_the_session_into_planning() {
        let mut state = state_with_two_exchanges();
        let call_id = drive_single_tool_call(&mut state, "enter_plan_mode");
        assert!(state.session.plan.is_none(), "not planning during the call");
        let (state, _) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(9),
                call_id,
                outcome: ToolOutcome::success("Plan mode is on", "plan mode on", 0.0),
            },
        );
        let plan = state.session.plan.expect("tool success enters plan mode");
        assert!(plan.plan_path.starts_with("/tmp/project/.mermaid/plans"));
    }

    #[test]
    fn plan_config_picker_cycles_values_and_persists() {
        use crate::app::{PlanPermLevel, PlanPermissions};
        let (mut state, _) = update(
            fresh_state(),
            Msg::Slash(SlashCmd::Plan(Some("config".into()))),
        );
        assert!(matches!(state.ui.mode, UiMode::PlanConfig { cursor: 0 }));
        let key = |code| {
            Msg::Key(Key {
                code,
                modifiers: KeyMods::NONE,
            })
        };
        // Row 0 forward: default -> strict.
        let (next, cmds) = update(state, key(KeyCode::Enter));
        assert_eq!(next.settings.plan.permissions, PlanPermissions::strict());
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::PersistPlanConfig(_))),
            "every change persists the [plan] table"
        );
        state = next;
        // Down to builds, cycle allow -> auto (strict starts at deny -> allow).
        let (next, _) = update(state, key(KeyCode::Down));
        let (next, _) = update(next, key(KeyCode::Right));
        assert_eq!(next.settings.plan.permissions.builds, PlanPermLevel::Allow);
        // Custom values flip the preset display to none.
        assert!(next.settings.plan.permissions.preset_name().is_none());
        // Esc closes.
        let (next, _) = update(next, key(KeyCode::Escape));
        assert!(matches!(next.ui.mode, UiMode::EditingInput));
    }

    #[test]
    fn plan_model_override_swaps_on_entry_and_restores_on_exit() {
        let mut state = fresh_state();
        state.settings.plan.model = Some("anthropic/frontier".to_string());
        state.settings.plan.reasoning = Some(crate::models::ReasoningLevel::High);
        let (state, _) = update(state, Msg::Slash(SlashCmd::Plan(None)));
        assert_eq!(state.session.model_id, "anthropic/frontier");
        assert_eq!(state.session.reasoning, crate::models::ReasoningLevel::High);
        let plan = state.session.plan.clone().expect("planning");
        assert_eq!(plan.prev_model_id.as_deref(), Some("ollama/test"));
        // Exit restores.
        let (state, _) = update(state, Msg::Slash(SlashCmd::Plan(Some("off".into()))));
        assert_eq!(state.session.model_id, "ollama/test");
        assert_eq!(
            state.session.reasoning,
            crate::models::ReasoningLevel::Medium,
            "reasoning restored to the pre-plan default"
        );
    }

    #[test]
    fn plan_capabilities_line_tracks_the_profile() {
        use crate::app::PlanPermissions;
        let line = plan_capabilities_line(&PlanPermissions::default());
        assert!(line.contains("build and test"));
        assert!(line.contains("web search/fetch"));
        let line = plan_capabilities_line(&PlanPermissions::strict());
        assert!(!line.contains("build and test"));
        assert!(!line.contains("web search/fetch"));
        assert!(line.contains("plan file ONLY"));
    }

    fn plan_outcome(fresh: bool, fork: bool, model: Option<&str>) -> ToolOutcome {
        ToolOutcome::success("approved", "plan approved", 0.1).with_metadata(
            crate::domain::ToolRunMetadata {
                detail: crate::domain::ToolMetadata::Plan {
                    path: ".mermaid/plans/x.md".to_string(),
                    body: "## Tasks\n1. Step one\n2. Step two\n".to_string(),
                    start: true,
                    fresh,
                    fork,
                    model: model.map(str::to_string),
                },
                ..Default::default()
            },
        )
    }

    #[test]
    fn clear_context_approval_hands_off_to_a_fresh_conversation() {
        let mut state = state_with_two_exchanges();
        state.session.plan = Some(crate::domain::PlanState {
            plan_path: PathBuf::from("/tmp/project/.mermaid/plans/x.md"),
            prev_model_id: None,
            prev_reasoning: None,
        });
        let original_id = state.session.conversation.id.clone();
        let call_id = drive_single_tool_call(&mut state, "exit_plan_mode");
        let (state, cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(9),
                call_id,
                outcome: plan_outcome(true, false, Some("ollama/executor")),
            },
        );
        assert!(state.session.plan.is_none());
        assert_ne!(
            state.session.conversation.id, original_id,
            "execution continues in a new conversation"
        );
        assert_eq!(state.session.model_id, "ollama/executor");
        assert_eq!(state.session.conversation.tasks.visible().count(), 2);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CancelScope(_))),
            "the exploration turn's scope is cancelled"
        );
        // The kickoff self-submitted within this update: the fresh transcript
        // holds exactly the preamble+plan user message, and its turn is live.
        let users: Vec<&ChatMessage> = state
            .session
            .messages()
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .collect();
        assert_eq!(users.len(), 1, "only the kickoff rides the fresh context");
        assert!(
            users[0]
                .content
                .starts_with(crate::prompts::PLAN_HANDOFF_PREAMBLE)
        );
        assert!(users[0].content.contains("## Tasks"));
        assert!(
            matches!(state.turn, TurnState::Generating { .. }),
            "the kickoff turn starts immediately"
        );
    }

    #[test]
    fn fork_handoff_carries_the_transcript() {
        let mut state = state_with_two_exchanges();
        state.session.plan = Some(crate::domain::PlanState {
            plan_path: PathBuf::from("/tmp/project/.mermaid/plans/x.md"),
            prev_model_id: None,
            prev_reasoning: None,
        });
        let original_id = state.session.conversation.id.clone();
        let original_len = state.session.messages().len();
        let call_id = drive_single_tool_call(&mut state, "exit_plan_mode");
        let mid_len = state.session.messages().len();
        assert!(mid_len >= original_len, "tool_use assistant committed");
        let (state, _cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(9),
                call_id,
                outcome: plan_outcome(false, true, None),
            },
        );
        assert_ne!(state.session.conversation.id, original_id);
        assert_eq!(
            state.session.conversation.forked_from.as_deref(),
            Some(original_id.as_str())
        );
        // The transcript carried over, plus the self-submitted kickoff.
        assert_eq!(
            state.session.messages().len(),
            mid_len + 1,
            "fork carries the transcript and appends the kickoff"
        );
        assert_eq!(
            state.session.messages().last().map(|m| m.content.as_str()),
            Some("Implement the plan.")
        );
        assert!(matches!(state.turn, TurnState::Generating { .. }));
    }

    #[test]
    fn fork_fires_the_checkpoint_lookup_with_the_original_session_id() {
        let mut state = state_with_two_exchanges();
        let original_id = state.session.conversation.id.clone();
        let mut cmds = Vec::new();
        fork_conversation_at(&mut state, &mut cmds, 2);
        let found = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::ListForkCheckpoints {
                    session_id,
                    message_index,
                } => Some((session_id.clone(), *message_index)),
                _ => None,
            })
            .expect("fork queries anchored checkpoints");
        assert_eq!(found.0, original_id, "anchors reference the ORIGINAL id");
        assert_eq!(found.1, 2);
        assert_ne!(state.session.conversation.id, original_id, "forked");
    }

    fn anchored_checkpoint(id: &str, index: i64) -> crate::runtime::CheckpointRecord {
        crate::runtime::CheckpointRecord {
            id: id.to_string(),
            task_id: None,
            project_path: "/tmp/p".to_string(),
            snapshot_path: format!("/snap/{id}"),
            changed_files_json: "[]".to_string(),
            pending_action_json: None,
            approval_id: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            archived_at: None,
            archive_reason: None,
            session_id: Some("sess".to_string()),
            message_index: Some(index),
        }
    }

    #[test]
    fn fork_checkpoints_found_names_the_oldest_or_stays_silent() {
        let state = fresh_state();
        let before = state.session.messages().len();
        let (state, _) = update(
            state,
            Msg::ForkCheckpointsFound(vec![
                anchored_checkpoint("cp-old", 4),
                anchored_checkpoint("cp-new", 8),
            ]),
        );
        let notice = state.session.messages().last().expect("notice appended");
        assert!(notice.content.contains("cp-old"), "{}", notice.content);
        assert!(notice.content.contains("Files were not rewound"));
        assert!(notice.content.contains("2 file checkpoint(s)"));

        let (state, _) = update(state, Msg::ForkCheckpointsFound(Vec::new()));
        assert_eq!(
            state.session.messages().len(),
            before + 1,
            "empty reply emits nothing"
        );
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
                    ..Default::default()
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
        assert!(
            state
                .session
                .messages()
                .last()
                .is_some_and(|m| m.content.contains("MCP server s1 errored: exit 1")),
            "the MCP error must be posted to the chat transcript"
        );
    }

    #[test]
    fn push_system_during_compacting_inserts_before_tool_call_pair() {
        // D1: while Compacting with a trailing committed `assistant(tool_calls)`
        // (a context-limit compaction preserves the unpaired tool_use), an
        // `McpServerErrored` note must be inserted BEFORE that assistant message,
        // not appended after it — keeping the tool_use adjacent to its tool_result.
        let mut state = fresh_state();
        let source = crate::models::tool_call::ToolCall {
            id: Some("call-1".to_string()),
            function: crate::models::tool_call::FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "foo"}),
            },
        };
        state.session.append(
            ChatMessage::assistant("running a tool").with_tool_calls(vec![source]),
            state.now,
        );
        state.turn = TurnState::Compacting {
            id: TurnId(9),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::ContextLimitRetry,
            resume_continuation: false,
        };

        let (state, _) = update(
            state,
            Msg::McpServerErrored {
                name: "s1".to_string(),
                reason: "exit 1".to_string(),
            },
        );

        let messages = state.session.messages();
        let n = messages.len();
        assert!(
            n >= 2
                && messages[n - 1].role == MessageRole::Assistant
                && messages[n - 1].tool_calls.is_some(),
            "the assistant(tool_calls) must stay last so its tool_result can follow"
        );
        assert!(
            messages[n - 2].role == MessageRole::System
                && messages[n - 2].content.contains("MCP server s1 errored"),
            "the system note sits directly before the tool-call pair, not after it"
        );
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
        state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
        // The reducer looks up the "last assistant message" to attach
        // an ActionDisplay — plant one so the lookup doesn't silently
        // no-op in this test.
        state
            .session
            .append(ChatMessage::assistant("tools follow"), state.now);

        let (state, cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(3),
                call_id: super::super::ids::ToolCallId(1),
                outcome: ToolOutcome::success("file contents", "file contents", 0.05),
            },
        );

        assert!(matches!(state.turn, TurnState::Generating { .. }));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
        // Tool result message was appended.
        let last = state.session.messages().last().unwrap();
        assert_eq!(last.role, MessageRole::Tool);
    }

    /// A finished `execute_command` must request a full repaint — a shell
    /// child may have scribbled on the terminal behind ratatui's back buffer.
    #[test]
    fn exec_tool_finished_bumps_full_redraw_seq() {
        let mut state = fresh_state();
        let call = PendingToolCall {
            call_id: super::super::ids::ToolCallId(1),
            source: crate::models::tool_call::ToolCall {
                id: Some("c1".to_string()),
                function: crate::models::tool_call::FunctionCall {
                    name: "execute_command".to_string(),
                    arguments: serde_json::json!({"command": "echo hi"}),
                },
            },
        };
        state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
        let before = state.ui.full_redraw_seq;

        let (state, _cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(3),
                call_id: super::super::ids::ToolCallId(1),
                outcome: ToolOutcome::success("hi", "hi", 0.01),
            },
        );

        assert_eq!(
            state.ui.full_redraw_seq,
            before.wrapping_add(1),
            "execute_command completion must bump the repaint counter"
        );
    }

    /// Tools that can't touch the tty must NOT trigger a repaint — clearing
    /// on every tool completion would flash during rapid read/edit loops.
    #[test]
    fn non_exec_tool_finished_does_not_bump_full_redraw_seq() {
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
        state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
        let before = state.ui.full_redraw_seq;

        let (state, _cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(3),
                call_id: super::super::ids::ToolCallId(1),
                outcome: ToolOutcome::success("contents", "contents", 0.01),
            },
        );

        assert_eq!(state.ui.full_redraw_seq, before);
    }

    #[test]
    fn ctrl_l_bumps_full_redraw_seq() {
        let state = fresh_state();
        let before = state.ui.full_redraw_seq;
        let (state, cmds) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char('l'),
                modifiers: KeyMods {
                    ctrl: true,
                    ..KeyMods::NONE
                },
            }),
        );
        assert_eq!(state.ui.full_redraw_seq, before.wrapping_add(1));
        assert!(!state.should_exit, "Ctrl+L must not exit");
        assert!(cmds.is_empty(), "Ctrl+L is reducer-only: {cmds:?}");
    }

    #[test]
    fn ctrl_t_toggles_the_task_checklist() {
        let ctrl_t = || {
            Msg::Key(Key {
                code: KeyCode::Char('t'),
                modifiers: KeyMods {
                    ctrl: true,
                    ..KeyMods::NONE
                },
            })
        };
        let state = fresh_state();
        assert!(!state.ui.tasks_collapsed, "expanded by default");
        let (state, cmds) = update(state, ctrl_t());
        assert!(state.ui.tasks_collapsed);
        assert!(cmds.is_empty(), "Ctrl+T is reducer-only: {cmds:?}");
        let (state, _) = update(state, ctrl_t());
        assert!(!state.ui.tasks_collapsed, "second press expands again");
    }

    fn sample_task_store(statuses: &[crate::domain::TaskStatus]) -> crate::domain::TaskStore {
        use crate::domain::tasks::{Stamp, TaskEdit, TaskSpec};
        let mut store = crate::domain::TaskStore::default();
        store.create(
            statuses
                .iter()
                .enumerate()
                .map(|(i, _)| TaskSpec {
                    subject: format!("task {i}"),
                    active_form: format!("doing {i}"),
                    description: None,
                    in_progress: false,
                })
                .collect(),
            crate::domain::TaskOrigin::Model,
            Stamp::default(),
        );
        let edits: Vec<TaskEdit> = statuses
            .iter()
            .enumerate()
            .filter(|(_, s)| **s != crate::domain::TaskStatus::Pending)
            .map(|(i, s)| TaskEdit {
                id: (i + 1) as u32,
                status: Some(*s),
                ..TaskEdit::default()
            })
            .collect();
        store.apply(&edits, Stamp::default());
        store
    }

    #[test]
    fn tasks_updated_replaces_snapshot_and_diffs_completions() {
        use crate::domain::TaskStatus::{Completed, InProgress, Pending};
        let state = fresh_state();
        // First snapshot: nothing completed — no notifications.
        let (state, cmds) = update(
            state,
            Msg::TasksUpdated {
                store: sample_task_store(&[InProgress, Pending]),
            },
        );
        assert!(cmds.is_empty(), "{cmds:?}");
        assert_eq!(state.session.conversation.tasks.visible().count(), 2);

        // Task 1 completes: exactly one notification, with counts.
        let (state, cmds) = update(
            state,
            Msg::TasksUpdated {
                store: sample_task_store(&[Completed, InProgress]),
            },
        );
        let notes: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, Cmd::NotifyTaskCompleted { .. }))
            .collect();
        assert_eq!(notes.len(), 1);
        if let Cmd::NotifyTaskCompleted {
            task,
            completed,
            total,
        } = notes[0]
        {
            assert_eq!(task.id, 1);
            assert_eq!((*completed, *total), (1, 2));
        }

        // The same snapshot re-sent: the diff is the dedupe — no re-fire.
        let (_state, cmds) = update(
            state,
            Msg::TasksUpdated {
                store: sample_task_store(&[Completed, InProgress]),
            },
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::NotifyTaskCompleted { .. })),
            "identical snapshot must not re-notify: {cmds:?}"
        );
    }

    #[test]
    fn fork_clears_tasks_and_syncs_the_broker() {
        use crate::domain::TaskStatus::InProgress;
        let state = fresh_state();
        let (mut state, _) = update(
            state,
            Msg::TasksUpdated {
                store: sample_task_store(&[InProgress]),
            },
        );
        // Two committed user messages so index 1 is a valid fork cut.
        state
            .session
            .append(crate::models::ChatMessage::user("one"), state.now);
        state
            .session
            .append(crate::models::ChatMessage::user("two"), state.now);
        let mut cmds = Vec::new();
        super::fork_conversation_at(&mut state, &mut cmds, 1);
        assert!(state.session.conversation.tasks.is_empty());
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Cmd::SyncTaskStore(store) if store.tasks.is_empty()
            )),
            "fork must clear the broker too: {cmds:?}"
        );
    }

    #[test]
    fn todos_command_routes_edits_and_prints_list() {
        // Bare /todos on an empty list: helpful system line, no Cmd.
        let state = fresh_state();
        let (state, cmds) = update(state, Msg::Slash(SlashCmd::Todos(None)));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::UserTaskEdit(_))));
        assert!(
            state
                .session
                .messages()
                .last()
                .is_some_and(|m| m.content.contains("No tasks")),
        );

        // add routes through the broker command, never mutating state directly.
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Todos(Some("add review the docs".to_string()))),
        );
        assert!(cmds.iter().any(|c| matches!(
            c,
            Cmd::UserTaskEdit(crate::domain::UserTaskEdit::Add { subject }) if subject == "review the docs"
        )));
        assert!(state.session.conversation.tasks.is_empty());

        // done accepts a #-prefixed id; garbage prints usage.
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Todos(Some("done #3".to_string()))),
        );
        assert!(cmds.iter().any(|c| matches!(
            c,
            Cmd::UserTaskEdit(crate::domain::UserTaskEdit::Done { id: 3 })
        )));
        let (state, _) = update(
            state,
            Msg::Slash(SlashCmd::Todos(Some("frobnicate".to_string()))),
        );
        assert!(
            state
                .session
                .messages()
                .last()
                .is_some_and(|m| m.content.contains("usage: /todos")),
        );
    }

    #[test]
    fn task_notices_ride_the_next_request_then_clear() {
        let state = fresh_state();
        let (mut state, _) = update(
            state,
            Msg::TaskNotice {
                text: "The user edited the task checklist: Added task #1 'x'.".to_string(),
            },
        );
        let mut cmds = Vec::new();
        super::push_call_model(&mut state, &mut cmds, TurnId(1));
        let Some(Cmd::CallModel { request, .. }) = cmds.first() else {
            panic!("expected CallModel: {cmds:?}");
        };
        let instructions = request.instructions.clone().unwrap_or_default();
        assert!(instructions.contains("# Task Checklist Notices"));
        assert!(instructions.contains("Added task #1 'x'"));
        assert!(
            state.pending_task_notices.is_empty(),
            "notices are consumed by the dispatch"
        );
    }

    #[test]
    fn stale_in_progress_task_triggers_a_nudge_every_n_calls() {
        use crate::domain::TaskStatus::InProgress;
        let state = fresh_state();
        let (mut state, _) = update(
            state,
            Msg::TasksUpdated {
                store: sample_task_store(&[InProgress]),
            },
        );
        // Calls 1..4: no nudge. Call 5: nudge rides the request and re-arms.
        for i in 1..=4 {
            let mut cmds = Vec::new();
            super::push_call_model(&mut state, &mut cmds, TurnId(i));
            let Some(Cmd::CallModel { request, .. }) = cmds.first() else {
                panic!("expected CallModel");
            };
            assert!(
                !request
                    .instructions
                    .clone()
                    .unwrap_or_default()
                    .contains("in_progress for"),
                "no nudge before the threshold (call {i})"
            );
        }
        let mut cmds = Vec::new();
        super::push_call_model(&mut state, &mut cmds, TurnId(5));
        let Some(Cmd::CallModel { request, .. }) = cmds.first() else {
            panic!("expected CallModel");
        };
        let instructions = request.instructions.clone().unwrap_or_default();
        assert!(
            instructions.contains("Task #1 'task 0' has been in_progress"),
            "nudge fires at the threshold: {instructions}"
        );
        assert_eq!(state.runtime.calls_since_task_update, 0, "re-armed");

        // A checklist update resets the counter.
        let (state, _) = update(
            state,
            Msg::TasksUpdated {
                store: sample_task_store(&[InProgress]),
            },
        );
        assert_eq!(state.runtime.calls_since_task_update, 0);
    }

    /// Ctrl+L is meta-level (like Ctrl+C/Ctrl+B): it must work — and only
    /// repaint — while an approval modal is open.
    #[test]
    fn ctrl_l_works_during_approval_modal() {
        let state = pending_approval_state();
        let before = state.ui.full_redraw_seq;
        let (state, cmds) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char('l'),
                modifiers: KeyMods {
                    ctrl: true,
                    ..KeyMods::NONE
                },
            }),
        );
        assert_eq!(state.ui.full_redraw_seq, before.wrapping_add(1));
        assert_eq!(
            state.pending_approval.len(),
            1,
            "the approval must remain queued, not be resolved by Ctrl+L"
        );
        assert!(cmds.is_empty());
    }

    fn test_attachment(id: u64) -> crate::domain::Attachment {
        crate::domain::Attachment {
            id,
            // Mirror id → number so a test can reference the pill as `[Image #id]`.
            number: id,
            base64_data: "AAAA".to_string(),
            temp_path: PathBuf::from(format!("/tmp/a{id}.png")),
            size_bytes: 4,
            format: "png".to_string(),
        }
    }

    fn generating(id: u64, partial: &str) -> TurnState {
        TurnState::Generating {
            id: TurnId(id),
            started: std::time::SystemTime::now(),
            partial_text: partial.to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        }
    }

    #[test]
    fn queued_message_keeps_attachments_from_queue_time() {
        // Axis 1 #1: a message queued while busy must re-submit with the
        // attachments present when it was queued, not whatever is live when the
        // FIFO drains.
        let mut state = fresh_state();
        state.turn = generating(5, "answer");
        state.ui.attachments.push(test_attachment(1)); // id 1, number 1

        // Busy → queued, capturing id 1. The text carries its inline pill.
        let (mut state, _) = update(
            state,
            Msg::SubmitPrompt {
                text: "[Image #1] queued".to_string(),
                attachment_ids: vec![1],
            },
        );
        assert_eq!(state.ui.queued_messages.len(), 1);

        // User preps a different image for the NEXT message while the turn runs.
        state.ui.attachments.push(test_attachment(2)); // id 2, number 2

        // Turn completes → queued message drains and re-submits.
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );

        // The queued message consumed image #1 (matched by its token + queued id
        // scope); the live id 2 is untouched — proving the queue-time set was
        // used, not whatever is live at drain.
        assert_eq!(state.ui.attachments.len(), 1);
        assert_eq!(state.ui.attachments[0].id, 2);
        let queued_msg = state
            .session
            .messages()
            .iter()
            .find(|m| m.role == MessageRole::User && m.content == "[Image #1] queued")
            .expect("queued message submitted");
        assert_eq!(queued_msg.image_numbers, Some(vec![1]));
    }

    #[test]
    fn stream_done_without_usage_keeps_previous_last_token_usage() {
        // Axis 1 #2: a turn reporting no usage must not wipe the last request's
        // usage to "n/a".
        let mut state = fresh_state();
        state.turn = generating(1, "first");
        let (mut state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(1),
                usage: Some(crate::models::TokenUsage::provider(120, 30)),
                provider_continuation: None,
                stop_reason: None,
            },
        );
        assert_eq!(state.session.last_token_usage.unwrap().prompt_tokens, 120);

        // A second turn reports no usage (common on tool follow-ups).
        state.turn = generating(2, "second");
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(2),
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        assert_eq!(
            state
                .session
                .last_token_usage
                .expect("retained")
                .prompt_tokens,
            120
        );
    }

    #[test]
    fn stream_tool_call_outside_generating_is_dropped_without_panic() {
        // Axis 1 #5: a tool-call event arriving after the turn left Generating
        // is dropped (and logged), never panics or mutates state.
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
        state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
        let (state, cmds) = update(
            state,
            Msg::StreamToolCall {
                turn: TurnId(3),
                call: crate::models::tool_call::ToolCall {
                    id: Some("late".to_string()),
                    function: crate::models::tool_call::FunctionCall {
                        name: "write_file".to_string(),
                        arguments: serde_json::json!({}),
                    },
                },
            },
        );
        assert!(matches!(state.turn, TurnState::ExecutingTools { .. }));
        assert!(cmds.is_empty());
    }

    #[test]
    fn exit_commits_interrupted_partial_before_saving() {
        // Axis 1 #6: quitting mid-stream preserves the partial assistant reply
        // (with an interrupted marker) so `--continue` shows what was on screen.
        let mut state = fresh_state();
        state.turn = generating(1, "half written");
        let (state, cmds) = update(state, Msg::Quit);
        assert!(state.should_exit);
        let last = state.session.messages().last().expect("a message");
        assert_eq!(last.role, MessageRole::Assistant);
        assert!(last.content.contains("half written"));
        assert!(last.content.contains("[interrupted]"));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConversation(_))));
    }

    #[test]
    fn backgrounded_tool_completes_turn_not_stranded() {
        // Axis 1 #8 (verified non-bug): Ctrl+B fires BackgroundScope but leaves
        // the reducer in ExecutingTools; the detachable tool still returns a
        // success outcome, so the turn advances normally. Locks that behavior.
        let mut state = fresh_state();
        let call = PendingToolCall {
            call_id: super::super::ids::ToolCallId(1),
            source: crate::models::tool_call::ToolCall {
                id: Some("c1".to_string()),
                function: crate::models::tool_call::FunctionCall {
                    name: "execute_command".to_string(),
                    arguments: serde_json::json!({"command": "sleep 9"}),
                },
            },
        };
        state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
        state
            .session
            .append(ChatMessage::assistant("tools follow"), state.now);

        // Ctrl+B → BackgroundScope; reducer stays in ExecutingTools.
        let (state, cmds) = update(
            state,
            Msg::Key(Key {
                code: KeyCode::Char('b'),
                modifiers: KeyMods::ctrl(),
            }),
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::BackgroundScope(TurnId(3))))
        );
        assert!(matches!(state.turn, TurnState::ExecutingTools { .. }));

        // The detached command returns a normal success outcome → turn advances.
        let (state, cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(3),
                call_id: super::super::ids::ToolCallId(1),
                outcome: ToolOutcome::success(
                    "Moved to background.\nPID: 1234",
                    "moved to background",
                    0.1,
                ),
            },
        );
        assert!(matches!(state.turn, TurnState::Generating { .. }));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
    }

    #[test]
    fn builtin_tool_schema_tokens_msg_updates_runtime() {
        // Axis 1 #4: the runner's report lands on runtime state.
        let state = fresh_state();
        let (state, _) = update(state, Msg::BuiltinToolSchemaTokens(4321));
        assert_eq!(state.runtime.builtin_tool_schema_tokens, 4321);
    }

    #[test]
    fn context_text_folds_in_builtin_tool_tokens() {
        // Axis 1 #4: /context shows a disclaimer before the runner reports, and
        // the real figure afterward.
        let mut state = fresh_state();
        let before = context_text(&state);
        assert!(before.contains("built-in tool schemas: measured on the first model call"));

        state.runtime.builtin_tool_schema_tokens = 5000;
        let after = context_text(&state);
        assert!(after.contains("built-in tool schemas:"));
        assert!(!after.contains("measured on the first model call"));
    }

    #[test]
    fn context_text_shows_ollama_window_detail_and_tip() {
        use crate::domain::runtime::OllamaContextInfo;
        use crate::models::adapters::ollama_sizing::NumCtxSource;
        let mut state = fresh_state();
        // No probe yet → no Ollama window lines.
        assert!(!context_text(&state).contains("Active num_ctx"));

        state.runtime.ollama_context = Some(OllamaContextInfo {
            model_max: Some(262_144),
            effective: Some(12_288),
            source: Some(NumCtxSource::Auto),
        });
        let text = context_text(&state);
        assert!(text.contains("Model max window"));
        assert!(text.contains("Active num_ctx"));
        assert!(text.contains("(auto"));
        assert!(text.contains("Output budget (num_predict)"));
        assert!(text.contains("RAM offload: off"));
        // Auto-fit capped well below the model's max → point to the override.
        assert!(text.contains("/context max"));

        // Once auto-converge has picked a fitting window, label it as Mermaid's
        // choice ("GPU-fit"), not the user's "(override)".
        state
            .runtime
            .ollama_converged_num_ctx
            .insert("ollama/test".to_string(), 8_192);
        let text = context_text(&state);
        assert!(text.contains("auto (GPU-fit)"), "got: {text}");
        assert!(!text.contains("(override)"));
    }

    #[test]
    fn background_command_tool_finish_registers_process() {
        let mut state = fresh_state();
        let call = PendingToolCall {
            call_id: super::super::ids::ToolCallId(1),
            source: crate::models::tool_call::ToolCall {
                id: Some("c1".to_string()),
                function: crate::models::tool_call::FunctionCall {
                    name: "execute_command".to_string(),
                    arguments: serde_json::json!({
                        "command": "npm run dev",
                        "mode": "background",
                        "working_dir": "/tmp/project",
                    }),
                },
            },
        };
        state.turn = start_executing_tools(TurnId(3), vec![call], std::time::SystemTime::now());
        state
            .session
            .append(ChatMessage::assistant("tools follow"), state.now);

        let (state, _) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(3),
                call_id: super::super::ids::ToolCallId(1),
                outcome: ToolOutcome::success(
                    "Background command started.\nPID: 123\nLog: /tmp/mermaid-bg.log\nReady: matched pattern \"Local:\"\nDetected URL: http://127.0.0.1:5173\n",
                    "background process started",
                    0.2,
                )
                .with_metadata(crate::domain::ToolRunMetadata {
                    process: Some(crate::domain::ManagedProcess {
                        id: "bg-123".to_string(),
                        pid: 123,
                        command: "npm run dev".to_string(),
                        cwd: Some("/tmp/project".to_string()),
                        log_path: "/tmp/mermaid-bg.log".to_string(),
                        detected_url: Some("http://127.0.0.1:5173".to_string()),
                        status: crate::domain::ManagedProcessStatus::Running,
                    }),
                    ..crate::domain::ToolRunMetadata::default()
                }),
            },
        );

        assert_eq!(state.runtime.processes.len(), 1);
        let process = &state.runtime.processes[0];
        assert_eq!(process.pid, 123);
        assert_eq!(process.command, "npm run dev");
        assert_eq!(process.cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(
            process.detected_url.as_deref(),
            Some("http://127.0.0.1:5173")
        );
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
        state.turn = start_executing_tools(TurnId(3), calls, std::time::SystemTime::now());
        state
            .session
            .append(ChatMessage::assistant("tools follow"), state.now);

        let (state, cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(3),
                call_id: super::super::ids::ToolCallId(1),
                outcome: ToolOutcome::cancelled(),
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
            std::time::SystemTime::now(),
        );

        let (state, cmds) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(999),
                call_id: super::super::ids::ToolCallId(1),
                outcome: ToolOutcome::cancelled(),
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
    fn background_agent_lifecycle_registry_note_queue_and_usage() {
        // Started: adds a live-panel registry row.
        let state = fresh_state();
        let (state, _) = update(
            state,
            Msg::BackgroundAgentStarted {
                agent_id: "a3".to_string(),
                description: "audit docs".to_string(),
            },
        );
        assert_eq!(state.runtime.background_agents.len(), 1);
        assert_eq!(state.runtime.background_agents[0].description, "audit docs");

        // Progress: updates activity/tokens on the row.
        let (state, _) = update(
            state,
            Msg::BackgroundAgentProgress {
                agent_id: "a3".to_string(),
                activity: "read_file…".to_string(),
                tokens: 4_200,
            },
        );
        assert_eq!(state.runtime.background_agents[0].activity, "read_file…");
        assert_eq!(state.runtime.background_agents[0].tokens, 4_200);

        // Finished while IDLE: row removed, usage folded into session totals,
        // and the report auto-submits through the queued-message path (the
        // outer update() drains pending_msgs, so the turn starts immediately).
        let tokens_before = state.session.cumulative_token_usage.total_tokens();
        let (state, _) = update(
            state,
            Msg::BackgroundAgentFinished {
                agent_id: "a3".to_string(),
                description: "audit docs".to_string(),
                report: "docs are fine".to_string(),
                success: true,
                cancelled: false,
                usage: Some(crate::models::TokenUsage::provider(70_000, 20_000)),
                tokens: 90_000,
                duration_secs: 61,
            },
        );
        assert!(state.runtime.background_agents.is_empty());
        assert_eq!(
            state.session.cumulative_token_usage.total_tokens(),
            tokens_before + 90_000
        );
        assert!(
            !matches!(state.turn, TurnState::Idle),
            "idle delivery must auto-submit the report"
        );
        let last_user = state
            .session
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == crate::models::MessageRole::User)
            .expect("report submitted as a user message");
        assert!(last_user.content.contains("docs are fine"));
        assert!(last_user.content.contains("background agent 'audit docs'"));
    }

    #[test]
    fn background_agent_report_waits_in_queue_while_a_turn_runs() {
        let (state, _call_id) = state_executing_agent_call();
        let (state, _) = update(
            state,
            Msg::BackgroundAgentFinished {
                agent_id: "a9".to_string(),
                description: "security sweep".to_string(),
                report: "no findings".to_string(),
                success: true,
                cancelled: false,
                usage: None,
                tokens: 1_000,
                duration_secs: 5,
            },
        );
        // Busy turn: the report queues instead of interrupting.
        assert_eq!(state.ui.queued_messages.len(), 1);
        assert!(
            state.ui.queued_messages[0].text.contains("no findings"),
            "queued report carries the child's output"
        );
        assert!(matches!(state.turn, TurnState::ExecutingTools { .. }));
    }

    /// Seed a state with one detached background agent in the registry.
    fn state_with_background_agent(agent_id: &str, description: &str) -> State {
        let (state, _) = update(
            fresh_state(),
            Msg::BackgroundAgentStarted {
                agent_id: agent_id.to_string(),
                description: description.to_string(),
            },
        );
        state
    }

    #[test]
    fn slash_agents_lists_registry_or_reports_none() {
        // Empty registry: a "none" note, no Cmd beyond the transcript save.
        let (state, _) = update(fresh_state(), Msg::Slash(SlashCmd::Agents(None)));
        let last = state.session.messages().last().expect("system note");
        assert!(last.content.contains("No background agents"));

        // Populated registry: one line per agent with id + description.
        let state = state_with_background_agent("a3", "audit docs");
        let (state, _) = update(state, Msg::Slash(SlashCmd::Agents(None)));
        let last = state.session.messages().last().expect("listing");
        assert!(last.content.contains("Background agents (1)"));
        assert!(last.content.contains("a3"));
        assert!(last.content.contains("audit docs"));
        // Listing must not kill anything.
        assert_eq!(state.runtime.background_agents.len(), 1);
    }

    #[test]
    fn slash_agents_kill_validates_id_and_fires_cmd() {
        // Unknown id: feedback, no kill Cmd.
        let state = state_with_background_agent("a3", "audit docs");
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Agents(Some("kill a99".to_string()))),
        );
        let last = state.session.messages().last().expect("note");
        assert!(last.content.contains("No background agent 'a99'"));
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::KillBackgroundAgent { .. })),
            "unknown id must not fire a kill"
        );

        // Known id: row marked cancelling, targeted kill Cmd emitted.
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Agents(Some("kill a3".to_string()))),
        );
        assert_eq!(state.runtime.background_agents[0].activity, "cancelling…");
        assert!(cmds.iter().any(|c| matches!(
            c,
            Cmd::KillBackgroundAgent { agent_id: Some(id) } if id == "a3"
        )));

        // Kill all: every row marked, broadcast Cmd emitted.
        let (state, cmds) = update(
            state,
            Msg::Slash(SlashCmd::Agents(Some("kill all".to_string()))),
        );
        assert!(
            state
                .runtime
                .background_agents
                .iter()
                .all(|a| a.activity == "cancelling…")
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::KillBackgroundAgent { agent_id: None }))
        );
    }

    #[test]
    fn cancelled_background_agent_notes_but_never_queues_a_report() {
        let state = state_with_background_agent("a3", "audit docs");
        let tokens_before = state.session.cumulative_token_usage.total_tokens();
        let (state, _) = update(
            state,
            Msg::BackgroundAgentFinished {
                agent_id: "a3".to_string(),
                description: "audit docs".to_string(),
                report: "partial findings".to_string(),
                success: false,
                cancelled: true,
                usage: Some(crate::models::TokenUsage::provider(10_000, 5_000)),
                tokens: 15_000,
                duration_secs: 42,
            },
        );
        // Row cleared, spend still folded (the work was billed).
        assert!(state.runtime.background_agents.is_empty());
        assert_eq!(
            state.session.cumulative_token_usage.total_tokens(),
            tokens_before + 15_000
        );
        // Cancelled note in the transcript, but NO queued report and no
        // auto-submitted turn — a deliberate kill must not spend a model call.
        let last = state.session.messages().last().expect("note");
        assert!(last.content.contains("cancelled"));
        assert!(state.ui.queued_messages.is_empty());
        assert!(matches!(state.turn, TurnState::Idle));
    }

    /// Build a one-call ExecutingTools state around an `agent` tool call,
    /// shared by the subagent progress/rollup tests below.
    fn state_executing_agent_call() -> (State, super::super::ids::ToolCallId) {
        let mut state = fresh_state();
        let call_id = super::super::ids::ToolCallId(1);
        state.turn = start_executing_tools(
            TurnId(3),
            vec![PendingToolCall {
                call_id,
                source: crate::models::tool_call::ToolCall {
                    id: None,
                    function: crate::models::tool_call::FunctionCall {
                        name: "agent".to_string(),
                        arguments: serde_json::json!({"description": "explore"}),
                    },
                },
            }],
            std::time::SystemTime::now(),
        );
        state
            .session
            .append(ChatMessage::assistant("spawning"), state.now);
        (state, call_id)
    }

    #[test]
    fn subagent_progress_feeds_live_status_and_finish_clears_it() {
        use crate::providers::{ProgressEvent, SubagentPhase};
        let (state, call_id) = state_executing_agent_call();

        // A child tool starting shows as "<tool>…" on the parent call.
        let (state, _) = update(
            state,
            Msg::ToolProgress {
                turn: TurnId(3),
                call_id,
                event: ProgressEvent::SubagentToolCall {
                    child_call_id: super::super::ids::ToolCallId(9),
                    tool_name: "read_file".to_string(),
                    phase: SubagentPhase::Started,
                },
            },
        );
        assert_eq!(
            state
                .ui
                .live_tool_status
                .get(&call_id)
                .map(|s| s.activity.as_str()),
            Some("read_file…"),
        );

        // Coarse phase changes overwrite the activity; token counts land on
        // the same entry without touching it.
        let (state, _) = update(
            state,
            Msg::ToolProgress {
                turn: TurnId(3),
                call_id,
                event: ProgressEvent::SubagentActivity("thinking".to_string()),
            },
        );
        let (state, _) = update(
            state,
            Msg::ToolProgress {
                turn: TurnId(3),
                call_id,
                event: ProgressEvent::SubagentTokens(1234),
            },
        );
        assert_eq!(
            state.ui.live_tool_status.get(&call_id),
            Some(&crate::domain::LiveToolStatus {
                activity: "thinking".to_string(),
                tokens: 1234,
            }),
        );

        // Progress for a stale turn must not touch the live map.
        let (state, _) = update(
            state,
            Msg::ToolProgress {
                turn: TurnId(999),
                call_id,
                event: ProgressEvent::SubagentActivity("late straggler".to_string()),
            },
        );
        let (state, _) = update(
            state,
            Msg::ToolProgress {
                turn: TurnId(999),
                call_id,
                event: ProgressEvent::SubagentTokens(9_999_999),
            },
        );
        assert_eq!(
            state
                .ui
                .live_tool_status
                .get(&call_id)
                .map(|s| (s.activity.as_str(), s.tokens)),
            Some(("thinking", 1234)),
        );

        // The call finishing removes its entry (and here ends the turn).
        let (state, _) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(3),
                call_id,
                outcome: ToolOutcome::success("report", "subagent completed", 0.5),
            },
        );
        assert!(
            state.ui.live_tool_status.is_empty(),
            "live status must not outlive the call",
        );
    }

    #[test]
    fn subagent_usage_rolls_into_session_totals_and_run_counter() {
        let (state, call_id) = state_executing_agent_call();
        let before_cum = state.session.cumulative_token_usage.total_tokens();
        assert_eq!(state.runtime.run_tokens.output_tokens, 0);

        let usage = crate::models::TokenUsage::provider(1_000, 250).with_reasoning_output(50);
        let metadata = crate::domain::ToolRunMetadata {
            detail: crate::domain::ToolMetadata::Subagent {
                model_id: "ollama/test".to_string(),
                agent_id: "a1".to_string(),
            },
            token_usage: Some(usage),
            ..Default::default()
        };
        let (state, _) = update(
            state,
            Msg::ToolFinished {
                turn: TurnId(3),
                call_id,
                outcome: ToolOutcome::success("report", "subagent completed", 1.0)
                    .with_metadata(metadata),
            },
        );

        // Session totals count the child's whole session (reasoning is
        // disjoint from completion, so 1000 + 250 + 50)…
        assert_eq!(
            state.session.cumulative_token_usage.total_tokens(),
            before_cum + 1_300
        );
        assert_eq!(state.session.cumulative_token_usage.completion_tokens, 250);
        // …the run counter banks its generated tokens (completion + reasoning),
        // as a real provider count (no `~` taint)…
        assert_eq!(state.runtime.run_tokens.output_tokens, 300);
        assert!(!state.runtime.run_tokens.contains_estimate);
        // …but the child never poses as the parent's own last request (that
        // field feeds the context-size estimate for the PARENT's window).
        assert!(state.session.last_token_usage.is_none());
    }

    #[test]
    fn system_prompt_appends_subagent_contract_only_when_flagged() {
        let mut state = fresh_state();
        assert!(
            !system_prompt_for_state(&state).contains("Subagent Contract"),
            "a user-facing session must not carry the subagent contract",
        );
        state.session.is_subagent = true;
        let prompt = system_prompt_for_state(&state);
        assert!(prompt.contains("## Subagent Contract"), "got {prompt}");
        assert!(
            prompt.contains("returned to the parent as the tool result"),
            "the contract must state the report semantics",
        );
        // An agent type's preamble rides after the contract.
        state.session.agent_preamble = Some("## Explore Agent\nRead-only recon.".to_string());
        let prompt = system_prompt_for_state(&state);
        assert!(prompt.contains("## Explore Agent"), "got {prompt}");
        assert!(
            prompt.find("## Subagent Contract") < prompt.find("## Explore Agent"),
            "type preamble must follow the contract",
        );
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
        let (state, cmds) = update(
            fresh_state(),
            Msg::Resize {
                width: 80,
                height: 24,
            },
        );
        assert!(cmds.is_empty());
        assert!(matches!(state.turn, TurnState::Idle));
    }

    #[test]
    fn ui_state_default_is_empty() {
        let s = UiState::default();
        assert!(s.input_buffer.is_empty());
        assert!(matches!(s.mode, UiMode::EditingInput));
    }
}
