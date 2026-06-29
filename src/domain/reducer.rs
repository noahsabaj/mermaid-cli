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

use crate::constants::{DEFAULT_MAX_TOKENS, DEFAULT_TEMPERATURE};
use crate::models::{ChatMessage, MessageRole};
use crate::prompts::get_system_prompt;
use crate::runtime::TaskStatus;

use super::cmd::{ChatRequest, Cmd};
use super::compaction::{
    CompactionArchive, CompactionPolicy, CompactionRequest, CompactionResult, CompactionTrigger,
    context_exceeds_hard_limit, should_auto_compact,
};
use super::ids::TurnId;
use super::msg::{KeyCode, KeyMods, Msg, Paste, SlashCmd};
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

/// The public reducer entry point. Runs one `update_step` for the
/// incoming `msg`, then drains any follow-up `Msg`s the handler
/// pushed onto `state.ui.pending_msgs`. All emitted `Cmd`s coalesce
/// into the returned vector.
pub fn update(mut state: State, msg: Msg) -> (State, Vec<Cmd>) {
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
                thinking_signature,
                tokens,
                ..
            } = &mut state.turn
                && *id == turn
            {
                partial_reasoning.push_str(&chunk.text);
                *phase = GenPhase::Thinking;
                if let Some(sig) = chunk.signature {
                    *thinking_signature = Some(sig);
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
            state.runtime.builtin_tool_schema_tokens = tokens;
        },
        Msg::ProviderContextResolved {
            model_max,
            effective,
            source,
            ..
        } => {
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
            thinking_signature,
            stop_reason,
        } => {
            handle_stream_done(
                &mut state,
                &mut cmds,
                turn,
                usage,
                thinking_signature,
                stop_reason,
            );
        },
        Msg::UpstreamError { turn, error } => {
            handle_upstream_error(&mut state, turn, error);
        },
        Msg::TurnCancelled(turn) => {
            handle_turn_cancelled(&mut state, turn);
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
            call_id: _,
            event,
        } => {
            handle_tool_progress(&mut state, &mut cmds, turn, event);
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
                    config: crate::app::McpServerConfig {
                        command: String::new(),
                        args: Vec::new(),
                        env: std::collections::HashMap::new(),
                    },
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
                    config: crate::app::McpServerConfig {
                        command: String::new(),
                        args: Vec::new(),
                        env: std::collections::HashMap::new(),
                    },
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
        Msg::InstructionsChanged(loaded) => {
            state.instructions = loaded;
        },
        Msg::MemoryChanged(loaded) => {
            state.memory = loaded;
        },
        Msg::SessionSaved => {
            // Silent. Reducer already committed; save is just durability.
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
                state.pending_approval.clear();
            }
            state.session.conversation = history;
            state.turn = TurnState::Idle;
            state.ui.mode = UiMode::EditingInput;
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
        Msg::RuntimeTasksListed(tasks) => {
            state
                .session
                .append(ChatMessage::system(tasks_text(&tasks)));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        Msg::RuntimeTaskLoaded { task, events } => {
            state.session.append(ChatMessage::system(task_detail_text(
                task.as_ref(),
                &events,
            )));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        Msg::RuntimeProcessesListed(processes) => {
            state
                .session
                .append(ChatMessage::system(processes_text(&processes)));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        Msg::RuntimeText(text) => {
            state.session.append(ChatMessage::system(text));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        Msg::RuntimeApprovalsListed(approvals) => {
            state
                .session
                .append(ChatMessage::system(approvals_text(&approvals)));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        Msg::RuntimeCheckpointsListed(checkpoints) => {
            state
                .session
                .append(ChatMessage::system(checkpoints_text(&checkpoints)));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        Msg::RuntimePluginsListed(plugins) => {
            state
                .session
                .append(ChatMessage::system(plugins_text(&plugins)));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
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
        Msg::StatusDismiss => {
            state.status = None;
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
        Msg::TransientStatus {
            text,
            kind: _,
            dismiss_ms: _,
        } => {
            // Generic async feedback from effect handlers ("clipboard is empty",
            // "config saved", etc.). Routed into the chat transcript instead of
            // the old transient banner above the input.
            push_system(&mut state, &mut cmds, text);
        },
        Msg::OpenImageAt {
            message_index,
            image_index,
        } => {
            handle_open_image_at(&mut state, &mut cmds, message_index, image_index);
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
    let current = state.session.conversation.title.clone();
    if state.ui.last_title_dispatched.as_deref() != Some(current.as_str()) {
        cmds.push(Cmd::SetTerminalTitle(format!("mermaid - {}", current)));
        state.ui.last_title_dispatched = Some(current);
    }
}

// ─── helpers ────────────────────────────────────────────────────────

fn handle_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode, mods: KeyMods) {
    // Ctrl+C is the hard "leave the TUI" path. If work is active,
    // emit a cancellation first so shutdown does not wait on a live
    // provider/tool scope before returning the terminal.
    if mods.ctrl && code == KeyCode::Char('c') {
        request_exit(state, cmds);
        return;
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
    // hundreds of ms on macOS); result comes back as
    // `Msg::Paste(Image|Text)` or `Msg::TransientStatus` on failure.
    if mods.ctrl
        && code == KeyCode::Char('v')
        && matches!(state.ui.mode, UiMode::EditingInput)
        && state.confirm.is_none()
    {
        cmds.push(Cmd::ReadClipboard);
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

    // Shift+Tab cycles the safety mode (read-only → ask → auto → full-access).
    // Session-scoped: the `[safety]` config value stays the persistent default,
    // so a session never silently inherits a more-permissive mode from a
    // previous run. Mirrors the Alt+T reasoning cycle above.
    if code == KeyCode::BackTab {
        let next = cycle_safety(state.session.safety_mode);
        state.session.safety_mode = next;
        // The bottom status bar already shows the new safety mode — no banner.
        return;
    }

    // Conversation-list picker (UiMode::ConversationList): ↑/↓
    // navigate, Enter loads the highlighted session, Esc dismisses.
    if matches!(state.ui.mode, UiMode::ConversationList { .. }) {
        handle_conversation_list_key(state, cmds, code);
        return;
    }

    // Attachment-focus mode: keyboard navigates the bar.
    if state.ui.attachment_focused {
        handle_attachment_key(state, code);
        return;
    }

    // Slash-palette navigation — intercepts ↑/↓/Tab/Esc while the
    // input buffer opens with `/`. Enter falls through to the normal
    // handler below so the command actually dispatches.
    if state.ui.input_buffer.starts_with('/') {
        use crate::domain::slash_commands::filter_by_prefix;
        let typed = state
            .ui
            .input_buffer
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        let candidates = filter_by_prefix(typed);
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
                if let Some(cmd) = candidates.get(sel) {
                    state.ui.input_buffer = format!("/{} ", cmd.name);
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
                if let Some(cmd) = candidates.get(sel) {
                    let raw = state.ui.input_buffer.clone();
                    let after_slash = raw.trim_start_matches('/');
                    let rest = match after_slash.find(char::is_whitespace) {
                        Some(idx) => &after_slash[idx..],
                        None => "",
                    };
                    state.ui.input_buffer = format!("/{}{}", cmd.name, rest);
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

    // Enter submits the current input (or triggers the slash palette
    // pick). Shift+Enter is a newline for multi-line input. This arm
    // enqueues a synthetic `Msg` on `pending_msgs` rather than
    // invoking the dispatch directly — the outer `update()` drain
    // will run the follow-up with stale-filter + pending-msgs
    // guarantees intact.
    if code == KeyCode::Enter && !mods.shift {
        let buf = state.ui.input_buffer.trim().to_string();
        if buf.is_empty() {
            return;
        }
        if let Some(rest) = buf.strip_prefix('/') {
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
        return;
    }

    if mods.is_empty() || mods.shift {
        match code {
            KeyCode::Char(c) => {
                // Any text mutation resets history nav — the user's
                // typing wins over whatever historical entry was
                // on-screen.
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
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
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                if pos > 0 {
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
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                if pos < state.ui.input_buffer.len() {
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
                // Up precedence: attachment focus wins ONLY when the
                // input is empty AND attachments exist — otherwise
                // step back through input history.
                if state.ui.input_buffer.is_empty() && !state.ui.attachments.is_empty() {
                    state.ui.attachment_focused = true;
                    state.ui.attachment_selected = state
                        .ui
                        .attachment_selected
                        .min(state.ui.attachments.len() - 1);
                } else {
                    history_nav_back(state);
                }
            },
            KeyCode::Down => {
                history_nav_forward(state);
            },
            KeyCode::Escape => {
                state.ui.attachment_focused = false;
                // Also clear any in-progress history nav.
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
            },
            _ => {},
        }
    }
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

/// Handle keyboard input while the attachment bar has keyboard
/// focus. Returns without emitting Cmds; attachment removal happens
/// inline on state.ui.attachments.
fn handle_attachment_key(state: &mut State, code: KeyCode) {
    match code {
        KeyCode::Escape | KeyCode::Down => {
            state.ui.attachment_focused = false;
        },
        KeyCode::Left => {
            if !state.ui.attachments.is_empty() {
                state.ui.attachment_selected = state
                    .ui
                    .attachment_selected
                    .checked_sub(1)
                    .unwrap_or(state.ui.attachments.len() - 1);
            }
        },
        KeyCode::Right => {
            if !state.ui.attachments.is_empty() {
                state.ui.attachment_selected =
                    (state.ui.attachment_selected + 1) % state.ui.attachments.len();
            }
        },
        KeyCode::Delete | KeyCode::Backspace => {
            let idx = state.ui.attachment_selected;
            if idx < state.ui.attachments.len() {
                state.ui.attachments.remove(idx);
            }
            if state.ui.attachments.is_empty() {
                state.ui.attachment_focused = false;
                state.ui.attachment_selected = 0;
            } else if state.ui.attachment_selected >= state.ui.attachments.len() {
                state.ui.attachment_selected = state.ui.attachments.len() - 1;
            }
        },
        _ => {},
    }
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

fn handle_paste(state: &mut State, cmds: &mut Vec<Cmd>, paste: Paste) {
    match paste {
        Paste::Text(t) => {
            // Insert at the cursor and advance it, exactly like typing. Pasted
            // text and individual key presses MUST agree on position: on the
            // Windows console a paste arrives as a mix of coalesced `Paste`
            // chunks and stray `Char` key events, so appending to the end here
            // while keys insert at the cursor scrambled the result (uppercase
            // letters piled at the front).
            state.ui.input_history_cursor = None;
            state.ui.history_draft.clear();
            let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
            state.ui.input_buffer.insert_str(pos, &t);
            state.ui.input_cursor = clamp_cursor(&state.ui.input_buffer, pos + t.len());
            if state.ui.input_buffer.starts_with('/') {
                state.ui.palette_cursor = Some(0);
            }
        },
        Paste::Image { bytes, format } => {
            let id = state.ids.tool_call.next();
            let temp_path = state
                .temp_dir
                .join(format!("mermaid-img-{}.{}", id, format));
            state.ui.attachments.push(super::state::Attachment {
                id,
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
        },
    }
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
        state
            .ui
            .queued_messages
            .push_back(super::state::QueuedMessage {
                text,
                attachment_ids: attachment_ids.to_vec(),
            });
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
    // Submitting consumes attachments while the bar may still be focused;
    // re-clamp the selection so the render layer never indexes past the
    // shrunken list (mirrors the Delete-key path in handle_key).
    if state.ui.attachments.is_empty() {
        state.ui.attachment_focused = false;
        state.ui.attachment_selected = 0;
    } else if state.ui.attachment_selected >= state.ui.attachments.len() {
        state.ui.attachment_selected = state.ui.attachments.len() - 1;
    }

    let mut user_msg = ChatMessage::user(text.clone());
    if !images.is_empty() {
        user_msg = user_msg.with_images(images);
    }
    state.session.append(user_msg);
    state.session.conversation.add_to_input_history(text);
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
    state.runtime.run_committed_tokens = 0;
    // Fresh run — clear the truncation-recovery guard from any prior run.
    state.runtime.truncation_recoveries = 0;
    state.turn = start_generating(turn, std::time::SystemTime::from(state.now));
    cmds.push(Cmd::CallModel {
        turn,
        request: build_chat_request(state),
    });
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
            state.session.safety_mode = mode;
            // The bottom status bar shows the new mode — no banner.
        },
        SlashCmd::VisibleReasoning(arg) => {
            match visible_reasoning_value(arg.as_deref(), state.ui.show_reasoning) {
                Ok(next) => {
                    state.ui.show_reasoning = next;
                    set_status(
                        state,
                        cmds,
                        if next {
                            "Visible reasoning: on"
                        } else {
                            "Visible reasoning: off"
                        },
                        StatusKind::Info,
                        3_000,
                    );
                },
                Err(usage) => {
                    set_status(state, cmds, usage, StatusKind::Warn, 4_000);
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
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
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
            state.session.append(ChatMessage::system(usage_text(state)));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        SlashCmd::Context(cmd) => {
            use crate::domain::ContextCmd;
            let model_id = state.session.model_id.clone();
            let is_ollama = model_id.starts_with("ollama/");
            match cmd {
                ContextCmd::Show => {
                    state
                        .session
                        .append(ChatMessage::system(context_text(state)));
                    cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
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
            set_status(
                state,
                cmds,
                "Usage: /remember <fact to remember>",
                StatusKind::Info,
                3_000,
            );
        },
        SlashCmd::Forget(Some(id)) => {
            cmds.push(Cmd::ForgetMemory { id });
        },
        SlashCmd::Forget(None) => {
            set_status(
                state,
                cmds,
                "Usage: /forget <memory name> (see /memory for names)",
                StatusKind::Info,
                3_000,
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
                .append(ChatMessage::system(doctor_text(state)));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        SlashCmd::Tasks => {
            cmds.push(Cmd::ListRuntimeTasks { limit: 10 });
        },
        SlashCmd::Task(Some(id)) => {
            cmds.push(Cmd::LoadRuntimeTask { id });
        },
        SlashCmd::Task(None) => {
            set_status(state, cmds, "Usage: /task <id>", StatusKind::Info, 3_000);
        },
        SlashCmd::Pause(Some(id)) => {
            cmds.push(Cmd::UpdateRuntimeTaskStatus {
                id,
                status: TaskStatus::Blocked,
                final_report: Some("Paused from TUI".to_string()),
            });
        },
        SlashCmd::Pause(None) => {
            set_status(
                state,
                cmds,
                "Usage: /pause <task-id>",
                StatusKind::Info,
                3_000,
            );
        },
        SlashCmd::Resume(Some(id)) => {
            cmds.push(Cmd::UpdateRuntimeTaskStatus {
                id,
                status: TaskStatus::Running,
                final_report: None,
            });
        },
        SlashCmd::Resume(None) => {
            set_status(
                state,
                cmds,
                "Usage: /resume <task-id>",
                StatusKind::Info,
                3_000,
            );
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
                set_status(
                    state,
                    cmds,
                    "No active turn to cancel.",
                    StatusKind::Info,
                    2_500,
                );
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
            state.session.append(ChatMessage::system(text));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        SlashCmd::Report(None) => {
            let text = format!(
                "Runtime report\n\n{}\n\n{}",
                context_text(state),
                usage_text(state)
            );
            state.session.append(ChatMessage::system(text));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        SlashCmd::Processes => {
            cmds.push(Cmd::ListRuntimeProcesses { limit: 10 });
        },
        SlashCmd::Logs(Some(id)) => {
            cmds.push(Cmd::ShowRuntimeProcessLogs { id });
        },
        SlashCmd::Logs(None) => {
            set_status(
                state,
                cmds,
                "Usage: /logs <process-id>",
                StatusKind::Info,
                3_000,
            );
        },
        SlashCmd::Stop(Some(id)) => {
            cmds.push(Cmd::StopRuntimeProcess { id });
        },
        SlashCmd::Stop(None) => {
            set_status(
                state,
                cmds,
                "Usage: /stop <process-id>",
                StatusKind::Info,
                3_000,
            );
        },
        SlashCmd::Restart(Some(id)) => {
            cmds.push(Cmd::RestartRuntimeProcess { id });
        },
        SlashCmd::Restart(None) => {
            set_status(
                state,
                cmds,
                "Usage: /restart <process-id>",
                StatusKind::Info,
                3_000,
            );
        },
        SlashCmd::Open(Some(target)) => {
            cmds.push(Cmd::OpenRuntimeTarget { target });
        },
        SlashCmd::Open(None) => {
            set_status(
                state,
                cmds,
                "Usage: /open <url|path|process-id>",
                StatusKind::Info,
                3_000,
            );
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
            set_status(
                state,
                cmds,
                "Usage: /approve <approval-id>",
                StatusKind::Info,
                3_000,
            );
        },
        SlashCmd::Deny(Some(id)) => {
            cmds.push(Cmd::DecideRuntimeApproval {
                id,
                decision: "denied".to_string(),
            });
        },
        SlashCmd::Deny(None) => {
            set_status(
                state,
                cmds,
                "Usage: /deny <approval-id>",
                StatusKind::Info,
                3_000,
            );
        },
        SlashCmd::Checkpoint(Some(paths)) => {
            let paths = paths
                .split_whitespace()
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>();
            cmds.push(Cmd::CreateRuntimeCheckpoint { paths });
        },
        SlashCmd::Checkpoint(None) => {
            set_status(
                state,
                cmds,
                "Usage: /checkpoint <path...>",
                StatusKind::Info,
                3_000,
            );
        },
        SlashCmd::Checkpoints => {
            cmds.push(Cmd::ListRuntimeCheckpoints { limit: 10 });
        },
        SlashCmd::Restore(Some(id)) => {
            cmds.push(Cmd::RestoreRuntimeCheckpoint { id });
        },
        SlashCmd::Restore(None) => {
            set_status(
                state,
                cmds,
                "Usage: /restore <checkpoint-id>",
                StatusKind::Info,
                3_000,
            );
        },
        SlashCmd::Plugins => {
            cmds.push(Cmd::ListRuntimePlugins);
        },
        SlashCmd::ModelInfo(Some(model)) => {
            cmds.push(Cmd::ShowRuntimeModelInfo { model });
        },
        SlashCmd::ModelInfo(None) => {
            set_status(
                state,
                cmds,
                "Usage: /model-info <model>",
                StatusKind::Info,
                3_000,
            );
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
        SlashCmd::Help => {
            state.session.append(ChatMessage::system(help_text()));
            cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
        },
        SlashCmd::Quit => {
            request_exit(state, cmds);
        },
        SlashCmd::Unknown(name) => {
            push_system(state, cmds, format!("Unknown command: /{}", name));
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
    state.session.append(ChatMessage::system(text.into()));
    cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
}

/// Back-compat shim for the many call sites that used the old transient banner.
/// Routes their text into the chat transcript via [`push_system`]; the severity
/// and dismiss timeout no longer mean anything (there's no banner to color or
/// auto-clear).
fn set_status(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    text: impl Into<String>,
    _kind: StatusKind,
    _dismiss_ms: u64,
) {
    push_system(state, cmds, text);
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
    };
    // The live "Compacting…" status comes from the TurnState::Compacting status
    // line (the blue indicator); no separate gray status message — it was a
    // redundant duplicate. The completion receipt is set on CompactionFinished.
    cmds.push(Cmd::CompactConversation {
        turn,
        request: CompactionRequest::manual(build_chat_request(state), instructions),
    });
}

fn help_text() -> String {
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
        safety_mode_name(state.settings.safety.mode),
        state.settings.safety.checkpoint_on_mutation
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

fn safety_mode_name(mode: crate::runtime::SafetyMode) -> &'static str {
    mode.as_str()
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
    lines.push(format!(
        "Session processed: {}",
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
            request.reasoning,
            ctx.effective,
            next_snapshot.used_tokens,
        );
        lines.push(format!(
            "Output budget (num_predict): {}",
            format_compact_count(num_predict as usize)
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
    let response_reserve = policy.response_reserve(request.max_tokens);
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
    let auto_status = match should_auto_compact(&next_snapshot, &request, policy) {
        Ok(()) => "would run before the next model call".to_string(),
        Err(reason) => format!("not needed ({reason})"),
    };
    lines.push(format!("Auto compact: {auto_status}"));
    if auto_status.starts_with("would run") {
        lines.push(
            "Suggested action: continue normally; Mermaid will compact before the next model call."
                .to_string(),
        );
    } else {
        lines.push("Suggested action: no manual compaction needed unless you want a handoff checkpoint now.".to_string());
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
            "- verification: {}",
            if last.verified {
                "verified".to_string()
            } else {
                last.verification_error
                    .as_ref()
                    .map(|err| format!("draft fallback ({err})"))
                    .unwrap_or_else(|| "draft fallback".to_string())
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
        format!("total {}", format_compact_count(usage.total_tokens)),
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

fn format_compact_count(value: usize) -> String {
    if value >= 1_000_000 {
        format_scaled(value, 1_000_000, "m")
    } else if value >= 10_000 {
        format_scaled(value, 1_000, "k")
    } else {
        value.to_string()
    }
}

fn format_scaled(value: usize, divisor: usize, suffix: &str) -> String {
    let whole = value / divisor;
    let decimal = ((value % divisor) * 10) / divisor;
    if decimal == 0 {
        format!("{}{}", whole, suffix)
    } else {
        format!("{}.{}{}", whole, decimal, suffix)
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
    // The cancelled turn's tool tasks are aborted; their parked approval
    // requests can never be answered, so drop any queued prompts.
    state.pending_approval.clear();
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
    state.pending_approval.clear();
    // Quitting mid-stream: preserve whatever the model already produced so
    // `--continue` shows what was on screen. Commit the in-flight partial
    // as an assistant message with an interrupted marker before saving.
    let now = state.now;
    if let TurnState::Generating {
        partial_text,
        partial_reasoning,
        thinking_signature,
        ..
    } = &mut state.turn
        && !partial_text.trim().is_empty()
    {
        let text = std::mem::take(partial_text);
        let reasoning = std::mem::take(partial_reasoning);
        let sig = thinking_signature.take();
        let msg = commit_assistant_message(
            format!("{text}\n\n_[interrupted]_"),
            reasoning,
            Vec::new(),
            sig,
            now,
        );
        state.session.append(msg);
    }
    cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));
    cmds.push(Cmd::Exit);
}

fn handle_confirm_accepted(state: &mut State, cmds: &mut Vec<Cmd>) {
    let Some(confirm) = state.confirm.take() else {
        return;
    };
    match confirm.accept_msg_token {
        super::state::ConfirmationTarget::ClearConversation => {
            // Clear = start a fresh conversation: new ID, new default
            // title, empty history, zero cumulative tokens. Matches
            // user mental model ("wipe everything").
            let project_path = state.session.conversation.project_path.clone();
            let model_name = state.session.conversation.model_name.clone();
            state.session.conversation =
                crate::session::ConversationHistory::new(project_path, model_name);
            state.session.cumulative_tokens = 0;
            state.session.last_token_usage = None;
            state.session.cumulative_token_usage = TokenUsageTotals::default();
            state.session.context_usage = None;
            emit_title_if_changed(state, cmds);
        },
    }
}

fn handle_compaction_finished(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    turn: TurnId,
    result: CompactionResult,
) {
    // Manual `/compact` ends the turn; a truncation recovery resumes the run; a
    // pre-turn auto-compaction (still `Generating`) just swaps in the compacted
    // messages and lets the in-flight stream continue in the effect.
    enum Outcome {
        Manual,
        Recovery,
        AutoMidTurn,
    }
    let outcome = match state.turn {
        TurnState::Compacting { id, trigger, .. } if id == turn => match trigger {
            CompactionTrigger::TruncationRecovery => Outcome::Recovery,
            _ => Outcome::Manual,
        },
        TurnState::Generating { id, .. } if id == turn => Outcome::AutoMidTurn,
        _ => return,
    };

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
        .replace_messages(result.replacement_messages);
    state.session.conversation.add_compaction(record.clone());
    state.session.context_usage = Some(result.after_snapshot);

    if let Some(usage) = result.usage {
        let totals = TokenUsageTotals::from_usage(&usage);
        state.session.last_token_usage = Some(totals);
        state.session.cumulative_token_usage.add_assign(totals);
        state.session.cumulative_tokens = state
            .session
            .cumulative_tokens
            .saturating_add(usage.total_tokens);
    }

    match outcome {
        Outcome::Manual => {
            state.turn = TurnState::Idle;
            // Drain one queued message on the way out, same as the no-tool-calls
            // tail of `handle_stream_done` / `handle_turn_cancelled` (#73). A
            // message the user typed during `/compact` would otherwise sit in the
            // FIFO until some later turn happened to end.
            if let Some(next) = state.ui.queued_messages.pop_front() {
                state.ui.pending_msgs.push_back(Msg::SubmitPrompt {
                    text: next.text,
                    attachment_ids: next.attachment_ids,
                });
            }
        },
        Outcome::Recovery => {
            // Resume the run with the compacted context so the model can finish the
            // work the truncation cut off (mirrors `handle_tool_finished`'s
            // follow-up dispatch).
            let next_turn = state.ids.fresh_turn();
            state.turn = start_generating(next_turn, std::time::SystemTime::from(state.now));
            cmds.push(Cmd::CallModel {
                turn: next_turn,
                request: build_chat_request(state),
            });
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
        },
        TurnState::Generating { id, .. } if id == turn => {},
        _ => return,
    }

    let prefix = match trigger {
        CompactionTrigger::Manual => "Compaction failed",
        // Auto-compaction is best-effort preflight: when it can't run (e.g. too
        // little history to compact) Mermaid just proceeds with the un-compacted
        // request, so there's nothing for the user to act on. Stay silent rather
        // than printing a scary "Invalid request" every turn (still logged at WARN).
        CompactionTrigger::AutoThreshold => return,
        CompactionTrigger::ContextLimitRetry => "Context-limit compaction failed",
        // The response truncated and recovery couldn't reduce the context (e.g. the
        // preserved tail already fills the window). Stop the run cleanly with the
        // manual levers instead of the raw "did not reduce" error or a retry loop.
        CompactionTrigger::TruncationRecovery => {
            let hint = truncation_hint(state);
            state.session.append(ChatMessage::system(hint));
            return;
        },
    };
    let _ = kind;
    state
        .session
        .append(ChatMessage::system(format!("{}: {}", prefix, message)));
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

fn handle_stream_done(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    turn: TurnId,
    usage: Option<crate::models::TokenUsage>,
    thinking_signature: Option<String>,
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
            thinking_signature: accumulated_sig,
            pending_tool_calls,
            ..
        } if id == turn => (
            partial_text,
            partial_reasoning,
            accumulated_sig,
            pending_tool_calls,
        ),
        other => {
            state.turn = other;
            return;
        },
    };

    let (partial_text, partial_reasoning, accumulated_sig, tool_calls) = generating;
    // Bank this phase's generated tokens into the run total so the spinner's
    // counter carries across the tool step into the next model call (matches the
    // live estimate in StreamText/StreamReasoning).
    state.runtime.run_committed_tokens += (partial_text.len() + partial_reasoning.len()) / 4;
    // A turn that ends with no text, no reasoning, and no tool calls is a dead
    // end — the run silently stops and looks broken. Flag it so the user knows to
    // retry (seen with smaller local models that occasionally return nothing).
    let empty_response = partial_text.trim().is_empty() && partial_reasoning.trim().is_empty();
    let final_sig = thinking_signature.or(accumulated_sig);

    // Commit the assistant message (with any tool calls attached —
    // the adapter will serialize them into the next conversation
    // turn).
    let msg = commit_assistant_message(
        partial_text,
        partial_reasoning,
        tool_calls.clone(),
        final_sig,
        state.now,
    );
    state.session.append(msg);

    // A bare length-truncation (no tool calls) is the recoverable case below; any
    // other ending means the run made progress, so reset the recovery guard — it
    // should count only *consecutive* no-progress truncations.
    let dry_truncation =
        tool_calls.is_empty() && matches!(stop_reason, Some(crate::models::FinishReason::Length));
    if !dry_truncation {
        state.runtime.truncation_recoveries = 0;
    }

    // Set when a length-truncation is recoverable: instead of ending the run with
    // a hint, compact the conversation and resume (handled after the save below).
    let mut recovering = false;

    // Surface a terminal stop reason that would otherwise leave the response
    // silently incomplete. (A refusal with no content is turned into an error
    // upstream in the adapter; here we only see reasons that still produced
    // output.) Skip it when tool calls are pending: a system message inserted
    // between the assistant's `tool_calls` and their results breaks provider
    // pairing → 400 (#72). A Length/ContentFilter stop *with* tool calls is
    // contradictory anyway, so dropping the note in that case is safe.
    if tool_calls.is_empty() {
        match stop_reason {
            Some(crate::models::FinishReason::Length) => {
                // The window filled mid-turn. If there's history to compact and
                // we're under the per-run cap, recover (compact + continue) rather
                // than stopping; otherwise fall back to the manual-levers hint.
                let cap = state.settings.compaction.max_truncation_recoveries;
                let under_cap = cap == 0 || state.runtime.truncation_recoveries < cap as u32;
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
            Some(crate::models::FinishReason::ContentFilter) => push_system(
                state,
                cmds,
                "Response was flagged by the provider's content filter.",
            ),
            _ if empty_response => push_system(
                state,
                cmds,
                "The model ended its turn with no output (no text or tool calls). \
                 Send your message again, or try rephrasing.",
            ),
            _ => {},
        }
    }

    // Provider token usage is per API request. Track both the last
    // reported request and the session total so the footer can label
    // the number honestly instead of presenting a giant raw counter.
    if let Some(u) = usage {
        let totals = TokenUsageTotals::from_usage(&u);
        state.session.last_token_usage = Some(totals);
        state.session.cumulative_token_usage.add_assign(totals);
        state.session.cumulative_tokens = state
            .session
            .cumulative_tokens
            .saturating_add(u.total_tokens);
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

    cmds.push(Cmd::SaveConversation(state.session.conversation.clone()));

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
        // Captured once for the whole batch: the live safety mode + the
        // turn's intent (for the Auto-mode classifier).
        let intent = latest_user_intent(&state.session);
        for call in &pending {
            cmds.push(Cmd::ExecuteTool {
                turn,
                call_id: call.call_id,
                source: call.source.clone(),
                // F7: pass the session's current model id so subagent
                // tools can spawn children against the same provider.
                model_id: state.session.model_id.clone(),
                safety_mode: state.session.safety_mode,
                intent: intent.clone(),
            });
        }
        state.turn = super::transition::start_executing_tools(
            turn,
            pending,
            std::time::SystemTime::from(state.now),
        );
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

    // No tool calls — turn ends here. Drain the queued-message FIFO.
    // The follow-up goes through `pending_msgs` so the outer
    // `update()` re-enters cleanly — preserves stale-filter
    // semantics instead of inline-invoking.
    if let Some(next) = state.ui.queued_messages.pop_front() {
        state.ui.pending_msgs.push_back(Msg::SubmitPrompt {
            text: next.text,
            attachment_ids: next.attachment_ids,
        });
    }
}

/// Handle `Msg::OpenImageAt { message_index, image_index }`. Resolves
/// the base64 payload from the committed message history, writes it
/// to a temp file, and dispatches `Cmd::OpenInSystem` so the user's
/// default image viewer opens it. F13.
fn handle_open_image_at(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    message_index: usize,
    image_index: usize,
) {
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
fn handle_turn_cancelled(state: &mut State, turn: TurnId) {
    match state.turn {
        TurnState::Cancelling { id, .. } if id == turn => {
            state.turn = TurnState::Idle;
            if let Some(next) = state.ui.queued_messages.pop_front() {
                state.ui.pending_msgs.push_back(Msg::SubmitPrompt {
                    text: next.text,
                    attachment_ids: next.attachment_ids,
                });
            }
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

fn handle_upstream_error(state: &mut State, turn: TurnId, error: crate::models::UserFacingError) {
    // Defense in depth (F4): even though the stale-filter at the top of
    // `update_step` gates on `turn_id()`, re-check here so a future
    // refactor that weakens the filter can't silently wipe the active
    // turn with an error message that belongs to a superseded one.
    if state.turn.id() != Some(turn) {
        return;
    }

    // End the current turn. Surface the error through a single
    // channel — the ActionDisplay attached to an empty assistant
    // message. The chat widget paints ActionDisplays as colored
    // error blocks, so committing to both `content` and `actions`
    // would paint the same error twice.
    //
    // Do NOT also set `state.status`: the F9 banner would render the
    // same error a second time directly above the input, which is
    // just noise. The chat entry is persistent (scrollable);
    // duplicating as a transient banner adds nothing.
    let now = state.now;
    state.turn = TurnState::Idle;
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
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
        thinking_signature: None,
    };
    state.session.append(msg);
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
    _turn: TurnId,
    event: crate::providers::ProgressEvent,
) {
    use crate::providers::ProgressEvent;
    use base64::{Engine as _, engine::general_purpose};

    if let ProgressEvent::Artifact { mime, data, .. } = event
        && mime.starts_with("image/")
        && matches!(
            state.turn,
            TurnState::ExecutingTools { .. } | TurnState::Generating { .. }
        )
        && let Some(last) = state.session.conversation.messages.last_mut()
        && last.role == MessageRole::Assistant
    {
        let encoded = general_purpose::STANDARD.encode(&data);
        last.images.get_or_insert_with(Vec::new).push(encoded);
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
            // Attach action display to the last assistant message so
            // the renderer can show it.
            if let Some(call) = calls.iter().find(|c| c.call_id == call_id) {
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
        state.turn = start_generating(next_turn, std::time::SystemTime::from(state.now));
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
    // Project instructions + the always-loaded memory index compose into the
    // single dynamic suffix. The memory block carries its own `# Memory`
    // header, so it stays clearly separated from AGENTS.md/MERMAID.md and the
    // model adapters need no changes.
    let instructions = match (
        state.instructions.as_ref().map(|i| i.content.clone()),
        state.memory.as_ref().map(|m| m.index.clone()),
    ) {
        (Some(i), Some(m)) => Some(format!("{i}\n\n{m}")),
        (Some(i), None) => Some(i),
        (None, Some(m)) => Some(m),
        (None, None) => None,
    };

    // Pass user-configured values verbatim. `ModelSettings::default()`
    // already supplies `DEFAULT_TEMPERATURE` / `DEFAULT_MAX_TOKENS`,
    // so config never has a real "zero" — the old `.max(DEFAULT_*)`
    // was clobbering low user settings (e.g. temperature=0.1 became
    // 0.7).
    let settings = &state.settings.default_model;
    let temperature = if settings.temperature > 0.0 {
        settings.temperature
    } else {
        DEFAULT_TEMPERATURE
    };
    let max_tokens = if settings.max_tokens > 0 {
        settings.max_tokens
    } else {
        DEFAULT_MAX_TOKENS
    };

    // MCP tools the model should see — each advertised by a Ready
    // server, fully-qualified as `mcp__<server>__<tool>`. The effect
    // runner prepends built-in tools before dispatching, so this
    // vector is the MCP-only portion.
    let mcp_tools: Vec<crate::domain::ToolDefinition> = state
        .mcp
        .servers
        .iter()
        .filter(|(_, entry)| matches!(entry.status, crate::domain::McpServerStatus::Ready))
        .flat_map(|(server_name, entry)| {
            entry
                .tools
                .iter()
                .map(move |tool| crate::domain::ToolDefinition {
                    name: format!("mcp__{}__{}", server_name, tool.name),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                })
        })
        .collect();

    ChatRequest {
        model_id: state.session.model_id.clone(),
        messages: evict_stale_screenshots(state.session.messages().to_vec()),
        system_prompt: system_prompt_for_state(state),
        instructions,
        reasoning: state.session.reasoning,
        temperature,
        max_tokens,
        tools: mcp_tools,
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
    }
}

fn system_prompt_for_state(state: &State) -> String {
    let base = state
        .settings
        .prompt
        .render_system_prompt(&get_system_prompt());
    format!(
        "{}\n\n## Current Session\nCurrent working directory: {}\nSafety mode: {} (live — the user can switch it anytime with Shift+Tab or /safety; trust this over any earlier tool error, and attempt gated actions rather than assuming they will fail).\nTreat this as the project root unless the user specifies a different path.",
        base,
        state.cwd.display(),
        state.session.safety_mode.as_str()
    )
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
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                thinking_signature: None,
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
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                thinking_signature: None,
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
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                thinking_signature: None,
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
    fn ctrl_c_on_idle_with_input_exits() {
        let mut state = fresh_state();
        state.ui.input_buffer = "partial".to_string();
        let msg = Msg::Key(Key {
            code: KeyCode::Char('c'),
            modifiers: KeyMods::ctrl(),
        });
        let (state, cmds) = update(state, msg);
        assert!(state.should_exit);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
    }

    /// Tool stdout must NOT reach the status banner. Surfacing every
    /// progress line there flickered noise above the input (build output,
    /// pids, streamed file contents) that appeared for a fraction of a second
    /// and vanished. The status line names the running tool and the full
    /// output lands in chat; the banner is reserved for discrete messages.
    #[test]
    fn tool_progress_output_does_not_touch_status_banner() {
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
            state.status.is_none(),
            "tool stdout must not appear in the status banner"
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

    /// Generic async feedback (`Msg::TransientStatus`, e.g. clipboard results)
    /// posts a system message into the chat transcript — there is no banner.
    #[test]
    fn transient_status_posts_to_chat_transcript() {
        let state = fresh_state();
        let (state, cmds) = update(
            state,
            Msg::TransientStatus {
                text: "Clipboard is empty".to_string(),
                kind: StatusKind::Info,
                dismiss_ms: 2_000,
            },
        );
        assert!(state.status.is_none(), "no banner is set");
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

    /// F14: a Paste::Image Msg (whatever its origin — bracketed paste,
    /// Ctrl+V via clipboard, or a future drag-drop) creates an
    /// Attachment entry and emits Cmd::WriteImageToTemp. This is the
    /// existing contract; the test pins it so the Ctrl+V wiring has a
    /// known-good downstream to rely on.
    #[test]
    fn paste_image_creates_attachment_and_writes_temp() {
        let state = fresh_state();
        let (state, cmds) = update(
            state,
            Msg::Paste(super::super::msg::Paste::Image {
                bytes: vec![0x89, 0x50, 0x4E, 0x47], // PNG magic bytes
                format: "png".to_string(),
            }),
        );
        assert_eq!(state.ui.attachments.len(), 1);
        let att = &state.ui.attachments[0];
        assert_eq!(att.format, "png");
        assert_eq!(att.size_bytes, 4);
        assert!(cmds.iter().any(|c| {
            matches!(c, Cmd::WriteImageToTemp { path, .. } if path == &att.temp_path)
        }));
    }

    #[test]
    fn open_image_writes_and_opens_the_same_temp_path() {
        let mut state = fresh_state();
        let image =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"image bytes");
        state
            .session
            .append(ChatMessage::assistant("image").with_images(vec![image]));

        let (_, cmds) = update(
            state,
            Msg::OpenImageAt {
                message_index: 0,
                image_index: 0,
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
    fn ctrl_c_during_turn_exits_and_cancels_scope() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(5), std::time::SystemTime::now());
        let msg = Msg::Key(Key {
            code: KeyCode::Char('c'),
            modifiers: KeyMods::ctrl(),
        });
        let (state, cmds) = update(state, msg);
        assert!(state.should_exit);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::CancelScope(TurnId(5))))
        );
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Exit)));
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
        // watcher (#45), so submit neither refreshes inline nor emits a Cmd.
        assert!(cmds.iter().any(|c| matches!(c, Cmd::CallModel { .. })));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::RefreshInstructions)),
            "submit must not emit a refresh Cmd; the watcher keeps config fresh (#45)",
        );
        // user message committed
        assert_eq!(state.session.messages().len(), 1);
        assert_eq!(state.session.messages()[0].content, "hi there");
    }

    #[test]
    fn submit_prompt_when_busy_is_dropped() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(1), std::time::SystemTime::now());
        let msg = Msg::SubmitPrompt {
            text: "ignored".to_string(),
            attachment_ids: vec![],
        };
        let (state, cmds) = update(state, msg);
        assert!(matches!(
            state.turn,
            TurnState::Generating { id: TurnId(1), .. }
        ));
        assert!(cmds.is_empty());
        assert!(state.session.messages().is_empty());
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
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
        };
        let (state, cmds) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                thinking_signature: None,
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
        state.runtime.run_committed_tokens = 999;
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
            state.runtime.run_committed_tokens, 0,
            "token counter reset on submit"
        );
    }

    #[test]
    fn run_token_counter_banks_each_phase_across_tool_steps() {
        // The counter must accumulate, not reset, as each model call completes —
        // so a multi-step agentic run shows one growing total.
        let mut state = fresh_state();
        state.runtime.run_committed_tokens = 100; // earlier phases this run
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: "x".repeat(400),      // ~100 tokens
            partial_reasoning: "y".repeat(400), // ~100 tokens
            tokens: 200,
            phase: GenPhase::Streaming,
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
        };
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                thinking_signature: None,
                stop_reason: None,
            },
        );
        // 100 prior + (400 + 400)/4 = 200 this phase.
        assert_eq!(state.runtime.run_committed_tokens, 300);
    }

    #[test]
    fn stream_done_flags_a_completely_empty_turn() {
        // No text, no reasoning, no tool calls → the run would silently dead-end.
        let mut state = fresh_state();
        state.turn = TurnState::Generating {
            id: TurnId(5),
            started: std::time::SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
        };
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                thinking_signature: None,
                stop_reason: None,
            },
        );
        assert!(matches!(state.turn, TurnState::Idle));
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.role == MessageRole::System && m.content.contains("no output")),
            "an empty turn must tell the user instead of silently stopping"
        );
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
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
        };
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                thinking_signature: None,
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
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
        }
    }

    fn length_done() -> Msg {
        Msg::StreamDone {
            turn: TurnId(5),
            usage: None,
            thinking_signature: None,
            stop_reason: Some(crate::models::FinishReason::Length),
        }
    }

    #[test]
    fn length_truncation_recovers_by_compacting_and_continuing() {
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("build a site"));
        state
            .session
            .append(ChatMessage::assistant("ok, writing files"));
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
    fn length_truncation_at_cap_stops_with_hint() {
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("build a site"));
        state.session.append(ChatMessage::assistant("ok"));
        // Already at the default cap of consecutive recoveries.
        state.runtime.truncation_recoveries =
            state.settings.compaction.max_truncation_recoveries as u32;
        state.turn = truncating_turn("more");
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
        state.session.append(ChatMessage::user("x"));
        state.session.append(ChatMessage::assistant("y"));
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
                thinking_signature: None,
                stop_reason: None, // not a length truncation
            },
        );
        assert_eq!(state.runtime.truncation_recoveries, 0);
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
                summary_tokens: 10,
                duration_secs: 0.0,
                verified: true,
                verification_error: None,
                focus: None,
                archive_path: None,
            },
            replacement_messages: replacement,
            archived_messages: vec![ChatMessage::user("archived")],
            before_snapshot: snap.clone(),
            after_snapshot: snap,
            usage: None,
        }
    }

    #[test]
    fn finished_truncation_recovery_resumes_the_run() {
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("original prompt"));
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::TruncationRecovery,
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
    fn finished_manual_compaction_still_goes_idle() {
        // Regression guard: only TruncationRecovery resumes; manual /compact ends.
        let mut state = fresh_state();
        state.session.append(ChatMessage::user("original prompt"));
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::Manual,
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
        state.session.append(ChatMessage::user("x"));
        state.turn = TurnState::Compacting {
            id: TurnId(7),
            started: std::time::SystemTime::now(),
            trigger: CompactionTrigger::TruncationRecovery,
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
    fn compaction_config_defaults_to_three() {
        let cfg = crate::app::CompactionConfig::default();
        assert_eq!(cfg.max_truncation_recoveries, 3);
        // An absent [compaction] section deserializes to the default.
        let parsed: crate::app::Config = toml::from_str("").unwrap();
        assert_eq!(parsed.compaction.max_truncation_recoveries, 3);
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
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
        };

        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: Some(crate::models::TokenUsage::provider(120, 30, 150)),
                thinking_signature: None,
                stop_reason: None,
            },
        );

        assert_eq!(state.session.last_token_usage.unwrap().prompt_tokens, 120);
        assert_eq!(state.session.cumulative_token_usage.total_tokens, 150);
        assert_eq!(state.session.cumulative_tokens, 150);
        assert_eq!(
            state.session.context_usage.as_ref().unwrap().used_tokens,
            150
        );
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
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
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
        state.session.append(ChatMessage::user("hello"));

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
        super::handle_upstream_error(&mut state, TurnId(999), err);
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
        // Error surfaces through the ActionDisplay only — content is
        // intentionally empty so the chat widget doesn't paint the
        // error twice (once as a content line, once as an action).
        assert_eq!(m.content, "");
        assert_eq!(m.actions.len(), 1);
        assert_eq!(m.actions[0].target, "Server error");
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
                model_max: Some(262_144),
                effective: Some(12_288),
                source: Some(NumCtxSource::Auto),
            },
        );
        let ctx = state.runtime.ollama_context.expect("stored");
        assert_eq!(ctx.model_max, Some(262_144));
        assert_eq!(ctx.effective, Some(12_288));
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
            .append(ChatMessage::user("word ".repeat(8_000))); // ≫ 4096 tokens
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
        assert!(
            state.status.is_none(),
            "no banner — errors go to the transcript"
        );
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
        state.session.append(ChatMessage::assistant("tools follow"));

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

    fn test_attachment(id: u64) -> crate::domain::Attachment {
        crate::domain::Attachment {
            id,
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
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
        }
    }

    #[test]
    fn queued_message_keeps_attachments_from_queue_time() {
        // Axis 1 #1: a message queued while busy must re-submit with the
        // attachments present when it was queued, not whatever is live when the
        // FIFO drains.
        let mut state = fresh_state();
        state.turn = generating(5, "answer");
        state.ui.attachments.push(test_attachment(1));

        // Busy → queued, capturing attachment id 1.
        let (mut state, _) = update(
            state,
            Msg::SubmitPrompt {
                text: "queued".to_string(),
                attachment_ids: vec![1],
            },
        );
        assert_eq!(state.ui.queued_messages.len(), 1);

        // User swaps attachments while the turn is still running.
        state.ui.attachments.clear();
        state.ui.attachments.push(test_attachment(2));

        // Turn completes → queued message drains and re-submits.
        let (state, _) = update(
            state,
            Msg::StreamDone {
                turn: TurnId(5),
                usage: None,
                thinking_signature: None,
                stop_reason: None,
            },
        );

        // The re-submit targeted attachment id 1 (now gone), so live id 2 is
        // untouched — proving the queued ids were used, not the live set.
        assert_eq!(state.ui.attachments.len(), 1);
        assert_eq!(state.ui.attachments[0].id, 2);
        let queued_msg = state
            .session
            .messages()
            .iter()
            .find(|m| m.role == MessageRole::User && m.content == "queued")
            .expect("queued message submitted");
        assert!(queued_msg.images.is_none());
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
                usage: Some(crate::models::TokenUsage::provider(120, 30, 150)),
                thinking_signature: None,
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
                thinking_signature: None,
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
    fn submit_clears_attachment_focus_when_consumed() {
        // Axis 1 #7: submitting while the attachment bar is focused must
        // re-clamp the selection so the render layer can't index past the
        // now-empty list.
        let mut state = fresh_state();
        state.ui.attachments.push(test_attachment(1));
        state.ui.attachment_focused = true;
        state.ui.attachment_selected = 0;
        let (state, _) = update(
            state,
            Msg::SubmitPrompt {
                text: "go".to_string(),
                attachment_ids: vec![1],
            },
        );
        assert!(state.ui.attachments.is_empty());
        assert!(!state.ui.attachment_focused);
        assert_eq!(state.ui.attachment_selected, 0);
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
        state.session.append(ChatMessage::assistant("tools follow"));

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
        state.session.append(ChatMessage::assistant("tools follow"));

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
        state.session.append(ChatMessage::assistant("tools follow"));

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
        assert_eq!(s.chat_scroll, 0);
        assert!(matches!(s.mode, UiMode::EditingInput));
    }
}
