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
}

/// Authoritative registry of all slash commands. Order here is the
/// order shown in the palette and `/help`.
pub const COMMAND_REGISTRY: &[SlashCommand] = &[
    SlashCommand {
        name: "model",
        aliases: &[],
        description: "Switch model (auto-pulls if needed) or show current",
        arg_hint: Some("[name]"),
    },
    SlashCommand {
        name: "reasoning",
        aliases: &[],
        description: "Set reasoning depth (none, minimal, low, medium, high, max, xhigh)",
        arg_hint: Some("[level]"),
    },
    SlashCommand {
        name: "clear",
        aliases: &[],
        description: "Clear chat history",
        arg_hint: None,
    },
    SlashCommand {
        name: "save",
        aliases: &[],
        description: "Save current conversation",
        arg_hint: Some("[name]"),
    },
    SlashCommand {
        name: "load",
        aliases: &[],
        description: "Load a conversation",
        arg_hint: Some("[name]"),
    },
    SlashCommand {
        name: "list",
        aliases: &[],
        description: "List saved conversations",
        arg_hint: None,
    },
    SlashCommand {
        name: "usage",
        aliases: &[],
        description: "Show provider token usage and session totals",
        arg_hint: None,
    },
    SlashCommand {
        name: "context",
        aliases: &[],
        description: "Show current context-window estimate and prompt budget",
        arg_hint: None,
    },
    SlashCommand {
        name: "compact",
        aliases: &["compress", "summarize"],
        description: "Compact conversation context with optional focus instructions",
        arg_hint: Some("[instructions]"),
    },
    SlashCommand {
        name: "cloud-setup",
        aliases: &[],
        description: "Configure Ollama Cloud API key",
        arg_hint: None,
    },
    SlashCommand {
        name: "help",
        aliases: &["h"],
        description: "Show command help",
        arg_hint: None,
    },
    SlashCommand {
        name: "quit",
        aliases: &["q"],
        description: "Quit the application",
        arg_hint: None,
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
            cmd.name.starts_with(&needle) || cmd.aliases.iter().any(|a| a.starts_with(&needle))
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
