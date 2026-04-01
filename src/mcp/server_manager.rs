//! MCP server lifecycle management.
//!
//! Manages multiple MCP server processes, handles tool discovery,
//! and routes tool calls to the correct server.

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use tracing::{info, warn};

use super::client::{ContentBlock, McpClient, McpToolDef, McpToolResult};
use super::transport::StdioTransport;
use crate::app::McpServerConfig;

/// Manages multiple MCP server connections.
pub struct McpServerManager {
    /// Active server connections: server_name → client
    servers: HashMap<String, McpClient>,
    /// Cached tool definitions: (server_name, tool_def)
    tools: Vec<(String, McpToolDef)>,
}

impl McpServerManager {
    /// Start all configured MCP servers, initialize them, and discover tools.
    ///
    /// Servers that fail to start are logged and skipped (non-fatal).
    pub async fn start(configs: &HashMap<String, McpServerConfig>) -> Self {
        let mut servers = HashMap::new();
        let mut all_tools = Vec::new();

        for (name, config) in configs {
            info!("Starting MCP server: {} ({} {})", name, config.command, config.args.join(" "));

            match Self::start_one(name, config).await {
                Ok((client, tools)) => {
                    let tool_count = tools.len();
                    for tool in &tools {
                        all_tools.push((name.clone(), tool.clone()));
                    }
                    info!(
                        "MCP server '{}' ready: {} tools ({})",
                        name,
                        tool_count,
                        client
                            .server_info
                            .as_ref()
                            .map(|s| s.name.as_str())
                            .unwrap_or("?")
                    );
                    servers.insert(name.clone(), client);
                },
                Err(e) => {
                    warn!("Failed to start MCP server '{}': {}", name, e);
                },
            }
        }

        Self {
            servers,
            tools: all_tools,
        }
    }

    /// Start a single MCP server, initialize, and list tools.
    async fn start_one(
        name: &str,
        config: &McpServerConfig,
    ) -> Result<(McpClient, Vec<McpToolDef>)> {
        let transport =
            StdioTransport::spawn(&config.command, &config.args, &config.env).await?;
        let mut client = McpClient::new(transport);

        client.initialize().await.map_err(|e| {
            anyhow!("MCP server '{}' initialization failed: {}", name, e)
        })?;

        let tools = client.list_tools().await.map_err(|e| {
            anyhow!("MCP server '{}' tool discovery failed: {}", name, e)
        })?;

        Ok((client, tools))
    }

    /// Get all discovered tools with their server names.
    pub fn get_all_tools(&self) -> &[(String, McpToolDef)] {
        &self.tools
    }

    /// Check if any MCP servers are active.
    pub fn has_servers(&self) -> bool {
        !self.servers.is_empty()
    }

    /// Call a tool on a specific server.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<McpToolResult> {
        let client = self
            .servers
            .get(server_name)
            .ok_or_else(|| anyhow!("MCP server '{}' not found or not running", server_name))?;

        client.call_tool(tool_name, arguments).await
    }

    /// Convert an MCP tool result into text suitable for a tool result message.
    /// Images are returned separately for multimodal attachment.
    pub fn format_tool_result(result: &McpToolResult) -> (String, Option<Vec<String>>) {
        let mut text_parts = Vec::new();
        let mut images = Vec::new();

        for block in &result.content {
            match block {
                ContentBlock::Text(text) => text_parts.push(text.clone()),
                ContentBlock::Image { data, .. } => images.push(data.clone()),
            }
        }

        let text = if text_parts.is_empty() {
            if result.is_error {
                "MCP tool returned an error with no message".to_string()
            } else {
                "MCP tool returned no text content".to_string()
            }
        } else {
            text_parts.join("\n")
        };

        let images = if images.is_empty() {
            None
        } else {
            Some(images)
        };

        (text, images)
    }

    /// Gracefully shut down all MCP servers.
    pub async fn shutdown(&self) {
        for (name, client) in &self.servers {
            info!("Shutting down MCP server: {}", name);
            client.shutdown().await;
        }
    }
}
