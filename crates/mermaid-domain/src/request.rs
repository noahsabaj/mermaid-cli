//! Building the outgoing `ChatRequest` from `State`.
//!
//! `build_chat_request` plus the four helpers only it uses: the system prompt,
//! the plan-capabilities line, stale-screenshot eviction, and neutralising
//! superseded policy denials so a `grep` hit that once tripped read-only mode
//! does not keep tripping it.

#![allow(
    unused_imports,
    reason = "the import block is shared verbatim with `reducer.rs`. Trimming \n     it per-file makes the three drift, and invites the automated pass that \n     broke this split twice."
)]

use crate::prompts::get_system_prompt;
use crate::{ProgressEvent, SubagentPhase};
use mermaid_model::models::{ChatMessage, MessageRole, ProviderContinuation, TokenUsage};
use mermaid_runtime::TaskStatus;

use super::cmd::{ChatRequest, Cmd};
use super::compaction::{
    CompactionArchive, CompactionRequest, CompactionResult, CompactionTrigger,
    context_exceeds_hard_limit, format_compact_count, should_auto_compact,
};
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
use mermaid_model::ids::TurnId;

use super::reducer::*;
use super::reports::*;

#[must_use]
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
            .filter(|m| m.kind != mermaid_model::models::ChatMessageKind::RunSummary)
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
    // `Plan` IS the read-only floor, so the live mode already describes
    // reality — there is no separate floor to substitute.
    neutralize_superseded_policy_denials(&mut messages, state.session.safety_mode);
    // Same contract for plan-mode denials: they stop applying the moment plan
    // mode ends (approve or cancel).
    neutralize_superseded_plan_denials(&mut messages, state.session.safety_mode.is_planning());
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
        // Plan mode hides the checklist WRITERS (their descriptions actively
        // recommend the call the gate then hard-errors); `task_list` stays
        // (post-compaction re-anchoring is legitimate while planning), and
        // the policy-gated tools (write_file, execute_command, …) stay too —
        // their teaching denials are part of the plan-mode surface. An
        // explicit `tasks = allow` in the plan profile restores the writers,
        // matching the runtime backstop in `tasks::plan_mode_block`.
        suppressed_builtin_tools: if checklist_writers_suppressed(state) {
            vec!["task_create", "task_update"]
        } else {
            Vec::new()
        },
    }
}

pub(crate) fn system_prompt_for_state(state: &State) -> String {
    // While planning, the base prompt's execution imperatives ("task_create
    // the FULL initial plan", "do not stop at a proposal") contradict the
    // plan appendix; swap them for plan-shaped stubs so the model never has
    // to resolve the conflict.
    //
    // The adaptation runs on the BASE prompt, BEFORE `append_system_prompt`
    // extras are appended. Running it on the rendered string let the section
    // splice — which extends to the next `\n## ` heading, or to end-of-string
    // when there is none — delete the user's appended instructions along with
    // the section. A base prompt whose last section is `## Task Planning` was
    // enough to silently drop every `append_system_prompt` entry while
    // planning. A fully custom base prompt misses the anchors and passes
    // through untouched, which is the intended behavior for user-owned text.
    let default_prompt = get_system_prompt();
    let chosen = state.settings.prompt.base_prompt(&default_prompt);
    let planning = state.session.safety_mode.is_planning();
    let base = if planning {
        state
            .settings
            .prompt
            .append_extras(&crate::prompts::adapt_prompt_for_plan_mode(chosen))
    } else {
        state.settings.prompt.append_extras(chosen)
    };
    // While a plan is being drafted the live-mode line would mislead ("attempt
    // gated actions") — the effective policy is the plan-mode read-only floor.
    // There is no restore target to name: plan is one position in the same
    // Shift+Tab cycle, and the user leaves it by picking another mode.
    let safety_line = if planning {
        "Safety mode: plan (the strictest mode: a plan is being drafted and the plan-mode \
         read-only floor is in effect; the user leaves it with Shift+Tab or /safety like any \
         other mode)."
            .to_string()
    } else {
        format!(
            "Safety mode: {} (live — the user can switch it anytime with Shift+Tab or /safety; \
             trust this over any earlier tool error, and attempt gated actions rather than \
             assuming they will fail).",
            state.session.safety_mode.as_str()
        )
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
pub(crate) fn plan_capabilities_line(perms: &crate::PlanPermissions) -> String {
    use crate::PlanPermLevel as L;
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
    line.push_str(", and authoring the plan file (write_file or apply_patch on the plan path — the ONLY writable path).");
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
pub(crate) fn evict_stale_screenshots(mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    use mermaid_model::constants::MAX_RETAINED_SCREENSHOTS;
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

/// The user loosening the safety mode (e.g. `read_only` → `full_access`) leaves the
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
///   [`mermaid_runtime::READ_ONLY_DENIAL_MARKER`]), so a `grep` hit that happens
///   to contain the marker text is not rewritten;
/// - it is a no-op in `read_only` (the denials still apply) and self-corrects if
///   the user toggles back down.
///
/// Runs on the CLONED request vec (like [`evict_stale_screenshots`]); the
/// on-screen transcript is untouched, and only `content` changes so the
/// `tool_use/tool_result` pairing is preserved.
pub(crate) fn neutralize_superseded_policy_denials(
    messages: &mut [ChatMessage],
    mode: mermaid_runtime::SafetyMode,
) {
    use mermaid_runtime::SafetyMode;
    // `Plan` carries the same read-only floor, so a read-only denial recorded
    // earlier still describes reality and must NOT be retired.
    if matches!(mode, SafetyMode::ReadOnly | SafetyMode::Plan) {
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
/// [`mermaid_runtime::READ_ONLY_DENIAL_MARKER`]. Matching the *contiguous* phrase
/// (not the bare marker) avoids rewriting a `grep` hit that merely contains the
/// marker text.
pub(crate) fn readonly_denial_signature() -> String {
    format!(
        "blocked by policy: {}",
        mermaid_runtime::READ_ONLY_DENIAL_MARKER
    )
}

/// True if the conversation still carries a read-only policy denial (a tool
/// result matching [`readonly_denial_signature`]) — used to decide whether a
/// loosening mode-switch is worth announcing.
pub(crate) fn history_has_readonly_denial(messages: &[ChatMessage]) -> bool {
    let signature = readonly_denial_signature();
    messages
        .iter()
        .any(|m| m.role == MessageRole::Tool && m.content.contains(&signature))
}
