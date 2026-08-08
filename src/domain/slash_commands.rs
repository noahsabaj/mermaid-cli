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

/// Authoritative list of keyboard shortcuts (key → description), surfaced in
/// `/help` beside the commands and reusable for a future `?` overlay. Kept in
/// sync by hand with the bindings in `reducer::handle_key`. Plain text only.
pub const KEYBINDINGS: &[(&str, &str)] = &[
    ("Enter", "Submit the prompt"),
    ("Ctrl+J", "Insert a newline (multi-line input)"),
    ("Esc", "Interrupt the current turn"),
    ("Up / Down", "Browse input history"),
    ("PageUp / PageDown", "Scroll the transcript"),
    ("Shift+Up / Shift+Down", "Scroll the transcript one line"),
    ("End", "Jump to the newest message"),
    ("Ctrl+V", "Paste (including images)"),
    ("Ctrl+O", "Compose the prompt in $VISUAL/$EDITOR"),
    ("Ctrl+B", "Background a running command"),
    ("Ctrl+T", "Expand or collapse the task checklist"),
    ("Alt+T", "Cycle reasoning depth"),
    ("Shift+Tab", "Cycle the safety mode (plan is one of them)"),
    ("Ctrl+C", "Cancel, then exit"),
    ("Ctrl+D", "Quit"),
];

/// Authoritative registry of all slash commands. Order here is the
/// order shown in the palette and `/help`.
pub const COMMAND_REGISTRY: &[SlashCommand] = &[
    SlashCommand {
        name: "model",
        aliases: &[],
        description: "Open the model picker, or switch directly with a name",
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
        name: "todos",
        aliases: &["todo"],
        description: "Show or edit the task checklist",
        arg_hint: Some("[add <subject>|rm <id>|done <id>|clear]"),
        group: SlashCommandGroup::Everyday,
    },
    SlashCommand {
        name: "scratchpad",
        aliases: &[],
        description: "Show the session scratch directory and its contents",
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
        description: "Show context window/budget; set Ollama num_ctx (Ollama auto-fits to VRAM)",
        arg_hint: Some("[n|auto|max|offload on|off]"),
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
        name: "agents",
        aliases: &[],
        description: "List or kill background agents",
        arg_hint: Some("[kill <id>|all]"),
        group: SlashCommandGroup::AdvancedRuntime,
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
        arg_hint: Some("[plan|read_only|ask|auto|full_access]"),
        group: SlashCommandGroup::SafetyRecovery,
    },
    SlashCommand {
        name: "plan",
        aliases: &[],
        description: "Enter or leave plan mode (Shift+Tab cycles into it too)",
        arg_hint: Some("[off|show|config]"),
        group: SlashCommandGroup::SafetyRecovery,
    },
    SlashCommand {
        name: "config",
        aliases: &[],
        description: "Open the settings picker (plan mode section)",
        arg_hint: None,
        group: SlashCommandGroup::Everyday,
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
        name: "theme",
        aliases: &[],
        description: "Switch the color theme or show the current one",
        arg_hint: Some("[dark|light]"),
        group: SlashCommandGroup::Everyday,
    },
    SlashCommand {
        name: "editor",
        aliases: &[],
        description: "Compose the prompt in $VISUAL/$EDITOR (Ctrl+O)",
        arg_hint: None,
        group: SlashCommandGroup::Everyday,
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

/// One row of the slash palette: a built-in registry command or a
/// plugin-contributed prompt command. Unifying them in ONE list, produced
/// by ONE function ([`filter_entries`]), keeps the palette widget, the
/// row-count layout, and the reducer's cursor/Tab handling agreeing on
/// indices.
pub enum PaletteEntry<'a> {
    Builtin(&'static SlashCommand),
    Plugin(&'a crate::domain::PluginCommand),
}

impl PaletteEntry<'_> {
    pub fn name(&self) -> &str {
        match self {
            PaletteEntry::Builtin(c) => c.name,
            PaletteEntry::Plugin(p) => &p.name,
        }
    }

    /// Palette/hint description; plugin rows carry their origin.
    pub fn description(&self) -> String {
        match self {
            PaletteEntry::Builtin(c) => c.description.to_string(),
            PaletteEntry::Plugin(p) => {
                if p.description.is_empty() {
                    format!("(plugin:{})", p.plugin)
                } else {
                    format!("{} (plugin:{})", p.description, p.plugin)
                }
            },
        }
    }

    pub fn arg_hint(&self) -> Option<&'static str> {
        match self {
            PaletteEntry::Builtin(c) => c.arg_hint,
            PaletteEntry::Plugin(_) => Some("[args]"),
        }
    }
}

/// The palette's single source of truth: built-ins (registry order) then
/// plugin commands (already name-sorted by the loader), both prefix-filtered.
/// EVERY palette consumer (widget rows, layout row count, reducer cursor)
/// must use this so their indices agree.
pub fn filter_entries<'a>(
    typed: &str,
    plugin: &'a [crate::domain::PluginCommand],
) -> Vec<PaletteEntry<'a>> {
    let needle = typed.to_lowercase();
    let mut entries: Vec<PaletteEntry<'a>> = filter_by_prefix(typed)
        .into_iter()
        .map(PaletteEntry::Builtin)
        .collect();
    entries.extend(
        plugin
            .iter()
            .filter(|p| needle.is_empty() || p.name.starts_with(&needle))
            .map(PaletteEntry::Plugin),
    );
    entries
}

/// Filter the registry by a typed prefix (after stripping the leading
/// `/`). An empty prefix returns the full registry. Matches against the
/// canonical name AND any aliases — typing `/q` finds `quit` because
/// `q` is a `quit` alias. Result preserves registry order (stable).
pub fn filter_by_prefix(typed: &str) -> Vec<&'static SlashCommand> {
    let needle = typed.to_lowercase();
    if needle.is_empty() {
        return COMMAND_REGISTRY.iter().collect();
    }
    // Plain prefix match against the canonical name and aliases. Typing the
    // first word of a hyphenated command (`consolidate`) must reveal it
    // (`consolidate-memory`) — an earlier carve-out that hid hyphenated names
    // from hyphenless prefixes broke that for commands without a hyphenless
    // alias.
    COMMAND_REGISTRY
        .iter()
        .filter(|cmd| {
            cmd.name.starts_with(&needle) || cmd.aliases.iter().any(|a| a.starts_with(&needle))
        })
        .collect()
}

/// Parse a slash-command input line (without the leading `/`) into a
/// `SlashCmd`. Returns `SlashCmd::Unknown` if the command isn't in
/// the registry. Shared between the TUI dispatcher (C8) and any
/// non-interactive command dispatch.
pub fn parse_slash_command(raw: &str) -> crate::domain::SlashCmd {
    use crate::domain::SlashCmd;
    let trimmed = raw.trim();
    let (name, arg) = match trimmed.split_once(' ') {
        Some((n, a)) => (n.to_lowercase(), Some(a.trim().to_string())),
        None => (trimmed.to_lowercase(), None),
    };

    // Route through the registry so command aliases (/q → /quit) work.
    use COMMAND_REGISTRY;
    let canonical = COMMAND_REGISTRY
        .iter()
        .find(|c| c.name == name.as_str() || c.aliases.contains(&name.as_str()))
        .map(|c| c.name);

    match canonical {
        Some("model") => SlashCmd::Model(arg),
        Some("reasoning") => match arg.as_deref() {
            None => SlashCmd::Reasoning(None),
            Some(level) => SlashCmd::Reasoning(mermaid_model::models::ReasoningLevel::parse(level)),
        },
        Some("visible-reasoning") => SlashCmd::VisibleReasoning(arg),
        Some("safety") => match arg.as_deref() {
            None => SlashCmd::Safety(None),
            // Invalid value ⇒ `None` ⇒ the reducer shows current + options.
            Some(mode) => {
                SlashCmd::Safety(mermaid_runtime::SafetyMode::parse(&mode.to_lowercase()))
            },
        },
        Some("plan") => SlashCmd::Plan(arg),
        Some("config") => SlashCmd::Config,
        Some("clear") => SlashCmd::Clear,
        Some("save") => SlashCmd::Save(arg),
        Some("load") => SlashCmd::Load(arg),
        Some("list") => SlashCmd::List,
        Some("usage") => SlashCmd::Usage,
        Some("todos") => SlashCmd::Todos(arg),
        Some("scratchpad") => SlashCmd::Scratchpad,
        Some("context") => {
            use crate::domain::ContextCmd;
            let a = arg.as_deref().map(str::trim);
            SlashCmd::Context(match a {
                None | Some("") => ContextCmd::Show,
                Some("auto") => ContextCmd::Auto,
                Some("max") | Some("full") => ContextCmd::Max,
                Some(s) => {
                    if let Some(rest) = s.strip_prefix("offload") {
                        match rest.trim() {
                            "on" | "true" | "enable" | "yes" => ContextCmd::Offload(true),
                            "off" | "false" | "disable" | "no" | "" => ContextCmd::Offload(false),
                            // "offload garbage" → just show.
                            _ => ContextCmd::Show,
                        }
                    } else if let Ok(n) = s.parse::<u32>() {
                        ContextCmd::Set(n)
                    } else {
                        // Unrecognized arg → show (self-documenting report).
                        ContextCmd::Show
                    }
                },
            })
        },
        Some("compact") => SlashCmd::Compact(arg),
        Some("memory") => SlashCmd::Memory,
        Some("remember") => SlashCmd::Remember(arg),
        Some("forget") => SlashCmd::Forget(arg),
        Some("consolidate-memory") => SlashCmd::ConsolidateMemory,
        Some("doctor") => SlashCmd::Doctor,
        Some("tasks") => SlashCmd::Tasks,
        Some("task") => SlashCmd::Task(arg),
        Some("pause") => SlashCmd::Pause(arg),
        Some("resume") => SlashCmd::Resume(arg),
        Some("cancel") => SlashCmd::Cancel(arg),
        Some("handoff") => SlashCmd::Handoff(arg),
        Some("report") => SlashCmd::Report(arg),
        Some("agents") => SlashCmd::Agents(arg),
        Some("processes") => SlashCmd::Processes,
        Some("logs") => SlashCmd::Logs(arg),
        Some("stop") => SlashCmd::Stop(arg),
        Some("restart") => SlashCmd::Restart(arg),
        Some("open") => SlashCmd::Open(arg),
        Some("ports") => SlashCmd::Ports,
        Some("approvals") => SlashCmd::Approvals,
        Some("approve") => SlashCmd::Approve(arg),
        Some("deny") => SlashCmd::Deny(arg),
        Some("checkpoint") => SlashCmd::Checkpoint(arg),
        Some("checkpoints") => SlashCmd::Checkpoints,
        Some("restore") => SlashCmd::Restore(arg),
        Some("plugins") => SlashCmd::Plugins,
        Some("model-info") => SlashCmd::ModelInfo(arg),
        Some("cloud-setup") => SlashCmd::CloudSetup,
        Some("theme") => SlashCmd::Theme(arg),
        Some("editor") => SlashCmd::Editor,
        Some("help") => SlashCmd::Help,
        Some("quit") => SlashCmd::Quit,
        _ => SlashCmd::Unknown(name),
    }
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
    fn filter_by_prefix_includes_exact_name_first() {
        // Prefix match surfaces `model` (and any longer `model-*`); the exact
        // name stays first by registry order.
        let result = filter_by_prefix("model");
        assert!(result.iter().any(|c| c.name == "model"));
        assert_eq!(result[0].name, "model");
    }

    #[test]
    fn filter_by_prefix_partial_prefix_includes_model() {
        let result = filter_by_prefix("mod");
        assert!(result.iter().any(|c| c.name == "model"));
    }

    #[test]
    fn filter_by_prefix_matches_hyphenated_command_by_first_word() {
        // Regression: typing the first word of a hyphenated command must
        // reveal it, even before the hyphen — and at shorter prefixes too.
        // Previously `/consolidate` showed nothing until `/consolidate-`.
        assert!(
            filter_by_prefix("consolidate")
                .iter()
                .any(|c| c.name == "consolidate-memory"),
            "/consolidate must surface /consolidate-memory"
        );
        assert!(
            filter_by_prefix("conso")
                .iter()
                .any(|c| c.name == "consolidate-memory")
        );
        // Other hyphenated commands too.
        assert!(
            filter_by_prefix("cloud")
                .iter()
                .any(|c| c.name == "cloud-setup")
        );
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
        assert!(upper.iter().any(|c| c.name == "model"));
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
