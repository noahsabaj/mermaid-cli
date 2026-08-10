//! The `/help`, `/doctor`, `/usage`, `/context`, `/todos`, `/tasks` and
//! `/runtime` report renderers.
//!
//! Every one is a pure `&State -> String`, and several already take a narrower
//! slice. They were ~640 contiguous lines inside `reducer.rs` carrying no
//! reducer semantics at all: no `Msg`, no `Cmd`, no mutation. A read-only
//! `&State` is exactly as pure as `transition::fill_outcome` — the tier-1 rule
//! is about mutation escaping, not argument width.

#![allow(
    unused_imports,
    reason = "the import block is shared verbatim with `reducer.rs`. Trimming \n     it per-file makes the three drift, and invites the automated pass that \n     broke this split twice."
)]

use crate::prompts::get_system_prompt;
use crate::{ProgressEvent, SubagentPhase};
use mermaid_model::models::{ChatMessage, MessageRole, ProviderContinuation, TokenUsage};
use mermaid_model::records::TaskStatus;

use super::action_display::action_display_for;
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
    commit_assistant_message, fill_outcome, start_generating, tool_result_messages,
    try_complete_outcomes,
};
use super::{COMMAND_GROUPS, COMMAND_REGISTRY};
use mermaid_model::ids::TurnId;

use super::reducer::*;
use super::request::*;

pub(crate) fn help_text(plugin_commands: &[crate::PluginCommand]) -> String {
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
                format!(" {hint}")
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
    for (keys, desc) in crate::slash_commands::KEYBINDINGS {
        lines.push(format!("  {keys} - {desc}"));
    }
    lines.join("\n")
}

pub(crate) fn doctor_text(state: &State) -> String {
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
            .filter(|entry| matches!(entry.status, crate::McpServerStatus::Ready))
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
pub(crate) fn latest_user_intent(session: &super::state::Session) -> Option<String> {
    const MAX: usize = 2000;
    session
        .messages()
        .iter()
        .rev()
        .find(|m| matches!(m.role, mermaid_model::models::MessageRole::User))
        .map(|m| {
            let c = m.content.trim();
            if c.len() > MAX {
                format!("{}…", &c[..c.floor_char_boundary(MAX)])
            } else {
                c.to_string()
            }
        })
}

pub(crate) fn usage_text(state: &State) -> String {
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
                    .map(|p| format!(" ({p}%, {source})"))
                    .unwrap_or_else(|| format!(" ({source})"))
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

/// Estimate the context the NEXT dispatch would carry, from the conversation
/// as it stands. Same computation `/context` shows, so the footer gauge and
/// `/context` never disagree.
///
/// Used wherever the transcript is rewritten out from under a
/// provider-reported figure (a rewind fork): the old number described messages
/// that no longer exist, but the new one is computable — so recompute it
/// rather than blanking the gauge.
pub(crate) fn estimate_current_context(state: &State) -> super::state::ContextUsageSnapshot {
    let request = build_chat_request(state);
    let max_context = state
        .session
        .context_usage
        .as_ref()
        .and_then(|snapshot| snapshot.max_tokens)
        .or(state.runtime.provider_capabilities.max_context_tokens);
    // The request carries MCP tools only; the effect runner appends the
    // built-in schemas at dispatch. Fold them in so the gauge matches what the
    // next call actually sends (the `/context` precedent).
    super::state::estimate_context_usage_for_request(&request, max_context)
        .with_additional_tokens(state.runtime.builtin_tool_schema_tokens)
}

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub(crate) fn context_text(state: &State) -> String {
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
        let num_predict =
            mermaid_model::models::adapters::ollama_sizing::default_ollama_num_predict(
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

    let policy = state.settings.compaction.policy();
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
        Err(reason @ crate::compaction::CompactionSkip::Suppressed) => reason.to_string(),
        Err(reason) => format!("not needed ({reason})"),
    };
    lines.push(format!("Auto compact: {auto_status}"));
    match &auto_skip {
        Ok(()) => lines.push(
            "Suggested action: continue normally; Mermaid will compact before the next model call."
                .to_string(),
        ),
        Err(crate::compaction::CompactionSkip::Suppressed) => lines.push(
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
                crate::CompactionReviewStatus::Reviewed => "reviewed".to_string(),
                crate::CompactionReviewStatus::DraftValidated => last
                    .review_error
                    .as_ref()
                    .map(|err| format!("validated draft ({err})"))
                    .unwrap_or_else(|| "validated draft".to_string()),
            }
        ));
        if let Some(path) = &last.archive_path {
            lines.push(format!("- archive: {path}"));
        }
        lines.push("- inspect: use the archive path above to review the raw messages Mermaid removed from context.".to_string());
    } else {
        lines.push(String::new());
        lines.push("Last compaction: none yet.".to_string());
    }

    lines.join("\n")
}

pub(crate) fn tasks_text(tasks: &[mermaid_model::records::TaskRecord]) -> String {
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

pub(crate) fn task_detail_text(
    task: Option<&mermaid_model::records::TaskRecord>,
    events: &[mermaid_model::records::TaskTimelineEvent],
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

pub(crate) fn processes_text(processes: &[mermaid_model::records::ProcessRecord]) -> String {
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
            lines.push(format!("  task: {task_id}"));
        }
        if let Some(url) = &process.detected_url {
            lines.push(format!("  url: {url}"));
        }
        if let Some(log_path) = &process.log_path {
            lines.push(format!("  log: {log_path}"));
        }
    }
    lines.join("\n")
}

pub(crate) fn approvals_text(approvals: &[mermaid_model::records::ApprovalRecord]) -> String {
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
            lines.push(format!("  checkpoint: {checkpoint_id}"));
        }
        if approval.pending_action_json.is_some() {
            lines.push("  pending action: recorded".to_string());
        }
    }
    lines.join("\n")
}

pub(crate) fn checkpoints_text(checkpoints: &[mermaid_model::records::CheckpointRecord]) -> String {
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

pub(crate) fn plugins_text(plugins: &[mermaid_model::records::PluginInstallRecord]) -> String {
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

pub(crate) fn usage_totals_line(usage: TokenUsageTotals) -> String {
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
