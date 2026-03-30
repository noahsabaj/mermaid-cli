//! Tool call parsing and conversion to AgentAction
//!
//! Handles deserialization of Ollama tool_calls responses and converts
//! them to Mermaid's internal AgentAction enum.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::warn;

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
                AgentAction::ReadFile { paths: vec![path] }
            },

            "write_file" => {
                let path = Self::get_string_arg(args, "path")?;
                let content = Self::get_string_arg(args, "content")?;
                AgentAction::WriteFile { path, content }
            },

            "delete_file" => {
                let path = Self::get_string_arg(args, "path")?;
                AgentAction::DeleteFile { path }
            },

            "create_directory" => {
                let path = Self::get_string_arg(args, "path")?;
                AgentAction::CreateDirectory { path }
            },

            "execute_command" => {
                let command = Self::get_string_arg(args, "command")?;
                let working_dir = Self::get_optional_string_arg(args, "working_dir");
                let timeout = args.get("timeout").and_then(|v| v.as_u64());
                AgentAction::ExecuteCommand {
                    command,
                    working_dir,
                    timeout,
                }
            },

            "web_search" => {
                let query = Self::get_string_arg(args, "query")?;
                let max_results = Self::get_int_arg(args, "max_results")
                    .or_else(|_| Self::get_int_arg(args, "result_count"))
                    .unwrap_or(5)
                    .clamp(1, 10);
                AgentAction::WebSearch {
                    queries: vec![(query, max_results)],
                }
            },

            "edit_file" => {
                let path = Self::get_string_arg(args, "path")?;
                let old_string = Self::get_string_arg(args, "old_string")?;
                let new_string = Self::get_string_arg(args, "new_string")?;
                AgentAction::EditFile {
                    path,
                    old_string,
                    new_string,
                }
            },

            "web_fetch" => {
                let url = Self::get_string_arg(args, "url")?;
                AgentAction::WebFetch { url }
            },

            "agent" => {
                let prompt = Self::get_string_arg(args, "prompt")?;
                let description = Self::get_string_arg(args, "description")?;
                AgentAction::SpawnAgent { prompt, description }
            },

            name => {
                return Err(anyhow!(
                    "Unknown tool: '{}'. Model attempted to call a tool that doesn't exist.",
                    name
                ));
            },
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
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    fn get_int_arg(args: &serde_json::Value, key: &str) -> Result<usize> {
        args.get(key)
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
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
                warn!(tool = %tc.function.name, "Failed to parse tool call: {}", e);
                None
            },
        })
        .collect()
}

/// Group consecutive same-type read operations into a single ReadFile action
/// The executor will decide whether to parallelize based on the number of paths
pub fn group_parallel_reads(actions: Vec<AgentAction>) -> Vec<AgentAction> {
    if actions.is_empty() {
        return actions;
    }

    let mut result = Vec::new();
    let mut current_group: Vec<String> = Vec::new();

    for action in actions {
        match action {
            AgentAction::ReadFile { paths } => {
                current_group.extend(paths);
            },
            other => {
                // Flush current read group
                if !current_group.is_empty() {
                    result.push(AgentAction::ReadFile {
                        paths: std::mem::take(&mut current_group),
                    });
                }
                result.push(other);
            },
        }
    }

    // Flush remaining read group
    if !current_group.is_empty() {
        result.push(AgentAction::ReadFile {
            paths: current_group,
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
            AgentAction::ReadFile { paths } => {
                assert_eq!(paths.len(), 1);
                assert_eq!(paths[0], "src/main.rs");
            },
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
            },
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
                timeout,
            } => {
                assert_eq!(command, "cargo test");
                assert_eq!(working_dir, Some("/path/to/project".to_string()));
                assert_eq!(timeout, None);
            },
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
            AgentAction::WebSearch { queries } => {
                assert_eq!(queries.len(), 1);
                assert_eq!(queries[0].0, "Rust async features");
                assert_eq!(queries[0].1, 5);
            },
            _ => panic!("Expected WebSearch action"),
        }
    }

    #[test]
    fn test_parse_agent_tool_call() {
        let tool_call = ToolCall {
            id: Some("call_agent_1".to_string()),
            function: FunctionCall {
                name: "agent".to_string(),
                arguments: json!({
                    "prompt": "Read all files in src/models/ and summarize them",
                    "description": "Read src/models/ files"
                }),
            },
        };

        let action = tool_call.to_agent_action().unwrap();
        match action {
            AgentAction::SpawnAgent {
                prompt,
                description,
            } => {
                assert!(prompt.contains("src/models/"));
                assert_eq!(description, "Read src/models/ files");
            },
            _ => panic!("Expected SpawnAgent action"),
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
                paths: vec!["file1.rs".to_string()],
            },
            AgentAction::ReadFile {
                paths: vec!["file2.rs".to_string()],
            },
            AgentAction::ReadFile {
                paths: vec!["file3.rs".to_string()],
            },
        ];

        let grouped = group_parallel_reads(actions);
        assert_eq!(grouped.len(), 1);

        match &grouped[0] {
            AgentAction::ReadFile { paths } => {
                assert_eq!(paths.len(), 3);
                assert_eq!(paths[0], "file1.rs");
                assert_eq!(paths[1], "file2.rs");
                assert_eq!(paths[2], "file3.rs");
            },
            _ => panic!("Expected ReadFile action"),
        }
    }

    #[test]
    fn test_group_parallel_reads_single_read() {
        let actions = vec![AgentAction::ReadFile {
            paths: vec!["file1.rs".to_string()],
        }];

        let grouped = group_parallel_reads(actions);
        assert_eq!(grouped.len(), 1);

        match &grouped[0] {
            AgentAction::ReadFile { paths } => {
                assert_eq!(paths.len(), 1);
                assert_eq!(paths[0], "file1.rs");
            },
            _ => panic!("Expected ReadFile action"),
        }
    }
}
