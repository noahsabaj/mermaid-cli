//! Conversation context compaction.
//!
//! The reducer/effect boundary treats compaction as a first-class
//! operation: effects generate a checkpoint summary, the reducer swaps
//! the model-visible history, and persistence archives the removed raw
//! messages. This keeps compaction observable instead of hiding it inside
//! a provider adapter.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::constants::{
    COMPACTION_AUTO_THRESHOLD_PERCENT, COMPACTION_MAX_RESPONSE_RESERVE_TOKENS,
    COMPACTION_MIN_RESPONSE_RESERVE_TOKENS, COMPACTION_SUMMARIZER_INPUT_TOKEN_BUDGET,
    COMPACTION_SUMMARY_MAX_TOKENS, COMPACTION_TAIL_TOKEN_BUDGET, COMPACTION_TAIL_TURNS,
    COMPACTION_TOOL_OUTPUT_MAX_CHARS,
};
use crate::models::{ChatMessage, ChatMessageKind, MessageRole, ReasoningLevel, TokenUsage};

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
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::AutoThreshold => "auto_threshold",
            Self::ContextLimitRetry => "context_limit_retry",
            Self::TruncationRecovery => "truncation_recovery",
        }
    }

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

impl CompactionPolicy {
    pub fn response_reserve(self, request_max_tokens: usize) -> usize {
        request_max_tokens
            .max(self.min_response_reserve_tokens)
            .min(self.max_response_reserve_tokens)
    }
}

#[derive(Debug, Clone)]
pub struct CompactionRequest {
    pub chat: ChatRequest,
    pub trigger: CompactionTrigger,
    pub instructions: Option<String>,
    pub force: bool,
    pub policy: CompactionPolicy,
}

impl CompactionRequest {
    pub fn manual(chat: ChatRequest, instructions: Option<String>) -> Self {
        Self {
            chat,
            trigger: CompactionTrigger::Manual,
            instructions,
            force: true,
            policy: CompactionPolicy::default(),
        }
    }

    pub fn auto(chat: ChatRequest, trigger: CompactionTrigger) -> Self {
        Self {
            chat,
            trigger,
            instructions: None,
            force: false,
            policy: CompactionPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub id: String,
    pub trigger: CompactionTrigger,
    pub created_at: DateTime<Local>,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub archived_message_count: usize,
    pub preserved_message_count: usize,
    pub summary_tokens: usize,
    pub duration_secs: f64,
    #[serde(default = "default_verified")]
    pub verified: bool,
    #[serde(default)]
    pub verification_error: Option<String>,
    #[serde(default)]
    pub focus: Option<String>,
    #[serde(default)]
    pub archive_path: Option<String>,
}

fn default_verified() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionArchive {
    pub id: String,
    pub conversation_id: String,
    pub created_at: DateTime<Local>,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub record: CompactionRecord,
    pub replacement_messages: Vec<ChatMessage>,
    pub archived_messages: Vec<ChatMessage>,
    pub before_snapshot: ContextUsageSnapshot,
    pub after_snapshot: ContextUsageSnapshot,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone)]
pub struct PreparedCompaction {
    pub archived_messages: Vec<ChatMessage>,
    pub preserved_messages: Vec<ChatMessage>,
    pub previous_summary: Option<String>,
    pub history_excerpt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionSkip {
    NoKnownContextLimit,
    AutoDisabled,
    BelowThreshold,
    NothingToCompact,
}

impl std::fmt::Display for CompactionSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKnownContextLimit => write!(f, "model context limit is unknown"),
            Self::AutoDisabled => write!(f, "automatic compaction is disabled"),
            Self::BelowThreshold => write!(f, "context is below compaction threshold"),
            Self::NothingToCompact => write!(f, "not enough conversation history to summarize"),
        }
    }
}

pub fn should_auto_compact(
    snapshot: &ContextUsageSnapshot,
    request: &ChatRequest,
    policy: CompactionPolicy,
) -> Result<(), CompactionSkip> {
    if !policy.auto_enabled {
        return Err(CompactionSkip::AutoDisabled);
    }
    let Some(max_tokens) = snapshot.max_tokens else {
        return Err(CompactionSkip::NoKnownContextLimit);
    };
    if max_tokens == 0 {
        return Err(CompactionSkip::NoKnownContextLimit);
    }

    let reserve = policy.response_reserve(request.max_tokens);
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

pub fn context_exceeds_hard_limit(
    snapshot: &ContextUsageSnapshot,
    request: &ChatRequest,
    policy: CompactionPolicy,
) -> bool {
    let Some(max_tokens) = snapshot.max_tokens else {
        return false;
    };
    let reserve = policy.response_reserve(request.max_tokens);
    snapshot.used_tokens.saturating_add(reserve) >= max_tokens
}

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

    let max_input_tokens = max_context_tokens
        .map(|max| max.saturating_sub(request.policy.response_reserve(request.chat.max_tokens)))
        .filter(|max| *max > 0)
        .unwrap_or(request.policy.summarizer_input_token_budget)
        .min(request.policy.summarizer_input_token_budget);
    let max_chars = max_input_tokens.saturating_mul(4).max(4_000);
    let history_excerpt = truncate_middle(
        &format_history_excerpt(&archived_messages, request.policy),
        max_chars,
    );

    Ok(PreparedCompaction {
        archived_messages,
        preserved_messages,
        previous_summary,
        history_excerpt,
    })
}

pub fn build_summary_request(
    base: &ChatRequest,
    prepared: &PreparedCompaction,
    focus: Option<&str>,
    policy: CompactionPolicy,
) -> ChatRequest {
    ChatRequest {
        model_id: base.model_id.clone(),
        messages: vec![ChatMessage::user(summary_prompt(prepared, focus))],
        system_prompt: compaction_system_prompt().to_string(),
        instructions: None,
        reasoning: compaction_reasoning(base.reasoning),
        temperature: 0.0,
        max_tokens: policy.summary_max_tokens,
        tools: Vec::new(),
        ollama_num_ctx: base.ollama_num_ctx,
        ollama_allow_ram_offload: base.ollama_allow_ram_offload,
    }
}

pub fn build_verification_request(
    base: &ChatRequest,
    prepared: &PreparedCompaction,
    draft_summary: &str,
    focus: Option<&str>,
    policy: CompactionPolicy,
) -> ChatRequest {
    let prompt = format!(
        "{}\n\n# Draft Summary\n{}\n\n# Verification Task\nCritically check the draft against the conversation excerpt. If it omitted specific file paths, commands, test results, tool results, user constraints, current state, or next steps, return an improved complete checkpoint. Otherwise return the draft unchanged. Return only the final checkpoint markdown.",
        summary_prompt(prepared, focus),
        draft_summary.trim()
    );
    ChatRequest {
        model_id: base.model_id.clone(),
        messages: vec![ChatMessage::user(prompt)],
        system_prompt: compaction_system_prompt().to_string(),
        instructions: None,
        reasoning: compaction_reasoning(base.reasoning),
        temperature: 0.0,
        max_tokens: policy.summary_max_tokens,
        tools: Vec::new(),
        ollama_num_ctx: base.ollama_num_ctx,
        ollama_allow_ram_offload: base.ollama_allow_ram_offload,
    }
}

pub fn build_replacement_messages(
    summary: &str,
    prepared: &PreparedCompaction,
    record: &CompactionRecord,
) -> Vec<ChatMessage> {
    // The summary is model-generated from the full conversation and is persisted
    // (replacement message + conversation file). Scrub any credential it echoed
    // back from the archived turns before it's written (#70).
    let summary = crate::utils::redact_secrets(summary);
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
        "duration_secs": record.duration_secs,
        "verified": record.verified,
        "verification_error": record.verification_error,
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

pub fn compaction_receipt(record: &CompactionRecord) -> String {
    let verification = if record.verified {
        "Verified.".to_string()
    } else if let Some(error) = &record.verification_error {
        format!("Used draft summary because verification failed: {error}.")
    } else {
        "Used draft summary without verification.".to_string()
    };
    format!(
        "Context compacted: {} -> {} tokens, archived {} messages, preserved {} messages, took {:.1}s. {} I will continue from this checkpoint.",
        format_compact_count(record.before_tokens),
        format_compact_count(record.after_tokens),
        record.archived_message_count,
        record.preserved_message_count,
        record.duration_secs,
        verification
    )
}

pub fn normalize_summary(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(summary) = extract_tagged_summary(trimmed) {
        return summary.trim().to_string();
    }
    trimmed.to_string()
}

pub fn combine_usage(a: Option<TokenUsage>, b: Option<TokenUsage>) -> Option<TokenUsage> {
    match (a, b) {
        (None, None) => None,
        (Some(u), None) | (None, Some(u)) => Some(u),
        (Some(mut left), Some(right)) => {
            left.prompt_tokens = left.prompt_tokens.saturating_add(right.prompt_tokens);
            left.completion_tokens = left
                .completion_tokens
                .saturating_add(right.completion_tokens);
            left.total_tokens = left.total_tokens.saturating_add(right.total_tokens);
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

pub fn format_compact_count(value: usize) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
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
fn drop_orphan_tool_calls(messages: &mut Vec<ChatMessage>, preserve_pending_tail: bool) {
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

fn format_history_excerpt(messages: &[ChatMessage], policy: CompactionPolicy) -> String {
    let mut out = String::new();
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
            out.push_str(&format!("tool_name: {}\n", name));
        }
        if let Some(id) = &msg.tool_call_id {
            out.push_str(&format!("tool_call_id: {}\n", id));
        }
        if let Some(calls) = &msg.tool_calls {
            let names: Vec<&str> = calls
                .iter()
                .map(|call| call.function.name.as_str())
                .collect();
            out.push_str(&format!("tool_calls: {}\n", names.join(", ")));
        }
        if let Some(images) = &msg.images
            && !images.is_empty()
        {
            out.push_str(&format!("[{} image attachment(s) omitted]\n", images.len()));
        }
        for action in &msg.actions {
            out.push_str(&format!(
                "action: {}({}) duration={:?}\n",
                action.action_type, action.target, action.duration_seconds
            ));
            if let Some(metadata) = &action.metadata {
                out.push_str(&format!("action_metadata: {:?}\n", metadata));
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
        }
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
    fn prepare_preserves_recent_two_user_turns() {
        let messages = vec![
            ChatMessage::user("one"),
            ChatMessage::assistant("one answer"),
            ChatMessage::user("two"),
            ChatMessage::assistant("two answer"),
            ChatMessage::user("three"),
        ];
        let request = CompactionRequest::manual(request_with(messages), None);
        let prepared = prepare_compaction(&request, Some(100_000)).expect("prepared");
        assert_eq!(prepared.archived_messages.len(), 2);
        assert_eq!(prepared.preserved_messages.len(), 3);
        assert_eq!(prepared.preserved_messages[0].content, "two");
    }

    fn tool_call(id: &str, name: &str) -> crate::models::tool_call::ToolCall {
        crate::models::tool_call::ToolCall {
            id: Some(id.to_string()),
            function: crate::models::tool_call::FunctionCall {
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
        let request = CompactionRequest::manual(request_with(messages), None);
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
        let request = CompactionRequest::manual(request_with(messages), None);
        let prepared = prepare_compaction(&request, Some(100_000)).expect("prepared");
        let kept = prepared
            .preserved_messages
            .iter()
            .any(|m| m.tool_calls.as_ref().is_some_and(|c| !c.is_empty()));
        assert!(kept, "a tool_call paired with its result must be preserved");
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
        let request = CompactionRequest::manual(request_with(messages), None);
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
        let request =
            CompactionRequest::auto(request_with(messages), CompactionTrigger::ContextLimitRetry);
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
        let request = CompactionRequest::manual(request_with(messages), None);
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
        };
        let record = CompactionRecord {
            id: "c1".to_string(),
            trigger: CompactionTrigger::Manual,
            created_at: Local::now(),
            before_tokens: 100,
            after_tokens: 25,
            archived_message_count: 1,
            preserved_message_count: 1,
            summary_tokens: 10,
            duration_secs: 1.0,
            verified: true,
            verification_error: None,
            focus: None,
            archive_path: None,
        };
        let messages = build_replacement_messages("## Goal\n- continue", &prepared, &record);
        assert_eq!(messages[0].kind, ChatMessageKind::ContextCheckpoint);
        assert!(messages[0].content.contains(CHECKPOINT_MARKER));
        assert_eq!(messages[2].content, "new");
    }

    #[test]
    fn replacement_metadata_records_verification_status() {
        let prepared = PreparedCompaction {
            archived_messages: vec![ChatMessage::user("old")],
            preserved_messages: vec![ChatMessage::user("new")],
            previous_summary: None,
            history_excerpt: "old".to_string(),
        };
        let record = CompactionRecord {
            id: "c1".to_string(),
            trigger: CompactionTrigger::Manual,
            created_at: Local::now(),
            before_tokens: 100,
            after_tokens: 25,
            archived_message_count: 1,
            preserved_message_count: 1,
            summary_tokens: 10,
            duration_secs: 1.0,
            verified: false,
            verification_error: Some("provider overloaded".to_string()),
            focus: None,
            archive_path: None,
        };
        let messages = build_replacement_messages("## Goal\n- continue", &prepared, &record);
        let metadata = messages[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            metadata.get("verified").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            metadata.get("verification_error").and_then(|v| v.as_str()),
            Some("provider overloaded")
        );
        assert!(messages[1].content.contains("Used draft summary"));
    }
}
