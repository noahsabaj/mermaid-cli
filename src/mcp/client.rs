//! MCP protocol client — higher-level API over a [`Transport`] (stdio child
//! process or Streamable HTTP endpoint).
//!
//! Implements the three protocol methods we need:
//! - `initialize` — handshake and capability negotiation
//! - `tools/list` — discover available tools
//! - `tools/call` — invoke a tool and get results

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::transport::Transport;

/// MCP protocol client for a single server connection.
pub struct McpClient {
    transport: Transport,
    /// Server info from initialization
    pub server_info: Option<ServerInfo>,
    /// Set once [`Self::shutdown`] runs, so a later `call_tool` returns a clean
    /// "stopped" error instead of a broken-pipe transport error — the manager
    /// keeps the entry in its frozen map, so the client outlives its process.
    shutdown: std::sync::atomic::AtomicBool,
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
    /// The server's `annotations.readOnlyHint` — an UNTRUSTED self-declaration
    /// that the tool has no side effects. Absent ⇒ false, i.e. write-shaped
    /// (fail closed). Feeds the external-writes policy floor.
    pub read_only_hint: bool,
}

/// Result of calling an MCP tool
#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
}

/// A content block in an MCP tool result.
///
/// Per the 2025-11-25 spec, servers may return text, image, audio,
/// `resource_link` (URI reference), or embedded resource content. Older
/// servers only emit text/image.
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
    /// Embedded resource — same shape as a `read_resource` response.
    /// Either `text` or `blob` (base64) is present depending on the
    /// resource's kind. Rendered as text for the model.
    Resource {
        uri: String,
        mime_type: Option<String>,
        text: Option<String>,
        blob: Option<String>,
    },
}

/// Parse one `tools/list` entry. `None` for nameless entries (skipped, as
/// before). Annotations are optional per the MCP spec; a missing or
/// non-boolean `readOnlyHint` is treated as false — write-shaped, fail
/// closed.
fn tool_def_from_json(tool: &Value) -> Option<McpToolDef> {
    let name = tool.get("name").and_then(|v| v.as_str())?;
    if name.is_empty() {
        return None;
    }
    Some(McpToolDef {
        name: name.to_string(),
        description: tool
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        input_schema: tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        read_only_hint: tool
            .pointer("/annotations/readOnlyHint")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

impl McpClient {
    /// Create a new MCP client wrapping a transport.
    pub(super) fn new(transport: Transport) -> Self {
        Self {
            transport,
            server_info: None,
            shutdown: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Perform the MCP initialization handshake.
    ///
    /// Sends `initialize` request with our client info and protocol version,
    /// then sends `notifications/initialized` to signal readiness.
    ///
    /// # Errors
    ///
    /// The `initialize` request failing — transport, timeout, or a JSON-RPC
    /// error from the server — and the `notifications/initialized` send. A
    /// server that omits `serverInfo` or negotiates down to an older
    /// `protocolVersion` is not an error: the name falls back to `"unknown"`
    /// and whatever version it names is what subsequent requests carry.
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

        // Record the negotiated protocol version BEFORE the initialized
        // notification: over HTTP every request after initialize — including
        // that notification — must carry the MCP-Protocol-Version header.
        if let Some(version) = result.get("protocolVersion").and_then(|v| v.as_str()) {
            self.transport.set_protocol_version(version);
        }

        // Send initialized notification
        self.transport
            .send_notification("notifications/initialized", json!({}))
            .await?;

        self.server_info = Some(server_info.clone());
        Ok(server_info)
    }

    /// Discover all tools available from this server, following `nextCursor`
    /// pagination so a server that pages its tool list isn't silently truncated
    /// to page one. Bounded by a page cap so a server that echoes a stuck cursor
    /// can't loop forever.
    ///
    /// # Errors
    ///
    /// Any page's `tools/list` request failing, and a response with no
    /// `tools` array. A tool entry that does not parse is skipped rather than
    /// failing the discovery, and hitting the page cap returns what was
    /// collected — so a short list is not necessarily the server's whole
    /// catalog.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        const MAX_PAGES: usize = 100;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.transport.send_request("tools/list", params).await?;

            let tools_array = result
                .get("tools")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("MCP tools/list response missing 'tools' array"))?;

            for tool in tools_array {
                if let Some(def) = tool_def_from_json(tool) {
                    tools.push(def);
                }
            }

            match result.get("nextCursor").and_then(|v| v.as_str()) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => break,
            }
        }

        Ok(tools)
    }

    /// Call a tool on this server and return the result.
    ///
    /// # Errors
    ///
    /// The `tools/call` request failing: transport, the tool-call timeout, or
    /// a JSON-RPC error. A tool that runs and reports failure is not among
    /// them — that is `isError` on the returned [`McpToolResult`], which the
    /// model is meant to see and react to.
    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<McpToolResult> {
        let params = json!({
            "name": name,
            "arguments": arguments,
        });

        let result = self
            .transport
            .send_request_with_timeout("tools/call", params, Transport::tool_call_timeout_secs())
            .await?;

        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let content_array = result
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let content = content_array
            .iter()
            .filter_map(parse_content_block)
            .collect();

        Ok(McpToolResult { content, is_error })
    }

    /// Shut down the transport (kills the server process).
    pub async fn shutdown(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        self.transport.shutdown().await;
    }

    /// `true` once [`Self::shutdown`] has run. The manager checks this so a
    /// `call_tool` to a stopped-but-still-registered server returns a clean
    /// error rather than a broken-pipe transport failure.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Decode one `tools/call` content block. Unknown block types fall back to
/// their `text` field; a block with nothing usable yields `None`.
fn parse_content_block(block: &Value) -> Option<ContentBlock> {
    let str_field = |v: &Value, key: &str| v.get(key).and_then(|v| v.as_str()).map(String::from);
    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match block_type {
        "text" => str_field(block, "text").map(ContentBlock::Text),
        "image" => Some(ContentBlock::Image {
            data: str_field(block, "data").unwrap_or_default(),
            mime_type: str_field(block, "mimeType").unwrap_or_else(|| "image/png".to_string()),
        }),
        "audio" => Some(ContentBlock::Audio {
            data: str_field(block, "data").unwrap_or_default(),
            mime_type: str_field(block, "mimeType").unwrap_or_else(|| "audio/wav".to_string()),
        }),
        "resource_link" => {
            let uri = str_field(block, "uri").filter(|uri| !uri.is_empty())?;
            Some(ContentBlock::ResourceLink {
                uri,
                name: str_field(block, "name"),
                description: str_field(block, "description"),
                mime_type: str_field(block, "mimeType"),
            })
        },
        "resource" => {
            // Embedded resource — nested under `resource`.
            let res = block.get("resource")?;
            let uri = str_field(res, "uri").filter(|uri| !uri.is_empty())?;
            Some(ContentBlock::Resource {
                uri,
                mime_type: str_field(res, "mimeType"),
                text: str_field(res, "text"),
                blob: str_field(res, "blob"),
            })
        },
        // Unknown content type — treat as text if it has a text field.
        _ => str_field(block, "text").map(ContentBlock::Text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_def_parses_read_only_hint() {
        // Annotated read-only tool carries the hint through.
        let def = tool_def_from_json(&json!({
            "name": "get_thing",
            "description": "d",
            "inputSchema": {"type": "object"},
            "annotations": {"readOnlyHint": true}
        }))
        .unwrap();
        assert!(def.read_only_hint);

        // Absent annotations (the common case) ⇒ write-shaped, fail closed.
        let def = tool_def_from_json(&json!({"name": "send_thing"})).unwrap();
        assert!(!def.read_only_hint);
        assert_eq!(
            def.input_schema,
            json!({"type": "object", "properties": {}})
        );

        // Non-boolean hints and nameless entries are rejected safely.
        let def = tool_def_from_json(&json!({
            "name": "odd",
            "annotations": {"readOnlyHint": "yes"}
        }))
        .unwrap();
        assert!(!def.read_only_hint);
        assert!(tool_def_from_json(&json!({"description": "nameless"})).is_none());
    }
}
