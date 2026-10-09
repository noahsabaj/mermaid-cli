//! MCP protocol client — higher-level API over a [`Transport`] (stdio child
//! process or Streamable HTTP endpoint).
//!
//! Dual-era: Mermaid speaks MCP 2026-07-28 ("modern": no handshake, every
//! request carries its version, client info and capabilities in `_meta`)
//! and falls back to 2025-11-25 ("legacy": the `initialize` handshake) for
//! servers that do not speak it yet. [`McpClient::initialize`] finds out
//! which with a `server/discover` probe, per the spec's backward
//! compatibility rules, and the answer holds for the life of the connection.
//!
//! Methods used: `server/discover` or `initialize`, `tools/list`,
//! `tools/call`; `resources/list` / `resources/read` when the server declares
//! the `resources` capability, `prompts/list` / `prompts/get` when it
//! declares `prompts`.

use anyhow::{Result, anyhow, bail};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::RwLock;

use super::param_headers::{self, ParamHeader};
use super::transport::{ConnectionClosed, JsonRpcError, RequestTimeout, Transport};
use super::transport_http::HttpStatusError;

/// The newest MCP revision; tried first.
pub(super) const MODERN_VERSION: &str = "2026-07-28";
/// The handshake revision used for servers that do not speak
/// [`MODERN_VERSION`].
pub(super) const LEGACY_VERSION: &str = "2025-11-25";

/// How long a stdio server has to answer the `server/discover` probe before
/// it counts as legacy (some legacy servers ignore an unknown request sent
/// before `initialize`). Long enough for an `npx` server that is still
/// starting.
const PROBE_TIMEOUT_SECS: u64 = 20;
/// Retries of a `tools/call` that answers `input_required` with only a
/// `requestState` (the server shedding load) before giving up.
const MAX_INPUT_ROUNDS: usize = 5;

/// 2026-07-28 JSON-RPC error codes. Any of them proves the server is modern.
const HEADER_MISMATCH: i64 = -32020;
const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// Which protocol a connected server speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Era {
    /// 2026-07-28: per-request `_meta`, no session.
    Modern,
    /// 2025-11-25: the `initialize` handshake.
    Legacy,
}

/// The stdio server exited when it got the `server/discover` probe, so it
/// cannot fall back to `initialize` in the same process. The caller starts
/// it again and calls [`McpClient::initialize_legacy`].
#[derive(Debug)]
pub(super) struct ProbeEndedServer;

impl std::fmt::Display for ProbeEndedServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MCP server exited on the server/discover probe")
    }
}

impl std::error::Error for ProbeEndedServer {}

/// MCP protocol client for a single server connection.
pub struct McpClient {
    transport: Transport,
    /// Server info from initialization
    pub server_info: Option<ServerInfo>,
    /// Optional capabilities the server declared, in its `DiscoverResult` or
    /// its `initialize` result.
    pub capabilities: ServerCapabilities,
    /// Set by [`Self::initialize`].
    era: Era,
    /// The `x-mcp-header` parameters of each tool, by tool name, from the
    /// last `tools/list`. Only kept for a modern server over HTTP.
    param_headers: RwLock<HashMap<String, Vec<ParamHeader>>>,
    /// [`PROBE_TIMEOUT_SECS`]; shorter in tests.
    probe_timeout_secs: u64,
    /// Set once [`Self::shutdown`] runs, so a later `call_tool` returns a clean
    /// "stopped" error instead of a broken-pipe transport error — the manager
    /// keeps the entry in its frozen map, so the client outlives its process.
    shutdown: std::sync::atomic::AtomicBool,
}

/// What the `server/discover` probe says about the server.
#[derive(Debug)]
enum Probe {
    /// A `DiscoverResult`.
    Modern(Value),
    /// A modern server that does not support 2026-07-28; its supported list.
    Unsupported(Vec<String>),
    /// Any other error: a legacy server.
    Legacy,
    /// No answer from a stdio server: legacy, unless `initialize` then shows
    /// it was only slow to start.
    Silent,
    /// The stdio server exited.
    Ended,
}

/// Info returned by the server during initialization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: Option<String>,
}

/// The optional server capabilities Mermaid uses beyond tools. A capability
/// counts as declared when its key is present in the `capabilities` object
/// of the `DiscoverResult` or the `initialize` result (the spec's sub-flags like `listChanged` don't
/// matter here).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    pub resources: bool,
    pub prompts: bool,
}

impl ServerCapabilities {
    fn from_result(result: &Value) -> Self {
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
            era: Era::Legacy,
            param_headers: RwLock::new(HashMap::new()),
            probe_timeout_secs: PROBE_TIMEOUT_SECS,
            shutdown: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The protocol the server speaks, once [`Self::initialize`] has run.
    pub fn era(&self) -> Era {
        self.era
    }

    /// Connect: probe with `server/discover` and speak 2026-07-28 if the
    /// server does; else run the 2025-11-25 `initialize` handshake.
    ///
    /// Per the spec, a `DiscoverResult` or a 2026-07-28 error code means
    /// modern. Any other JSON-RPC error, an HTTP `4xx`/`5xx` without one, or
    /// (stdio) no answer in [`PROBE_TIMEOUT_SECS`] means legacy.
    ///
    /// # Errors
    ///
    /// [`ProbeEndedServer`] when a stdio server exited on the probe (start it
    /// again and call [`Self::initialize_legacy`]). Otherwise the transport
    /// failing, a sign-in being needed, a modern server that supports
    /// neither revision, or the legacy handshake failing. A server that
    /// omits its name is not an error: it reads as `"unknown"`.
    pub async fn initialize(&mut self) -> Result<ServerInfo> {
        self.transport.set_modern(Some(MODERN_VERSION));
        let reply = self
            .transport
            .send_request_with(
                "server/discover",
                with_meta(json!({})),
                self.probe_timeout_secs,
                &HeaderMap::new(),
            )
            .await;
        match self.classify_probe(reply).await? {
            Probe::Modern(result) => self.adopt_discover(&result).await,
            Probe::Unsupported(versions) => self.choose_from(&versions).await,
            Probe::Legacy => self.initialize_legacy().await,
            Probe::Silent => match self.initialize_legacy().await {
                Ok(info) => Ok(info),
                // A modern server that was only slow to start answered the
                // handshake with an error; it can answer the probe now.
                Err(e) if json_rpc_error(&e).is_some() => self.reprobe(e).await,
                Err(e) => Err(e),
            },
            Probe::Ended => Err(ProbeEndedServer.into()),
        }
    }

    /// Sort the probe's reply into a [`Probe`]. `Err` for failures that say
    /// nothing about the era: an HTTP transport error or timeout, a sign-in
    /// requirement, a modern error other than an unsupported version.
    async fn classify_probe(&self, reply: Result<Value>) -> Result<Probe> {
        let e = match reply {
            Ok(result) if result.get("supportedVersions").is_some_and(Value::is_array) => {
                return Ok(Probe::Modern(result));
            },
            // A result that is no DiscoverResult: an old server answering
            // whatever it got.
            Ok(_) => return Ok(Probe::Legacy),
            Err(e) => e,
        };
        if let Some(rpc) = json_rpc_error(&e) {
            return match rpc.code {
                UNSUPPORTED_PROTOCOL_VERSION => Ok(Probe::Unsupported(supported_versions(rpc))),
                HEADER_MISMATCH | MISSING_REQUIRED_CLIENT_CAPABILITY => Err(e),
                _ => Ok(Probe::Legacy),
            };
        }
        if let Some(status) = find::<HttpStatusError>(&e) {
            // 401/403 are about the sign-in, not the protocol.
            return if matches!(status.status, 401 | 403) {
                Err(e)
            } else {
                Ok(Probe::Legacy)
            };
        }
        if self.transport.is_http() {
            return Err(e);
        }
        if find::<ConnectionClosed>(&e).is_some() || self.transport.has_exited().await {
            return Ok(Probe::Ended);
        }
        if find::<RequestTimeout>(&e).is_some() {
            return Ok(Probe::Silent);
        }
        Ok(Probe::Legacy)
    }

    /// Use a `DiscoverResult`: modern if it lists 2026-07-28, else the
    /// handshake if it lists a legacy revision.
    async fn adopt_discover(&mut self, result: &Value) -> Result<ServerInfo> {
        let versions = string_list(result.get("supportedVersions"));
        if !versions.iter().any(|v| v == MODERN_VERSION) {
            return self.choose_from(&versions).await;
        }
        self.era = Era::Modern;
        self.capabilities = ServerCapabilities::from_result(result);
        let info = result
            .pointer("/_meta/io.modelcontextprotocol~1serverInfo")
            .map_or_else(unknown_server, server_info_from);
        self.server_info = Some(info.clone());
        Ok(info)
    }

    /// A modern server that does not support 2026-07-28: use the handshake
    /// if it still supports a legacy revision.
    async fn choose_from(&mut self, versions: &[String]) -> Result<ServerInfo> {
        if versions.iter().any(|v| v.as_str() <= LEGACY_VERSION) {
            return self.initialize_legacy().await;
        }
        bail!(
            "MCP server supports protocol versions [{}]; Mermaid speaks {MODERN_VERSION} and {LEGACY_VERSION}",
            versions.join(", ")
        )
    }

    /// The handshake failed after a silent probe: probe once more, now that
    /// the server is up. `first` is the handshake's error, returned if the
    /// server is not modern after all.
    async fn reprobe(&mut self, first: anyhow::Error) -> Result<ServerInfo> {
        self.transport.set_modern(Some(MODERN_VERSION));
        let reply = self
            .transport
            .send_request_with(
                "server/discover",
                with_meta(json!({})),
                self.probe_timeout_secs,
                &HeaderMap::new(),
            )
            .await;
        match self.classify_probe(reply).await {
            Ok(Probe::Modern(result)) => self.adopt_discover(&result).await,
            _ => {
                self.transport.set_modern(None);
                Err(first)
            },
        }
    }

    /// The 2025-11-25 handshake: `initialize`, then
    /// `notifications/initialized`.
    ///
    /// # Errors
    ///
    /// The `initialize` request failing — transport, timeout, or a JSON-RPC
    /// error from the server — and the `notifications/initialized` send. A
    /// server that negotiates down to an older `protocolVersion` is not an
    /// error: whatever version it names is what later requests carry.
    pub async fn initialize_legacy(&mut self) -> Result<ServerInfo> {
        self.transport.set_modern(None);
        self.era = Era::Legacy;
        let result = self
            .transport
            .send_request(
                "initialize",
                json!({
                    "protocolVersion": LEGACY_VERSION,
                    "capabilities": {},
                    "clientInfo": client_info(),
                }),
            )
            .await?;

        let server_info = result
            .get("serverInfo")
            .map_or_else(unknown_server, server_info_from);
        self.capabilities = ServerCapabilities::from_result(&result);

        // Record the negotiated protocol version BEFORE the initialized
        // notification: over HTTP every request after initialize — including
        // that notification — must carry the MCP-Protocol-Version header.
        if let Some(version) = result.get("protocolVersion").and_then(|v| v.as_str()) {
            self.transport.set_protocol_version(version);
        }

        self.transport
            .send_notification("notifications/initialized", json!({}))
            .await?;

        self.server_info = Some(server_info.clone());
        Ok(server_info)
    }

    /// Send a request in the server's era: a modern request carries the
    /// `_meta` protocol fields.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout_secs: u64,
        headers: &HeaderMap,
    ) -> Result<Value> {
        let params = match self.era {
            Era::Modern => with_meta(params),
            Era::Legacy => params,
        };
        self.transport
            .send_request_with(method, params, timeout_secs, headers)
            .await
    }

    /// True when calls must carry the 2026-07-28 HTTP mirror headers.
    fn mirrors_headers(&self) -> bool {
        self.era == Era::Modern && self.transport.is_http()
    }

    /// Discover all tools available from this server, following `nextCursor`
    /// pagination so a server that pages its tool list isn't silently truncated
    /// to page one. Bounded by a page cap so a server that echoes a stuck cursor
    /// can't loop forever.
    ///
    /// On a modern HTTP server, a tool whose `x-mcp-header` annotations
    /// break the spec's rules is left out with a warning, as the spec
    /// requires.
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
        let mut headers = HashMap::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self
                .request(
                    "tools/list",
                    params,
                    super::transport::REQUEST_TIMEOUT_SECS,
                    &HeaderMap::new(),
                )
                .await?;

            let tools_array = result
                .get("tools")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow!("MCP tools/list response missing 'tools' array"))?;

            for tool in tools_array {
                let Some(def) = tool_def_from_json(tool) else {
                    continue;
                };
                if self.mirrors_headers() {
                    match param_headers::param_headers(&def.input_schema) {
                        Ok(params) => {
                            headers.insert(def.name.clone(), params);
                        },
                        Err(reason) => {
                            tracing::warn!("MCP: leaving out tool '{}': {reason:#}", def.name);
                            continue;
                        },
                    }
                }
                tools.push(def);
            }

            match result.get("nextCursor").and_then(|v| v.as_str()) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => break,
            }
        }

        if self.mirrors_headers() {
            *self
                .param_headers
                .write()
                .expect("mcp param_headers lock poisoned") = headers;
        }
        Ok(tools)
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
            let result = self
                .request(
                    method,
                    params,
                    super::transport::REQUEST_TIMEOUT_SECS,
                    &HeaderMap::new(),
                )
                .await?;

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
            .request(
                "resources/read",
                json!({ "uri": uri }),
                Transport::tool_call_timeout_secs(),
                &HeaderMap::new(),
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
            .request(
                "prompts/get",
                json!({ "name": name, "arguments": arguments }),
                super::transport::REQUEST_TIMEOUT_SECS,
                &HeaderMap::new(),
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

    /// The HTTP mirror headers for a call to `name`; empty off modern HTTP.
    fn call_headers(&self, name: &str, arguments: &Value) -> Result<HeaderMap> {
        if !self.mirrors_headers() {
            return Ok(HeaderMap::new());
        }
        let guard = self
            .param_headers
            .read()
            .expect("mcp param_headers lock poisoned");
        let params = guard.get(name).map_or(&[][..], Vec::as_slice);
        param_headers::call_headers(name, params, arguments)
    }

    /// Call a tool on this server and return the result.
    ///
    /// A modern server may answer `input_required`. With only a
    /// `requestState` (no questions), the call is sent again with that
    /// state, up to [`MAX_INPUT_ROUNDS`] times. A server that asks
    /// questions (elicitation, sampling, roots) gets an error: Mermaid
    /// declares none of those capabilities. A `HeaderMismatch` reply reloads
    /// the tool list once and retries, as the spec advises.
    ///
    /// # Errors
    ///
    /// The `tools/call` request failing: transport, the tool-call timeout, or
    /// a JSON-RPC error; an `input_required` reply that asks for input, or
    /// that does not settle; an unknown `resultType`. A tool that runs and
    /// reports failure is not among them — that is `isError` on the returned
    /// [`McpToolResult`], which the model is meant to see and react to.
    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<McpToolResult> {
        let mut params = json!({
            "name": name,
            "arguments": arguments,
        });
        let mut headers = self.call_headers(name, arguments)?;
        let mut reloaded = false;
        let mut rounds = 0;

        loop {
            let result = match self
                .request(
                    "tools/call",
                    params.clone(),
                    Transport::tool_call_timeout_secs(),
                    &headers,
                )
                .await
            {
                Err(e)
                    if !reloaded
                        && self.mirrors_headers()
                        && json_rpc_error(&e).is_some_and(|r| r.code == HEADER_MISMATCH) =>
                {
                    reloaded = true;
                    self.list_tools().await?;
                    headers = self.call_headers(name, arguments)?;
                    continue;
                },
                other => other?,
            };

            match result.get("resultType").and_then(Value::as_str) {
                None | Some("complete") => return Ok(tool_result_from_json(&result)),
                Some("input_required") => {},
                Some(other) => bail!("MCP tool '{name}' returned unknown resultType '{other}'"),
            }
            if let Some(asks) = result
                .get("inputRequests")
                .and_then(Value::as_object)
                .filter(|m| !m.is_empty())
            {
                let methods: Vec<&str> = asks
                    .values()
                    .filter_map(|r| r.get("method").and_then(Value::as_str))
                    .collect();
                bail!(
                    "MCP tool '{name}' asked for input Mermaid does not support ({})",
                    methods.join(", ")
                );
            }
            rounds += 1;
            if rounds > MAX_INPUT_ROUNDS {
                bail!(
                    "MCP tool '{name}' still answered input_required after {MAX_INPUT_ROUNDS} retries"
                );
            }
            // Echo the state exactly; with none, send none.
            let object = params.as_object_mut().expect("params is an object");
            match result.get("requestState").and_then(Value::as_str) {
                Some(state) => {
                    object.insert("requestState".into(), Value::String(state.to_string()));
                },
                None => {
                    object.remove("requestState");
                },
            }
        }
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

/// Mermaid's `Implementation` object.
fn client_info() -> Value {
    json!({
        "name": "mermaid",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

/// Add the 2026-07-28 per-request protocol fields to `params._meta`,
/// keeping any `_meta` the caller set.
pub(super) fn with_meta(mut params: Value) -> Value {
    if !params.is_object() {
        params = json!({});
    }
    let object = params.as_object_mut().expect("params is an object");
    let meta = object.entry("_meta").or_insert_with(|| json!({}));
    if !meta.is_object() {
        *meta = json!({});
    }
    let meta = meta.as_object_mut().expect("_meta is an object");
    meta.insert(
        "io.modelcontextprotocol/protocolVersion".into(),
        json!(MODERN_VERSION),
    );
    meta.insert("io.modelcontextprotocol/clientInfo".into(), client_info());
    // Mermaid offers no client features (sampling, elicitation, roots).
    meta.insert(
        "io.modelcontextprotocol/clientCapabilities".into(),
        json!({}),
    );
    params
}

fn server_info_from(info: &Value) -> ServerInfo {
    ServerInfo {
        name: info
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        version: info
            .get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

fn unknown_server() -> ServerInfo {
    ServerInfo {
        name: "unknown".to_string(),
        version: None,
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// `data.supported` of an `UnsupportedProtocolVersionError`.
fn supported_versions(rpc: &JsonRpcError) -> Vec<String> {
    string_list(rpc.data.as_ref().and_then(|d| d.get("supported")))
}

/// The first error of type `T` in `e`'s chain.
fn find<T: std::error::Error + Send + Sync + 'static>(e: &anyhow::Error) -> Option<&T> {
    e.chain().find_map(|cause| cause.downcast_ref::<T>())
}

/// The JSON-RPC error in `e`: from a JSON-RPC reply, or from the body of an
/// HTTP error status.
fn json_rpc_error(e: &anyhow::Error) -> Option<&JsonRpcError> {
    find::<JsonRpcError>(e).or_else(|| find::<HttpStatusError>(e).and_then(|s| s.rpc.as_ref()))
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

/// Parse a complete `tools/call` result.
fn tool_result_from_json(result: &Value) -> McpToolResult {
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let content = result
        .get("content")
        .and_then(|v| v.as_array())
        .map(|blocks| blocks.iter().filter_map(parse_content_block).collect())
        .unwrap_or_default();

    McpToolResult { content, is_error }
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

    /// A client on a `sh -c` fake server. Requests are numbered from 1:
    /// the probe is 1.
    #[cfg(unix)]
    async fn sh_client(script: &str) -> McpClient {
        let t = super::super::transport::StdioTransport::spawn(
            "sh",
            &["-c".to_string(), script.to_string()],
            &HashMap::new(),
        )
        .await
        .expect("spawn");
        let mut client = McpClient::new(t.into());
        client.probe_timeout_secs = 1;
        client
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_legacy_server_that_rejects_the_probe_gets_the_handshake() {
        let mut client = sh_client(
            r#"read probe
printf '{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}\n'
read init
case "$init" in *'"method":"initialize"'*) ;; *) exit 1 ;; esac
printf '{"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"old"}}}\n'
read initialized
read list
case "$list" in *io.modelcontextprotocol*) exit 1 ;; esac
printf '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"a"}]}}\n'
sleep 5"#,
        )
        .await;
        assert_eq!(client.initialize().await.expect("initialize").name, "old");
        assert_eq!(client.era(), Era::Legacy);
        assert_eq!(client.list_tools().await.expect("list").len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_modern_server_gets_meta_and_no_handshake() {
        let mut client = sh_client(
            r#"read probe
case "$probe" in *'"io.modelcontextprotocol/protocolVersion":"2026-07-28"'*) ;; *) exit 1 ;; esac
printf '{"jsonrpc":"2.0","id":1,"result":{"supportedVersions":["2026-07-28","2025-11-25"],"capabilities":{},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"new"}}}}\n'
read list
case "$list" in *'"method":"tools/list"'*'"io.modelcontextprotocol/clientInfo"'*) ;; *) exit 1 ;; esac
printf '{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"a","inputSchema":{"properties":{"n":{"type":"number","x-mcp-header":"N"}}}}]}}\n'
sleep 5"#,
        )
        .await;
        assert_eq!(client.initialize().await.expect("initialize").name, "new");
        assert_eq!(client.era(), Era::Modern);
        // stdio ignores x-mcp-header, so even this annotation keeps the tool.
        assert_eq!(client.list_tools().await.expect("list").len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_server_silent_on_the_probe_counts_as_legacy() {
        let mut client = sh_client(
            r#"read probe
read init
printf '{"jsonrpc":"2.0","id":2,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"quiet"}}}\n'
sleep 5"#,
        )
        .await;
        assert_eq!(client.initialize().await.expect("initialize").name, "quiet");
        assert_eq!(client.era(), Era::Legacy);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_server_that_exits_on_the_probe_reports_it() {
        let mut client = sh_client("read probe; exit 0").await;
        let err = client.initialize().await.expect_err("server ended");
        assert!(err.is::<ProbeEndedServer>(), "{err:#}");
    }

    #[test]
    fn meta_keeps_caller_fields_and_adds_the_protocol_fields() {
        let params = with_meta(json!({"cursor": "c", "_meta": {"trace": "t"}}));
        assert_eq!(params["cursor"], "c");
        assert_eq!(params["_meta"]["trace"], "t");
        assert_eq!(
            params["_meta"]["io.modelcontextprotocol/protocolVersion"],
            MODERN_VERSION
        );
        assert_eq!(
            params["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
            "mermaid"
        );
        assert_eq!(
            params["_meta"]["io.modelcontextprotocol/clientCapabilities"],
            json!({})
        );
    }

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
    async fn handshake_records_resources_and_prompts_capabilities() {
        let init = r#"{"protocolVersion":"2025-11-25","capabilities":{"tools":{},"resources":{"subscribe":false},"prompts":{"listChanged":true}},"serverInfo":{"name":"fx"}}"#;
        let fx = fixture(vec![
            json_reply(&rpc_response(1, init)),
            status_reply(202, "Accepted"),
        ])
        .await;
        let mut client = McpClient::new(fx.transport().into());
        client.initialize_legacy().await.expect("initialize");
        assert_eq!(
            client.capabilities,
            ServerCapabilities {
                resources: true,
                prompts: true
            }
        );
        // Tools-only (or a null entry) declares neither.
        let caps = ServerCapabilities::from_result(
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
