//! Ollama Tools API support for native function calling
//!
//! This module defines Mermaid's available tools in Ollama's JSON Schema format,
//! replacing the legacy text-based action block system.

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::LazyLock;

/// A tool available to the model (Ollama format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub type_: String,
    pub function: ToolFunction,
}

/// Function definition for a tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Registry of all available Mermaid tools
pub struct ToolRegistry {
    tools: Vec<Tool>,
}

/// Cached Ollama JSON format for the static tool definitions.
/// Built once on first access, reused for every chat() call.
static OLLAMA_TOOLS_CACHE: LazyLock<Vec<serde_json::Value>> = LazyLock::new(|| {
    let registry = ToolRegistry::mermaid_tools();
    registry.tools.iter().map(|t| json!(t)).collect()
});

impl ToolRegistry {
    /// Create a new registry with all Mermaid tools
    pub fn mermaid_tools() -> Self {
        Self {
            tools: vec![
                Self::read_file_tool(),
                Self::write_file_tool(),
                Self::delete_file_tool(),
                Self::create_directory_tool(),
                Self::execute_command_tool(),
                Self::edit_file_tool(),
                Self::web_search_tool(),
                Self::web_fetch_tool(),
                Self::agent_tool(),
            ],
        }
    }

    /// Get a reference to the cached Ollama tool definitions without constructing a registry
    pub fn ollama_tools_cached() -> &'static [serde_json::Value] {
        &OLLAMA_TOOLS_CACHE
    }

    /// Get all tools
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    // Tool Definitions

    fn read_file_tool() -> Tool {
        Tool {
            type_: "function".to_string(),
            function: ToolFunction {
                name: "read_file".to_string(),
                description: "Read a file from the filesystem. Can read files anywhere on the system the user has access to, including outside the current project directory. Supports text files, PDFs (sent to vision models), and images.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute or relative path to the file to read. Use absolute paths (e.g., /home/user/file.pdf) for files outside the project."
                        }
                    },
                    "required": ["path"]
                }),
            },
        }
    }

    fn write_file_tool() -> Tool {
        Tool {
            type_: "function".to_string(),
            function: ToolFunction {
                name: "write_file".to_string(),
                description: "Write or create a file in the current project directory. Creates parent directories if they don't exist. Creates a timestamped backup if the file already exists.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to write, relative to the project root or absolute (must be within project)"
                        },
                        "content": {
                            "type": "string",
                            "description": "The complete file content to write"
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
        }
    }

    fn delete_file_tool() -> Tool {
        Tool {
            type_: "function".to_string(),
            function: ToolFunction {
                name: "delete_file".to_string(),
                description: "Delete a file from the project directory. Creates a timestamped backup before deletion for recovery.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to delete"
                        }
                    },
                    "required": ["path"]
                }),
            },
        }
    }

    fn create_directory_tool() -> Tool {
        Tool {
            type_: "function".to_string(),
            function: ToolFunction {
                name: "create_directory".to_string(),
                description:
                    "Create a new directory in the project. Creates parent directories if needed."
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the directory to create"
                        }
                    },
                    "required": ["path"]
                }),
            },
        }
    }

    fn execute_command_tool() -> Tool {
        Tool {
            type_: "function".to_string(),
            function: ToolFunction {
                name: "execute_command".to_string(),
                description: "Execute a shell command. Use for running tests, builds, git operations, or any terminal command. For long-running processes like servers, set a short timeout (e.g., 5) — the process will keep running after timeout.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute (e.g., 'cargo test', 'npm install')"
                        },
                        "working_dir": {
                            "type": "string",
                            "description": "Optional working directory to run the command in. Defaults to project root."
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Timeout in seconds (default: 30, max: 300). For servers/daemons, use a short timeout like 5 since the process continues running after timeout."
                        }
                    },
                    "required": ["command"]
                }),
            },
        }
    }

    fn edit_file_tool() -> Tool {
        Tool {
            type_: "function".to_string(),
            function: ToolFunction {
                name: "edit_file".to_string(),
                description: "Make targeted edits to a file by replacing specific text. \
                    The old_string must match exactly and uniquely in the file. \
                    Prefer this over write_file for modifying existing files."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to edit"
                        },
                        "old_string": {
                            "type": "string",
                            "description": "The exact text to find and replace (must be unique in the file)"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "The new text to replace old_string with"
                        }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
        }
    }

    fn web_search_tool() -> Tool {
        Tool {
            type_: "function".to_string(),
            function: ToolFunction {
                name: "web_search".to_string(),
                description: "Search the web for information. Returns full page content in markdown format for deep analysis. Use for current information, library documentation, version-specific questions, or any time-sensitive data.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query. Be specific and include version numbers when relevant (e.g., 'Rust async tokio 1.40 new features')"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Number of results to fetch (1-10). Use 3 for simple facts, 5-7 for research, 10 for comprehensive analysis.",
                            "minimum": 1,
                            "maximum": 10
                        }
                    },
                    "required": ["query", "max_results"]
                }),
            },
        }
    }

    fn web_fetch_tool() -> Tool {
        Tool {
            type_: "function".to_string(),
            function: ToolFunction {
                name: "web_fetch".to_string(),
                description: "Fetch content from a URL and return it as clean markdown. Use for reading documentation pages, articles, GitHub READMEs, or any web page the user references.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The URL to fetch content from (e.g., 'https://docs.rs/tokio/latest')"
                        }
                    },
                    "required": ["url"]
                }),
            },
        }
    }

    fn agent_tool() -> Tool {
        Tool {
            type_: "function".to_string(),
            function: ToolFunction {
                name: "agent".to_string(),
                description: "Spawn an autonomous sub-agent to handle a task independently. \
                    The agent gets its own conversation context and full tool access. \
                    Give it a self-contained task via the prompt parameter. \
                    Multiple agent calls in one response run in parallel.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The task for the agent to complete"
                        },
                        "description": {
                            "type": "string",
                            "description": "Short label for the UI (e.g., 'Read src/models/ files')"
                        }
                    },
                    "required": ["prompt", "description"]
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_registry_creation() {
        let registry = ToolRegistry::mermaid_tools();
        assert_eq!(registry.tools().len(), 9, "Should have 9 tools defined");
    }

    #[test]
    fn test_tool_serialization() {
        let ollama_tools = ToolRegistry::ollama_tools_cached();

        assert_eq!(ollama_tools.len(), 9);

        // Verify first tool has correct structure
        let first_tool = &ollama_tools[0];
        assert!(first_tool.get("type").is_some());
        assert!(first_tool.get("function").is_some());
    }

    #[test]
    fn test_read_file_tool_schema() {
        let tool = ToolRegistry::read_file_tool();
        assert_eq!(tool.function.name, "read_file");
        assert!(tool.function.description.contains("Read a file"));

        let params = tool.function.parameters.as_object().unwrap();
        assert!(params.get("properties").is_some());
        assert!(params.get("required").is_some());
    }
}
