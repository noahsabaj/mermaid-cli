//! MCP protocol client — higher-level API built on StdioTransport.
//!
//! Implements the three protocol methods we need:
//! - `initialize` — handshake and capability negotiation
//! - `tools/list` — discover available tools
//! - `tools/call` — invoke a tool and get results

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::transport::StdioTransport;

/// MCP protocol client for a single server connection.
pub struct McpClient {
    transport: StdioTransport,
    /// Server info from initialization
    pub server_info: Option<ServerInfo>,
}

/// Info returned by the server during initialization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: Option<String>,
}

/// A tool definition discovered from an MCP server
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Result of calling an MCP tool
#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
}

/// A content block in an MCP tool result. Per the 2025-11-25 spec,
/// servers may return text, image, audio, resource_link (URI reference),
/// or embedded resource content. Older servers only emit text/image.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    Image {
        data: String,
        mime_type: String,
    },
    /// Audio content — base64-encoded data + mime type (e.g., `audio/wav`).
    /// Routed to the model's image attachment channel for now; adapters
    /// that don't support audio will silently drop the bytes but keep
    /// the text hint from the tool output.
    Audio {
        data: String,
        mime_type: String,
    },
    /// URI reference to an external resource. Rendered as text for the
    /// model so it can follow up with another tool call if needed.
    ResourceLink {
        uri: String,
        name: Option<String>,
        description: Option<String>,
        mime_type: Option<String>,
    },
    /// Embedded resource — same shape as a read_resource response.
    /// Either `text` or `blob` (base64) is present depending on the
    /// resource's kind. Rendered as text for the model.
    Resource {
        uri: String,
        mime_type: Option<String>,
        text: Option<String>,
        blob: Option<String>,
    },
}

impl McpClient {
    /// Create a new MCP client wrapping a transport.
    pub fn new(transport: StdioTransport) -> Self {
        Self {
            transport,
            server_info: None,
        }
    }

    /// Perform the MCP initialization handshake.
    ///
    /// Sends `initialize` request with our client info and protocol version,
    /// then sends `notifications/initialized` to signal readiness.
    pub async fn initialize(&mut self) -> Result<ServerInfo> {
        let result = self
            .transport
            .send_request(
                "initialize",
                json!({
                    // MCP spec version as of 2026-04. Servers negotiate
                    // down to older versions if they don't support this;
                    // spec requires them to respond with their latest
                    // supported version, which we currently accept
                    // silently. Bump when MCP ships a newer revision
                    // with features we depend on.
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "mermaid",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .await?;

        // Parse server info
        let server_info = ServerInfo {
            name: result
                .pointer("/serverInfo/name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            version: result
                .pointer("/serverInfo/version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };

        // Send initialized notification
        self.transport
            .send_notification("notifications/initialized", json!({}))
            .await?;

        self.server_info = Some(server_info.clone());
        Ok(server_info)
    }

    /// Discover all tools available from this server.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        let result = self.transport.send_request("tools/list", json!({})).await?;

        let tools_array = result
            .get("tools")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("MCP tools/list response missing 'tools' array"))?;

        let mut tools = Vec::new();
        for tool in tools_array {
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

            if !name.is_empty() {
                tools.push(McpToolDef {
                    name,
                    description,
                    input_schema,
                });
            }
        }

        Ok(tools)
    }

    /// Call a tool on this server and return the result.
    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<McpToolResult> {
        let params = json!({
            "name": name,
            "arguments": arguments,
        });

        let result = self.transport.send_request("tools/call", params).await?;

        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let content_array = result
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut content = Vec::new();
        for block in content_array {
            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match block_type {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        content.push(ContentBlock::Text(text.to_string()));
                    }
                },
                "image" => {
                    let data = block
                        .get("data")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let mime_type = block
                        .get("mimeType")
                        .and_then(|v| v.as_str())
                        .unwrap_or("image/png")
                        .to_string();
                    content.push(ContentBlock::Image { data, mime_type });
                },
                "audio" => {
                    let data = block
                        .get("data")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let mime_type = block
                        .get("mimeType")
                        .and_then(|v| v.as_str())
                        .unwrap_or("audio/wav")
                        .to_string();
                    content.push(ContentBlock::Audio { data, mime_type });
                },
                "resource_link" => {
                    let uri = block
                        .get("uri")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if uri.is_empty() {
                        continue;
                    }
                    content.push(ContentBlock::ResourceLink {
                        uri,
                        name: block.get("name").and_then(|v| v.as_str()).map(String::from),
                        description: block
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        mime_type: block
                            .get("mimeType")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    });
                },
                "resource" => {
                    // Embedded resource — nested under `resource`.
                    let res = match block.get("resource") {
                        Some(r) => r,
                        None => continue,
                    };
                    let uri = res
                        .get("uri")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if uri.is_empty() {
                        continue;
                    }
                    content.push(ContentBlock::Resource {
                        uri,
                        mime_type: res
                            .get("mimeType")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        text: res.get("text").and_then(|v| v.as_str()).map(String::from),
                        blob: res.get("blob").and_then(|v| v.as_str()).map(String::from),
                    });
                },
                _ => {
                    // Unknown content type — treat as text if it has a text field
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        content.push(ContentBlock::Text(text.to_string()));
                    }
                },
            }
        }

        Ok(McpToolResult { content, is_error })
    }

    /// Shut down the transport (kills the server process).
    pub async fn shutdown(&self) {
        self.transport.shutdown().await;
    }
}
