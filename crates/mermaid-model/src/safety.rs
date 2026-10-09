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

/// Marker embedded verbatim in every read-only policy-denial `reason` (see
/// the runtime engine's `PolicyEngine::decide`). Exposed so the
/// message-history layer can detect a denial that a since-loosened safety
/// mode has superseded, without re-hardcoding the wording in a second place.
pub const READ_ONLY_DENIAL_MARKER: &str = "read-only safety mode";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyMode {
    ReadOnly,
    Ask,
    #[default]
    Auto,
    FullAccess,
}

impl SafetyMode {
    /// Canonical serialized name — matches the serde `snake_case` rename.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
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
            "read_only" => Some(Self::ReadOnly),
            "ask" => Some(Self::Ask),
            "auto" => Some(Self::Auto),
            "full_access" => Some(Self::FullAccess),
            _ => None,
        }
    }

    /// Permissiveness rank for combining modes: `read_only` is strictest,
    /// `full_access` loosest.
    #[must_use]
    pub fn permissiveness(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::Ask => 1,
            Self::Auto => 2,
            Self::FullAccess => 3,
        }
    }

    /// Serde `deserialize_with` for a persisted `Option<SafetyMode>`: a mode
    /// name this build does not know (one since retired) reads as `None`, so
    /// the session falls back to the configured mode instead of failing to
    /// load at all.
    ///
    /// # Errors
    ///
    /// Only when the value is neither null nor a string.
    pub fn deserialize_optional_lenient<'de, D>(deserializer: D) -> Result<Option<Self>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw: Option<String> = Option::deserialize(deserializer)?;
        Ok(raw.as_deref().and_then(Self::parse))
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
    /// Mouse and keyboard on the user's real screen.
    Computer,
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
            Self::Computer => "computer",
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
    /// For `ToolCategory::Shell` only: the caller will run this command inside
    /// the read-only OS sandbox, where the kernel denies writes, sockets, IPC,
    /// outward signals and privileges. In `read_only` mode that containment,
    /// not the command's classification, is what keeps it read-only, so the
    /// engine lets any command through that is not hard-denied or denied by
    /// a user override. Only `execute_command` sets it, and only when its
    /// launcher will enforce that sandbox for this very spawn.
    #[serde(default)]
    pub read_only_contained: bool,
    /// For `ToolCategory::Computer` only: the screen the model last saw, as a
    /// base64 PNG, so the Auto-mode check can see what the clicks land on.
    /// Never stored.
    #[serde(skip)]
    pub screen: Option<String>,
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
            read_only_contained: false,
            screen: None,
        }
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
/// (`classify_command_for`), and the transcript label (`display_info_for`)
/// all key on this one value, so they cannot drift apart again — classifying
/// (or labeling) for a different interpreter than the one that executes is
/// exactly the bug family that made `read_only` deny every read-only
/// PowerShell pipeline on Windows while the transcript wrapped those
/// pipelines in `Bash(...)`.
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
