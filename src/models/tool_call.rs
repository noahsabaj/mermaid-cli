//! Tool call parsing and conversion to AgentAction
//!
//! Handles deserialization of Ollama tool_calls responses and converts
//! them to Mermaid's internal AgentAction enum.

use anyhow::{Result, anyhow};
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
                AgentAction::SpawnAgent {
                    prompt,
                    description,
                }
            },

            "screenshot" => {
                let mode = Self::get_optional_string_arg(args, "mode")
                    .unwrap_or_else(|| "fullscreen".to_string());
                let monitor = Self::get_optional_string_arg(args, "monitor");
                let region = Self::get_optional_string_arg(args, "region");
                let window = Self::get_optional_string_arg(args, "window");
                AgentAction::Screenshot {
                    mode,
                    monitor,
                    region,
                    window,
                }
            },

            "list_windows" => AgentAction::ListWindows,

            "click" => {
                let x = Self::get_int_arg(args, "x")? as i32;
                let y = Self::get_int_arg(args, "y")? as i32;
                let button = Self::get_optional_string_arg(args, "button")
                    .unwrap_or_else(|| "left".to_string());
                let screenshot_id = Self::get_int_arg(args, "screenshot_id")
                    .ok()
                    .map(|v| v as u64);
                AgentAction::Click {
                    x,
                    y,
                    button,
                    screenshot_id,
                }
            },

            "type_text" => {
                let text = Self::get_string_arg(args, "text")?;
                AgentAction::TypeText { text }
            },

            "press_key" => {
                let key = Self::get_string_arg(args, "key")?;
                AgentAction::PressKey { key }
            },

            "scroll" => {
                let direction = Self::get_string_arg(args, "direction")?;
                let amount = Self::get_int_arg(args, "amount").unwrap_or(3) as i32;
                AgentAction::Scroll { direction, amount }
            },

            "mouse_move" => {
                let x = Self::get_int_arg(args, "x")? as i32;
                let y = Self::get_int_arg(args, "y")? as i32;
                let screenshot_id = Self::get_int_arg(args, "screenshot_id")
                    .ok()
                    .map(|v| v as u64);
                AgentAction::MouseMove {
                    x,
                    y,
                    screenshot_id,
                }
            },

            // MCP tools: mcp__{server_name}__{tool_name}
            name if name.starts_with("mcp__") => {
                let rest = &name[5..]; // skip "mcp__"
                if let Some((server_name, tool_name)) = rest.split_once("__") {
                    AgentAction::McpToolCall {
                        server_name: server_name.to_string(),
                        tool_name: tool_name.to_string(),
                        arguments: args.clone(),
                    }
                } else {
                    return Err(anyhow!(
                        "Invalid MCP tool name format: '{}'. Expected 'mcp__{{server}}__{{tool}}'.",
                        name
                    ));
                }
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
}
