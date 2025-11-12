use crate::agents::types::{ActionResult, AgentAction};
use std::time::Instant;

/// The status of a planned action
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStatus {
    /// Not executed yet
    Pending,
    /// Currently running
    Executing,
    /// Successfully finished
    Completed,
    /// Failed with error
    Failed,
    /// User chose to skip
    Skipped,
}

impl ActionStatus {
    /// Get status indicator for display
    pub fn indicator(&self) -> &str {
        match self {
            ActionStatus::Pending => "•",
            ActionStatus::Executing => "...",
            ActionStatus::Completed => "✓",
            ActionStatus::Failed => "✗",
            ActionStatus::Skipped => "-",
        }
    }

    /// Check if action is in a terminal state
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ActionStatus::Completed | ActionStatus::Failed | ActionStatus::Skipped
        )
    }
}

/// A single action within a plan
#[derive(Debug, Clone)]
pub struct PlannedAction {
    /// The action to execute
    pub action: AgentAction,
    /// Current status of this action
    pub status: ActionStatus,
    /// Result of the action (if completed)
    pub result: Option<ActionResult>,
    /// Error message (if failed)
    pub error: Option<String>,
}

impl PlannedAction {
    /// Create a new pending action
    pub fn new(action: AgentAction) -> Self {
        Self {
            action,
            status: ActionStatus::Pending,
            result: None,
            error: None,
        }
    }

    /// Get a short description of this action for display
    pub fn description(&self) -> String {
        match &self.action {
            AgentAction::ReadFile { path } => format!("Read {}", path),
            AgentAction::WriteFile { path, .. } => format!("Write {}", path),
            AgentAction::DeleteFile { path } => format!("Delete {}", path),
            AgentAction::CreateDirectory { path } => format!("Create dir {}", path),
            AgentAction::ExecuteCommand { command, .. } => format!("Run: {}", command),
            AgentAction::GitDiff { path } => {
                format!("Git diff {}", path.as_deref().unwrap_or("*"))
            },
            AgentAction::GitCommit { message, .. } => format!("Git commit: {}", message),
            AgentAction::GitStatus => "Git status".to_string(),
            AgentAction::WebSearch { query, .. } => format!("Search: {}", query),
            AgentAction::ParallelRead { paths } => format!("Read {} files", paths.len()),
            AgentAction::ParallelWebSearch { queries } => format!("Search {} queries", queries.len()),
            AgentAction::ParallelGitDiff { paths } => format!("Git diff {} paths", paths.len()),
        }
    }

    /// Get action type for display
    pub fn action_type(&self) -> &str {
        match &self.action {
            AgentAction::ReadFile { .. } => "Read",
            AgentAction::WriteFile { .. } => "Write",
            AgentAction::DeleteFile { .. } => "Delete",
            AgentAction::CreateDirectory { .. } => "Create",
            AgentAction::ExecuteCommand { .. } => "Bash",
            AgentAction::GitDiff { .. } => "GitDiff",
            AgentAction::GitCommit { .. } => "GitCommit",
            AgentAction::GitStatus => "GitStatus",
            AgentAction::WebSearch { .. } => "WebSearch",
            AgentAction::ParallelRead { .. } => "ReadFiles",
            AgentAction::ParallelWebSearch { .. } => "WebSearches",
            AgentAction::ParallelGitDiff { .. } => "GitDiffs",
        }
    }
}

/// A complete plan of actions to execute
#[derive(Debug, Clone)]
pub struct Plan {
    /// All actions in the plan
    pub actions: Vec<PlannedAction>,
    /// When this plan was created
    pub created_at: Instant,
    /// LLM's explanation of what it plans to do
    pub explanation: Option<String>,
    /// Pre-formatted markdown text for display
    pub display_text: String,
}

impl Plan {
    /// Create a new plan from a list of actions
    pub fn new(actions: Vec<AgentAction>) -> Self {
        Self::with_explanation(None, actions)
    }

    /// Create a new plan with an explanation from the LLM
    pub fn with_explanation(explanation: Option<String>, actions: Vec<AgentAction>) -> Self {
        let planned_actions: Vec<PlannedAction> =
            actions.into_iter().map(PlannedAction::new).collect();

        let display_text = Self::format_display_with_explanation(&explanation, &planned_actions);

        Self {
            actions: planned_actions,
            created_at: Instant::now(),
            explanation,
            display_text,
        }
    }

    /// Format plan with explanation and actions for display
    fn format_display_with_explanation(
        explanation: &Option<String>,
        actions: &[PlannedAction],
    ) -> String {
        let mut output = String::new();

        // Add explanation if provided
        if let Some(exp) = explanation {
            let trimmed = exp.trim();
            if !trimmed.is_empty() {
                output.push_str(trimmed);
                output.push_str("\n\n");
            }
        }

        // Add action summary
        let actions_text = Self::format_display_actions(actions);
        output.push_str(&actions_text);
        output
    }

    /// Format plan actions for display as markdown
    #[allow(dead_code)]
    fn format_display(actions: &[PlannedAction]) -> String {
        Self::format_display_actions(actions)
    }

    /// Format only the actions portion of the plan
    fn format_display_actions(actions: &[PlannedAction]) -> String {
        if actions.is_empty() {
            return "No actions in plan".to_string();
        }

        let mut output = String::new();
        output.push_str("Plan: Ready to execute\n\n");

        let mut file_actions = Vec::new();
        let mut command_actions = Vec::new();
        let mut git_actions = Vec::new();

        for action in actions {
            match &action.action {
                AgentAction::ReadFile { .. }
                | AgentAction::WriteFile { .. }
                | AgentAction::DeleteFile { .. }
                | AgentAction::CreateDirectory { .. }
                | AgentAction::ParallelRead { .. } => file_actions.push(action),
                AgentAction::ExecuteCommand { .. } => command_actions.push(action),
                AgentAction::GitDiff { .. }
                | AgentAction::GitCommit { .. }
                | AgentAction::GitStatus
                | AgentAction::ParallelGitDiff { .. } => git_actions.push(action),
                AgentAction::WebSearch { .. }
                | AgentAction::ParallelWebSearch { .. } => {
                    // Web search actions are executed inline
                },
            }
        }

        if !file_actions.is_empty() {
            output.push_str("File Operations:\n");
            for (i, action) in file_actions.iter().enumerate() {
                output.push_str(&format!(
                    "  {}. {} {}\n",
                    i + 1,
                    action.status.indicator(),
                    action.description()
                ));
            }
            output.push('\n');
        }

        if !command_actions.is_empty() {
            output.push_str("Commands:\n");
            for (i, action) in command_actions.iter().enumerate() {
                output.push_str(&format!(
                    "  {}. {} {}\n",
                    i + 1,
                    action.status.indicator(),
                    action.description()
                ));
            }
            output.push('\n');
        }

        if !git_actions.is_empty() {
            output.push_str("Git Operations:\n");
            for (i, action) in git_actions.iter().enumerate() {
                output.push_str(&format!(
                    "  {}. {} {}\n",
                    i + 1,
                    action.status.indicator(),
                    action.description()
                ));
            }
            output.push('\n');
        }

        output.push_str("Approve with Ctrl+Y, Cancel with Ctrl+N");
        output
    }

    /// Update an action's status and regenerate display text
    pub fn update_action_status(
        &mut self,
        index: usize,
        status: ActionStatus,
        result: Option<ActionResult>,
        error: Option<String>,
    ) {
        if let Some(action) = self.actions.get_mut(index) {
            action.status = status;
            action.result = result;
            action.error = error;
        }
        self.regenerate_display();
    }

    /// Regenerate the display text with current action statuses
    fn regenerate_display(&mut self) {
        let mut output = String::new();

        let completed = self
            .actions
            .iter()
            .filter(|a| a.status == ActionStatus::Completed)
            .count();
        let failed = self
            .actions
            .iter()
            .filter(|a| a.status == ActionStatus::Failed)
            .count();
        let total = self.actions.len();

        if completed == total {
            output.push_str(&format!("Plan: Completed ({}/{})\n\n", completed, total));
        } else if failed > 0 {
            output.push_str(&format!(
                "Plan: In Progress ({}/{}, {} failed)\n\n",
                completed, total, failed
            ));
        } else {
            output.push_str(&format!("Plan: In Progress ({}/{})\n\n", completed, total));
        }

        let mut file_actions = Vec::new();
        let mut command_actions = Vec::new();
        let mut git_actions = Vec::new();

        for action in &self.actions {
            match &action.action {
                AgentAction::ReadFile { .. }
                | AgentAction::WriteFile { .. }
                | AgentAction::DeleteFile { .. }
                | AgentAction::CreateDirectory { .. }
                | AgentAction::ParallelRead { .. } => file_actions.push(action),
                AgentAction::ExecuteCommand { .. } => command_actions.push(action),
                AgentAction::GitDiff { .. }
                | AgentAction::GitCommit { .. }
                | AgentAction::GitStatus
                | AgentAction::ParallelGitDiff { .. } => git_actions.push(action),
                AgentAction::WebSearch { .. }
                | AgentAction::ParallelWebSearch { .. } => {
                    // Web search actions are executed inline, not categorized
                },
            }
        }

        if !file_actions.is_empty() {
            output.push_str("File Operations:\n");
            for action in file_actions {
                output.push_str(&format!(
                    "  {} {}\n",
                    action.status.indicator(),
                    action.description()
                ));
                if let Some(ref err) = action.error {
                    output.push_str(&format!("    Error: {}\n", err));
                }
            }
            output.push('\n');
        }

        if !command_actions.is_empty() {
            output.push_str("Commands:\n");
            for action in command_actions {
                output.push_str(&format!(
                    "  {} {}\n",
                    action.status.indicator(),
                    action.description()
                ));
                if let Some(ref err) = action.error {
                    output.push_str(&format!("    Error: {}\n", err));
                }
            }
            output.push('\n');
        }

        if !git_actions.is_empty() {
            output.push_str("Git Operations:\n");
            for action in git_actions {
                output.push_str(&format!(
                    "  {} {}\n",
                    action.status.indicator(),
                    action.description()
                ));
                if let Some(ref err) = action.error {
                    output.push_str(&format!("    Error: {}\n", err));
                }
            }
            output.push('\n');
        }

        if completed + failed == total {
            output.push_str("Plan: Complete");
        } else {
            output.push_str("Executing plan... Alt+Esc to abort");
        }

        self.display_text = output;
    }

    /// Get next pending action
    pub fn next_pending_action(&self) -> Option<(usize, &PlannedAction)> {
        self.actions
            .iter()
            .enumerate()
            .find(|(_, a)| a.status == ActionStatus::Pending)
    }

    /// Get completion statistics
    pub fn stats(&self) -> PlanStats {
        PlanStats {
            total: self.actions.len(),
            completed: self
                .actions
                .iter()
                .filter(|a| a.status == ActionStatus::Completed)
                .count(),
            failed: self
                .actions
                .iter()
                .filter(|a| a.status == ActionStatus::Failed)
                .count(),
            skipped: self
                .actions
                .iter()
                .filter(|a| a.status == ActionStatus::Skipped)
                .count(),
            executing: self
                .actions
                .iter()
                .filter(|a| a.status == ActionStatus::Executing)
                .count(),
        }
    }
}

/// Statistics about plan execution
#[derive(Debug, Clone, Copy)]
pub struct PlanStats {
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub executing: usize,
}

impl PlanStats {
    /// Get completion percentage
    pub fn completion_percent(&self) -> u8 {
        if self.total == 0 {
            100
        } else {
            ((self.completed + self.failed + self.skipped) as f64 / self.total as f64 * 100.0) as u8
        }
    }

    /// Check if plan is complete
    pub fn is_complete(&self) -> bool {
        self.completed + self.failed + self.skipped == self.total
    }

    /// Check if plan has failures
    pub fn has_failures(&self) -> bool {
        self.failed > 0
    }

    /// Get status message
    pub fn status_message(&self) -> String {
        if self.is_complete() {
            if self.has_failures() {
                format!(
                    "Plan completed: {}/{} successful, {} failed",
                    self.completed, self.total, self.failed
                )
            } else {
                format!("Plan completed: all {} actions successful", self.total)
            }
        } else {
            format!(
                "Plan: {} executing, {}/{} completed",
                self.executing, self.completed, self.total
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_status_indicators() {
        assert_eq!(ActionStatus::Pending.indicator(), "•");
        assert_eq!(ActionStatus::Executing.indicator(), "...");
        assert_eq!(ActionStatus::Completed.indicator(), "✓");
        assert_eq!(ActionStatus::Failed.indicator(), "✗");
        assert_eq!(ActionStatus::Skipped.indicator(), "-");
    }

    #[test]
    fn test_planned_action_new() {
        let action = AgentAction::ReadFile {
            path: "test.txt".to_string(),
        };
        let planned = PlannedAction::new(action);
        assert_eq!(planned.status, ActionStatus::Pending);
        assert!(planned.result.is_none());
        assert!(planned.error.is_none());
    }

    #[test]
    fn test_plan_stats() {
        let mut plan = Plan::new(vec![
            AgentAction::ReadFile {
                path: "a.txt".to_string(),
            },
            AgentAction::WriteFile {
                path: "b.txt".to_string(),
                content: "content".to_string(),
            },
        ]);

        let mut stats = plan.stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.completed, 0);
        assert!(!stats.is_complete());

        plan.update_action_status(0, ActionStatus::Completed, None, None);
        stats = plan.stats();
        assert_eq!(stats.completed, 1);
        assert!(!stats.is_complete());

        plan.update_action_status(1, ActionStatus::Completed, None, None);
        stats = plan.stats();
        assert_eq!(stats.completed, 2);
        assert!(stats.is_complete());
    }
}
