use crate::cmd::Cmd;
use crate::compaction::{CompactionRequest, CompactionResult, CompactionTrigger};
use crate::msg::Msg;
use crate::reducer::*;
use crate::reports::*;
use crate::request::*;
use crate::state::{State, StatusKind, TokenUsageTotals, ToolOutcome, TurnState};
use crate::transition::commit_assistant_message;
use mermaid_model::ids::TurnId;
use mermaid_model::models::{ChatMessage, MessageRole, ProviderContinuation, TokenUsage};

/// How one API request's usage folds into the session/run counters.
pub enum UsageFold {
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
pub fn fold_token_usage(
    session: &mut crate::state::Session,
    runtime: &mut crate::RuntimeState,
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

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub fn handle_compaction_finished(
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
    let intervening_tokens = crate::compaction::estimate_messages_tokens(&intervening);

    let conversation_id = state.session.conversation.id.clone();
    let mut record = result.record;
    // The dropped messages are not copied to a sidecar file any more: they
    // are the earlier `message` events of the session log, and the
    // `compaction` event this turn emits marks the boundary. So the
    // recorded path points at the log that holds both.
    record.archive_path = Some(format!(".mermaid/conversations/{conversation_id}.jsonl"));

    state
        .session
        .conversation
        .replace_messages(result.replacement_messages, state.now);
    if !intervening.is_empty() {
        let messages = state.session.conversation.messages_mut();
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
    // The event carries the FINAL post-splice transcript, so the fold
    // reproduces the replace + splice + record in one step.
    state.session.note_compaction(state.now, record.clone());
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
            state.turn = crate::transition::start_generating_with(
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
    // SaveCompaction appends the boundary event, then persists the stripped
    // conversation.
    let conversation = state.session.conversation.clone();
    let events = state.session.drain_events(&conversation);
    cmds.push(Cmd::SaveCompaction {
        record,
        conversation,
        events,
    });
}

#[must_use]
pub fn compaction_intervening_messages(
    current: &[ChatMessage],
    source: &[crate::CompactionBoundary],
) -> Vec<ChatMessage> {
    if source.is_empty() {
        return Vec::new();
    }
    let mut source_index = 0usize;
    let mut intervening = Vec::new();
    for message in current {
        // One hash per live message; the lookahead below compares strings.
        let fingerprint = crate::CompactionBoundary::fingerprint_of(message);
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

pub fn handle_compaction_failed(
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
        ChatMessage::system(format!("{prefix}: {message}")),
        state.now,
    );
}

pub fn handle_stream_tool_call(
    state: &mut State,
    turn: TurnId,
    call: mermaid_model::models::tool_call::ToolCall,
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
#[must_use]
pub fn truncation_hint(state: &State) -> String {
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
#[must_use]
pub fn output_cap_hint(state: &State) -> String {
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
pub const MAX_EMPTY_CONTINUATIONS: u32 = 1;

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub fn handle_stream_done(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    turn: TurnId,
    usage: Option<mermaid_model::models::TokenUsage>,
    provider_continuation: Option<ProviderContinuation>,
    stop_reason: Option<mermaid_model::models::FinishReason>,
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
        Some(mermaid_model::models::FinishReason::Length)
            | Some(mermaid_model::models::FinishReason::ContentFilter)
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
    let dry_truncation = tool_calls.is_empty()
        && matches!(
            stop_reason,
            Some(mermaid_model::models::FinishReason::Length)
        );
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
            Some(mermaid_model::models::FinishReason::Length) => {
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
                let reserve = state
                    .settings
                    .compaction
                    .policy()
                    .response_reserve(&build_chat_request(state));
                match crate::compaction::classify_length_stop(usage.as_ref(), window, reserve) {
                    crate::compaction::LengthCause::OutputCapped => {
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
                            < mermaid_model::constants::MAX_OUTPUT_CONTINUATIONS;
                        if !no_visible_output && !mid_reasoning && under_cap {
                            continuing = true;
                        } else {
                            let hint = output_cap_hint(state);
                            push_system(state, cmds, hint);
                        }
                    },
                    crate::compaction::LengthCause::ContextFull
                    | crate::compaction::LengthCause::Unknown => {
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
            Some(mermaid_model::models::FinishReason::ContentFilter) => push_system(
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
        let mut context = crate::state::ContextUsageSnapshot::from_usage(&u, max_context);
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

    cmds.push(state.session.save_conversation_cmd());

    // If the model asked for any tools, transition to ExecutingTools
    // and dispatch one ExecuteTool per call. The Vec<Option<ToolOutcome>>
    // invariant now has a real producer — ToolFinished messages
    // populate the slots, and try_complete_outcomes gates the
    // transition to the follow-up Generating turn.
    if !tool_calls.is_empty() {
        let pending: Vec<crate::state::PendingToolCall> = tool_calls
            .into_iter()
            .map(|source| crate::state::PendingToolCall {
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
        let intercepted: Vec<(mermaid_model::ids::ToolCallId, ToolOutcome)> = pending
            .iter()
            .filter(|call| call.source.function.name == crate::tool_search::TOOL_SEARCH_NAME)
            .map(|call| {
                let result =
                    crate::tool_search::run_tool_search(state, &call.source.function.arguments);
                crate::tool_search::apply_promotions(&mut state.mcp.promoted, result.promote);
                (
                    call.call_id,
                    ToolOutcome::success(result.text, result.summary, 0.0),
                )
            })
            .collect();
        // Captured once for the whole batch: the live safety mode + the
        // turn's intent (for the Auto-mode classifier).
        let intent = latest_user_intent(&state.session);
        // `SafetyMode::Plan` carries its own read-only floor in the policy
        // engine, so there is nothing to substitute here — the live mode is
        // the effective mode, always. The plan carve-outs (plan file, memory,
        // known-safe builds) key on `plan_file` inside the gate.
        let effective_safety = state.session.safety_mode;
        let plan_file = state
            .session
            .plan
            .as_ref()
            .map(|plan| plan.plan_path.clone());
        for call in &pending {
            if call.source.function.name == crate::tool_search::TOOL_SEARCH_NAME {
                continue;
            }
            cmds.push(Cmd::ExecuteTool {
                turn,
                call_id: call.call_id,
                source: call.source.clone(),
                dispatch: crate::cmd::ToolDispatch {
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
                    // Checkpoint anchoring: conversation id + length at
                    // DISPATCH. History here is [..., user@k,
                    // assistant(tool_use)], so any checkpoint this run takes
                    // has message_index >= k+1 and a fork at k discards it
                    // iff message_index > k (strict).
                    session_id: state.session.conversation.id.clone(),
                    message_index: state.session.messages().len(),
                    scratchpad: state.session.scratchpad.clone(),
                },
            });
        }
        state.turn = crate::transition::start_executing_tools(
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
                state.settings.compaction.policy(),
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
            mermaid_model::models::ChatMessageKind::RecoveryNudge,
        );
        let next_turn = state.ids.fresh_turn();
        state.turn = crate::transition::start_generating_with(
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
            mermaid_model::models::ChatMessageKind::RecoveryNudge,
        );
        let next_turn = state.ids.fresh_turn();
        // Propagate the continuation flag: an empty retry *inside* an
        // auto-continue chain must not strip the marker from the eventual
        // real continuation text.
        state.turn = crate::transition::start_generating_with(
            next_turn,
            std::time::SystemTime::from(state.now),
            continuation,
        );
        push_call_model(state, cmds, next_turn);
        return;
    }

    // The run is fully done (no tool calls, not recovering). Emit the
    // end-of-run summary where the spinner was.
    finish_run(state, cmds, RunEnd::Completed);

    // No tool calls — turn ends here. Drain the queued-message FIFO.
    drain_next_queued_message(state);
}

/// Drop the current run's summary counters without emitting a summary. For
/// the two paths that replace the conversation's identity out from under a
/// run (`/clear`, loading another conversation): the abandoned run's summary
/// belongs to a transcript that no longer exists in this session, so the
/// counters must not survive to stamp it into the new one.
pub fn reset_run_counters(state: &mut State) {
    state.runtime.run_started = None;
    state.runtime.run_tokens = Default::default();
    state.runtime.run_line_changes = Default::default();
}

/// How a run reached its end, for [`finish_run`]. Natural completion is the
/// only shape that retires a fully-completed checklist — an interrupted run
/// keeps its list so the next run carries the work forward — and an
/// interrupted summary says so, because "Worked for 2m" alone reads as a
/// finished job.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    Completed,
    /// Errored, cancelled, or the session is exiting mid-run.
    Interrupted,
}

/// End-of-run bookkeeping shared by EVERY way a run can end: the one-line
/// "Worked for … · used … tokens" summary, line-change totals, and (on
/// natural completion only) retirement of a fully-completed checklist.
///
/// Fires exactly once per run — `run_started` is taken here — and no-ops
/// for turns that never began with a user submit (compaction turns,
/// probes). Every terminal path calls this: normal completion, upstream
/// error, cancellation, and quit-mid-run. A saved log with no summary
/// previously meant "the run did not end normally" (observed in the field:
/// `20260704_155044` has none at all) — which is exactly when the duration
/// and spend matter most. It's a display-only line — `build_chat_request`
/// keeps it out of the model context.
pub fn finish_run(state: &mut State, cmds: &mut Vec<Cmd>, end: RunEnd) {
    let Some(started) = state.runtime.run_started.take() else {
        return;
    };
    let elapsed = std::time::SystemTime::from(state.now)
        .duration_since(started)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let run_tokens = state.runtime.run_tokens;
    let mut summary = format!(
        "Worked for {} · used {}{} tokens",
        crate::action_display::format_run_duration(elapsed),
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
    match end {
        RunEnd::Completed => {
            // Checklist retirement: a fully-completed checklist's lifetime is
            // the run's — the harness owns "the work is done" (models
            // demonstrably don't clean up after themselves, and a zombie list
            // re-renders on every later run and haunts saves). The summary
            // line absorbs the count, so retirement reads as completion, not
            // data loss. Fires only on natural completion — a cancelled or
            // errored run keeps its list, and lists with unfinished work
            // always carry over.
            let tasks = &state.session.conversation.tasks;
            if tasks.all_done() {
                let completed = tasks.visible().count();
                summary.push_str(&format!(
                    " · {completed} task{} completed",
                    if completed == 1 { "" } else { "s" }
                ));
                state.session.conversation.tasks = crate::ChecklistStore::default();
                cmds.push(Cmd::SyncTaskStore(crate::ChecklistStore::default()));
            }
        },
        RunEnd::Interrupted => summary.push_str(" · interrupted"),
    }
    state
        .session
        .append(ChatMessage::run_summary(summary), state.now);
    cmds.push(state.session.save_conversation_cmd());
}

/// Drain one message from the queued-message FIFO when a turn ends. The
/// follow-up is re-injected through `pending_msgs` so the outer `update()`
/// re-enters cleanly (preserving stale-filter semantics) rather than
/// inline-invoking a new turn. Shared by the stream-done, cancelled, and
/// upstream-error turn-end paths so a queued message is never stranded.
pub fn drain_next_queued_message(state: &mut State) {
    if let Some(next) = state.ui.queued_messages.pop_front() {
        state.ui.pending_msgs.push_back(Msg::SubmitPrompt {
            text: next.text,
            attachment_ids: next.attachment_ids,
        });
    }
}

pub fn handle_upstream_error(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    turn: TurnId,
    error: mermaid_model::models::UserFacingError,
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
        cmds.push(state.session.save_conversation_cmd());
    }
    let msg = ChatMessage {
        role: MessageRole::Assistant,
        content: String::new(),
        timestamp: now,
        kind: mermaid_model::models::ChatMessageKind::Normal,
        metadata: None,
        actions: vec![mermaid_model::action::ActionDisplay {
            action_type: "Error".to_string(),
            target: error.summary.clone(),
            result: mermaid_model::action::ActionResult::Error {
                error: error.message.clone(),
            },
            details: mermaid_model::action::ActionDetails::Simple,
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

    // The error ends the run: record how long it worked and what it spent —
    // an errored run's log is exactly where those numbers matter.
    finish_run(state, cmds, RunEnd::Interrupted);

    // A provider error ends the turn just like a normal completion — persist
    // the session too, so an errored headless run's emitted session id points
    // at a real file (`mermaid run --resume <id>` after a failed run).
    cmds.push(state.session.save_conversation_cmd());

    // Drain the queued-message FIFO so a message the user typed mid-turn isn't
    // stranded until their next manual submit (it would otherwise run out of
    // order).
    drain_next_queued_message(state);
}
