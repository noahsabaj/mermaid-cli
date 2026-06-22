//! Single source of truth for slash commands. Used by:
//! - The palette widget (rendering + filtering)
//! - The dispatcher in `command_handler.rs` (validating known commands)
//! - The `/help` handler (rendering the command list)
//!
//! Adding a new command means adding one entry to `COMMAND_REGISTRY`
//! plus wiring its handler into the `match` in `handle_command`.
//! The palette + help text update automatically.

/// Metadata for one slash command. Names exclude the leading `/`.
#[derive(Debug, Clone, Copy)]
pub struct SlashCommand {
    /// Canonical command name without the leading `/`. Lowercase.
    pub name: &'static str,
    /// Alternative names that route to the same handler. Used by the
    /// dispatcher AND prefix filter — typing `/q` matches `/quit`.
    pub aliases: &'static [&'static str],
    /// One-line user-visible description shown in palette and `/help`.
    pub description: &'static str,
    /// Optional argument hint shown after the command name in the
    /// palette, e.g. `Some("[name]")` for `/model [name]`.
    pub arg_hint: Option<&'static str>,
    /// UX grouping used by `/help`; the palette still preserves registry order.
    pub group: SlashCommandGroup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommandGroup {
    Everyday,
    ModelContext,
    SafetyRecovery,
    Integrations,
    AdvancedRuntime,
}

impl SlashCommandGroup {
    pub fn title(self) -> &'static str {
        match self {
            SlashCommandGroup::Everyday => "Everyday",
            SlashCommandGroup::ModelContext => "Model and context",
            SlashCommandGroup::SafetyRecovery => "Safety and recovery",
            SlashCommandGroup::Integrations => "Integrations",
            SlashCommandGroup::AdvancedRuntime => "Advanced runtime",
        }
    }
}

pub const COMMAND_GROUPS: &[SlashCommandGroup] = &[
    SlashCommandGroup::Everyday,
    SlashCommandGroup::ModelContext,
    SlashCommandGroup::SafetyRecovery,
    SlashCommandGroup::Integrations,
    SlashCommandGroup::AdvancedRuntime,
];

/// Authoritative registry of all slash commands. Order here is the
/// order shown in the palette and `/help`.
pub const COMMAND_REGISTRY: &[SlashCommand] = &[
    SlashCommand {
        name: "model",
        aliases: &[],
        description: "Switch model (auto-pulls if needed) or show current",
        arg_hint: Some("[name]"),
        group: SlashCommandGroup::ModelContext,
    },
    SlashCommand {
        name: "reasoning",
        aliases: &[],
        description: "Set reasoning depth (none, minimal, low, medium, high, max, xhigh)",
        arg_hint: Some("[level]"),
        group: SlashCommandGroup::ModelContext,
    },
    SlashCommand {
        name: "visible-reasoning",
        aliases: &["visiblereasoning"],
        description: "Show, hide, or toggle reasoning blocks in the transcript",
        arg_hint: Some("[on|off|toggle]"),
        group: SlashCommandGroup::ModelContext,
    },
    SlashCommand {
        name: "clear",
        aliases: &[],
        description: "Clear chat history",
        arg_hint: None,
        group: SlashCommandGroup::Everyday,
    },
    SlashCommand {
        name: "save",
        aliases: &[],
        description: "Save current conversation",
        arg_hint: Some("[name]"),
        group: SlashCommandGroup::Everyday,
    },
    SlashCommand {
        name: "load",
        aliases: &[],
        description: "Load a conversation",
        arg_hint: Some("[name]"),
        group: SlashCommandGroup::Everyday,
    },
    SlashCommand {
        name: "list",
        aliases: &[],
        description: "List saved conversations",
        arg_hint: None,
        group: SlashCommandGroup::Everyday,
    },
    SlashCommand {
        name: "usage",
        aliases: &[],
        description: "Show provider token usage and session totals",
        arg_hint: None,
        group: SlashCommandGroup::ModelContext,
    },
    SlashCommand {
        name: "context",
        aliases: &[],
        description: "Show current context-window estimate and prompt budget",
        arg_hint: None,
        group: SlashCommandGroup::ModelContext,
    },
    SlashCommand {
        name: "compact",
        aliases: &["compress", "summarize"],
        description: "Compact conversation context with optional focus instructions",
        arg_hint: Some("[instructions]"),
        group: SlashCommandGroup::ModelContext,
    },
    SlashCommand {
        name: "memory",
        aliases: &["memories"],
        description: "List durable memories saved across sessions",
        arg_hint: None,
        group: SlashCommandGroup::ModelContext,
    },
    SlashCommand {
        name: "remember",
        aliases: &[],
        description: "Save a fact to private durable memory",
        arg_hint: Some("<fact>"),
        group: SlashCommandGroup::ModelContext,
    },
    SlashCommand {
        name: "forget",
        aliases: &[],
        description: "Delete a saved memory by name",
        arg_hint: Some("<name>"),
        group: SlashCommandGroup::ModelContext,
    },
    SlashCommand {
        name: "consolidate-memory",
        aliases: &["memory-consolidate", "prune-memory"],
        description: "Prune duplicate or obsolete memories (model-assisted, reversible)",
        arg_hint: None,
        group: SlashCommandGroup::ModelContext,
    },
    SlashCommand {
        name: "doctor",
        aliases: &[],
        description: "Show in-session readiness, model, safety, and instruction status",
        arg_hint: None,
        group: SlashCommandGroup::Everyday,
    },
    SlashCommand {
        name: "tasks",
        aliases: &[],
        description: "List durable runtime tasks",
        arg_hint: None,
        group: SlashCommandGroup::AdvancedRuntime,
    },
    SlashCommand {
        name: "task",
        aliases: &[],
        description: "Show a durable runtime task",
        arg_hint: Some("<id>"),
        group: SlashCommandGroup::AdvancedRuntime,
    },
    SlashCommand {
        name: "pause",
        aliases: &[],
        description: "Pause a durable task by marking it blocked",
        arg_hint: Some("<task-id>"),
        group: SlashCommandGroup::AdvancedRuntime,
    },
    SlashCommand {
        name: "resume",
        aliases: &[],
        description: "Resume a durable task by marking it running",
        arg_hint: Some("<task-id>"),
        group: SlashCommandGroup::AdvancedRuntime,
    },
    SlashCommand {
        name: "cancel",
        aliases: &[],
        description: "Cancel the active turn or a durable task",
        arg_hint: Some("[task-id]"),
        group: SlashCommandGroup::Everyday,
    },
    SlashCommand {
        name: "handoff",
        aliases: &[],
        description: "Write a handoff report for the current or named task",
        arg_hint: Some("[task-id]"),
        group: SlashCommandGroup::Everyday,
    },
    SlashCommand {
        name: "report",
        aliases: &[],
        description: "Show current context report or task report",
        arg_hint: Some("[task-id]"),
        group: SlashCommandGroup::Everyday,
    },
    SlashCommand {
        name: "processes",
        aliases: &["procs"],
        description: "List durable runtime processes",
        arg_hint: None,
        group: SlashCommandGroup::AdvancedRuntime,
    },
    SlashCommand {
        name: "logs",
        aliases: &[],
        description: "Show a durable runtime process log",
        arg_hint: Some("<process-id>"),
        group: SlashCommandGroup::AdvancedRuntime,
    },
    SlashCommand {
        name: "stop",
        aliases: &[],
        description: "Stop a durable runtime process",
        arg_hint: Some("<process-id>"),
        group: SlashCommandGroup::AdvancedRuntime,
    },
    SlashCommand {
        name: "restart",
        aliases: &[],
        description: "Restart a durable runtime process",
        arg_hint: Some("<process-id>"),
        group: SlashCommandGroup::AdvancedRuntime,
    },
    SlashCommand {
        name: "open",
        aliases: &[],
        description: "Open a URL, file, or process target",
        arg_hint: Some("<url|path|process-id>"),
        group: SlashCommandGroup::AdvancedRuntime,
    },
    SlashCommand {
        name: "ports",
        aliases: &[],
        description: "Show listening TCP ports",
        arg_hint: None,
        group: SlashCommandGroup::AdvancedRuntime,
    },
    SlashCommand {
        name: "safety",
        aliases: &["permission"],
        description: "Show or set the session safety mode (Shift+Tab also cycles)",
        arg_hint: Some("[read_only|ask|auto|full_access]"),
        group: SlashCommandGroup::SafetyRecovery,
    },
    SlashCommand {
        name: "approvals",
        aliases: &[],
        description: "List pending approvals",
        arg_hint: None,
        group: SlashCommandGroup::SafetyRecovery,
    },
    SlashCommand {
        name: "approve",
        aliases: &[],
        description: "Approve a pending action",
        arg_hint: Some("<approval-id>"),
        group: SlashCommandGroup::SafetyRecovery,
    },
    SlashCommand {
        name: "deny",
        aliases: &[],
        description: "Deny a pending action",
        arg_hint: Some("<approval-id>"),
        group: SlashCommandGroup::SafetyRecovery,
    },
    SlashCommand {
        name: "checkpoint",
        aliases: &[],
        description: "Create a restore checkpoint for one or more paths",
        arg_hint: Some("<path...>"),
        group: SlashCommandGroup::SafetyRecovery,
    },
    SlashCommand {
        name: "checkpoints",
        aliases: &[],
        description: "List restore checkpoints",
        arg_hint: None,
        group: SlashCommandGroup::SafetyRecovery,
    },
    SlashCommand {
        name: "restore",
        aliases: &[],
        description: "Restore a checkpoint",
        arg_hint: Some("<id>"),
        group: SlashCommandGroup::SafetyRecovery,
    },
    SlashCommand {
        name: "plugins",
        aliases: &[],
        description: "List installed plugins",
        arg_hint: None,
        group: SlashCommandGroup::Integrations,
    },
    SlashCommand {
        name: "model-info",
        aliases: &[],
        description: "Show provider/model capability information",
        arg_hint: Some("<model>"),
        group: SlashCommandGroup::ModelContext,
    },
    SlashCommand {
        name: "cloud-setup",
        aliases: &[],
        description: "Configure Ollama Cloud API key",
        arg_hint: None,
        group: SlashCommandGroup::Integrations,
    },
    SlashCommand {
        name: "help",
        aliases: &["h"],
        description: "Show command help",
        arg_hint: None,
        group: SlashCommandGroup::Everyday,
    },
    SlashCommand {
        name: "quit",
        aliases: &["q"],
        description: "Quit the application",
        arg_hint: None,
        group: SlashCommandGroup::Everyday,
    },
];

/// Filter the registry by a typed prefix (after stripping the leading
/// `/`). An empty prefix returns the full registry. Matches against the
/// canonical name AND any aliases — typing `/q` finds `quit` because
/// `q` is a `quit` alias. Result preserves registry order (stable).
pub fn filter_by_prefix(typed: &str) -> Vec<&'static SlashCommand> {
    let needle = typed.to_lowercase();
    if needle.is_empty() {
        return COMMAND_REGISTRY.iter().collect();
    }
    COMMAND_REGISTRY
        .iter()
        .filter(|cmd| {
            let name_matches = cmd.name == needle
                || (!cmd.name.contains('-') || needle.contains('-'))
                    && cmd.name.starts_with(&needle);
            name_matches || cmd.aliases.iter().any(|a| a.starts_with(&needle))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_by_prefix_empty_returns_all() {
        let result = filter_by_prefix("");
        assert_eq!(result.len(), COMMAND_REGISTRY.len());
        // Order preserved.
        assert_eq!(result[0].name, COMMAND_REGISTRY[0].name);
    }

    #[test]
    fn filter_by_prefix_exact_match() {
        let result = filter_by_prefix("model");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "model");
    }

    #[test]
    fn filter_by_prefix_partial_prefix_matches_one() {
        let result = filter_by_prefix("mod");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "model");
    }

    #[test]
    fn filter_by_prefix_no_match_returns_empty() {
        let result = filter_by_prefix("zzzzz");
        assert!(result.is_empty());
    }

    #[test]
    fn filter_by_prefix_matches_aliases() {
        // `/q` should find `quit` via its alias.
        let result = filter_by_prefix("q");
        assert!(
            result.iter().any(|c| c.name == "quit"),
            "expected quit in: {:?}",
            result.iter().map(|c| c.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn filter_by_prefix_is_case_insensitive() {
        // User shouldn't have to type lowercase /Q or /MODEL.
        let upper = filter_by_prefix("MODEL");
        assert_eq!(upper.len(), 1);
        assert_eq!(upper[0].name, "model");
    }

    #[test]
    fn registry_has_no_duplicate_names() {
        // Defensive: catches accidental duplicate entries during
        // registry maintenance. Duplicate names would route ambiguously
        // in the dispatcher.
        let mut names: Vec<&str> = COMMAND_REGISTRY.iter().map(|c| c.name).collect();
        names.sort_unstable();
        let len_before = names.len();
        names.dedup();
        assert_eq!(
            names.len(),
            len_before,
            "duplicate command name detected in COMMAND_REGISTRY"
        );
    }
}
