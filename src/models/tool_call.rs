/// Tool call parsing and conversion to AgentAction
///
/// Handles deserialization of Ollama tool_calls responses and converts
/// them to Mermaid's internal AgentAction enum.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::agents::AgentAction;

/// A tool call from the model (Ollama format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: Option<String>,
    pub function: FunctionCall,
}

/// Function call details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

impl ToolCall {
    /// Convert Ollama tool call to Mermaid AgentAction
    pub fn to_agent_action(&self) -> Result<AgentAction> {
        let args = &self.function.arguments;

        let action = match self.function.name.as_str() {
            "read_file" => {
                let path = Self::get_string_arg(args, "path")?;
                AgentAction::ReadFile { path }
            }

            "write_file" => {
                let path = Self::get_string_arg(args, "path")?;
                let content = Self::get_string_arg(args, "content")?;
                AgentAction::WriteFile { path, content }
            }

            "delete_file" => {
                let path = Self::get_string_arg(args, "path")?;
                AgentAction::DeleteFile { path }
            }

            "create_directory" => {
                let path = Self::get_string_arg(args, "path")?;
                AgentAction::CreateDirectory { path }
            }

            "execute_command" => {
                let command = Self::get_string_arg(args, "command")?;
                let working_dir = Self::get_optional_string_arg(args, "working_dir");
                AgentAction::ExecuteCommand {
                    command,
                    working_dir,
                }
            }

            "git_diff" => {
                let path = Self::get_optional_string_arg(args, "path");
                AgentAction::GitDiff { path }
            }

            "git_status" => AgentAction::GitStatus,

            "git_commit" => {
                let message = Self::get_string_arg(args, "message")?;
                let files = Self::get_string_array_arg(args, "files")?;
                AgentAction::GitCommit { message, files }
            }

            "web_search" => {
                let query = Self::get_string_arg(args, "query")?;
                let result_count = Self::get_int_arg(args, "result_count")
                    .unwrap_or(5)
                    .clamp(1, 10);
                AgentAction::WebSearch {
                    query,
                    result_count,
                }
            }

            name => {
                return Err(anyhow!(
                    "Unknown tool: '{}'. Model attempted to call a tool that doesn't exist.",
                    name
                ))
            }
        };

        Ok(action)
    }

    // Helper methods for argument extraction

    fn get_string_arg(args: &serde_json::Value, key: &str) -> Result<String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("Missing or invalid required argument: '{}'", key))
    }

    fn get_optional_string_arg(args: &serde_json::Value, key: &str) -> Option<String> {
        args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
    }

    fn get_int_arg(args: &serde_json::Value, key: &str) -> Result<usize> {
        args.get(key)
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .ok_or_else(|| anyhow!("Missing or invalid required argument: '{}'", key))
    }

    fn get_string_array_arg(args: &serde_json::Value, key: &str) -> Result<Vec<String>> {
        args.get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .ok_or_else(|| anyhow!("Missing or invalid required argument: '{}'", key))
    }
}

/// Parse multiple tool calls into agent actions
pub fn parse_tool_calls(tool_calls: &[ToolCall]) -> Vec<AgentAction> {
    tool_calls
        .iter()
        .filter_map(|tc| match tc.to_agent_action() {
            Ok(action) => Some(action),
            Err(e) => {
                eprintln!("Failed to parse tool call '{}': {}", tc.function.name, e);
                None
            }
        })
        .collect()
}

/// Group consecutive same-type read operations into parallel reads
pub fn group_parallel_reads(actions: Vec<AgentAction>) -> Vec<AgentAction> {
    if actions.is_empty() {
        return actions;
    }

    let mut result = Vec::new();
    let mut current_group: Vec<String> = Vec::new();

    for action in actions {
        match action {
            AgentAction::ReadFile { path } => {
                current_group.push(path);
            }
            other => {
                // Flush current read group if it has multiple items
                if current_group.len() > 1 {
                    result.push(AgentAction::ParallelRead {
                        paths: current_group.clone(),
                    });
                } else if current_group.len() == 1 {
                    result.push(AgentAction::ReadFile {
                        path: current_group[0].clone(),
                    });
                }
                current_group.clear();

                result.push(other);
            }
        }
    }

    // Flush remaining read group
    if current_group.len() > 1 {
        result.push(AgentAction::ParallelRead {
            paths: current_group,
        });
    } else if current_group.len() == 1 {
        result.push(AgentAction::ReadFile {
            path: current_group[0].clone(),
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_read_file_tool_call() {
        let tool_call = ToolCall {
            id: Some("call_123".to_string()),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: json!({
                    "path": "src/main.rs"
                }),
            },
        };

        let action = tool_call.to_agent_action().unwrap();
        match action {
            AgentAction::ReadFile { path } => assert_eq!(path, "src/main.rs"),
            _ => panic!("Expected ReadFile action"),
        }
    }

    #[test]
    fn test_parse_write_file_tool_call() {
        let tool_call = ToolCall {
            id: None,
            function: FunctionCall {
                name: "write_file".to_string(),
                arguments: json!({
                    "path": "test.txt",
                    "content": "Hello, world!"
                }),
            },
        };

        let action = tool_call.to_agent_action().unwrap();
        match action {
            AgentAction::WriteFile { path, content } => {
                assert_eq!(path, "test.txt");
                assert_eq!(content, "Hello, world!");
            }
            _ => panic!("Expected WriteFile action"),
        }
    }

    #[test]
    fn test_parse_execute_command_tool_call() {
        let tool_call = ToolCall {
            id: None,
            function: FunctionCall {
                name: "execute_command".to_string(),
                arguments: json!({
                    "command": "cargo test",
                    "working_dir": "/path/to/project"
                }),
            },
        };

        let action = tool_call.to_agent_action().unwrap();
        match action {
            AgentAction::ExecuteCommand {
                command,
                working_dir,
            } => {
                assert_eq!(command, "cargo test");
                assert_eq!(working_dir, Some("/path/to/project".to_string()));
            }
            _ => panic!("Expected ExecuteCommand action"),
        }
    }

    #[test]
    fn test_parse_web_search_tool_call() {
        let tool_call = ToolCall {
            id: None,
            function: FunctionCall {
                name: "web_search".to_string(),
                arguments: json!({
                    "query": "Rust async features",
                    "result_count": 5
                }),
            },
        };

        let action = tool_call.to_agent_action().unwrap();
        match action {
            AgentAction::WebSearch {
                query,
                result_count,
            } => {
                assert_eq!(query, "Rust async features");
                assert_eq!(result_count, 5);
            }
            _ => panic!("Expected WebSearch action"),
        }
    }

    #[test]
    fn test_unknown_tool_returns_error() {
        let tool_call = ToolCall {
            id: None,
            function: FunctionCall {
                name: "unknown_tool".to_string(),
                arguments: json!({}),
            },
        };

        assert!(tool_call.to_agent_action().is_err());
    }

    #[test]
    fn test_group_parallel_reads() {
        let actions = vec![
            AgentAction::ReadFile {
                path: "file1.rs".to_string(),
            },
            AgentAction::ReadFile {
                path: "file2.rs".to_string(),
            },
            AgentAction::ReadFile {
                path: "file3.rs".to_string(),
            },
        ];

        let grouped = group_parallel_reads(actions);
        assert_eq!(grouped.len(), 1);

        match &grouped[0] {
            AgentAction::ParallelRead { paths } => {
                assert_eq!(paths.len(), 3);
            }
            _ => panic!("Expected ParallelRead action"),
        }
    }

    #[test]
    fn test_group_parallel_reads_single_read() {
        let actions = vec![AgentAction::ReadFile {
            path: "file1.rs".to_string(),
        }];

        let grouped = group_parallel_reads(actions);
        assert_eq!(grouped.len(), 1);

        match &grouped[0] {
            AgentAction::ReadFile { path } => {
                assert_eq!(path, "file1.rs");
            }
            _ => panic!("Expected ReadFile action"),
        }
    }
}
