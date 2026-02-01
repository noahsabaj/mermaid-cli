use crate::agents::types::{ActionResult, AgentAction};
use std::time::Instant;

/// Category of action for display grouping
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionCategory {
    /// File operations (read, write, delete, create dir)
    File,
    /// Shell commands
    Command,
    /// Git operations (diff, commit, status)
    Git,
    /// Web search (not displayed in plan, executed inline)
    WebSearch,
}

impl ActionCategory {
    /// Get display header for this category
    pub fn header(&self) -> &str {
        match self {
            ActionCategory::File => "File Operations:",
            ActionCategory::Command => "Commands:",
            ActionCategory::Git => "Git Operations:",
            ActionCategory::WebSearch => "Web Searches:",
        }
    }
}

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
            AgentAction::ReadFile { paths } => {
                if paths.len() == 1 {
                    format!("Read {}", paths[0])
                } else {
                    format!("Read {} files", paths.len())
                }
            }
            AgentAction::WriteFile { path, .. } => format!("Write {}", path),
            AgentAction::DeleteFile { path } => format!("Delete {}", path),
            AgentAction::CreateDirectory { path } => format!("Create dir {}", path),
            AgentAction::ExecuteCommand { command, .. } => format!("Run: {}", command),
            AgentAction::GitDiff { paths } => {
                if paths.len() == 1 {
                    format!("Git diff {}", paths[0].as_deref().unwrap_or("*"))
                } else {
                    format!("Git diff {} paths", paths.len())
                }
            }
            AgentAction::GitCommit { message, .. } => format!("Git commit: {}", message),
            AgentAction::GitStatus => "Git status".to_string(),
            AgentAction::WebSearch { queries } => {
                if queries.len() == 1 {
                    format!("Search: {}", queries[0].0)
                } else {
                    format!("Search {} queries", queries.len())
                }
            }
        }
    }

    /// Get action type for display
    pub fn action_type(&self) -> &str {
        match &self.action {
            AgentAction::ReadFile { paths } => {
                if paths.len() == 1 { "Read" } else { "ReadFiles" }
            }
            AgentAction::WriteFile { .. } => "Write",
            AgentAction::DeleteFile { .. } => "Delete",
            AgentAction::CreateDirectory { .. } => "Create",
            AgentAction::ExecuteCommand { .. } => "Bash",
            AgentAction::GitDiff { paths } => {
                if paths.len() == 1 { "GitDiff" } else { "GitDiffs" }
            }
            AgentAction::GitCommit { .. } => "GitCommit",
            AgentAction::GitStatus => "GitStatus",
            AgentAction::WebSearch { queries } => {
                if queries.len() == 1 { "WebSearch" } else { "WebSearches" }
            }
        }
    }

    /// Get the category of this action for display grouping
    pub fn category(&self) -> ActionCategory {
        match &self.action {
            AgentAction::ReadFile { .. }
            | AgentAction::WriteFile { .. }
            | AgentAction::DeleteFile { .. }
            | AgentAction::CreateDirectory { .. } => ActionCategory::File,
            AgentAction::ExecuteCommand { .. } => ActionCategory::Command,
            AgentAction::GitDiff { .. }
            | AgentAction::GitCommit { .. }
            | AgentAction::GitStatus => ActionCategory::Git,
            AgentAction::WebSearch { .. } => ActionCategory::WebSearch,
        }
    }
}

/// Categorized actions for display
#[derive(Debug, Default)]
struct CategorizedActions<'a> {
    file: Vec<&'a PlannedAction>,
    command: Vec<&'a PlannedAction>,
    git: Vec<&'a PlannedAction>,
}

impl<'a> CategorizedActions<'a> {
    /// Categorize actions from a slice (web search actions are excluded)
    fn from_actions(actions: &'a [PlannedAction]) -> Self {
        let mut categorized = Self::default();
        for action in actions {
            match action.category() {
                ActionCategory::File => categorized.file.push(action),
                ActionCategory::Command => categorized.command.push(action),
                ActionCategory::Git => categorized.git.push(action),
                ActionCategory::WebSearch => {} // Excluded from display
            }
        }
        categorized
    }

    /// Render all categories to output string
    fn render(&self, output: &mut String, numbered: bool, show_errors: bool) {
        self.render_category(output, &self.file, ActionCategory::File, numbered, show_errors);
        self.render_category(output, &self.command, ActionCategory::Command, numbered, show_errors);
        self.render_category(output, &self.git, ActionCategory::Git, numbered, show_errors);
    }

    /// Render a single category of actions
    fn render_category(
        &self,
        output: &mut String,
        actions: &[&PlannedAction],
        category: ActionCategory,
        numbered: bool,
        show_errors: bool,
    ) {
        if actions.is_empty() {
            return;
        }

        output.push_str(category.header());
        output.push('\n');

        for (i, action) in actions.iter().enumerate() {
            if numbered {
                output.push_str(&format!(
                    "  {}. {} {}\n",
                    i + 1,
                    action.status.indicator(),
                    action.description()
                ));
            } else {
                output.push_str(&format!(
                    "  {} {}\n",
                    action.status.indicator(),
                    action.description()
                ));
            }

            if show_errors {
                if let Some(ref err) = action.error {
                    output.push_str(&format!("    Error: {}\n", err));
                }
            }
        }
        output.push('\n');
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

    /// Format only the actions portion of the plan
    fn format_display_actions(actions: &[PlannedAction]) -> String {
        if actions.is_empty() {
            return "No actions in plan".to_string();
        }

        let mut output = String::new();
        output.push_str("Plan: Ready to execute\n\n");

        let categorized = CategorizedActions::from_actions(actions);
        categorized.render(&mut output, true, false); // numbered, no errors

        output.push_str("Approve with Y, Cancel with N");
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
        let stats = self.stats();
        let mut output = String::new();

        // Header with progress
        if stats.completed == stats.total {
            output.push_str(&format!(
                "Plan: Completed ({}/{})\n\n",
                stats.completed, stats.total
            ));
        } else if stats.failed > 0 {
            output.push_str(&format!(
                "Plan: In Progress ({}/{}, {} failed)\n\n",
                stats.completed, stats.total, stats.failed
            ));
        } else {
            output.push_str(&format!(
                "Plan: In Progress ({}/{})\n\n",
                stats.completed, stats.total
            ));
        }

        // Render categorized actions (unnumbered, with errors)
        let categorized = CategorizedActions::from_actions(&self.actions);
        categorized.render(&mut output, false, true);

        // Footer
        if stats.is_complete() {
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
            paths: vec!["test.txt".to_string()],
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
                paths: vec!["a.txt".to_string()],
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
