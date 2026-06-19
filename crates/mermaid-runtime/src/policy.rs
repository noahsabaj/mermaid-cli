use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyMode {
    ReadOnly,
    #[default]
    Ask,
    AutoReview,
    FullAccess,
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
}

impl ToolCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolCategory::Read => "read",
            ToolCategory::Edit => "edit",
            ToolCategory::Shell => "shell",
            ToolCategory::Web => "web",
            ToolCategory::ExternalDirectory => "external_directory",
            ToolCategory::ComputerUse => "computer_use",
            ToolCategory::Mcp => "mcp",
            ToolCategory::Subagent => "subagent",
            ToolCategory::Network => "network",
            ToolCategory::Git => "git",
            ToolCategory::Process => "process",
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
    Destructive,
}

impl RiskClass {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskClass::ReadOnly => "read_only",
            RiskClass::LowMutation => "low_mutation",
            RiskClass::FileMutation => "file_mutation",
            RiskClass::ShellMutation => "shell_mutation",
            RiskClass::Network => "network",
            RiskClass::Process => "process",
            RiskClass::ExternalAccess => "external_access",
            RiskClass::Destructive => "destructive",
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow { risk: RiskClass, checkpoint: bool },
    Ask { risk: RiskClass, checkpoint: bool },
    Deny { risk: RiskClass, reason: String },
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
    pub fn risk(&self) -> RiskClass {
        match self {
            PolicyDecision::Allow { risk, .. }
            | PolicyDecision::Ask { risk, .. }
            | PolicyDecision::Deny { risk, .. } => *risk,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            PolicyDecision::Allow { .. } => "allow",
            PolicyDecision::Ask { .. } => "ask",
            PolicyDecision::Deny { .. } => "deny",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PolicyEngine {
    mode: SafetyMode,
    overrides: Vec<PolicyOverride>,
}

impl PolicyEngine {
    pub fn new(mode: SafetyMode) -> Self {
        Self {
            mode,
            overrides: Vec::new(),
        }
    }

    pub fn with_overrides(mut self, overrides: Vec<PolicyOverride>) -> Self {
        self.overrides = overrides;
        self
    }

    pub fn decide(&self, request: &ActionRequest) -> PolicyDecision {
        let risk = classify(request);
        if risk == RiskClass::Destructive {
            return PolicyDecision::Deny {
                risk,
                reason: "hard-denied destructive pattern".to_string(),
            };
        }

        if let Some(decision) = self
            .overrides
            .iter()
            .find(|override_rule| override_matches(override_rule, request))
            .map(|override_rule| override_decision(override_rule, risk))
        {
            return decision;
        }

        match self.mode {
            SafetyMode::ReadOnly => {
                if risk == RiskClass::ReadOnly {
                    PolicyDecision::Allow {
                        risk,
                        checkpoint: false,
                    }
                } else {
                    PolicyDecision::Deny {
                        risk,
                        reason: "read-only safety mode blocks mutations and control actions"
                            .to_string(),
                    }
                }
            },
            SafetyMode::Ask => PolicyDecision::Ask {
                risk,
                checkpoint: risk != RiskClass::ReadOnly,
            },
            SafetyMode::AutoReview => match risk {
                RiskClass::ReadOnly | RiskClass::LowMutation => PolicyDecision::Allow {
                    risk,
                    checkpoint: risk != RiskClass::ReadOnly,
                },
                RiskClass::FileMutation => PolicyDecision::Allow {
                    risk,
                    checkpoint: true,
                },
                RiskClass::ShellMutation
                | RiskClass::Network
                | RiskClass::Process
                | RiskClass::ExternalAccess => PolicyDecision::Ask {
                    risk,
                    checkpoint: true,
                },
                RiskClass::Destructive => unreachable!("handled above"),
            },
            SafetyMode::FullAccess => PolicyDecision::Allow {
                risk,
                checkpoint: risk != RiskClass::ReadOnly,
            },
        }
    }
}

fn override_matches(rule: &PolicyOverride, request: &ActionRequest) -> bool {
    if let Some(category) = rule.category
        && category != request.category
    {
        return false;
    }
    if let Some(tool) = rule.tool.as_deref()
        && tool != request.tool
    {
        return false;
    }
    if let Some(pattern) = rule.pattern.as_deref() {
        let haystack = request
            .command
            .as_deref()
            .or(request.path.as_deref())
            .unwrap_or(&request.summary);
        if !haystack.contains(pattern) {
            return false;
        }
    }
    rule.category.is_some() || rule.tool.is_some() || rule.pattern.is_some()
}

fn override_decision(rule: &PolicyOverride, risk: RiskClass) -> PolicyDecision {
    let checkpoint = rule.checkpoint.unwrap_or(risk != RiskClass::ReadOnly);
    match rule.decision {
        PolicyOverrideDecision::Allow => PolicyDecision::Allow { risk, checkpoint },
        PolicyOverrideDecision::Ask => PolicyDecision::Ask { risk, checkpoint },
        PolicyOverrideDecision::Deny => PolicyDecision::Deny {
            risk,
            reason: rule
                .reason
                .clone()
                .unwrap_or_else(|| "blocked by policy override".to_string()),
        },
    }
}

fn classify(request: &ActionRequest) -> RiskClass {
    if request
        .command
        .as_deref()
        .is_some_and(contains_destructive_pattern)
    {
        return RiskClass::Destructive;
    }

    match request.category {
        ToolCategory::Read => RiskClass::ReadOnly,
        ToolCategory::Edit => RiskClass::FileMutation,
        ToolCategory::Shell | ToolCategory::Git => request
            .command
            .as_deref()
            .map(classify_shell_command)
            .unwrap_or(RiskClass::ShellMutation),
        ToolCategory::Web | ToolCategory::Network => RiskClass::Network,
        ToolCategory::ExternalDirectory | ToolCategory::ComputerUse | ToolCategory::Mcp => {
            RiskClass::ExternalAccess
        },
        ToolCategory::Subagent => RiskClass::Process,
        ToolCategory::Process => RiskClass::Process,
    }
}

fn classify_shell_command(command: &str) -> RiskClass {
    let lower = command.to_ascii_lowercase();
    if contains_destructive_pattern(&lower) {
        return RiskClass::Destructive;
    }
    if lower.contains(" rm ")
        || lower.starts_with("rm ")
        || lower.contains(" mv ")
        || lower.starts_with("mv ")
        || lower.contains(" cp ")
        || lower.starts_with("cp ")
        || lower.contains(" >")
        || lower.contains("git commit")
        || lower.contains("git push")
        || lower.contains("cargo publish")
        || lower.contains("npm publish")
    {
        RiskClass::ShellMutation
    } else if lower.contains("npm run dev")
        || lower.contains("cargo run")
        || lower.contains("python -m http.server")
    {
        RiskClass::Process
    } else {
        RiskClass::ReadOnly
    }
}

fn contains_destructive_pattern(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("rm -rf /")
        || lower.contains(":(){")
        || lower.contains("mkfs.")
        || lower.contains("dd if=")
        || lower.contains("chmod -r 777 /")
        || lower.contains("chown -r ")
        || lower.contains("git reset --hard")
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn read_only_mode_denies_mutation() {
        let request = ActionRequest::new("write_file", ToolCategory::Edit, "write src/lib.rs");
        let decision = PolicyEngine::new(SafetyMode::ReadOnly).decide(&request);
        assert!(matches!(decision, PolicyDecision::Deny { .. }));
    }

    #[test]
    fn auto_review_allows_file_mutation_with_checkpoint() {
        let request = ActionRequest::new("write_file", ToolCategory::Edit, "write src/lib.rs");
        let decision = PolicyEngine::new(SafetyMode::AutoReview).decide(&request);
        assert!(matches!(
            decision,
            PolicyDecision::Allow {
                risk: RiskClass::FileMutation,
                checkpoint: true
            }
        ));
    }

    #[test]
    fn destructive_command_hard_denies_even_full_access() {
        let mut request = ActionRequest::new("execute_command", ToolCategory::Shell, "reset");
        request.command = Some("git reset --hard".to_string());
        let decision = PolicyEngine::new(SafetyMode::FullAccess).decide(&request);
        assert!(matches!(
            decision,
            PolicyDecision::Deny {
                risk: RiskClass::Destructive,
                ..
            }
        ));
    }

    #[test]
    fn override_can_ask_for_specific_tool_in_full_access() {
        let request = ActionRequest::new("write_file", ToolCategory::Edit, "write src/lib.rs");
        let decision = PolicyEngine::new(SafetyMode::FullAccess)
            .with_overrides(vec![PolicyOverride {
                tool: Some("write_file".to_string()),
                decision: PolicyOverrideDecision::Ask,
                ..PolicyOverride::default()
            }])
            .decide(&request);
        assert!(matches!(decision, PolicyDecision::Ask { .. }));
    }
}
