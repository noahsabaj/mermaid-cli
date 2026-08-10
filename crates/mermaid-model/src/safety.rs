//! The safety-policy vocabulary: modes, risk classes, requests, decisions.
//!
//! Pure data and its immediate logic -- no classification, no engine. It
//! lives in this bottom crate so the pure MVU core (`mermaid-domain`) can
//! speak safety modes and floors without depending on `mermaid-runtime`
//! (whose manifest carries rusqlite and the OS surface). The engine that
//! folds this vocabulary into a verdict is `mermaid-runtime`'s
//! `policy::engine`; the shell classifier that feeds it lives beside it,
//! and `mermaid-runtime` re-exports these names for its own API surface.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Marker embedded verbatim in every read-only policy-denial `reason` (see
/// the runtime engine's `PolicyEngine::decide`). Exposed so the
/// message-history layer can detect a denial that a since-loosened safety
/// mode has superseded, without re-hardcoding the wording in a second place.
pub const READ_ONLY_DENIAL_MARKER: &str = "read-only safety mode";

/// Marker embedded verbatim in every plan-mode policy-denial `reason` (the
/// policy gate rewrites the read-only mode-default deny to a plan-flavored one
/// while a plan is being drafted). Sibling of [`READ_ONLY_DENIAL_MARKER`]: the
/// message-history layer matches `"blocked by policy: "` + this marker to
/// neutralize denials once plan mode ends.
pub const PLAN_DENIAL_MARKER: &str = "plan mode";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyMode {
    /// A plan is being drafted: a read-only floor plus the plan-mode
    /// carve-outs the policy gate layers on (the plan file is writable,
    /// `[plan]` permissions may re-open memory/builds/web).
    ///
    /// Plan is a MODE, not a flag alongside one, and it is a full position in
    /// the Shift+Tab cycle — the strictest one. It used to be a separate
    /// `Session.plan: Option<_>` orthogonal to `safety_mode`, which meant the
    /// two could disagree: Shift+Tab while planning set `full_access` and the
    /// harness then told the model "safety mode changed to `full_access`" while
    /// the plan read-only floor was still in force — a contradiction the model
    /// resolved by attempting mutations and collecting denials. With one mode
    /// value that state is unrepresentable. `Session.plan` still carries the
    /// plan DATA (path, saved overrides), never the fact of being in plan mode,
    /// and it never carries a mode to "restore": leaving plan means picking
    /// another mode, like leaving any other.
    Plan,
    ReadOnly,
    #[default]
    Ask,
    Auto,
    FullAccess,
}

impl SafetyMode {
    /// Canonical serialized name — matches the serde `snake_case` rename.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::ReadOnly => "read_only",
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::FullAccess => "full_access",
        }
    }

    /// Parse a canonical mode name. Accepts ONLY the canonical `snake_case`
    /// names — no legacy aliases (the old `"auto_review"` is gone).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "plan" => Some(Self::Plan),
            "read_only" => Some(Self::ReadOnly),
            "ask" => Some(Self::Ask),
            "auto" => Some(Self::Auto),
            "full_access" => Some(Self::FullAccess),
            _ => None,
        }
    }

    /// Is a plan being drafted? The single source of truth — never infer this
    /// from `Session.plan`, which is the plan's DATA and outlives nothing.
    #[must_use]
    pub fn is_planning(self) -> bool {
        matches!(self, Self::Plan)
    }

    /// Permissiveness rank for combining modes: `plan/read_only` are strictest,
    /// `full_access` loosest. Plan ranks below read-only because its carve-outs
    /// only ever open paths the gate re-checks, and a subagent must never
    /// inherit "planning" as a ceiling (children explore, they don't plan).
    #[must_use]
    pub fn permissiveness(self) -> u8 {
        match self {
            Self::Plan => 0,
            Self::ReadOnly => 1,
            Self::Ask => 2,
            Self::Auto => 3,
            Self::FullAccess => 4,
        }
    }

    /// The stricter of two modes. Used to apply an agent type's safety
    /// ceiling to a session's live mode — a ceiling can only tighten what
    /// the parent already allows, never loosen it.
    #[must_use]
    pub fn least_permissive(a: Self, b: Self) -> Self {
        if a.permissiveness() <= b.permissiveness() {
            a
        } else {
            b
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    Read,
    Edit,
    Shell,
    Web,
    ExternalDirectory,
    ComputerUse,
    Mcp,
    Subagent,
    Network,
    Git,
    Process,
    /// Agent-owned durable memory writes. Ungated in every mode except
    /// read-only (see `decide`); transparency comes from the surfaced
    /// transcript action, the plain editable files, and git for shared.
    Memory,
}

impl ToolCategory {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Memory => "memory",
            Self::Edit => "edit",
            Self::Shell => "shell",
            Self::Web => "web",
            Self::ExternalDirectory => "external_directory",
            Self::ComputerUse => "computer_use",
            Self::Mcp => "mcp",
            Self::Subagent => "subagent",
            Self::Network => "network",
            Self::Git => "git",
            Self::Process => "process",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    ReadOnly,
    LowMutation,
    FileMutation,
    ShellMutation,
    Network,
    Process,
    ExternalAccess,
    /// Machine-scoped package operations (`npm -g`, `cargo install`,
    /// `pip install`, `brew`/`apt`/`winget` installs): they mutate the
    /// MACHINE, not the project — outside checkpoint reach, visible to every
    /// other project — so the `system_installs` floor vets them even in
    /// `full_access`. Project-local installs (`npm install`, `cargo add`)
    /// deliberately stay Process.
    SystemMutation,
    Destructive,
}

impl RiskClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::LowMutation => "low_mutation",
            Self::FileMutation => "file_mutation",
            Self::ShellMutation => "shell_mutation",
            Self::Network => "network",
            Self::Process => "process",
            Self::ExternalAccess => "external_access",
            Self::SystemMutation => "system_mutation",
            Self::Destructive => "destructive",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRequest {
    pub tool: String,
    pub category: ToolCategory,
    pub summary: String,
    pub command: Option<String>,
    pub path: Option<String>,
    /// Complete structured tool arguments. Treat as untrusted input and redact
    /// before sending it to an external classifier or persistence sink.
    pub arguments: Option<serde_json::Value>,
    /// For `ToolCategory::Mcp` only: the server-advertised `readOnlyHint`.
    /// UNTRUSTED (servers self-declare), so it can only keep a read at the
    /// permissiveness every MCP tool had before the external-writes floor
    /// existed — it never grants more than the safety mode gives. `false`
    /// (the default, and every unannotated tool) means write-shaped and
    /// subject to the floor.
    pub mcp_read_only_hint: bool,
    /// The directory `command` will actually run in, when that is not the
    /// project root — i.e. an explicit `working_dir` argument.
    ///
    /// Relative paths in a command resolve against THIS, not the project root.
    /// The gate used to match the plan-file carve-out against the project root
    /// while the shell ran the command elsewhere, so
    /// `execute_command{command: "echo … > .mermaid/plans/x.md",
    /// working_dir: "other/tree"}` was approved as a plan write and landed
    /// somewhere else entirely. Carrying the cwd on the request keeps the
    /// wrong value out of reach: see [`ActionRequest::resolve_dir`].
    pub cwd: Option<std::path::PathBuf>,
}

impl ActionRequest {
    pub fn new(
        tool: impl Into<String>,
        category: ToolCategory,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            tool: tool.into(),
            category,
            summary: summary.into(),
            command: None,
            path: None,
            arguments: None,
            mcp_read_only_hint: false,
            cwd: None,
        }
    }

    /// The directory command-relative paths must resolve against: the
    /// request's own cwd when it has one, else `fallback` (the project root).
    #[must_use]
    pub fn resolve_dir<'a>(&'a self, fallback: &'a Path) -> &'a Path {
        self.cwd.as_deref().unwrap_or(fallback)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow {
        risk: RiskClass,
        checkpoint: bool,
    },
    Ask {
        risk: RiskClass,
        checkpoint: bool,
    },
    /// Auto mode only: a borderline action the rule engine won't decide
    /// alone. The caller (the `mermaid-cli` policy gate) resolves it by
    /// asking the LLM classifier to vet the action against the user's
    /// intent — aligned ⇒ proceed, otherwise escalate to a human approval.
    /// The runtime crate stays model-free; it only signals "needs vetting".
    Classify {
        risk: RiskClass,
        checkpoint: bool,
    },
    Deny {
        risk: RiskClass,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyOverrideDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyOverride {
    pub category: Option<ToolCategory>,
    pub tool: Option<String>,
    pub pattern: Option<String>,
    pub decision: PolicyOverrideDecision,
    pub checkpoint: Option<bool>,
    pub reason: Option<String>,
}

impl Default for PolicyOverride {
    fn default() -> Self {
        Self {
            category: None,
            tool: None,
            pattern: None,
            decision: PolicyOverrideDecision::Ask,
            checkpoint: None,
            reason: None,
        }
    }
}

impl PolicyDecision {
    #[must_use]
    pub fn risk(&self) -> RiskClass {
        match self {
            Self::Allow { risk, .. }
            | Self::Ask { risk, .. }
            | Self::Classify { risk, .. }
            | Self::Deny { risk, .. } => *risk,
        }
    }

    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Allow { .. } => "allow",
            Self::Ask { .. } => "ask",
            Self::Classify { .. } => "classify",
            Self::Deny { .. } => "deny",
        }
    }
}

/// Enforcement floor for actions whose blast radius exceeds the project:
/// write-shaped MCP tools (`external_writes`) and machine-scoped package
/// operations (`system_installs`). Safety mode alone never authorizes them:
/// the mode's decision is strengthened to at least this level (severity
/// order `Allow < Auto < Ask < Deny`). Default `Auto`: the intent
/// classifier vets the call against the user's request — aligned runs
/// silently, off-task escalates — even in `full_access`. `allow` restores
/// the old unconditional-allow behavior per knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FloorLevel {
    Allow,
    #[default]
    Auto,
    Ask,
    Deny,
}

/// Which shell `execute_command` hands model commands to on this host.
///
/// THE single answer to "what interpreter runs shell commands?": the exec
/// tool's spawn (`shell_invocation`), risk classification
/// (`classify_command_for`), the plan-mode carve-outs
/// (`is_plan_safe_build_command`, `is_plan_file_only_write`), and the
/// transcript label (`display_info_for`) all key on this one value, so they
/// cannot drift apart again — classifying (or labeling) for a different
/// interpreter than the one that executes is exactly the bug family that
/// made plan mode deny every read-only PowerShell pipeline on Windows while
/// the transcript wrapped those pipelines in `Bash(...)`.
///
/// Windows executes under PowerShell (`pwsh` when installed, Windows
/// PowerShell 5.1 otherwise); everywhere else `sh`. [`Self::current`] is the
/// only `cfg!` site for the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostShell {
    Posix,
    PowerShell,
}

impl HostShell {
    /// The shell of the machine this binary runs on.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::PowerShell
        } else {
            Self::Posix
        }
    }

    /// Transcript label for an `execute_command` row (`Bash(cargo test)`,
    /// `PowerShell(Get-ChildItem)`). "Bash" is the colloquial POSIX label —
    /// the interpreter is `sh` — kept for familiarity.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Posix => "Bash",
            Self::PowerShell => "PowerShell",
        }
    }

    /// Prompt sigil an approval modal puts in front of a command so it reads
    /// as one. Dialect-specific for the same reason the label is: `$ ` in
    /// front of `Get-ChildItem` tells the reader they are approving a POSIX
    /// shell command, which is not what will run.
    #[must_use]
    pub const fn prompt_sigil(self) -> &'static str {
        match self {
            Self::Posix => "$ ",
            Self::PowerShell => "PS> ",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SafetyMode;

    #[test]
    fn least_permissive_picks_the_stricter_mode() {
        use SafetyMode::*;
        // A ceiling can only tighten: whichever side is stricter wins.
        assert_eq!(SafetyMode::least_permissive(FullAccess, ReadOnly), ReadOnly);
        assert_eq!(SafetyMode::least_permissive(ReadOnly, FullAccess), ReadOnly);
        assert_eq!(SafetyMode::least_permissive(Ask, Auto), Ask);
        assert_eq!(SafetyMode::least_permissive(Auto, Ask), Ask);
        // Identity: combining a mode with itself changes nothing.
        for m in [ReadOnly, Ask, Auto, FullAccess] {
            assert_eq!(SafetyMode::least_permissive(m, m), m);
        }
        // A FullAccess ceiling is a no-op for every live mode.
        for m in [ReadOnly, Ask, Auto, FullAccess] {
            assert_eq!(SafetyMode::least_permissive(m, FullAccess), m);
        }
    }
}
