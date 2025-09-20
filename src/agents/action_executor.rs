use anyhow::Result;

use super::executor;
use super::filesystem;
use super::git;
use super::types::{ActionResult, AgentAction};

/// Execute an agent action
pub async fn execute_action(action: &AgentAction) -> Result<ActionResult> {
    match action {
        AgentAction::ReadFile { path } => {
            filesystem::read_file(path).map(|content| ActionResult::Success {
                output: content,
            })
        }
        AgentAction::WriteFile { path, content } => {
            filesystem::write_file(path, content).map(|_| ActionResult::Success {
                output: format!("File written: {}", path),
            })
        }
        AgentAction::DeleteFile { path } => {
            filesystem::delete_file(path).map(|_| ActionResult::Success {
                output: format!("File deleted: {}", path),
            })
        }
        AgentAction::CreateDirectory { path } => {
            filesystem::create_directory(path).map(|_| ActionResult::Success {
                output: format!("Directory created: {}", path),
            })
        }
        AgentAction::ExecuteCommand { command, working_dir } => {
            executor::execute_command(command, working_dir.as_deref()).await
        }
        AgentAction::GitDiff { path } => {
            git::get_diff(path.as_deref()).map(|diff| ActionResult::Success { output: diff })
        }
        AgentAction::GitStatus => {
            git::get_status().map(|status| ActionResult::Success { output: status })
        }
        AgentAction::GitCommit { message, files } => {
            git::commit(message, files).map(|_| ActionResult::Success {
                output: format!("Committed with message: {}", message),
            })
        }
    }
    .map_err(|e| ActionResult::Error {
        error: e.to_string(),
    })
    .or_else(|e| Ok(e))
}