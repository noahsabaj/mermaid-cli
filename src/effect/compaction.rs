//! Conversation compaction: prepare the request, fit it in the window, fold
//! the result back.
use crate::providers::model::ModelProvider;
use mermaid_model::models::{ModelError, TokenUsage};
use std::sync::Arc;
use std::time::Instant;

use super::*;

pub(super) async fn dispatch_compact_conversation(
    msg_tx: MsgSender,
    providers: Option<Arc<ProviderFactory>>,
    turn: TurnId,
    mut request: CompactionRequest,
    token: tokio_util::sync::CancellationToken,
) {
    let Some(factory) = providers else {
        let _ = msg_tx
            .send(Msg::CompactionFailed {
                turn,
                trigger: request.trigger,
                message: "EffectRunner has no ProviderFactory bound".to_string(),
                kind: mermaid_domain::StatusKind::Error,
            })
            .await;
        return;
    };

    let provider = match factory.resolve(&request.chat.model_id).await {
        Ok(provider) => provider,
        Err(err) => {
            let _ = msg_tx
                .send(Msg::CompactionFailed {
                    turn,
                    trigger: request.trigger,
                    message: err.to_string(),
                    kind: mermaid_domain::StatusKind::Error,
                })
                .await;
            return;
        },
    };

    // Resolve the window live (cache-first, so a manual /compact right after
    // a turn is a pure cache read). Static capabilities are `None` for
    // providers that discover limits at turn time (Anthropic/Gemini) — using
    // them here would regress manual /compact to "unknown window".
    let sizing = provider.resolve_context_window(&request.chat).await;
    request.chat.resolved_context_window = sizing.effective.or(sizing.model_max);
    request.chat.resolved_max_output = sizing.max_output;
    let max_context_tokens = request.chat.resolved_context_window.or_else(|| {
        mermaid_domain::runtime::infer_static_context_window_for_model_id(&request.chat.model_id)
    });
    let before_snapshot =
        mermaid_domain::estimate_context_usage_for_request(&request.chat, max_context_tokens);

    let trigger = request.trigger;
    // A benign precondition (e.g. too little history to summarize) is a no-op, not
    // a failure — surface it as `Info` so the reducer shows a calm note instead of
    // a "Compaction failed: Invalid request" error. Real failures (model errors,
    // an empty/non-reducing summary) still flow through `run_compaction` as errors.
    let prepared = match mermaid_domain::prepare_compaction(&request, max_context_tokens) {
        Ok(prepared) => prepared,
        Err(skip) => {
            let _ = msg_tx
                .send(Msg::CompactionFailed {
                    turn,
                    trigger,
                    message: skip.to_string(),
                    kind: mermaid_domain::StatusKind::Info,
                })
                .await;
            return;
        },
    };
    match run_compaction(
        provider,
        turn,
        request,
        prepared,
        before_snapshot,
        max_context_tokens,
        token,
    )
    .await
    {
        Ok(result) => {
            let _ = msg_tx.send(Msg::CompactionFinished { turn, result }).await;
        },
        Err(err) => {
            let _ = msg_tx
                .send(Msg::CompactionFailed {
                    turn,
                    trigger,
                    message: err.to_string(),
                    kind: mermaid_domain::StatusKind::Error,
                })
                .await;
        },
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub(super) async fn run_compaction(
    provider: Arc<dyn ModelProvider>,
    turn: TurnId,
    request: CompactionRequest,
    prepared: mermaid_domain::PreparedCompaction,
    before_snapshot: mermaid_domain::ContextUsageSnapshot,
    max_context_tokens: Option<usize>,
    token: tokio_util::sync::CancellationToken,
) -> Result<CompactionResult, ModelError> {
    let started = Instant::now();

    let summary_request = mermaid_domain::build_summary_request(
        &request.chat,
        &prepared,
        request.instructions.as_deref(),
        request.policy,
        max_context_tokens,
    );
    ensure_compaction_request_fits(&summary_request, max_context_tokens)?;
    let (draft, draft_usage) =
        collect_compaction_text(Arc::clone(&provider), turn, summary_request, token.clone())
            .await?;
    let draft_summary = mermaid_domain::normalize_summary(&draft);
    let draft_validation = mermaid_domain::validate_summary_structure(&draft_summary);

    let verify_request = mermaid_domain::build_verification_request(
        &request.chat,
        &prepared,
        &draft_summary,
        request.instructions.as_deref(),
        request.policy,
        max_context_tokens,
    );
    let review_fits = compaction_request_fits(&verify_request, max_context_tokens);
    let (final_summary, verify_usage, review_status, review_error) = if review_fits {
        match collect_compaction_text(Arc::clone(&provider), turn, verify_request, token).await {
            Ok((verified_text, verify_usage)) => {
                let verified_summary = mermaid_domain::normalize_summary(&verified_text);
                match mermaid_domain::validate_summary_structure(&verified_summary) {
                    Ok(()) => (
                        verified_summary,
                        verify_usage,
                        mermaid_domain::CompactionReviewStatus::Reviewed,
                        None,
                    ),
                    Err(error) => match draft_validation {
                        Ok(()) => (
                            draft_summary,
                            verify_usage,
                            mermaid_domain::CompactionReviewStatus::DraftValidated,
                            Some(format!("review returned an invalid checkpoint: {error}")),
                        ),
                        Err(draft_error) => {
                            return Err(ModelError::InvalidRequest(format!(
                                "compaction produced no structurally valid checkpoint (draft: {draft_error}; review: {error})"
                            )));
                        },
                    },
                }
            },
            Err(ModelError::Cancelled) => return Err(ModelError::Cancelled),
            Err(err) => match draft_validation {
                Ok(()) => (
                    draft_summary,
                    None,
                    mermaid_domain::CompactionReviewStatus::DraftValidated,
                    Some(format!("review failed: {err}")),
                ),
                Err(draft_error) => {
                    return Err(ModelError::InvalidRequest(format!(
                        "compaction draft was invalid and review failed (draft: {draft_error}; review: {err})"
                    )));
                },
            },
        }
    } else {
        match draft_validation {
            Ok(()) => (
                draft_summary,
                None,
                mermaid_domain::CompactionReviewStatus::DraftValidated,
                Some(
                    "review skipped because the complete request would exceed the context window"
                        .to_string(),
                ),
            ),
            Err(error) => {
                return Err(ModelError::InvalidRequest(format!(
                    "compaction draft was invalid and the review request did not fit: {error}"
                )));
            },
        }
    };

    let id = format!(
        "compact_{}",
        chrono::Local::now().format("%Y%m%d_%H%M%S_%3f")
    );
    let mut record = mermaid_domain::CompactionEvent {
        id,
        trigger: request.trigger,
        created_at: chrono::Local::now(),
        before_tokens: before_snapshot.used_tokens,
        after_tokens: 0,
        archived_message_count: prepared.archived_messages.len(),
        preserved_message_count: prepared.preserved_messages.len(),
        preserved_turn_count: prepared
            .preserved_messages
            .iter()
            .filter(|message| message.role == mermaid_model::models::MessageRole::User)
            .count(),
        summary_tokens: final_summary.len().div_ceil(4),
        duration_secs: started.elapsed().as_secs_f64(),
        review_status,
        review_error,
        focus: request.instructions.clone(),
        archive_path: None,
    };

    record.duration_secs = started.elapsed().as_secs_f64();
    // `after_tokens` is self-referential: it counts a replacement whose receipt
    // text prints `after_tokens`. Iterate to a fixpoint, and keep the
    // replacement built FROM the record being reported — the previous code ran
    // exactly two passes and kept the pair from different iterations, so the
    // receipt in the transcript and the number in `/context` disagreed.
    //
    // Convergence is fast because the receipt renders the count abbreviated
    // (`43.8k`), so its length only moves when the abbreviation does. The cap
    // is a guard against a pathological oscillation, not an expected path.
    const AFTER_TOKENS_PASSES: usize = 4;
    let mut compacted_request = request.chat.clone();
    let mut replacement =
        mermaid_domain::build_replacement_messages(&final_summary, &prepared, &record);
    for _ in 0..AFTER_TOKENS_PASSES {
        compacted_request.messages = replacement.clone();
        let measured = mermaid_domain::estimate_context_usage_for_request(
            &compacted_request,
            max_context_tokens,
        );
        if measured.used_tokens == record.after_tokens {
            break;
        }
        record.after_tokens = measured.used_tokens;
        replacement =
            mermaid_domain::build_replacement_messages(&final_summary, &prepared, &record);
    }
    // Whatever happened above, `replacement` was built from `record` — so the
    // snapshot is measured on the messages actually kept, and the receipt text
    // quotes the same `after_tokens` the record carries.
    compacted_request.messages = replacement.clone();
    let after_snapshot =
        mermaid_domain::estimate_context_usage_for_request(&compacted_request, max_context_tokens);

    if after_snapshot.used_tokens >= before_snapshot.used_tokens {
        return Err(ModelError::InvalidRequest(format!(
            "compaction did not reduce context ({} -> {} tokens)",
            before_snapshot.used_tokens, after_snapshot.used_tokens
        )));
    }

    if mermaid_domain::context_exceeds_hard_limit(
        &after_snapshot,
        &compacted_request,
        request.policy,
    ) {
        return Err(ModelError::InvalidRequest(format!(
            "compacted context still exceeds response reserve ({} tokens used)",
            after_snapshot.used_tokens
        )));
    }

    Ok(CompactionResult {
        record,
        replacement_messages: replacement,
        archived_messages: prepared.archived_messages,
        before_snapshot,
        after_snapshot,
        usage: mermaid_domain::combine_usage(draft_usage, verify_usage),
        source_boundaries: request
            .chat
            .messages
            .iter()
            .map(mermaid_domain::CompactionBoundary::from_message)
            .collect(),
    })
}

pub(super) fn compaction_request_fits(
    request: &mermaid_domain::ChatRequest,
    max_context_tokens: Option<usize>,
) -> bool {
    let Some(max_tokens) = max_context_tokens else {
        return true;
    };
    let used = mermaid_domain::estimate_context_usage_for_request(request, Some(max_tokens));
    used.used_tokens.saturating_add(request.max_tokens) <= max_tokens
}

pub(super) fn ensure_compaction_request_fits(
    request: &mermaid_domain::ChatRequest,
    max_context_tokens: Option<usize>,
) -> Result<(), ModelError> {
    if compaction_request_fits(request, max_context_tokens) {
        Ok(())
    } else {
        Err(ModelError::InvalidRequest(
            "complete compaction request exceeds the model context window".to_string(),
        ))
    }
}

pub(super) async fn collect_compaction_text(
    provider: Arc<dyn ModelProvider>,
    turn: TurnId,
    request: mermaid_domain::ChatRequest,
    token: tokio_util::sync::CancellationToken,
) -> Result<(String, Option<TokenUsage>), ModelError> {
    // Shared with the Auto-mode safety classifier — see
    // `crate::providers::model::collect_text`.
    let collected = crate::providers::model::collect_text(provider, turn, request, token).await?;
    Ok((collected.text, collected.usage))
}

pub(super) fn record_provider_capabilities(
    model_id: &str,
    caps: &mermaid_model::models::ModelCapabilities,
) {
    let (provider, model) = split_model_id(model_id);
    let _ = mermaid_runtime::with_shared_store(|store| {
        for (key, value) in [
            ("tools_support", caps.supports_tools.to_string()),
            ("vision_support", caps.supports_vision.to_string()),
            (
                "context_limit",
                caps.max_context_tokens
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
            (
                "reasoning_parameter_shape",
                format!("{:?}", caps.supports_reasoning),
            ),
            (
                "streaming_usage_available",
                "provider_dependent".to_string(),
            ),
            ("token_usage_field_shape", "normalized".to_string()),
        ] {
            let _ = store
                .provider_probes()
                .upsert(mermaid_runtime::NewProviderProbe {
                    provider: provider.clone(),
                    model_id: model.clone(),
                    capability_key: key.to_string(),
                    capability_value: value,
                    confidence: "verified".to_string(),
                    error: None,
                });
        }
        Ok(())
    });
}

pub(super) fn split_model_id(model_id: &str) -> (String, String) {
    match model_id.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (provider.to_ascii_lowercase(), model.to_string())
        },
        _ => ("ollama".to_string(), model_id.to_string()),
    }
}

/// Hard cap on paths returned by [`walk_project_files`]. Well past any
/// project the picker is useful on; keeps a runaway monorepo walk bounded.
pub(super) const MAX_PROJECT_FILES: usize = 20_000;

/// Enumerate the project for the @-mention picker: gitignore-aware
/// (ripgrep's walker — .gitignore/.ignore/global excludes), hidden entries
/// and `.git` skipped, symlinks not followed. Returns RELATIVE UTF-8 paths
/// sorted lexicographically, directories with a trailing `/`, capped at
/// [`MAX_PROJECT_FILES`]. Non-UTF-8 paths are skipped — the mention is
/// spliced into the text prompt, so it must be valid text.
pub(super) fn walk_project_files(root: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .build()
        .flatten()
    {
        if files.len() >= MAX_PROJECT_FILES {
            break;
        }
        let path = entry.path();
        if path == root {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let Some(mut rel) = rel.to_str().map(str::to_string) else {
            continue;
        };
        // Normalize Windows separators so a mention is stable text.
        if std::path::MAIN_SEPARATOR != '/' {
            rel = rel.replace(std::path::MAIN_SEPARATOR, "/");
        }
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            rel.push('/');
        }
        files.push(rel);
    }
    files.sort();
    files
}

pub(super) fn is_context_limit_error(error: &ModelError) -> bool {
    let text = error.to_string().to_lowercase();
    text.contains("context")
        && (text.contains("too large")
            || text.contains("exceed")
            || text.contains("maximum")
            || text.contains("token"))
}
