//! Conversation context compaction.
//!
//! The reducer/effect boundary treats compaction as a first-class
//! operation: effects generate a checkpoint summary, the reducer swaps
//! the model-visible history, and persistence archives the removed raw
//! messages. This keeps compaction observable instead of hiding it inside
//! a provider adapter.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use mermaid_model::constants::{
    COMPACTION_AUTO_THRESHOLD_PERCENT, COMPACTION_MAX_RESPONSE_RESERVE_TOKENS,
    COMPACTION_MIN_RESPONSE_RESERVE_TOKENS, COMPACTION_SUMMARIZER_INPUT_TOKEN_BUDGET,
    COMPACTION_SUMMARY_MAX_TOKENS, COMPACTION_TAIL_TOKEN_BUDGET, COMPACTION_TAIL_TURNS,
    COMPACTION_TOOL_OUTPUT_MAX_CHARS,
};
use mermaid_model::models::{
    ChatMessage, ChatMessageKind, MessageRole, ReasoningLevel, TokenUsage,
};

use super::cmd::ChatRequest;
use super::state::ContextUsageSnapshot;

const CHECKPOINT_MARKER: &str = "MERMAID CONTEXT CHECKPOINT";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    AutoThreshold,
    ContextLimitRetry,
    /// A response was truncated because the context window filled mid-turn;
    /// compact and resume the run (see the reducer's truncation-recovery path).
    TruncationRecovery,
}

impl CompactionTrigger {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::AutoThreshold => "auto_threshold",
            Self::ContextLimitRetry => "context_limit_retry",
            Self::TruncationRecovery => "truncation_recovery",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::AutoThreshold => "automatic",
            Self::ContextLimitRetry => "context-limit retry",
            Self::TruncationRecovery => "truncation recovery",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionPolicy {
    pub auto_enabled: bool,
    pub auto_threshold_percent: u8,
    pub tail_turns: usize,
    pub tail_token_budget: usize,
    pub tool_output_max_chars: usize,
    pub summary_max_tokens: usize,
    pub summarizer_input_token_budget: usize,
    pub min_response_reserve_tokens: usize,
    pub max_response_reserve_tokens: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            auto_enabled: true,
            auto_threshold_percent: COMPACTION_AUTO_THRESHOLD_PERCENT,
            tail_turns: COMPACTION_TAIL_TURNS,
            tail_token_budget: COMPACTION_TAIL_TOKEN_BUDGET,
            tool_output_max_chars: COMPACTION_TOOL_OUTPUT_MAX_CHARS,
            summary_max_tokens: COMPACTION_SUMMARY_MAX_TOKENS,
            summarizer_input_token_budget: COMPACTION_SUMMARIZER_INPUT_TOKEN_BUDGET,
            min_response_reserve_tokens: COMPACTION_MIN_RESPONSE_RESERVE_TOKENS,
            max_response_reserve_tokens: COMPACTION_MAX_RESPONSE_RESERVE_TOKENS,
        }
    }
}

/// Share of a known context window the summarizer may spend on its own
/// output. The rest is the input budget (scaffold + history excerpt).
///
/// A fixed output cap does not survive small windows: at a 4k window the flat
/// 8k cap asked for twice the entire window, so compaction could never fit and
/// every attempt failed. Scaling keeps the same behavior on large windows —
/// `summary_max_tokens` is still the ceiling — while making small ones work.
const SUMMARY_OUTPUT_WINDOW_SHARE: usize = 4;

/// Floor for the scaled output cap. Below this a checkpoint cannot carry the
/// ten required sections, so the window is genuinely too small to compact and
/// [`prepare_compaction`] skips instead of emitting a request that must fail.
const SUMMARY_OUTPUT_FLOOR_TOKENS: usize = 512;

impl CompactionPolicy {
    /// The summarizer's output cap for a model whose window is `window`.
    ///
    /// Unknown window keeps the flat `summary_max_tokens` (the remote-provider
    /// case — nothing better to go on). A known window caps the summary at
    /// `1/SUMMARY_OUTPUT_WINDOW_SHARE` of it, so the input budget always has
    /// room left; large windows are unaffected because `summary_max_tokens`
    /// stays the ceiling.
    #[must_use]
    pub fn summary_output_tokens(self, window: Option<usize>) -> usize {
        match window {
            None => self.summary_max_tokens,
            Some(window) => self
                .summary_max_tokens
                .min(window / SUMMARY_OUTPUT_WINDOW_SHARE),
        }
    }

    /// Total input budget (fixed scaffold + history excerpt) for the
    /// summarizer, given the model's window.
    ///
    /// Monotonic in the window, which the previous computation was NOT: it
    /// subtracted a flat output cap and then treated a non-positive result as
    /// "window unknown", falling back to the *most permissive* budget. An 8k
    /// model therefore got a 64k input budget while a 9k model got 1k —
    /// shrinking the window made the request bigger, and 8k-and-below could
    /// never compact.
    #[must_use]
    pub fn summary_input_budget(self, window: Option<usize>) -> usize {
        match window {
            None => self.summarizer_input_token_budget,
            Some(size) => size
                .saturating_sub(self.summary_output_tokens(window))
                .min(self.summarizer_input_token_budget),
        }
    }

    /// Window room to hold back for the model's response when sizing
    /// compaction. Decoupled from the on-wire output cap: an explicit user cap
    /// is the best reserve estimate, but AUTO (`max_tokens == 0`) reserves the
    /// baseline plus the reasoning headroom the level implies — a High/Max
    /// turn needs more room before the window counts as "full".
    #[must_use]
    pub fn response_reserve(self, request: &ChatRequest) -> usize {
        let desired = if request.max_tokens > 0 {
            request.max_tokens
        } else {
            self.min_response_reserve_tokens
                + mermaid_model::models::adapters::output_budget::reasoning_output_reserve(
                    request.reasoning,
                )
        };
        desired
            .max(self.min_response_reserve_tokens)
            .min(self.max_response_reserve_tokens)
    }
}

/// Why a `FinishReason::Length` stop happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LengthCause {
    /// The per-response output cap was hit while the context window still had
    /// room — compacting the *input* cannot help.
    OutputCapped,
    /// The context window itself is (nearly) full — compaction can help.
    ContextFull,
    /// No usage data to classify with; callers should keep the legacy
    /// compact-and-continue behavior.
    Unknown,
}

/// Classify a `FinishReason::Length` stop from the response usage and the
/// known context window. The discriminator that holds even for providers whose
/// window is unknown: a length-stop with window room to spare (or no known
/// window at all — the normal remote-provider case) is the per-response output
/// cap, not a full window. `usage == None` (common on tool follow-ups) stays
/// `Unknown` so the caller preserves the legacy recovery path.
#[must_use]
pub fn classify_length_stop(
    usage: Option<&TokenUsage>,
    window: Option<usize>,
    reserve: usize,
) -> LengthCause {
    let Some(u) = usage else {
        return LengthCause::Unknown;
    };
    match window {
        None => LengthCause::OutputCapped,
        Some(w) => {
            if u.total_tokens().saturating_add(reserve) >= w {
                LengthCause::ContextFull
            } else {
                LengthCause::OutputCapped
            }
        },
    }
}

#[derive(Debug, Clone)]
pub struct CompactionRequest {
    pub chat: ChatRequest,
    pub trigger: CompactionTrigger,
    pub instructions: Option<String>,
    pub policy: CompactionPolicy,
}

impl CompactionRequest {
    // Both constructors take the policy explicitly rather than defaulting it.
    // `[compaction]` config exists precisely so these knobs can differ from the
    // constants, and a constructor that quietly substituted defaults would make
    // whether the user's settings applied depend on which call site ran.
    #[must_use]
    pub fn manual(
        chat: ChatRequest,
        instructions: Option<String>,
        policy: CompactionPolicy,
    ) -> Self {
        Self {
            chat,
            trigger: CompactionTrigger::Manual,
            instructions,
            policy,
        }
    }

    #[must_use]
    pub fn auto(chat: ChatRequest, trigger: CompactionTrigger, policy: CompactionPolicy) -> Self {
        Self {
            chat,
            trigger,
            instructions: None,
            policy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReviewStatus {
    Reviewed,
    DraftValidated,
}

impl CompactionReviewStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reviewed => "reviewed",
            Self::DraftValidated => "draft_validated",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionEvent {
    pub id: String,
    pub trigger: CompactionTrigger,
    pub created_at: DateTime<Local>,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub archived_message_count: usize,
    pub preserved_message_count: usize,
    pub preserved_turn_count: usize,
    pub summary_tokens: usize,
    pub duration_secs: f64,
    pub review_status: CompactionReviewStatus,
    pub review_error: Option<String>,
    #[serde(default)]
    pub focus: Option<String>,
    #[serde(default)]
    pub archive_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionArchive {
    pub id: String,
    pub conversation_id: String,
    pub created_at: DateTime<Local>,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompactionResult {
    pub record: CompactionEvent,
    pub replacement_messages: Vec<ChatMessage>,
    pub archived_messages: Vec<ChatMessage>,
    pub before_snapshot: ContextUsageSnapshot,
    pub after_snapshot: ContextUsageSnapshot,
    pub usage: Option<TokenUsage>,
    pub source_boundaries: Vec<CompactionBoundary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionBoundary {
    /// sha256 hex over (role Debug, kind Debug, UTC RFC3339-nanos timestamp,
    /// content), NUL-separated, content last. A fingerprint instead of a full
    /// message clone: `source_boundaries` rides `CompactionFinished` into the
    /// session recording, and cloning the entire pre-compaction transcript
    /// would double both memory and recording size. UTC-normalized because
    /// `DateTime<Local>` re-localizes on deserialize — a recording replayed
    /// under a different TZ must still match on the instant.
    pub fingerprint: String,
}

impl CompactionBoundary {
    #[must_use]
    pub fn from_message(message: &ChatMessage) -> Self {
        Self {
            fingerprint: Self::fingerprint_of(message),
        }
    }

    #[must_use]
    pub fn fingerprint_of(message: &ChatMessage) -> String {
        use sha2::{Digest, Sha256};
        use std::fmt::Write as _;
        let mut hasher = Sha256::new();
        hasher.update(format!("{:?}", message.role).as_bytes());
        hasher.update([0u8]);
        hasher.update(format!("{:?}", message.kind).as_bytes());
        hasher.update([0u8]);
        hasher.update(
            message
                .timestamp
                .to_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                .as_bytes(),
        );
        hasher.update([0u8]);
        hasher.update(message.content.as_bytes());
        let digest = hasher.finalize();
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    #[must_use]
    pub fn matches(&self, message: &ChatMessage) -> bool {
        Self::fingerprint_of(message) == self.fingerprint
    }
}

#[derive(Debug, Clone)]
pub struct PreparedCompaction {
    pub archived_messages: Vec<ChatMessage>,
    pub preserved_messages: Vec<ChatMessage>,
    pub previous_summary: Option<String>,
    pub history_excerpt: String,
    pub summary_images: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionSkip {
    NoKnownContextLimit,
    AutoDisabled,
    Suppressed,
    BelowThreshold,
    NothingToCompact,
    /// The window cannot hold a checkpoint at all: even an empty history
    /// excerpt leaves no room for the fixed prompt scaffold plus a usable
    /// summary. Skipping here turns what used to be a guaranteed
    /// "request exceeds the model context window" failure into a plain,
    /// honest note.
    WindowTooSmall,
}

impl std::fmt::Display for CompactionSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKnownContextLimit => write!(f, "model context limit is unknown"),
            Self::AutoDisabled => write!(f, "automatic compaction is disabled"),
            Self::Suppressed => write!(
                f,
                "automatic compaction is paused after a failed attempt; run /compact to retry"
            ),
            Self::BelowThreshold => write!(f, "context is below compaction threshold"),
            Self::NothingToCompact => write!(f, "not enough conversation history to summarize"),
            Self::WindowTooSmall => write!(
                f,
                "the model's context window is too small to hold a checkpoint"
            ),
        }
    }
}

/// # Errors
///
/// The `Err` is a [`CompactionSkip`] reason, not a failure: it says why this
/// turn does not need compacting. `AutoDisabled` and `Suppressed` come from
/// policy and the request; `NoKnownContextLimit` means the snapshot carries no
/// usable `max_tokens`, so there is no threshold to be over; `BelowThreshold`
/// means usage is under both the percentage trigger and the response reserve.
pub fn should_auto_compact(
    snapshot: &ContextUsageSnapshot,
    request: &ChatRequest,
    policy: CompactionPolicy,
) -> Result<(), CompactionSkip> {
    if !policy.auto_enabled {
        return Err(CompactionSkip::AutoDisabled);
    }
    if request.suppress_auto_compact {
        return Err(CompactionSkip::Suppressed);
    }
    let Some(max_tokens) = snapshot.max_tokens else {
        return Err(CompactionSkip::NoKnownContextLimit);
    };
    if max_tokens == 0 {
        return Err(CompactionSkip::NoKnownContextLimit);
    }

    let reserve = policy.response_reserve(request);
    let over_percent = snapshot
        .used_percent
        .is_some_and(|p| p >= policy.auto_threshold_percent);
    let low_remaining = snapshot
        .remaining_tokens
        .is_some_and(|remaining| remaining <= reserve);
    if over_percent || low_remaining {
        Ok(())
    } else {
        Err(CompactionSkip::BelowThreshold)
    }
}

#[must_use]
pub fn context_exceeds_hard_limit(
    snapshot: &ContextUsageSnapshot,
    request: &ChatRequest,
    policy: CompactionPolicy,
) -> bool {
    let Some(max_tokens) = snapshot.max_tokens else {
        return false;
    };
    let reserve = policy.response_reserve(request);
    snapshot.used_tokens.saturating_add(reserve) >= max_tokens
}

/// # Errors
///
/// The `Err` is a [`CompactionSkip`] reason, not a failure. `NothingToCompact`
/// when the history is too short to split, or the split would leave either
/// half empty. `WindowTooSmall` when the window cannot fund a summary at the
/// output floor — checkpointing into it would produce a request the model
/// cannot answer.
pub fn prepare_compaction(
    request: &CompactionRequest,
    max_context_tokens: Option<usize>,
) -> Result<PreparedCompaction, CompactionSkip> {
    let messages = &request.chat.messages;
    if messages.len() < 3 {
        return Err(CompactionSkip::NothingToCompact);
    }

    let split =
        tail_start_index(messages, request.policy).ok_or(CompactionSkip::NothingToCompact)?;
    if split == 0 {
        return Err(CompactionSkip::NothingToCompact);
    }

    let archived_messages = messages[..split].to_vec();
    let mut preserved_messages = messages[split..].to_vec();
    if archived_messages.is_empty() || preserved_messages.is_empty() {
        return Err(CompactionSkip::NothingToCompact);
    }
    // The tail is forwarded verbatim into the next request; scrub any
    // pre-existing orphan `tool_use`/`tool_result` so an unpaired block can't
    // 400 the provider (#71 forward, #F64 reverse). One exception: when the run
    // is resuming mid-tool (a context-limit retry or truncation recovery), a
    // genuinely-pending trailing `tool_use` is preserved so the model needn't
    // re-derive the action from the summary (#F65). A user cancel produces the
    // same trailing shape but ends the run, so only the resume triggers — where
    // the awaited `tool_result` really is forthcoming — opt into preserving it.
    let preserve_pending_tail = matches!(
        request.trigger,
        CompactionTrigger::ContextLimitRetry | CompactionTrigger::TruncationRecovery
    );
    drop_orphan_tool_calls(&mut preserved_messages, preserve_pending_tail);

    let previous_summary = archived_messages
        .iter()
        .rev()
        .find(|m| {
            m.kind == ChatMessageKind::ContextCheckpoint || m.content.contains(CHECKPOINT_MARKER)
        })
        .map(|m| m.content.clone());

    // Size the complete summarizer input against its own output cap, not the
    // interrupted turn's response reserve. Account for the fixed prompt,
    // previous checkpoint, focus, and attached image payloads before assigning
    // the remainder to history.
    //
    // Both halves scale with the window (see `summary_output_tokens` /
    // `summary_input_budget`) so the budget is monotonic: a smaller window can
    // only ever produce a smaller request.
    if request
        .policy
        .summary_output_tokens(max_context_tokens)
        .lt(&SUMMARY_OUTPUT_FLOOR_TOKENS)
    {
        return Err(CompactionSkip::WindowTooSmall);
    }
    let max_input_tokens = request.policy.summary_input_budget(max_context_tokens);
    let sizing_prepared = PreparedCompaction {
        archived_messages: Vec::new(),
        preserved_messages: Vec::new(),
        previous_summary: previous_summary.clone(),
        history_excerpt: String::new(),
        summary_images: Vec::new(),
    };
    // Measure the fixed request scaffold (system prompt, template, previous
    // checkpoint, focus, role metadata) with the SAME estimator the dispatch
    // fit check uses, so the two can never diverge. The per-image and excerpt
    // allocations below each round up at least as aggressively as the
    // estimator's summed rounding, so the assembled request stays within
    // `max_input_tokens` by construction.
    let probe = build_summary_request(
        &request.chat,
        &sizing_prepared,
        request.instructions.as_deref(),
        request.policy,
        max_context_tokens,
    );
    let fixed_tokens = super::state::estimate_context_usage_for_request(&probe, None).used_tokens;
    let mut remaining_tokens = max_input_tokens.saturating_sub(fixed_tokens);

    // Images are model-visible source material, not merely transcript markers.
    // Keep every image that fits, scanning newest first so recency wins the
    // budget — but do NOT stop at the first oversized one: a single giant
    // recent screenshot must not evict older small diagrams that fit. The text
    // projection states how many were omitted so the checkpoint cannot
    // silently imply complete visual coverage.
    let all_images: Vec<String> = archived_messages
        .iter()
        .flat_map(|message| message.images.iter().flatten().cloned())
        .collect();
    let mut summary_images = Vec::new();
    for image in all_images.iter().rev() {
        let image_tokens = image.len().div_ceil(4);
        if image_tokens <= remaining_tokens {
            summary_images.push(image.clone());
            remaining_tokens = remaining_tokens.saturating_sub(image_tokens);
        }
    }
    summary_images.reverse();

    let history = format_history_excerpt(
        &archived_messages,
        request.policy,
        all_images.len(),
        summary_images.len(),
    );
    let history_excerpt = truncate_middle(&history, remaining_tokens.saturating_mul(4));

    Ok(PreparedCompaction {
        archived_messages,
        preserved_messages,
        previous_summary,
        history_excerpt,
        summary_images,
    })
}

#[must_use]
pub fn build_summary_request(
    base: &ChatRequest,
    prepared: &PreparedCompaction,
    focus: Option<&str>,
    policy: CompactionPolicy,
    // The model's context window, so the summary's own output cap scales with
    // it. A flat cap asked a 4k model for 8k of output.
    window: Option<usize>,
) -> ChatRequest {
    let mut message = ChatMessage::user(summary_prompt(prepared, focus));
    if !prepared.summary_images.is_empty() {
        message.images = Some(prepared.summary_images.clone());
    }
    ChatRequest {
        model_id: base.model_id.clone(),
        messages: vec![message],
        system_prompt: compaction_system_prompt().to_string(),
        instructions: None,
        reasoning: compaction_reasoning(base.reasoning),
        temperature: 0.0,
        max_tokens: policy.summary_output_tokens(window),
        tools: Vec::new(),
        ollama_num_ctx: base.ollama_num_ctx,
        ollama_allow_ram_offload: base.ollama_allow_ram_offload,
        resolved_context_window: base.resolved_context_window,
        resolved_max_output: base.resolved_max_output,
        output_schema: None,
        suppress_auto_compact: false,
        suppressed_builtin_tools: Vec::new(),
    }
}

#[must_use]
pub fn build_verification_request(
    base: &ChatRequest,
    prepared: &PreparedCompaction,
    draft_summary: &str,
    focus: Option<&str>,
    policy: CompactionPolicy,
    window: Option<usize>,
) -> ChatRequest {
    let prompt = format!(
        "{}\n\n# Draft Summary\n{}\n\n# Verification Task\nCritically check the draft against the conversation excerpt. If it omitted specific file paths, commands, test results, tool results, user constraints, current state, or next steps, return an improved complete checkpoint. Otherwise return the draft unchanged. Return only the final checkpoint markdown.",
        summary_prompt(prepared, focus),
        draft_summary.trim()
    );
    let mut message = ChatMessage::user(prompt);
    if !prepared.summary_images.is_empty() {
        message.images = Some(prepared.summary_images.clone());
    }
    ChatRequest {
        model_id: base.model_id.clone(),
        messages: vec![message],
        system_prompt: compaction_system_prompt().to_string(),
        instructions: None,
        reasoning: compaction_reasoning(base.reasoning),
        temperature: 0.0,
        max_tokens: policy.summary_output_tokens(window),
        tools: Vec::new(),
        ollama_num_ctx: base.ollama_num_ctx,
        ollama_allow_ram_offload: base.ollama_allow_ram_offload,
        resolved_context_window: base.resolved_context_window,
        resolved_max_output: base.resolved_max_output,
        output_schema: None,
        suppress_auto_compact: false,
        suppressed_builtin_tools: Vec::new(),
    }
}

#[must_use]
pub fn build_replacement_messages(
    summary: &str,
    prepared: &PreparedCompaction,
    record: &CompactionEvent,
) -> Vec<ChatMessage> {
    // The summary is model-generated from the full conversation and is persisted
    // (replacement message + conversation file). Scrub any credential it echoed
    // back from the archived turns before it's written (#70).
    let summary = mermaid_model::utils::redact_secrets(summary);
    let summary = summary.as_str();
    let checkpoint = format!(
        "# {}\n\nCompaction id: {}\nTrigger: {}\nCreated: {}\nArchived messages: {}\nPreserved messages: {}\n\n{}",
        CHECKPOINT_MARKER,
        record.id,
        record.trigger.as_str(),
        record.created_at.to_rfc3339(),
        record.archived_message_count,
        record.preserved_message_count,
        summary.trim()
    );
    let mut user = ChatMessage::user(checkpoint);
    user.kind = ChatMessageKind::ContextCheckpoint;
    user.metadata = Some(serde_json::json!({
        "compaction_id": record.id,
        "trigger": record.trigger.as_str(),
        "before_tokens": record.before_tokens,
        "after_tokens": record.after_tokens,
        "archived_message_count": record.archived_message_count,
        "preserved_message_count": record.preserved_message_count,
        "preserved_turn_count": record.preserved_turn_count,
        "duration_secs": record.duration_secs,
        "review_status": record.review_status.as_str(),
        "review_error": record.review_error,
    }));

    let mut assistant = ChatMessage::assistant(compaction_receipt(record));
    assistant.kind = ChatMessageKind::ContextCheckpoint;
    assistant.metadata = user.metadata.clone();

    let mut messages = Vec::with_capacity(2 + prepared.preserved_messages.len());
    messages.push(user);
    messages.push(assistant);
    messages.extend(prepared.preserved_messages.clone());
    messages
}

#[must_use]
pub fn compaction_receipt(record: &CompactionEvent) -> String {
    let review = match record.review_status {
        CompactionReviewStatus::Reviewed => "Reviewed in a second pass.".to_string(),
        CompactionReviewStatus::DraftValidated => match &record.review_error {
            Some(error) => format!("Used the structurally validated draft: {error}."),
            None => "Used the structurally validated draft.".to_string(),
        },
    };
    format!(
        "Context compacted: {} -> {} tokens, archived {} messages, preserved {} messages, took {:.1}s. {} I will continue from this checkpoint.",
        format_compact_count(record.before_tokens),
        format_compact_count(record.after_tokens),
        record.archived_message_count,
        record.preserved_message_count,
        record.duration_secs,
        review
    )
}

#[must_use]
pub fn normalize_summary(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(summary) = extract_tagged_summary(trimmed) {
        return summary.trim().to_string();
    }
    trimmed.to_string()
}

/// # Errors
///
/// Returns the rejection message to show the model on a retry: either the ten
/// required headings are missing, reordered, or duplicated, or one of them has
/// a body that is empty, a bare `-`, or an unfilled `- [...]` placeholder.
/// Only the ten known headings count as structure — a checkpoint that quotes
/// other `## ` markdown in its body is valid.
pub fn validate_summary_structure(summary: &str) -> Result<(), String> {
    const HEADINGS: [&str; 10] = [
        "## Goal",
        "## User Preferences And Constraints",
        "## Project State",
        "## Completed Work",
        "## Current Work",
        "## Key Decisions",
        "## Critical Files And Symbols",
        "## Commands Tests And Results",
        "## Open Questions Or Risks",
        "## Next Steps",
    ];

    // Only the ten known headings are structure. Other `## `-prefixed lines
    // are body content — checkpoints legitimately quote markdown (commands,
    // error output, README excerpts) and must not fail closed over it.
    let lines: Vec<&str> = summary.lines().collect();
    let headings: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            HEADINGS.contains(&trimmed).then_some((index, trimmed))
        })
        .collect();
    let actual: Vec<&str> = headings.iter().map(|(_, heading)| *heading).collect();
    if actual != HEADINGS {
        return Err(format!(
            "checkpoint headings must exactly match the required order; got {}",
            actual.join(", ")
        ));
    }

    for (index, (line_index, heading)) in headings.iter().enumerate() {
        let body_end = headings
            .get(index + 1)
            .map(|(next_index, _)| *next_index)
            .unwrap_or(lines.len());
        let body = lines[line_index + 1..body_end].join("\n");
        let body = body.trim();
        if body.is_empty() || body == "-" || (body.starts_with("- [") && body.ends_with(']')) {
            return Err(format!(
                "checkpoint heading {heading} has placeholder content"
            ));
        }
    }
    Ok(())
}

#[must_use]
pub fn combine_usage(a: Option<TokenUsage>, b: Option<TokenUsage>) -> Option<TokenUsage> {
    match (a, b) {
        (None, None) => None,
        (Some(u), None) | (None, Some(u)) => Some(u),
        (Some(mut left), Some(right)) => {
            left.prompt_tokens = left.prompt_tokens.saturating_add(right.prompt_tokens);
            left.completion_tokens = left
                .completion_tokens
                .saturating_add(right.completion_tokens);
            left.cached_input_tokens = left
                .cached_input_tokens
                .saturating_add(right.cached_input_tokens);
            left.cache_creation_input_tokens = left
                .cache_creation_input_tokens
                .saturating_add(right.cache_creation_input_tokens);
            left.reasoning_output_tokens = left
                .reasoning_output_tokens
                .saturating_add(right.reasoning_output_tokens);
            Some(left)
        },
    }
}

pub fn estimate_messages_tokens(messages: &[ChatMessage]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Canonical compact token/count formatter shared across the reducer status
/// text, the footer widget, chat compaction receipts, and compaction records.
/// Abbreviates at 1k (`43.8k`, `1.2M`), exact below; a whole value drops the
/// decimal (`128k`, not `128.0k`). Previously three copies existed with two
/// different policies (threshold + rounding), so the same count rendered
/// inconsistently across the UI.
#[must_use]
pub fn format_compact_count(value: usize) -> String {
    if value >= 1_000_000 {
        format_scaled(value, 1_000_000, "M")
    } else if value >= 1_000 {
        format_scaled(value, 1_000, "k")
    } else {
        value.to_string()
    }
}

fn format_scaled(value: usize, divisor: usize, suffix: &str) -> String {
    let whole = value / divisor;
    let decimal = ((value % divisor) * 10) / divisor;
    if decimal == 0 {
        format!("{whole}{suffix}")
    } else {
        format!("{whole}.{decimal}{suffix}")
    }
}

fn compaction_system_prompt() -> &'static str {
    "You are performing context checkpoint compaction for Mermaid, a model-agnostic agentic coding CLI. Produce a faithful handoff summary for the next model call. Preserve exact file paths, commands, errors, tool results, user preferences, decisions, current state, and next steps. Do not invent facts. Be concise but complete."
}

fn compaction_reasoning(current: ReasoningLevel) -> ReasoningLevel {
    match current {
        ReasoningLevel::None | ReasoningLevel::Minimal => current,
        _ => ReasoningLevel::Low,
    }
}

fn summary_prompt(prepared: &PreparedCompaction, focus: Option<&str>) -> String {
    let anchor = prepared
        .previous_summary
        .as_deref()
        .map(|summary| {
            format!(
                "A previous checkpoint exists. Update it with the newer history, preserve still-true details, and remove stale details.\n\n<previous_checkpoint>\n{}\n</previous_checkpoint>",
                summary.trim()
            )
        })
        .unwrap_or_else(|| "Create a new checkpoint from the conversation history below.".to_string());

    let focus = focus
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!("\n# User Focus Instructions\n{}\n", s.trim()))
        .unwrap_or_default();

    format!(
        "{anchor}{focus}\n# Required Output\nReturn exactly this Markdown structure and keep section order:\n\n## Goal\n- [single-sentence task summary]\n\n## User Preferences And Constraints\n- [preferences, constraints, mode, or \"(none)\"]\n\n## Project State\n- [repo/product state and important architecture facts]\n\n## Completed Work\n- [what has already been done]\n\n## Current Work\n- [what is actively in progress]\n\n## Key Decisions\n- [decision and rationale]\n\n## Critical Files And Symbols\n- [file path or symbol: why it matters]\n\n## Commands Tests And Results\n- [command/test/result/error]\n\n## Open Questions Or Risks\n- [risk/question/blocker]\n\n## Next Steps\n- [ordered next action]\n\nRules:\n- Preserve exact paths, commands, error strings, identifiers, and numeric facts when known.\n- Mention important omitted or truncated data explicitly.\n- Do not mention that you are an AI or explain the compaction process.\n\n# Conversation History To Compact\n{}",
        prepared.history_excerpt
    )
}

/// Scrub orphan tool-call/tool-result pairs from the preserved tail so a
/// forwarded unpaired block can't 400 a provider (Anthropic). Compaction keeps
/// a recent tail verbatim; if the split inherits a pre-existing orphan, both
/// directions must be repaired symmetrically:
///
///   * Forward (#71): an assistant `tool_use` whose `tool_result` never
///     committed — e.g. a turn cancelled after the model emitted calls. Drop
///     the orphaned calls (keeping the assistant's text) rather than fabricate
///     results. A call with no `id` can't be paired, so it's treated as orphaned.
///   * Reverse (#F64): a `tool_result` (role=Tool) whose `tool_use` id is no
///     longer present among the retained messages — e.g. the assistant turn was
///     archived while its result landed in the tail. Anthropic equally rejects a
///     `tool_result` with no preceding `tool_use`, so drop the orphaned result.
///
/// When `preserve_pending_tail` is set (the run is resuming mid-tool after a
/// context-limit retry / truncation recovery), a *trailing* assistant `tool_use`
/// is genuinely pending execution rather than abandoned, so its calls are kept
/// across the checkpoint and the resumed turn appends the awaited results (#F65).
/// Only the final message qualifies: anything after an assistant `tool_use` (a
/// tool result, a follow-up, a user cancel) means it is no longer pending. This
/// is trigger-gated by the caller because a user cancel yields the same trailing
/// shape but must still drop — there, the result will never arrive.
/// Repair `tool_use`/`tool_result` pairing on a message list before it is sent
/// to a provider (or seeded as a resumed prefix). Drops orphans in both
/// directions: an assistant `tool_use` with no matching result, and a
/// `tool_result` whose call is gone.
///
/// `preserve_pending_tail` is always `false` here: an outgoing request must
/// never carry a trailing unanswered `tool_use` (providers 400 on it), and a
/// cold-loaded prefix has no in-flight turn to append the awaited result — a
/// preserved tail would be a permanent orphan. The compaction path calls
/// [`drop_orphan_tool_calls`] directly with `true` for the truncation-recovery
/// checkpoint, which *does* resume the pending call.
pub(crate) fn normalize_history(messages: &mut Vec<ChatMessage>) {
    drop_orphan_tool_calls(messages, false);
}

pub(crate) fn drop_orphan_tool_calls(messages: &mut Vec<ChatMessage>, preserve_pending_tail: bool) {
    let pending_tail = if preserve_pending_tail
        && messages.last().is_some_and(|m| {
            m.role == MessageRole::Assistant && m.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
        }) {
        Some(messages.len() - 1)
    } else {
        None
    };

    let answered: std::collections::HashSet<String> = messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    // Forward (#71): drop unanswered assistant `tool_use`, save a pending tail.
    for (idx, m) in messages.iter_mut().enumerate() {
        if Some(idx) == pending_tail {
            continue;
        }
        let Some(calls) = m.tool_calls.as_mut() else {
            continue;
        };
        calls.retain(|c| c.id.as_deref().is_some_and(|id| answered.contains(id)));
        if calls.is_empty() {
            m.tool_calls = None;
        }
        let kept: std::collections::HashSet<&str> = m
            .tool_calls
            .iter()
            .flatten()
            .filter_map(|call| call.id.as_deref())
            .collect();
        if let Some(continuation) = &mut m.provider_continuation {
            continuation.retain_meta_function_calls(|call_id| kept.contains(call_id));
        }
    }

    // Reverse (#F64): drop a `tool_result` whose `tool_use` id is no longer
    // present among the assistant messages retained above (symmetric orphan).
    let emitted: std::collections::HashSet<String> = messages
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flat_map(|calls| calls.iter())
        .filter_map(|c| c.id.clone())
        .collect();
    messages.retain(|m| {
        m.role != MessageRole::Tool
            || m.tool_call_id
                .as_deref()
                .is_some_and(|id| emitted.contains(id))
    });
}

fn tail_start_index(messages: &[ChatMessage], policy: CompactionPolicy) -> Option<usize> {
    let mut user_turns = 0usize;
    let mut start = None;
    for (idx, msg) in messages.iter().enumerate().rev() {
        if msg.role == MessageRole::User {
            user_turns += 1;
            start = Some(idx);
            if user_turns >= policy.tail_turns {
                break;
            }
        }
    }
    let mut start = start?;
    while estimate_messages_tokens(&messages[start..]) > policy.tail_token_budget {
        let next_user = messages
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, msg)| msg.role == MessageRole::User)
            .map(|(idx, _)| idx);
        match next_user {
            Some(idx) => start = idx,
            None => break,
        }
    }
    Some(start)
}

fn format_history_excerpt(
    messages: &[ChatMessage],
    policy: CompactionPolicy,
    total_images: usize,
    included_images: usize,
) -> String {
    let mut out = String::new();
    if total_images > 0 {
        out.push_str(&format!(
            "\n[Visual context: {included_images} of {total_images} archived image attachment(s) supplied with this request; {} omitted by the input budget.]\n",
            total_images.saturating_sub(included_images)
        ));
    }
    for (idx, msg) in messages.iter().enumerate() {
        let role = match msg.role {
            MessageRole::User => "USER",
            MessageRole::Assistant => "ASSISTANT",
            MessageRole::System => "SYSTEM",
            MessageRole::Tool => "TOOL",
        };
        out.push_str(&format!("\n\n--- MESSAGE {} [{}] ---\n", idx + 1, role));
        if msg.kind != ChatMessageKind::Normal {
            out.push_str(&format!("kind: {:?}\n", msg.kind));
        }
        if let Some(name) = &msg.tool_name {
            out.push_str(&format!("tool_name: {name}\n"));
        }
        if let Some(id) = &msg.tool_call_id {
            out.push_str(&format!("tool_call_id: {id}\n"));
        }
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                let mut arguments = call.function.arguments.clone();
                mermaid_model::utils::redact_json(&mut arguments);
                let arguments = truncate_middle(
                    &arguments.to_string(),
                    policy.tool_output_max_chars.saturating_mul(4),
                );
                out.push_str(&format!(
                    "tool_call: id={} name={} arguments={}\n",
                    call.id.as_deref().unwrap_or("<missing>"),
                    call.function.name,
                    arguments
                ));
            }
        }
        if let Some(images) = &msg.images
            && !images.is_empty()
        {
            out.push_str(&format!(
                "[{} image attachment(s) referenced above]\n",
                images.len()
            ));
        }
        for action in &msg.actions {
            out.push_str(&format!(
                "action: {}({}) duration={:?}\n",
                action.action_type, action.target, action.duration_seconds
            ));
            if let Some(metadata) = &action.metadata {
                out.push_str(&format!("action_metadata: {metadata:?}\n"));
            }
        }
        let cap = if msg.role == MessageRole::Tool {
            policy.tool_output_max_chars
        } else {
            policy.tool_output_max_chars.saturating_mul(4)
        };
        out.push_str(&truncate_middle(&msg.content, cap));
    }
    out
}

fn estimate_message_tokens(msg: &ChatMessage) -> usize {
    let mut chars = msg.content.len();
    chars = chars.saturating_add(format!("{:?}", msg.role).len());
    chars = chars.saturating_add(msg.tool_name.as_deref().map(str::len).unwrap_or(0));
    chars = chars.saturating_add(msg.tool_call_id.as_deref().map(str::len).unwrap_or(0));
    if let Some(images) = &msg.images {
        chars = chars.saturating_add(images.iter().map(String::len).sum::<usize>());
    }
    // Assistant tool calls carry the function name + a JSON arguments payload
    // (often kilobytes for a file write or shell script). Omitting these made
    // the estimate run systematically low for this tool-heavy agent, causing
    // under-compaction and provider-side context overflows.
    if let Some(tool_calls) = &msg.tool_calls {
        for tc in tool_calls {
            chars = chars.saturating_add(tc.function.name.len());
            chars = chars.saturating_add(tc.function.arguments.to_string().len());
            chars = chars.saturating_add(tc.id.as_deref().map(str::len).unwrap_or(0));
        }
    }
    chars.div_ceil(4)
}

fn truncate_middle(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars < 128 {
        return text.chars().take(max_chars).collect();
    }
    let marker = "\n\n[... truncated during context compaction ...]\n\n";
    let keep = max_chars.saturating_sub(marker.len());
    let head = keep / 2;
    let tail = keep.saturating_sub(head);
    let start: String = text.chars().take(head).collect();
    let end: String = text
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{start}{marker}{end}")
}

fn extract_tagged_summary(text: &str) -> Option<&str> {
    let start_tag = "<summary>";
    let end_tag = "</summary>";
    let start = text.find(start_tag)? + start_tag.len();
    let end = text[start..].find(end_tag)? + start;
    Some(&text[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model_id: "ollama/test".to_string(),
            messages,
            system_prompt: "system".to_string(),
            instructions: None,
            reasoning: ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 4096,
            tools: Vec::new(),
            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
            output_schema: None,
            suppress_auto_compact: false,
            suppressed_builtin_tools: Vec::new(),
        }
    }

    #[test]
    fn classify_length_stop_discriminates_output_cap_from_context_full() {
        let usage = TokenUsage::provider(16_600, 4_000);
        // No usage → Unknown (legacy recovery path preserved).
        assert_eq!(
            classify_length_stop(None, Some(100_000), 4_000),
            LengthCause::Unknown
        );
        // Unknown window + usage → the per-response output cap (the normal
        // remote-provider case — the GLM-5.2 misdiagnosis this fixes).
        assert_eq!(
            classify_length_stop(Some(&usage), None, 4_000),
            LengthCause::OutputCapped
        );
        // Window with plenty of room → still the output cap.
        assert_eq!(
            classify_length_stop(Some(&usage), Some(1_000_000), 4_000),
            LengthCause::OutputCapped
        );
        // prompt + completion + reserve reaching the window → genuinely full.
        assert_eq!(
            classify_length_stop(Some(&usage), Some(24_000), 4_000),
            LengthCause::ContextFull
        );
    }

    #[test]
    fn classify_length_stop_counts_cached_and_reasoning_tokens() {
        let usage = TokenUsage::provider(100, 100)
            .with_cached_input(700)
            .with_cache_creation(50)
            .with_reasoning_output(50);
        assert_eq!(usage.total_tokens(), 1_000);
        assert_eq!(
            classify_length_stop(Some(&usage), Some(1_100), 100),
            LengthCause::ContextFull
        );
    }

    #[test]
    fn summary_and_verification_requests_copy_resolved_limits() {
        // The summarizer calls the same model — its request must inherit the
        // live-discovered limits or Anthropic AUTO would fall to the 8192
        // floor mid-compaction.
        let mut base = request_with(vec![ChatMessage::user("hello")]);
        base.resolved_context_window = Some(1_000_000);
        base.resolved_max_output = Some(128_000);
        let prepared = PreparedCompaction {
            archived_messages: vec![ChatMessage::user("old")],
            preserved_messages: vec![],
            previous_summary: None,
            history_excerpt: "excerpt".to_string(),
            summary_images: Vec::new(),
        };
        let policy = CompactionPolicy::default();
        let summary = build_summary_request(&base, &prepared, None, policy, None);
        assert_eq!(summary.resolved_context_window, Some(1_000_000));
        assert_eq!(summary.resolved_max_output, Some(128_000));
        let verify = build_verification_request(&base, &prepared, "draft", None, policy, None);
        assert_eq!(verify.resolved_context_window, Some(1_000_000));
        assert_eq!(verify.resolved_max_output, Some(128_000));
    }

    #[test]
    fn response_reserve_is_reasoning_aware_on_auto() {
        let policy = CompactionPolicy::default();
        let mut req = request_with(vec![ChatMessage::user("hello")]);

        // AUTO: the reserve scales with the reasoning level instead of
        // mirroring a send-cap that no longer exists.
        req.max_tokens = 0;
        req.reasoning = ReasoningLevel::None;
        let base = policy.response_reserve(&req);
        assert_eq!(base, policy.min_response_reserve_tokens);
        req.reasoning = ReasoningLevel::Max;
        let deep = policy.response_reserve(&req);
        assert!(deep > base, "a Max-reasoning turn must reserve more room");
        assert!(deep <= policy.max_response_reserve_tokens);

        // An explicit cap is the best reserve estimate — honored (clamped).
        req.max_tokens = 12_000;
        assert_eq!(policy.response_reserve(&req), 12_000);
        req.max_tokens = 1_000_000;
        assert_eq!(
            policy.response_reserve(&req),
            policy.max_response_reserve_tokens
        );
    }

    #[test]
    fn auto_compaction_triggers_by_percent() {
        let snapshot = ContextUsageSnapshot::from_estimate(
            super::super::state::PromptTokenBreakdown {
                system_tokens: 0,
                instructions_tokens: 0,
                message_tokens: 86,
                tool_schema_tokens: 0,
                image_count: 0,
                message_count: 2,
                tool_count: 0,
            },
            Some(100),
        );
        let req = request_with(vec![ChatMessage::user("hello")]);
        assert!(should_auto_compact(&snapshot, &req, CompactionPolicy::default()).is_ok());
    }

    #[test]
    fn auto_compaction_pause_rides_the_request() {
        let snapshot = ContextUsageSnapshot::from_estimate(
            super::super::state::PromptTokenBreakdown {
                system_tokens: 0,
                instructions_tokens: 0,
                message_tokens: 86,
                tool_schema_tokens: 0,
                image_count: 0,
                message_count: 2,
                tool_count: 0,
            },
            Some(100),
        );
        let mut req = request_with(vec![ChatMessage::user("hello")]);
        req.suppress_auto_compact = true;
        assert_eq!(
            should_auto_compact(&snapshot, &req, CompactionPolicy::default()),
            Err(CompactionSkip::Suppressed)
        );
    }

    #[test]
    fn boundary_fingerprint_matches_only_the_identical_message() {
        let message = ChatMessage::user("hello");
        let boundary = CompactionBoundary::from_message(&message);
        assert!(boundary.matches(&message));

        let mut other_content = message.clone();
        other_content.content = "hello!".to_string();
        assert!(!boundary.matches(&other_content));

        let mut other_kind = message.clone();
        other_kind.kind = ChatMessageKind::ContextCheckpoint;
        assert!(!boundary.matches(&other_kind));

        let mut other_time = message.clone();
        other_time.timestamp += chrono::Duration::nanoseconds(1);
        assert!(!boundary.matches(&other_time));
    }

    #[test]
    fn prepare_preserves_recent_two_user_turns() {
        let messages = vec![
            ChatMessage::user("one"),
            ChatMessage::assistant("one answer"),
            ChatMessage::user("two"),
            ChatMessage::assistant("two answer"),
            ChatMessage::user("three"),
        ];
        let request =
            CompactionRequest::manual(request_with(messages), None, CompactionPolicy::default());
        let prepared = prepare_compaction(&request, Some(100_000)).expect("prepared");
        assert_eq!(prepared.archived_messages.len(), 2);
        assert_eq!(prepared.preserved_messages.len(), 3);
        assert_eq!(prepared.preserved_messages[0].content, "two");
    }

    #[test]
    fn prepare_projects_redacted_tool_arguments_and_archived_images() {
        let mut old = ChatMessage::user("inspect the screenshot");
        old.images = Some(vec!["aGVsbG8=".to_string()]);
        let mut call = ChatMessage::assistant("");
        call.tool_calls = Some(vec![mermaid_model::models::tool_call::ToolCall {
            id: Some("call_1".to_string()),
            function: mermaid_model::models::tool_call::FunctionCall {
                name: "execute_command".to_string(),
                arguments: serde_json::json!({
                    "cmd": "cargo test --workspace",
                    "api_key": "opaque-secret-value"
                }),
            },
        }]);
        let messages = vec![
            old,
            call,
            ChatMessage::tool("call_1", "execute_command", "tests passed"),
            ChatMessage::user("second"),
            ChatMessage::assistant("second answer"),
            ChatMessage::user("third"),
        ];
        let request =
            CompactionRequest::manual(request_with(messages), None, CompactionPolicy::default());
        let prepared = prepare_compaction(&request, Some(100_000)).expect("prepared");
        assert!(prepared.history_excerpt.contains("cargo test --workspace"));
        assert!(prepared.history_excerpt.contains("[REDACTED]"));
        assert!(!prepared.history_excerpt.contains("opaque-secret-value"));
        assert_eq!(prepared.summary_images, vec!["aGVsbG8=".to_string()]);
        let summary = build_summary_request(
            &request.chat,
            &prepared,
            None,
            CompactionPolicy::default(),
            Some(100_000),
        );
        assert_eq!(
            summary.messages[0].images.as_deref(),
            Some(prepared.summary_images.as_slice())
        );
    }

    /// Build a conversation with far more history than any budget will hold,
    /// so the sizing math — not the content — decides the request size.
    fn oversized_conversation() -> Vec<ChatMessage> {
        vec![
            ChatMessage::user("old ".repeat(40_000)),
            ChatMessage::assistant("old answer ".repeat(20_000)),
            ChatMessage::user("second".to_string()),
            ChatMessage::assistant("second answer".to_string()),
            ChatMessage::user("third".to_string()),
        ]
    }

    /// The summarizer's request must never grow as the window shrinks.
    ///
    /// It used to: `max_input_tokens` subtracted a flat 8k output cap and then
    /// treated a non-positive result as "window unknown", falling back to the
    /// *most permissive* 64k budget. Measured, an 8k model got a 16,059-char
    /// excerpt while a 9k model got 2,688 — so 9k compacted and 8k could not.
    #[test]
    fn summary_request_shrinks_monotonically_with_the_window() {
        let mut previous: Option<(usize, usize)> = None;
        for window in [128_000usize, 32_000, 16_000, 9_000, 8_000, 4_000] {
            let request = CompactionRequest::manual(
                request_with(oversized_conversation()),
                None,
                CompactionPolicy::default(),
            );
            let prepared =
                prepare_compaction(&request, Some(window)).expect("window is large enough");
            let summary = build_summary_request(
                &request.chat,
                &prepared,
                request.instructions.as_deref(),
                request.policy,
                Some(window),
            );
            let used =
                crate::estimate_context_usage_for_request(&summary, Some(window)).used_tokens;
            if let Some((prev_window, prev_used)) = previous {
                assert!(
                    used <= prev_used,
                    "shrinking the window {prev_window} -> {window} grew the request \
                     {prev_used} -> {used} tokens",
                );
            }
            previous = Some((window, used));
        }
    }

    /// The fit invariant, checked on BOTH sides of the old cliff. The existing
    /// coverage only ever used a 32k window — comfortably above the flat 8k
    /// output cap — which is exactly why the break went unnoticed.
    #[test]
    fn complete_summary_request_fits_every_supported_window() {
        for window in [128_000usize, 32_000, 16_000, 9_000, 8_000, 4_000, 2_048] {
            let request = CompactionRequest::manual(
                request_with(oversized_conversation()),
                None,
                CompactionPolicy::default(),
            );
            let prepared =
                prepare_compaction(&request, Some(window)).expect("window is large enough");
            let summary = build_summary_request(
                &request.chat,
                &prepared,
                request.instructions.as_deref(),
                request.policy,
                Some(window),
            );
            let used =
                crate::estimate_context_usage_for_request(&summary, Some(window)).used_tokens;
            assert!(
                used.saturating_add(summary.max_tokens) <= window,
                "window {window}: input {used} + output {} exceeds it",
                summary.max_tokens,
            );
            assert!(
                summary.max_tokens > 0,
                "window {window}: the summary must be allowed to produce output",
            );
        }
    }

    /// A window too small to hold a checkpoint skips with a plain reason
    /// instead of building a request that is guaranteed to be rejected.
    #[test]
    fn a_window_too_small_to_compact_skips_instead_of_failing() {
        let request = CompactionRequest::manual(
            request_with(oversized_conversation()),
            None,
            CompactionPolicy::default(),
        );
        assert_eq!(
            prepare_compaction(&request, Some(1_000)).err(),
            Some(CompactionSkip::WindowTooSmall)
        );
        // The message is user-facing (`Nothing to compact — …`), so it must read
        // as an explanation rather than an error code.
        assert_eq!(
            CompactionSkip::WindowTooSmall.to_string(),
            "the model's context window is too small to hold a checkpoint"
        );
    }

    /// An unknown window (the normal remote-provider case) keeps the flat
    /// budget it always had — the scaling only ever applies to a known window.
    #[test]
    fn unknown_window_keeps_the_flat_budget() {
        let policy = CompactionPolicy::default();
        assert_eq!(
            policy.summary_output_tokens(None),
            policy.summary_max_tokens
        );
        assert_eq!(
            policy.summary_input_budget(None),
            policy.summarizer_input_token_budget
        );
        // A large known window is likewise unchanged: the flat cap is the
        // ceiling, and the input budget is still the summarizer's own cap.
        assert_eq!(
            policy.summary_output_tokens(Some(1_000_000)),
            policy.summary_max_tokens
        );
        assert_eq!(
            policy.summary_input_budget(Some(1_000_000)),
            policy.summarizer_input_token_budget
        );
    }

    #[test]
    fn complete_summary_request_fits_known_window() {
        let messages = vec![
            ChatMessage::user("old ".repeat(40_000)),
            ChatMessage::assistant("old answer ".repeat(20_000)),
            ChatMessage::user("second"),
            ChatMessage::assistant("second answer"),
            ChatMessage::user("third"),
        ];
        let request = CompactionRequest::manual(
            request_with(messages),
            Some("focus".repeat(500)),
            CompactionPolicy::default(),
        );
        let window = 32_000;
        let prepared = prepare_compaction(&request, Some(window)).expect("prepared");
        let summary = build_summary_request(
            &request.chat,
            &prepared,
            request.instructions.as_deref(),
            request.policy,
            Some(window),
        );
        let usage = crate::estimate_context_usage_for_request(&summary, Some(window));
        assert!(usage.used_tokens.saturating_add(summary.max_tokens) <= window);
    }

    #[test]
    fn complete_summary_request_with_images_fits_known_window() {
        let mut old = ChatMessage::user("old ".repeat(40_000));
        old.images = Some(vec!["i".repeat(40_000), "j".repeat(40_000)]);
        let messages = vec![
            old,
            ChatMessage::assistant("old answer ".repeat(20_000)),
            ChatMessage::user("second"),
            ChatMessage::assistant("second answer"),
            ChatMessage::user("third"),
        ];
        let request = CompactionRequest::manual(
            request_with(messages),
            Some("focus".repeat(500)),
            CompactionPolicy::default(),
        );
        let window = 32_000;
        let prepared = prepare_compaction(&request, Some(window)).expect("prepared");
        let summary = build_summary_request(
            &request.chat,
            &prepared,
            request.instructions.as_deref(),
            request.policy,
            Some(window),
        );
        assert!(
            !prepared.summary_images.is_empty(),
            "the newest image fits the budget and must be attached"
        );
        let usage = crate::estimate_context_usage_for_request(&summary, Some(window));
        assert!(
            usage.used_tokens.saturating_add(summary.max_tokens) <= window,
            "used {} + max_tokens {} > window {}",
            usage.used_tokens,
            summary.max_tokens,
            window
        );
    }

    #[test]
    fn image_budget_keeps_every_fitting_image_newest_first() {
        let mut old = ChatMessage::user("inspect");
        // Oldest image is tiny, newest exceeds the whole input budget. One
        // oversized recent screenshot must not evict older small diagrams
        // that fit — the older image still rides along, and the projection
        // reports the omission honestly.
        old.images = Some(vec!["a".repeat(400), "b".repeat(400_000)]);
        let messages = vec![
            old,
            ChatMessage::assistant("looked"),
            ChatMessage::user("second"),
            ChatMessage::assistant("second answer"),
            ChatMessage::user("third"),
        ];
        let request =
            CompactionRequest::manual(request_with(messages), None, CompactionPolicy::default());
        let prepared = prepare_compaction(&request, None).expect("prepared");
        assert_eq!(prepared.summary_images, vec!["a".repeat(400)]);
        assert!(
            prepared
                .history_excerpt
                .contains("1 of 2 archived image attachment(s)")
        );
    }

    #[test]
    fn summary_structure_requires_ordered_non_placeholder_sections() {
        let valid = "## Goal\n- ship the fix\n\n## User Preferences And Constraints\n- none\n\n## Project State\n- ready\n\n## Completed Work\n- audit\n\n## Current Work\n- implementation\n\n## Key Decisions\n- preserve data\n\n## Critical Files And Symbols\n- compaction.rs\n\n## Commands Tests And Results\n- tests pass\n\n## Open Questions Or Risks\n- none\n\n## Next Steps\n- finish";
        assert!(validate_summary_structure(valid).is_ok());
        assert!(validate_summary_structure("## Goal\n- [single-sentence task summary]").is_err());
    }

    #[test]
    fn summary_structure_tolerates_quoted_markdown_in_bodies() {
        // Checkpoints legitimately quote markdown — a `## `-prefixed line
        // inside a section body is content, not structure, and must not fail
        // the checkpoint closed.
        let with_quoted_heading = "## Goal\n- ship the fix\n\n## User Preferences And Constraints\n- none\n\n## Project State\n- ready\n\n## Completed Work\n- audit\n\n## Current Work\n- implementation\n\n## Key Decisions\n- preserve data\n\n## Critical Files And Symbols\n- compaction.rs\n\n## Commands Tests And Results\n- README now starts with:\n## Quick Start\ninstall the CLI\n\n## Open Questions Or Risks\n- none\n\n## Next Steps\n- finish";
        assert!(validate_summary_structure(with_quoted_heading).is_ok());
    }

    fn tool_call(id: &str, name: &str) -> mermaid_model::models::tool_call::ToolCall {
        mermaid_model::models::tool_call::ToolCall {
            id: Some(id.to_string()),
            function: mermaid_model::models::tool_call::FunctionCall {
                name: name.to_string(),
                arguments: serde_json::json!({}),
            },
        }
    }

    #[test]
    fn prepare_strips_orphan_tool_call_from_preserved_tail() {
        // A tail that inherits an assistant(tool_calls) with no matching result
        // (e.g. a cancelled tool turn) must not forward the unpaired tool_use (#71).
        let mut orphan = ChatMessage::assistant("calling a tool");
        orphan.tool_calls = Some(vec![tool_call("call_1", "do_thing")]);
        let messages = vec![
            ChatMessage::user("one"),
            ChatMessage::assistant("one answer"),
            ChatMessage::user("two"),
            orphan,
            ChatMessage::user("three"),
        ];
        let request =
            CompactionRequest::manual(request_with(messages), None, CompactionPolicy::default());
        let prepared = prepare_compaction(&request, Some(100_000)).expect("prepared");
        let has_orphan = prepared
            .preserved_messages
            .iter()
            .any(|m| m.tool_calls.as_ref().is_some_and(|c| !c.is_empty()));
        assert!(
            !has_orphan,
            "orphan tool_use must be stripped from the tail"
        );
        // The message itself (its text) is preserved — only the calls are dropped.
        assert!(
            prepared
                .preserved_messages
                .iter()
                .any(|m| m.content == "calling a tool")
        );
    }

    #[test]
    fn prepare_keeps_paired_tool_call_in_tail() {
        // The mirror case: a tool_call whose result is also in the tail survives.
        let mut asst = ChatMessage::assistant("calling");
        asst.tool_calls = Some(vec![tool_call("call_1", "do_thing")]);
        let messages = vec![
            ChatMessage::user("one"),
            ChatMessage::assistant("one answer"),
            ChatMessage::user("two"),
            asst,
            ChatMessage::tool("call_1", "do_thing", "ok"),
            ChatMessage::user("three"),
        ];
        let request =
            CompactionRequest::manual(request_with(messages), None, CompactionPolicy::default());
        let prepared = prepare_compaction(&request, Some(100_000)).expect("prepared");
        let kept = prepared
            .preserved_messages
            .iter()
            .any(|m| m.tool_calls.as_ref().is_some_and(|c| !c.is_empty()));
        assert!(kept, "a tool_call paired with its result must be preserved");
    }

    #[test]
    fn normalize_history_drops_orphan_assistant_tool_use() {
        let mut orphan = ChatMessage::assistant("calling a tool");
        orphan.tool_calls = Some(vec![tool_call("call_1", "do_thing")]);
        let mut messages = vec![ChatMessage::user("hi"), orphan];
        normalize_history(&mut messages);
        assert!(
            messages
                .iter()
                .all(|m| m.tool_calls.as_ref().is_none_or(|c| c.is_empty())),
            "dangling tool_use must be dropped"
        );
        assert!(
            messages.iter().any(|m| m.content == "calling a tool"),
            "the assistant text is preserved — only the unpaired call is removed"
        );
    }

    #[test]
    fn normalize_history_drops_matching_meta_replay_function_call() {
        let mut orphan = ChatMessage::assistant("calling a tool");
        orphan.tool_calls = Some(vec![tool_call("call_1", "do_thing")]);
        orphan.provider_continuation =
            Some(mermaid_model::models::ProviderContinuation::MetaResponses {
                output: vec![mermaid_model::models::MetaResponseItem::from_wire(
                    serde_json::json!({
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "do_thing",
                        "arguments": "{}"
                    }),
                )],
            });
        let mut messages = vec![ChatMessage::user("hi"), orphan];
        normalize_history(&mut messages);
        let output = messages[1]
            .provider_continuation
            .as_ref()
            .and_then(mermaid_model::models::ProviderContinuation::meta_output)
            .unwrap();
        assert!(
            output.is_empty(),
            "orphan Meta function_call must also drop"
        );
    }

    #[test]
    fn normalize_history_drops_orphan_tool_result() {
        let mut messages = vec![
            ChatMessage::user("hi"),
            ChatMessage::tool("call_ghost", "do_thing", "result with no call"),
        ];
        normalize_history(&mut messages);
        assert!(
            !messages.iter().any(|m| m.role == MessageRole::Tool),
            "a tool_result whose call is absent must be dropped"
        );
    }

    #[test]
    fn normalize_history_keeps_well_paired_tool_calls() {
        let mut asst = ChatMessage::assistant("calling");
        asst.tool_calls = Some(vec![tool_call("call_1", "do_thing")]);
        let mut messages = vec![
            ChatMessage::user("hi"),
            asst,
            ChatMessage::tool("call_1", "do_thing", "ok"),
        ];
        let before = messages.len();
        normalize_history(&mut messages);
        assert_eq!(
            messages.len(),
            before,
            "a paired call+result survives intact"
        );
        assert!(
            messages[1]
                .tool_calls
                .as_ref()
                .is_some_and(|c| c.len() == 1)
        );
    }

    #[test]
    fn normalize_history_drops_idless_tool_use() {
        let mut asst = ChatMessage::assistant("calling");
        asst.tool_calls = Some(vec![mermaid_model::models::tool_call::ToolCall {
            id: None,
            function: mermaid_model::models::tool_call::FunctionCall {
                name: "do_thing".into(),
                arguments: serde_json::json!({}),
            },
        }]);
        let mut messages = vec![asst];
        normalize_history(&mut messages);
        assert!(
            messages[0].tool_calls.as_ref().is_none_or(|c| c.is_empty()),
            "an id-less tool_use is inherently unpaired → dropped"
        );
    }

    #[test]
    fn prepare_drops_reverse_orphan_tool_result_from_tail() {
        // The mirror of #71 (#F64): the assistant `tool_use` is archived (split
        // out of the tail) while its `tool_result` lands in the preserved tail.
        // A lone `tool_result` with no preceding `tool_use` 400s Anthropic, so it
        // must be dropped symmetrically.
        let mut asst = ChatMessage::assistant("calling");
        asst.tool_calls = Some(vec![tool_call("call_1", "do_thing")]);
        let messages = vec![
            ChatMessage::user("one"),
            asst,
            ChatMessage::user("two"),
            ChatMessage::tool("call_1", "do_thing", "result"),
            ChatMessage::user("three"),
        ];
        // Tail keeps the last two user turns ("two".., "three"), so the assistant
        // tool_use is archived but the tool_result survives into the tail.
        let request =
            CompactionRequest::manual(request_with(messages), None, CompactionPolicy::default());
        let prepared = prepare_compaction(&request, Some(100_000)).expect("prepared");
        assert!(
            prepared
                .preserved_messages
                .iter()
                .all(|m| m.role != MessageRole::Tool),
            "an orphan tool_result whose tool_use was archived must be dropped"
        );
    }

    #[test]
    fn prepare_keeps_pending_trailing_tool_use_on_retry() {
        // #F65: a context-limit retry / truncation recovery compacts mid-tool.
        // The trailing assistant tool_use is genuinely pending — the run resumes
        // and appends the result — so its calls must survive compaction.
        let mut pending = ChatMessage::assistant("calling a tool");
        pending.tool_calls = Some(vec![tool_call("call_9", "do_thing")]);
        let messages = vec![
            ChatMessage::user("one"),
            ChatMessage::assistant("a1"),
            ChatMessage::user("two"),
            ChatMessage::assistant("a2"),
            ChatMessage::user("three"),
            pending,
        ];
        let request = CompactionRequest::auto(
            request_with(messages),
            CompactionTrigger::ContextLimitRetry,
            CompactionPolicy::default(),
        );
        let prepared = prepare_compaction(&request, Some(100_000)).expect("prepared");
        let last = prepared
            .preserved_messages
            .last()
            .expect("non-empty preserved tail");
        assert!(
            last.tool_calls
                .as_ref()
                .is_some_and(|c| c.iter().any(|call| call.id.as_deref() == Some("call_9"))),
            "a pending trailing tool_use must be preserved across a retry compaction"
        );
    }

    #[test]
    fn prepare_drops_trailing_tool_use_on_manual_compaction() {
        // Same trailing shape, but a manual compaction is not a resume: the tool
        // is treated as abandoned/cancelled, so the unpaired call is still
        // scrubbed (#71) — only the assistant's text is kept.
        let mut pending = ChatMessage::assistant("calling a tool");
        pending.tool_calls = Some(vec![tool_call("call_9", "do_thing")]);
        let messages = vec![
            ChatMessage::user("one"),
            ChatMessage::assistant("a1"),
            ChatMessage::user("two"),
            ChatMessage::assistant("a2"),
            ChatMessage::user("three"),
            pending,
        ];
        let request =
            CompactionRequest::manual(request_with(messages), None, CompactionPolicy::default());
        let prepared = prepare_compaction(&request, Some(100_000)).expect("prepared");
        assert!(
            !prepared
                .preserved_messages
                .iter()
                .any(|m| m.tool_calls.as_ref().is_some_and(|c| !c.is_empty())),
            "manual compaction must scrub the trailing orphan tool_use"
        );
        assert!(
            prepared
                .preserved_messages
                .iter()
                .any(|m| m.content == "calling a tool"),
            "the assistant text is kept even though the orphan call is dropped"
        );
    }

    #[test]
    fn replacement_starts_with_checkpoint_and_ack() {
        let prepared = PreparedCompaction {
            archived_messages: vec![ChatMessage::user("old")],
            preserved_messages: vec![ChatMessage::user("new")],
            previous_summary: None,
            history_excerpt: "old".to_string(),
            summary_images: Vec::new(),
        };
        let record = CompactionEvent {
            id: "c1".to_string(),
            trigger: CompactionTrigger::Manual,
            created_at: Local::now(),
            before_tokens: 100,
            after_tokens: 25,
            archived_message_count: 1,
            preserved_message_count: 1,
            preserved_turn_count: 1,
            summary_tokens: 10,
            duration_secs: 1.0,
            review_status: CompactionReviewStatus::Reviewed,
            review_error: None,
            focus: None,
            archive_path: None,
        };
        let messages = build_replacement_messages("## Goal\n- continue", &prepared, &record);
        assert_eq!(messages[0].kind, ChatMessageKind::ContextCheckpoint);
        assert!(messages[0].content.contains(CHECKPOINT_MARKER));
        assert_eq!(messages[2].content, "new");
    }

    #[test]
    fn replacement_metadata_records_review_status() {
        let prepared = PreparedCompaction {
            archived_messages: vec![ChatMessage::user("old")],
            preserved_messages: vec![ChatMessage::user("new")],
            previous_summary: None,
            history_excerpt: "old".to_string(),
            summary_images: Vec::new(),
        };
        let record = CompactionEvent {
            id: "c1".to_string(),
            trigger: CompactionTrigger::Manual,
            created_at: Local::now(),
            before_tokens: 100,
            after_tokens: 25,
            archived_message_count: 1,
            preserved_message_count: 1,
            preserved_turn_count: 1,
            summary_tokens: 10,
            duration_secs: 1.0,
            review_status: CompactionReviewStatus::DraftValidated,
            review_error: Some("provider overloaded".to_string()),
            focus: None,
            archive_path: None,
        };
        let messages = build_replacement_messages("## Goal\n- continue", &prepared, &record);
        let metadata = messages[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            metadata.get("review_status").and_then(|v| v.as_str()),
            Some("draft_validated")
        );
        assert_eq!(
            metadata.get("review_error").and_then(|v| v.as_str()),
            Some("provider overloaded")
        );
        assert!(messages[1].content.contains("structurally validated draft"));
    }
}
