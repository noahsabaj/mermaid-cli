//! MCP protocol client — higher-level API over a [`Transport`] (stdio child
//! process or Streamable HTTP endpoint).
//!
//! Implements the protocol methods we need:
//! - `initialize` — handshake and capability negotiation
//! - `tools/list` / `tools/call` — discover and invoke tools
//! - `resources/list` / `resources/read` — when the server advertises the
//!   `resources` capability
//! - `prompts/list` / `prompts/get` — when it advertises `prompts`

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::transport::Transport;

/// MCP protocol client for a single server connection.
pub struct McpClient {
    transport: Transport,
    /// Server info from initialization
    pub server_info: Option<ServerInfo>,
    /// Optional capabilities the server declared in its `initialize` result.
    pub capabilities: ServerCapabilities,
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

/// The optional server capabilities Mermaid uses beyond tools. A capability
/// counts as declared when its key is present in the `initialize` result's
/// `capabilities` object (the spec's sub-flags like `listChanged` don't
/// matter here).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    pub resources: bool,
    pub prompts: bool,
}

impl ServerCapabilities {
    fn from_initialize(result: &Value) -> Self {
        let declared = |key: &str| {
            result
                .pointer(&format!("/capabilities/{key}"))
                .is_some_and(|v| !v.is_null())
        };
        Self {
            resources: declared("resources"),
            prompts: declared("prompts"),
        }
    }
}

/// One `resources/list` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// One `resources/read` content item: text, or base64 `blob`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceContents {
    pub uri: String,
    pub mime_type: Option<String>,
    pub text: Option<String>,
    pub blob: Option<String>,
}

/// One `prompts/list` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptDef {
    pub name: String,
    pub description: String,
    pub arguments: Vec<mermaid_domain::McpPromptArg>,
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
            capabilities: ServerCapabilities::default(),
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

        let capabilities = ServerCapabilities::from_initialize(&result);

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
        self.capabilities = capabilities;
        Ok(server_info)
    }

    /// Discover all tools available from this server, following `nextCursor`
    /// pagination so a server that pages its tool list isn't silently truncated
    /// to page one.
    ///
    /// # Errors
    ///
    /// Any page's `tools/list` request failing, and a response with no
    /// `tools` array. A tool entry that does not parse is skipped rather than
    /// failing the discovery, and hitting the page cap returns what was
    /// collected — so a short list is not necessarily the server's whole
    /// catalog.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>> {
        self.list_paginated("tools/list", "tools", tool_def_from_json)
            .await
    }

    /// Every resource the server lists (`resources/list`), following
    /// `nextCursor` pagination like [`Self::list_tools`].
    ///
    /// # Errors
    ///
    /// Any page's request failing, and a response with no `resources` array.
    /// Entries without a `uri` are skipped.
    pub async fn list_resources(&self) -> Result<Vec<McpResource>> {
        self.list_paginated("resources/list", "resources", resource_from_json)
            .await
    }

    /// Every prompt the server lists (`prompts/list`), paginated likewise.
    ///
    /// # Errors
    ///
    /// Any page's request failing, and a response with no `prompts` array.
    /// Nameless entries are skipped.
    pub async fn list_prompts(&self) -> Result<Vec<McpPromptDef>> {
        self.list_paginated("prompts/list", "prompts", prompt_def_from_json)
            .await
    }

    /// Collect a cursor-paginated list: request `method` (with `cursor` once
    /// the server hands one back), parse each entry of the `key` array, and
    /// follow `nextCursor` until it is absent or empty. Bounded by a page cap
    /// so a server that echoes a stuck cursor can't loop forever.
    async fn list_paginated<T>(
        &self,
        method: &str,
        key: &str,
        parse: impl Fn(&Value) -> Option<T>,
    ) -> Result<Vec<T>> {
        const MAX_PAGES: usize = 100;
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.transport.send_request(method, params).await?;

            let entries = result
                .get(key)
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("MCP {method} response missing '{key}' array"))?;
            items.extend(entries.iter().filter_map(&parse));

            match result.get("nextCursor").and_then(|v| v.as_str()) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => break,
            }
        }

        Ok(items)
    }

    /// Read one resource (`resources/read`). A server may answer with several
    /// content items (a directory-like URI); each is text or a base64 blob.
    ///
    /// # Errors
    ///
    /// The request failing (transport, timeout, JSON-RPC error such as an
    /// unknown URI) and a response with no `contents` array.
    pub async fn read_resource(&self, uri: &str) -> Result<Vec<ResourceContents>> {
        let result = self
            .transport
            .send_request_with_timeout(
                "resources/read",
                json!({ "uri": uri }),
                Transport::tool_call_timeout_secs(),
            )
            .await?;
        let contents = result
            .get("contents")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("MCP resources/read response missing 'contents' array"))?;
        Ok(contents
            .iter()
            .map(|item| ResourceContents {
                uri: str_field(item, "uri").unwrap_or_else(|| uri.to_string()),
                mime_type: str_field(item, "mimeType"),
                text: str_field(item, "text"),
                blob: str_field(item, "blob"),
            })
            .collect())
    }

    /// Fetch a prompt (`prompts/get`) with its arguments filled in, returning
    /// each message's content block in order. Roles are dropped: Mermaid
    /// submits the text as one user prompt.
    ///
    /// # Errors
    ///
    /// The request failing (transport, timeout, JSON-RPC error such as a
    /// missing required argument) and a response with no `messages` array.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: &std::collections::BTreeMap<String, String>,
    ) -> Result<Vec<ContentBlock>> {
        let result = self
            .transport
            .send_request(
                "prompts/get",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let messages = result
            .get("messages")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("MCP prompts/get response missing 'messages' array"))?;
        Ok(messages
            .iter()
            .filter_map(|message| message.get("content"))
            .filter_map(parse_content_block)
            .collect())
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

/// A string field of a JSON object, owned.
fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|v| v.as_str()).map(String::from)
}

/// Parse one `resources/list` entry; `None` without a `uri`. A missing name
/// falls back to the URI so every listed resource has something to show.
fn resource_from_json(entry: &Value) -> Option<McpResource> {
    let uri = str_field(entry, "uri").filter(|uri| !uri.is_empty())?;
    Some(McpResource {
        name: str_field(entry, "name").unwrap_or_else(|| uri.clone()),
        description: str_field(entry, "description").filter(|d| !d.is_empty()),
        mime_type: str_field(entry, "mimeType"),
        uri,
    })
}

/// Parse one `prompts/list` entry; `None` without a name. Arguments keep
/// their declared order — positional slash-command arguments map onto it.
fn prompt_def_from_json(entry: &Value) -> Option<McpPromptDef> {
    let name = str_field(entry, "name").filter(|name| !name.is_empty())?;
    let arguments = entry
        .get("arguments")
        .and_then(|v| v.as_array())
        .map(|args| {
            args.iter()
                .filter_map(|arg| {
                    Some(mermaid_domain::McpPromptArg {
                        name: str_field(arg, "name").filter(|n| !n.is_empty())?,
                        description: str_field(arg, "description").unwrap_or_default(),
                        required: arg
                            .get("required")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(McpPromptDef {
        name,
        description: str_field(entry, "description").unwrap_or_default(),
        arguments,
    })
}

/// Decode one `tools/call` content block. Unknown block types fall back to
/// their `text` field; a block with nothing usable yields `None`.
fn parse_content_block(block: &Value) -> Option<ContentBlock> {
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

    use super::super::transport_http::test_fixture::{
        fixture, json_reply, rpc_response, status_reply,
    };

    /// The JSON-RPC body of one recorded HTTP request.
    fn rpc_body(raw: &str) -> Value {
        let body = raw.split_once("\r\n\r\n").map_or("", |(_, body)| body);
        serde_json::from_str(body).unwrap_or_else(|e| panic!("bad body {body:?}: {e}"))
    }

    #[tokio::test]
    async fn initialize_records_resources_and_prompts_capabilities() {
        let init = r#"{"protocolVersion":"2025-11-25","capabilities":{"tools":{},"resources":{"subscribe":false},"prompts":{"listChanged":true}},"serverInfo":{"name":"fx"}}"#;
        let fx = fixture(vec![
            json_reply(&rpc_response(1, init)),
            status_reply(202, "Accepted"),
        ])
        .await;
        let mut client = McpClient::new(fx.transport().into());
        client.initialize().await.expect("initialize");
        assert_eq!(
            client.capabilities,
            ServerCapabilities {
                resources: true,
                prompts: true
            }
        );
        // Tools-only (or a null entry) declares neither.
        let caps = ServerCapabilities::from_initialize(
            &json!({"capabilities": {"tools": {}, "prompts": null}}),
        );
        assert_eq!(caps, ServerCapabilities::default());
    }

    #[tokio::test]
    async fn list_resources_follows_cursor_pagination() {
        let page1 = r#"{"resources":[{"uri":"file:///a.md","name":"a","description":"first","mimeType":"text/markdown"},{"name":"no uri"}],"nextCursor":"p2"}"#;
        let page2 = r#"{"resources":[{"uri":"db://rows/1"}],"nextCursor":""}"#;
        let fx = fixture(vec![
            json_reply(&rpc_response(1, page1)),
            json_reply(&rpc_response(2, page2)),
        ])
        .await;
        let client = McpClient::new(fx.transport().into());
        let resources = client.list_resources().await.expect("list");
        assert_eq!(
            resources,
            [
                McpResource {
                    uri: "file:///a.md".to_string(),
                    name: "a".to_string(),
                    description: Some("first".to_string()),
                    mime_type: Some("text/markdown".to_string()),
                },
                // A nameless resource shows its URI; the uri-less entry was
                // skipped.
                McpResource {
                    uri: "db://rows/1".to_string(),
                    name: "db://rows/1".to_string(),
                    description: None,
                    mime_type: None,
                },
            ]
        );
        let reqs = fx.requests().await;
        assert_eq!(reqs.len(), 2, "an empty nextCursor ends the walk");
        let first = rpc_body(&reqs[0]);
        assert_eq!(first["method"], "resources/list");
        assert_eq!(first["params"], json!({}));
        assert_eq!(rpc_body(&reqs[1])["params"], json!({"cursor": "p2"}));
        // Serialized for the model with the protocol's own key names.
        assert_eq!(
            serde_json::to_value(&resources[1]).unwrap(),
            json!({"uri": "db://rows/1", "name": "db://rows/1"})
        );
    }

    #[tokio::test]
    async fn read_resource_sends_the_uri_and_parses_text_and_blob() {
        let result = r##"{"contents":[{"uri":"file:///a.md","mimeType":"text/markdown","text":"# A"},{"uri":"file:///a.png","mimeType":"image/png","blob":"iVBORw=="}]}"##;
        let fx = fixture(vec![json_reply(&rpc_response(1, result))]).await;
        let client = McpClient::new(fx.transport().into());
        let contents = client.read_resource("file:///a.md").await.expect("read");
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].text.as_deref(), Some("# A"));
        assert_eq!(contents[1].blob.as_deref(), Some("iVBORw=="));
        assert_eq!(contents[1].mime_type.as_deref(), Some("image/png"));
        let body = rpc_body(&fx.requests().await[0]);
        assert_eq!(body["method"], "resources/read");
        assert_eq!(body["params"], json!({"uri": "file:///a.md"}));
    }

    #[tokio::test]
    async fn prompts_list_and_get_round_trip() {
        let list = r#"{"prompts":[{"name":"review","description":"Review code","arguments":[{"name":"file","description":"Path","required":true},{"name":"focus"}]},{"description":"nameless"}]}"#;
        let get = r#"{"messages":[{"role":"user","content":{"type":"text","text":"Review a.rs"}},{"role":"assistant","content":{"type":"image","data":"AA==","mimeType":"image/png"}}]}"#;
        let fx = fixture(vec![
            json_reply(&rpc_response(1, list)),
            json_reply(&rpc_response(2, get)),
        ])
        .await;
        let client = McpClient::new(fx.transport().into());
        let prompts = client.list_prompts().await.expect("list");
        assert_eq!(prompts.len(), 1, "the nameless prompt is skipped");
        assert_eq!(prompts[0].name, "review");
        assert_eq!(
            prompts[0].arguments,
            [
                mermaid_domain::McpPromptArg {
                    name: "file".to_string(),
                    description: "Path".to_string(),
                    required: true,
                },
                mermaid_domain::McpPromptArg {
                    name: "focus".to_string(),
                    description: String::new(),
                    required: false,
                },
            ]
        );

        let arguments =
            std::collections::BTreeMap::from([("file".to_string(), "a.rs".to_string())]);
        let blocks = client.get_prompt("review", &arguments).await.expect("get");
        assert!(matches!(&blocks[0], ContentBlock::Text(t) if t == "Review a.rs"));
        assert!(matches!(&blocks[1], ContentBlock::Image { .. }));
        let body = rpc_body(&fx.requests().await[1]);
        assert_eq!(body["method"], "prompts/get");
        assert_eq!(
            body["params"],
            json!({"name": "review", "arguments": {"file": "a.rs"}})
        );
    }
}
