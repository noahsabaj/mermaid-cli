//! The model-call path: open the stream, drive it, fold usage, classify
//! errors on the way out.
use crate::providers::model::ModelProvider;
use mermaid_model::models::ModelError;
use mermaid_model::utils::{join_logged, spawn_guarded};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::*;

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub(super) async fn dispatch_call_model(
    msg_tx: MsgSender,
    providers: Option<Arc<ProviderFactory>>,
    turn: TurnId,
    mut request: mermaid_domain::ChatRequest,
    token: tokio_util::sync::CancellationToken,
    tasks: crate::providers::TaskBroker,
) {
    use mermaid_model::models::UserFacingError;

    let Some(factory) = providers else {
        let error = UserFacingError {
            summary: "not wired".to_string(),
            message: "EffectRunner has no ProviderFactory bound".to_string(),
            suggestion: "construct via EffectRunner::pair_with_bindings".to_string(),
            category: mermaid_model::models::ErrorCategory::Internal,
            recoverable: false,
        };
        let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
        return;
    };

    // Lazily resolve the provider for this model.
    let provider = match factory.resolve(&request.model_id).await {
        Ok(p) => p,
        Err(e) => {
            let error = classify_error_for_ui(&e);
            let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
            return;
        },
    };
    {
        // Telemetry write — offload the synchronous DB upserts to the blocking
        // pool so they never stall this model-call dispatch path, which runs on
        // every turn (#39).
        let model_id = request.model_id.clone();
        let caps = provider.capabilities().clone();
        // Own this telemetry write inside the per-turn task (await it) instead of
        // a detached `spawn_blocking` whose handle was dropped — so a panic in the
        // upsert surfaces and shutdown isn't racing an untracked DB write (#F41).
        // It is a few-ms SQLite upsert before a multi-second model call, so
        // awaiting it here does not meaningfully stall the turn (the "never stall
        // dispatch" rule is about the synchronous reducer path, not this task).
        if let Err(e) =
            tokio::task::spawn_blocking(move || record_provider_capabilities(&model_id, &caps))
                .await
        {
            tracing::error!(error = %e, "effect: provider-capability telemetry write failed");
        }
    }
    if !request.tools.is_empty() && !provider.capabilities().supports_tools {
        let _ = msg_tx
            .send(Msg::TransientStatus {
                text: format!(
                    "{} does not advertise tool support; Mermaid will send the turn without tools",
                    request.model_id
                ),
            })
            .await;
        request.tools.clear();
    }

    // Resolve the *effective* context window. For Ollama this probes the model's
    // real window and auto-fits num_ctx to memory (cache-first, off the UI
    // thread); for other providers it's the static advertised window. Using the
    // effective value here is what un-skips auto-compaction for Ollama (which had
    // `NoKnownContextLimit`) and gives the status bar real numbers.
    let sizing = provider.resolve_context_window(&request).await;
    let max_context_tokens = sizing.effective.or_else(|| {
        mermaid_domain::runtime::infer_static_context_window_for_model_id(&request.model_id)
    });
    // Ride the discovered limits on the request itself so adapters size
    // `max_tokens` against the model's REAL window/ceiling (Anthropic
    // requires a concrete max_tokens; sizing it from a stale table either
    // wastes the ceiling or 400s). Set before the auto-compaction block so
    // `CompactionRequest::auto` inherits them for its summary calls.
    request.resolved_context_window = sizing.effective.or(sizing.model_max);
    request.resolved_max_output = sizing.max_output;
    // Report the resolved window to the reducer for the `/context` display +
    // truncation quick-fix. Harmless for non-Ollama (source is None → no extra
    // detail shown).
    let _ = msg_tx
        .send(Msg::ProviderContextResolved {
            model_id: request.model_id.clone(),
            model_max: sizing.model_max,
            effective: sizing.effective,
            source: sizing.source,
            max_output: sizing.max_output,
        })
        .await;
    // No-vision-model fallback: if this turn actually carries images, probe the
    // model's vision capability and let the reducer warn if it can't see them.
    // This backs up the proactive paste-time probe for the rare case where the
    // user pasted and sent before that probe resolved. Cheap — `supports_vision`
    // is cache-first, so a repeat probe in the same session is free.
    if request
        .messages
        .iter()
        .any(|m| m.images.as_ref().is_some_and(|v| !v.is_empty()))
    {
        let supports_vision = provider.supports_vision().await;
        let _ = msg_tx
            .send(Msg::ProviderVisionResolved {
                model_id: request.model_id.clone(),
                supports_vision,
                warn: true,
            })
            .await;
    }
    let context_snapshot =
        mermaid_domain::estimate_context_usage_for_request(&request, max_context_tokens);
    let _ = msg_tx
        .send(Msg::ContextUsageEstimated {
            turn,
            snapshot: context_snapshot.clone(),
        })
        .await;

    // The live `[compaction]` policy, not the constants: auto-compaction is the
    // one path the user never invokes by hand, so it is the one that most needs
    // to honor their settings.
    let policy = factory.config().compaction.policy();
    let mut compacted_before_stream = false;
    if mermaid_domain::should_auto_compact(&context_snapshot, &request, policy).is_ok() {
        let compaction =
            CompactionRequest::auto(request.clone(), CompactionTrigger::AutoThreshold, policy);
        // Best-effort preflight: if there's nothing to compact, proceed
        // un-compacted (the provider's own context limit is the real gate).
        if let Ok(prepared) = mermaid_domain::prepare_compaction(&compaction, max_context_tokens) {
            match run_compaction(
                Arc::clone(&provider),
                turn,
                compaction,
                prepared,
                context_snapshot.clone(),
                max_context_tokens,
                token.clone(),
            )
            .await
            {
                Ok(result) => {
                    request.messages = result.replacement_messages.clone();
                    compacted_before_stream = true;
                    let _ = msg_tx.send(Msg::CompactionFinished { turn, result }).await;
                },
                Err(err) => {
                    // Auto-compaction is best-effort. If it can't reduce the
                    // context — the estimate is roughest exactly at the limit, so
                    // a large preserved tail can read `after >= before` — don't
                    // kill the turn. Log it, surface a soft warning, and proceed
                    // with the original request; the provider's own context limit
                    // is the real gate. (Manual `/compact` keeps its hard error
                    // via `run_compaction`'s reduction guard.)
                    if token.is_cancelled() {
                        return;
                    }
                    tracing::warn!(
                        turn = %turn,
                        error = %err,
                        "auto-compaction failed; proceeding with the un-compacted request",
                    );
                    let _ = msg_tx
                        .send(Msg::CompactionFailed {
                            turn,
                            trigger: CompactionTrigger::AutoThreshold,
                            message: err.to_string(),
                            kind: mermaid_domain::StatusKind::Warn,
                        })
                        .await;
                },
            }
        }
    }

    // Build a StreamContext — provider writes typed events into the
    // internal sink; we relay each to the reducer as a Msg.
    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
    let ctx = StreamContext::new(token.clone(), stream_tx, turn);

    // Drain stream events into Msgs on a sibling task. Ends when the sink
    // closes (provider's final `Done` or completion) OR the turn token is
    // cancelled — `select!`ing on the token ties this relay to the turn's
    // structured cancellation so a cancel drops it within a tick instead of
    // waiting on the next event. (A separate task is required: the relay must
    // run concurrently with `provider.chat` for streaming backpressure.)
    let relay_tx = msg_tx.clone();
    let relay_token = token.clone();
    let relay_tasks = tasks.clone();
    let relay = spawn_guarded(async move {
        loop {
            let event = tokio::select! {
                biased;
                _ = relay_token.cancelled() => {
                    // #F40: a cancel landing right after the provider finished must
                    // not discard the terminal Done it already enqueued. Drain the
                    // buffered events and relay only a terminal Done — so the
                    // just-completed turn's usage is still recorded — while NOT
                    // painting buffered intermediate text (the turn is cancelled).
                    // `try_recv` drains the buffer without awaiting more.
                    while let Ok(buffered) = stream_rx.try_recv() {
                        if let StreamEvent::Done {
                            usage,
                            provider_continuation,
                            stop_reason,
                        } = buffered
                        {
                            note_stream_usage(&relay_tasks, &usage);
                            let _ = relay_tx
                                .send(Msg::StreamDone {
                                    turn,
                                    usage,
                                    provider_continuation,
                                    stop_reason,
                                })
                                .await;
                        }
                    }
                    break;
                },
                ev = stream_rx.recv() => match ev {
                    Some(ev) => ev,
                    None => break,
                },
            };
            let msg = match event {
                StreamEvent::Text(chunk) => Msg::StreamText { turn, chunk },
                StreamEvent::Reasoning(chunk) => Msg::StreamReasoning { turn, chunk },
                StreamEvent::ToolCall(call) => Msg::StreamToolCall { turn, call },
                // Plumbing notice ("Starting the local Ollama server…") —
                // a turn-independent system line, not response content.
                StreamEvent::Status(text) => Msg::TransientStatus { text },
                StreamEvent::Done {
                    usage,
                    provider_continuation,
                    stop_reason,
                } => {
                    note_stream_usage(&relay_tasks, &usage);
                    Msg::StreamDone {
                        turn,
                        usage,
                        provider_continuation,
                        stop_reason,
                    }
                },
            };
            if relay_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Run the actual provider. On error, the relay will have
    // already emitted partial events; we follow with a single
    // UpstreamError to terminate the turn cleanly.
    //
    // `ModelError::Cancelled` is swallowed — the terminal
    // `Msg::TurnCancelled` is emitted from `drop_scope` after the
    // turn's `TurnScope` drains. Emitting `UpstreamError` here would
    // commit a "cancelled" message the user didn't ask to see.
    let mut completed_ok = false;
    match provider.chat(request.clone(), ctx).await {
        Ok(_final_response) => {
            // Success — the final `Done` flowed through the sink.
            completed_ok = true;
        },
        Err(mermaid_model::models::ModelError::Cancelled) => {
            // Silent: `drop_scope` will emit `Msg::TurnCancelled`.
        },
        Err(e) => {
            let retry_context_limit = !compacted_before_stream && is_context_limit_error(&e);
            if retry_context_limit {
                let latest_snapshot = mermaid_domain::estimate_context_usage_for_request(
                    &request,
                    max_context_tokens,
                );
                let compaction = CompactionRequest::auto(
                    request.clone(),
                    CompactionTrigger::ContextLimitRetry,
                    policy,
                );
                // Only retry if there's something to compact; otherwise fall
                // through to surface the original context-limit error.
                if let Ok(prepared) =
                    mermaid_domain::prepare_compaction(&compaction, max_context_tokens)
                {
                    match run_compaction(
                        Arc::clone(&provider),
                        turn,
                        compaction,
                        prepared,
                        latest_snapshot,
                        max_context_tokens,
                        token.clone(),
                    )
                    .await
                    {
                        Ok(result) => {
                            let mut retry_request = request;
                            retry_request.messages = result.replacement_messages.clone();
                            let _ = msg_tx.send(Msg::CompactionFinished { turn, result }).await;
                            join_logged(relay.take(), "stream_relay").await;
                            dispatch_provider_stream(
                                msg_tx,
                                provider,
                                turn,
                                retry_request,
                                token,
                                tasks,
                            )
                            .await;
                            return;
                        },
                        Err(compact_err) => {
                            let _ = msg_tx
                                .send(Msg::CompactionFailed {
                                    turn,
                                    trigger: CompactionTrigger::ContextLimitRetry,
                                    message: compact_err.to_string(),
                                    kind: mermaid_domain::StatusKind::Error,
                                })
                                .await;
                        },
                    }
                }
            }
            let error = classify_error_for_ui(&e);
            run_provider_error_hook(&request.model_id, &error).await;
            let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
        },
    }

    join_logged(relay.take(), "stream_relay").await;

    // Post-turn (success only): verify the model actually fit VRAM. Skipped when
    // the user allowed RAM offload (no warning possible) and a no-op for
    // non-Ollama providers (verify_placement returns None). Off the critical path
    // — StreamDone is already enqueued, so any warning renders after the answer.
    if completed_ok
        && request.ollama_allow_ram_offload != Some(true)
        && let Some(p) = provider.verify_placement(sizing.effective).await
    {
        tracing::debug!(
            size_vram_bytes = p.size_vram_bytes,
            total_bytes = p.total_bytes,
            offloaded = p.size_vram_bytes < p.total_bytes,
            suggested_num_ctx = ?p.suggested_num_ctx,
            "Ollama placement"
        );
        let _ = msg_tx
            .send(Msg::OllamaPlacementResolved {
                model_id: request.model_id.clone(),
                size_vram_bytes: p.size_vram_bytes,
                total_bytes: p.total_bytes,
                suggested_num_ctx: p.suggested_num_ctx,
            })
            .await;
    }
}

/// Drop-based per-turn model-call timer: emits a structured `tracing` event with
/// the elapsed wall time when the stream dispatch returns (success, error, or
/// cancel). Impure-shell only — lands in the log / TRACE bundle.
pub(super) struct TurnTimer {
    turn: TurnId,
    model_id: String,
    started: std::time::Instant,
}

impl Drop for TurnTimer {
    fn drop(&mut self) {
        tracing::debug!(
            turn = %self.turn,
            model = %self.model_id,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "model turn complete"
        );
    }
}

pub(super) async fn dispatch_provider_stream(
    msg_tx: MsgSender,
    provider: Arc<dyn ModelProvider>,
    turn: TurnId,
    request: mermaid_domain::ChatRequest,
    token: tokio_util::sync::CancellationToken,
    tasks: crate::providers::TaskBroker,
) {
    let _turn_timer = TurnTimer {
        turn,
        model_id: request.model_id.clone(),
        started: std::time::Instant::now(),
    };
    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
    let ctx = StreamContext::new(token.clone(), stream_tx, turn);
    let relay_tx = msg_tx.clone();
    let relay_token = token.clone();
    let relay_tasks = tasks.clone();
    let relay = spawn_guarded(async move {
        loop {
            let event = tokio::select! {
                biased;
                _ = relay_token.cancelled() => {
                    // #F40: a cancel landing right after the provider finished must
                    // not discard the terminal Done it already enqueued. Drain the
                    // buffered events and relay only a terminal Done — so the
                    // just-completed turn's usage is still recorded — while NOT
                    // painting buffered intermediate text (the turn is cancelled).
                    // `try_recv` drains the buffer without awaiting more.
                    while let Ok(buffered) = stream_rx.try_recv() {
                        if let StreamEvent::Done {
                            usage,
                            provider_continuation,
                            stop_reason,
                        } = buffered
                        {
                            note_stream_usage(&relay_tasks, &usage);
                            let _ = relay_tx
                                .send(Msg::StreamDone {
                                    turn,
                                    usage,
                                    provider_continuation,
                                    stop_reason,
                                })
                                .await;
                        }
                    }
                    break;
                },
                ev = stream_rx.recv() => match ev {
                    Some(ev) => ev,
                    None => break,
                },
            };
            let msg = match event {
                StreamEvent::Text(chunk) => Msg::StreamText { turn, chunk },
                StreamEvent::Reasoning(chunk) => Msg::StreamReasoning { turn, chunk },
                StreamEvent::ToolCall(call) => Msg::StreamToolCall { turn, call },
                // Plumbing notice — turn-independent system line.
                StreamEvent::Status(text) => Msg::TransientStatus { text },
                StreamEvent::Done {
                    usage,
                    provider_continuation,
                    stop_reason,
                } => {
                    note_stream_usage(&relay_tasks, &usage);
                    Msg::StreamDone {
                        turn,
                        usage,
                        provider_continuation,
                        stop_reason,
                    }
                },
            };
            if relay_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let model_id = request.model_id.clone();
    match provider.chat(request, ctx).await {
        Ok(_) | Err(ModelError::Cancelled) => {},
        Err(e) => {
            let error = classify_error_for_ui(&e);
            run_provider_error_hook(&model_id, &error).await;
            let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
        },
    }

    join_logged(relay.take(), "stream_relay").await;
}

/// Run plugin hooks OFF the async executor. `run_plugin_hooks` is synchronous —
/// it spawns hook children and bounded-waits on them — so calling it inline
/// would block a tokio worker, or (on the `dispatch` path) the whole event loop.
/// `spawn_blocking` moves it to the blocking pool. Hooks are fire-and-forget
/// observers, so the result is dropped.
pub(super) async fn fire_plugin_hooks(event: &'static str, payload: serde_json::Value) {
    let _ = tokio::task::spawn_blocking(move || mermaid_runtime::run_plugin_hooks(event, &payload))
        .await;
}

/// Run hooks for an event whose responses GATE the action, returning the
/// aggregated verdict. Infrastructure failures (store/spawn errors, a panicked
/// blocking task) yield an empty gate — fail open; explicit hook denials
/// always deny.
pub(super) async fn run_plugin_hooks_gated(
    event: &'static str,
    payload: serde_json::Value,
) -> mermaid_runtime::HookGate {
    tokio::task::spawn_blocking(move || {
        mermaid_runtime::run_plugin_hooks(event, &payload)
            .map(mermaid_runtime::aggregate_hook_responses)
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

pub(super) async fn run_provider_error_hook(
    model_id: &str,
    error: &mermaid_model::models::UserFacingError,
) {
    fire_plugin_hooks(
        "provider_error",
        serde_json::json!({
            "model_id": model_id,
            "summary": &error.summary,
            "message": &error.message,
            "category": format!("{:?}", error.category),
            "recoverable": error.recoverable,
        }),
    )
    .await;
}
