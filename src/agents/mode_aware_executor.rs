use super::action_executor::execute_action;
use super::types::{ActionResult, AgentAction};
use crate::tui::OperationMode;
use anyhow::Result;

/// Mode-aware action executor that respects operation modes
pub struct ModeAwareExecutor {
    mode: OperationMode,
    bypass_confirmed: bool,
}

impl ModeAwareExecutor {
    /// Create a new mode-aware executor
    pub fn new(mode: OperationMode) -> Self {
        Self {
            mode,
            bypass_confirmed: false,
        }
    }

    /// Update the operation mode
    pub fn set_mode(&mut self, mode: OperationMode) {
        self.mode = mode;
        self.bypass_confirmed = false; // Reset confirmation when mode changes
    }

    /// Check if an action needs confirmation based on the current mode
    pub fn needs_confirmation(&self, action: &AgentAction) -> bool {
        // In plan mode, nothing needs confirmation as nothing executes
        if self.mode.is_planning_only() {
            return false;
        }

        match action {
            // File operations
            AgentAction::WriteFile { .. } | AgentAction::DeleteFile { .. } => {
                !self.mode.auto_accept_files()
            },

            // Shell commands
            AgentAction::ExecuteCommand { .. } => !self.mode.auto_accept_commands(),

            // Git operations
            AgentAction::GitCommit { .. } => !self.mode.auto_accept_git(),

            // Read operations are generally safe
            AgentAction::ReadFile { .. } | AgentAction::GitStatus | AgentAction::GitDiff { .. } => {
                false
            },

            // Directory creation needs confirmation unless in bypass mode
            AgentAction::CreateDirectory { .. } => !self.mode.auto_accept_files(),
        }
    }

    /// Check if an action is considered destructive
    pub fn is_destructive(&self, action: &AgentAction) -> bool {
        match action {
            AgentAction::DeleteFile { .. } => true,
            AgentAction::ExecuteCommand { command, .. } => {
                command.contains("rm")
                    || command.contains("del")
                    || command.contains("drop")
                    || command.contains("truncate")
            },
            _ => false,
        }
    }

    /// Execute an action with mode awareness
    pub async fn execute(&mut self, action: AgentAction) -> Result<ActionResult> {
        // Planning mode: just return what would happen
        if self.mode.is_planning_only() {
            return Ok(ActionResult::Success {
                output: format!("[PLANNED]: {}", self.describe_action(&action)),
            });
        }

        // Bypass mode with destructive operation: require double confirmation
        if self.mode == OperationMode::BypassAll && self.is_destructive(&action) {
            if !self.bypass_confirmed {
                self.bypass_confirmed = true;
                return Ok(ActionResult::Success {
                    output: format!(
                        "[WARNING] DESTRUCTIVE OPERATION in Bypass Mode: {}\n\
                         Press Enter to confirm or Esc to cancel.",
                        self.describe_action(&action)
                    ),
                });
            }
        }

        // Execute the action
        let result = execute_action(&action).await?;

        // Reset bypass confirmation after successful execution
        if self.bypass_confirmed {
            self.bypass_confirmed = false;
        }

        // Add mode indicator to result if not in Normal mode
        if self.mode != OperationMode::Normal {
            match result {
                ActionResult::Success { output } => Ok(ActionResult::Success {
                    output: format!("[{}] {}", self.mode.short_name(), output),
                }),
                other => Ok(other),
            }
        } else {
            Ok(result)
        }
    }

    /// Get a human-readable description of an action
    pub fn describe_action(&self, action: &AgentAction) -> String {
        match action {
            AgentAction::ReadFile { path } => {
                format!("Read file: {}", path)
            },
            AgentAction::WriteFile { path, content } => {
                format!("Write file: {} ({} bytes)", path, content.len())
            },
            AgentAction::DeleteFile { path } => {
                format!("Delete file: {}", path)
            },
            AgentAction::CreateDirectory { path } => {
                format!("Create directory: {}", path)
            },
            AgentAction::ExecuteCommand {
                command,
                working_dir,
            } => {
                if let Some(dir) = working_dir {
                    format!("Execute command in {}: {}", dir, command)
                } else {
                    format!("Execute command: {}", command)
                }
            },
            AgentAction::GitDiff { path } => {
                if let Some(p) = path {
                    format!("Git diff for: {}", p)
                } else {
                    format!("Git diff (all files)")
                }
            },
            AgentAction::GitStatus => "Git status".to_string(),
            AgentAction::GitCommit { message, files } => {
                if !files.is_empty() {
                    format!("Git commit ({} files): {}", files.len(), message)
                } else {
                    format!("Git commit (all): {}", message)
                }
            },
        }
    }

    /// Get current mode
    pub fn mode(&self) -> OperationMode {
        self.mode
    }

    /// Check if bypass is confirmed
    pub fn is_bypass_confirmed(&self) -> bool {
        self.bypass_confirmed
    }

    /// Reset bypass confirmation
    pub fn reset_bypass_confirmation(&mut self) {
        self.bypass_confirmed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_needs_confirmation() {
        let executor = ModeAwareExecutor::new(OperationMode::Normal);

        // Normal mode needs confirmation for writes
        assert!(executor.needs_confirmation(&AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "test".to_string(),
        }));

        // Normal mode doesn't need confirmation for reads
        assert!(!executor.needs_confirmation(&AgentAction::ReadFile {
            path: "test.txt".to_string(),
        }));
    }

    #[test]
    fn test_accept_edits_mode() {
        let executor = ModeAwareExecutor::new(OperationMode::AcceptEdits);

        // AcceptEdits auto-accepts file operations
        assert!(!executor.needs_confirmation(&AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "test".to_string(),
        }));

        // But still confirms commands
        assert!(executor.needs_confirmation(&AgentAction::ExecuteCommand {
            command: "ls".to_string(),
            working_dir: None,
        }));
    }

    #[test]
    fn test_bypass_all_mode() {
        let executor = ModeAwareExecutor::new(OperationMode::BypassAll);

        // BypassAll auto-accepts everything
        assert!(!executor.needs_confirmation(&AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "test".to_string(),
        }));

        assert!(!executor.needs_confirmation(&AgentAction::ExecuteCommand {
            command: "ls".to_string(),
            working_dir: None,
        }));

        assert!(!executor.needs_confirmation(&AgentAction::GitCommit {
            message: "test".to_string(),
            files: vec![],
        }));
    }

    #[test]
    fn test_destructive_detection() {
        let executor = ModeAwareExecutor::new(OperationMode::Normal);

        // Delete file is destructive
        assert!(executor.is_destructive(&AgentAction::DeleteFile {
            path: "test.txt".to_string(),
        }));

        // rm command is destructive
        assert!(executor.is_destructive(&AgentAction::ExecuteCommand {
            command: "rm -rf /".to_string(),
            working_dir: None,
        }));

        // Regular command is not destructive
        assert!(!executor.is_destructive(&AgentAction::ExecuteCommand {
            command: "ls -la".to_string(),
            working_dir: None,
        }));
    }
}
