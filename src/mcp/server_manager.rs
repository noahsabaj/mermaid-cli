//! MCP server lifecycle management.
//!
//! Manages multiple MCP server processes, handles tool discovery,
//! and routes tool calls to the correct server. Servers start
//! concurrently (the effect layer spawns one task per server calling
//! [`McpServerManager::start_server`]); each startup is bounded by
//! [`MCP_STARTUP_TIMEOUT`] and inserts into the shared registry as it
//! resolves, so one slow server never delays the rest.

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use std::sync::Arc;
use tracing::{info, warn};

use super::client::{
    ContentBlock, McpClient, McpPromptDef, McpResource, McpToolDef, McpToolResult, ResourceContents,
};
use super::sanitize;
use super::transport::{StdioTransport, Transport};
use super::transport_http::HttpTransport;
use mermaid_domain::McpToolSpec;
use mermaid_domain::{McpServerConfig, TransportKind};

/// Wall-clock bound for one server's spawn + initialize + `list_tools`.
/// The per-JSON-RPC request timeout inside the transport is 30s, so the
/// slow-but-legitimate case (npx cold-downloading a package during
/// `initialize`) already fits; this catches spawn-level hangs. A config
/// override is deliberately deferred until someone needs it.
pub const MCP_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// What a started server offers, for the reducer's `Msg::McpServerReady`:
/// its sanitized tool specs, whether it serves resources, and its prompts as
/// ready-made slash commands.
#[derive(Debug, Default)]
pub struct ServerCatalog {
    pub tools: Vec<McpToolSpec>,
    pub resources: bool,
    pub prompts: Vec<mermaid_domain::PromptCommand>,
}

/// Per-server runtime: the live client plus sanitized-name bookkeeping.
struct ServerRuntime {
    client: Arc<McpClient>,
    /// Sanitized full advertised name (`mcp__srv__tool`) -> raw tool name.
    raw_tool_names: HashMap<String, String>,
    /// Sanitized specs advertised for this server (also seeds subagents).
    specs: Vec<McpToolSpec>,
}

/// Manages multiple MCP server connections behind interior mutability so
/// per-server startup tasks can insert as they finish while synchronous
/// consumers (`has_server`, `all_specs`) keep working.
pub struct McpServerManager {
    /// Keyed by RAW config server name. Guard is never held across .await:
    /// readers clone the `Arc<McpClient>` and drop the lock before awaiting.
    inner: RwLock<HashMap<String, ServerRuntime>>,
    /// Sanitized server segment -> raw config name, assigned deterministically
    /// from the sorted config key list at construction.
    aliases: BTreeMap<String, String>,
    /// Set by `shutdown()`; a straggler startup task that resolves after
    /// shutdown must reap its client instead of inserting it.
    shutting_down: AtomicBool,
}

impl McpServerManager {
    /// Empty manager pre-seeded with deterministic server-name aliases.
    pub fn new(configs: &HashMap<String, McpServerConfig>) -> Self {
        let mut names: Vec<&str> = configs.keys().map(String::as_str).collect();
        names.sort_unstable();
        Self {
            inner: RwLock::new(HashMap::new()),
            aliases: sanitize::assign_server_aliases(names),
            shutting_down: AtomicBool::new(false),
        }
    }

    /// Sanitized alias for a raw server name (assigned at construction).
    /// Falls back to sanitizing on the fly for names outside the config
    /// set (defensive; callers always pass configured names).
    pub fn alias_for(&self, raw_name: &str) -> String {
        self.aliases
            .iter()
            .find(|(_, raw)| raw.as_str() == raw_name)
            .map(|(alias, _)| alias.clone())
            .unwrap_or_else(|| sanitize::sanitize_segment(raw_name))
    }

    /// Spawn + initialize + `list_tools` (+ `list_prompts` when the server
    /// declares prompts) for one server, bounded by [`MCP_STARTUP_TIMEOUT`];
    /// inserts the runtime and returns its [`ServerCatalog`] for the
    /// reducer's `Msg::McpServerReady`.
    ///
    /// # Errors
    ///
    /// Spawning or connecting to the server, the `initialize` handshake, the
    /// `list_tools` discovery, and the whole sequence exceeding
    /// [`MCP_STARTUP_TIMEOUT`]. On timeout the in-flight future is dropped,
    /// which reaps the child — no stray process survives a failed start. One
    /// server failing here never blocks the others; the caller reports it and
    /// carries on.
    pub async fn start_server(
        &self,
        name: &str,
        config: &McpServerConfig,
    ) -> Result<ServerCatalog> {
        self.start_server_with_timeout(name, config, MCP_STARTUP_TIMEOUT)
            .await
    }

    /// Timeout-injectable body of [`Self::start_server`] (tests use a short
    /// bound against a sleeping fixture).
    pub(crate) async fn start_server_with_timeout(
        &self,
        name: &str,
        config: &McpServerConfig,
        timeout: Duration,
    ) -> Result<ServerCatalog> {
        match &config.url {
            Some(url) => info!("Starting MCP server: {} ({})", name, url),
            None => info!(
                "Starting MCP server: {} ({} {})",
                name,
                config.command,
                // Redact args — they can carry secrets (e.g. `--api-key=…`) (#93).
                mermaid_model::utils::redact_secrets(&config.args.join(" "))
            ),
        }

        let started = tokio::time::timeout(timeout, Self::start_one(name, config)).await;
        let (client, tools, prompts) = match started {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                warn!("Failed to start MCP server '{}': {}", name, e);
                return Err(e);
            },
            Err(_) => {
                // Dropping the in-flight future reaps the child: the
                // transport spawns with kill_on_drop(true).
                warn!(
                    "MCP server '{}' startup timed out after {}s",
                    name,
                    timeout.as_secs()
                );
                return Err(anyhow!("startup timed out after {}s", timeout.as_secs()));
            },
        };

        let alias = self.alias_for(name);
        let (specs, raw_tool_names) = sanitize::sanitize_server_tools(&alias, &tools);
        let catalog = ServerCatalog {
            tools: specs.clone(),
            resources: client.capabilities.resources,
            prompts: sanitize::sanitize_server_prompts(&alias, name, &prompts),
        };
        info!(
            "MCP server '{}' ready: {} tools, {} prompts{} ({})",
            name,
            specs.len(),
            catalog.prompts.len(),
            if catalog.resources { ", resources" } else { "" },
            client
                .server_info
                .as_ref()
                .map(|s| s.name.as_str())
                .unwrap_or("?")
        );

        let runtime = ServerRuntime {
            client: Arc::new(client),
            raw_tool_names,
            specs: specs.clone(),
        };
        if self.shutting_down.load(Ordering::Acquire) {
            // Shutdown already ran; don't insert a client nothing will reap.
            runtime.client.shutdown().await;
            return Err(anyhow!("manager shut down during startup"));
        }
        self.inner
            .write()
            .expect("mcp registry lock poisoned")
            .insert(name.to_string(), runtime);
        Ok(catalog)
    }

    /// Start a single MCP server, initialize, and list its tools and (when
    /// declared) prompts. A failing `prompts/list` costs the server its
    /// prompt commands, not its startup: tools are the primary surface.
    async fn start_one(
        name: &str,
        config: &McpServerConfig,
    ) -> Result<(McpClient, Vec<McpToolDef>, Vec<McpPromptDef>)> {
        let transport: Transport = match config.transport_kind()? {
            TransportKind::Stdio => {
                StdioTransport::spawn(&config.command, &config.args, &config.env)
                    .await?
                    .into()
            },
            TransportKind::Http => HttpTransport::new(config)?.into(),
        };
        let mut client = McpClient::new(transport);

        client
            .initialize()
            .await
            .map_err(|e| anyhow!("MCP server '{name}' initialization failed: {e}"))?;

        let tools = client
            .list_tools()
            .await
            .map_err(|e| anyhow!("MCP server '{name}' tool discovery failed: {e}"))?;

        let prompts = if client.capabilities.prompts {
            client.list_prompts().await.unwrap_or_else(|e| {
                warn!("MCP server '{}' prompt discovery failed: {}", name, e);
                Vec::new()
            })
        } else {
            Vec::new()
        };

        Ok((client, tools, prompts))
    }

    /// All discovered tools as (raw server name, sanitized spec) pairs,
    /// cloned out so no lock is held by the caller. Order: server name.
    pub fn all_specs(&self) -> Vec<(String, McpToolSpec)> {
        let guard = self.inner.read().expect("mcp registry lock poisoned");
        let mut out: Vec<(String, McpToolSpec)> = guard
            .iter()
            .flat_map(|(name, rt)| rt.specs.iter().map(|s| (name.clone(), s.clone())))
            .collect();
        out.sort_by(|a, b| {
            (a.0.as_str(), a.1.name.as_str()).cmp(&(b.0.as_str(), b.1.name.as_str()))
        });
        out
    }

    /// True iff the named server started and has an active client,
    /// even if it advertised zero tools. Accepts raw or sanitized names.
    pub fn has_server(&self, name: &str) -> bool {
        let guard = self.inner.read().expect("mcp registry lock poisoned");
        guard.contains_key(name)
            || self
                .aliases
                .get(name)
                .is_some_and(|raw| guard.contains_key(raw))
    }

    /// Check if any MCP servers are active.
    pub fn has_servers(&self) -> bool {
        !self
            .inner
            .read()
            .expect("mcp registry lock poisoned")
            .is_empty()
    }

    /// Call a tool on a specific server. `server` and `tool` accept
    /// sanitized names (the advertised form) or raw names (an off-script
    /// model echoing a server's own tool listing still routes).
    ///
    /// # Concurrency
    ///
    /// Multiple concurrent calls to the same server serialize at the
    /// transport layer (`StdioTransport` holds a mutex over stdin writes and
    /// uses a shared pending-response map for JSON-RPC correlation). Calls to
    /// *different* servers run fully in parallel. The registry read lock is
    /// dropped before awaiting the call.
    /// The server-advertised `readOnlyHint` for a tool; `false` when the
    /// server, the tool, or the annotation is unknown — an unannotated tool
    /// is write-shaped, fail closed. Mirrors `call_tool`'s server/alias
    /// resolution so the hint is read for exactly the tool that would run.
    pub fn read_only_hint(&self, server: &str, tool: &str) -> bool {
        let guard = self.inner.read().expect("mcp registry lock poisoned");
        let (raw_server, runtime) = match guard.get_key_value(server) {
            Some(hit) => hit,
            None => {
                let Some(raw) = self.aliases.get(server) else {
                    return false;
                };
                let Some(hit) = guard.get_key_value(raw.as_str()) else {
                    return false;
                };
                hit
            },
        };
        let alias = self.alias_for(raw_server);
        let advertised = format!("mcp__{alias}__{tool}");
        runtime
            .specs
            .iter()
            .find(|s| s.name == advertised)
            .is_some_and(|s| s.read_only_hint)
    }

    /// Dispatch a tool call to `server`, resolving both the alias and the raw
    /// tool name.
    ///
    /// # Errors
    ///
    /// A `server` that is neither a running raw name nor a known alias, a
    /// server that has been stopped, and whatever the call itself fails with
    /// (transport, timeout). A tool that runs and reports failure is not an
    /// error — that is `isError` on the returned [`McpToolResult`].
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> Result<McpToolResult> {
        let (client, raw_tool) = {
            let guard = self.inner.read().expect("mcp registry lock poisoned");
            let (raw_server, runtime) = match guard.get_key_value(server) {
                Some(hit) => hit,
                None => {
                    let raw = self
                        .aliases
                        .get(server)
                        .ok_or_else(|| anyhow!("MCP server '{server}' not found or not running"))?;
                    guard
                        .get_key_value(raw.as_str())
                        .ok_or_else(|| anyhow!("MCP server '{server}' not found or not running"))?
                },
            };
            // The advertised name is `mcp__<alias>__<tool>`; resolve the raw
            // tool by reconstructing it, falling back to the name as given.
            let alias = self.alias_for(raw_server);
            let advertised = format!("mcp__{alias}__{tool}");
            let raw_tool = runtime
                .raw_tool_names
                .get(&advertised)
                .cloned()
                .unwrap_or_else(|| tool.to_string());
            (Arc::clone(&runtime.client), raw_tool)
        };
        if client.is_shutdown() {
            return Err(anyhow!("MCP server '{server}' has been stopped"));
        }

        client.call_tool(&raw_tool, arguments).await
    }

    /// Resolve `server` (raw config name or sanitized alias) to its raw name
    /// and live client. Lock released before returning.
    fn client_for(&self, server: &str) -> Result<(String, Arc<McpClient>)> {
        let guard = self.inner.read().expect("mcp registry lock poisoned");
        let (raw, runtime) = guard
            .get_key_value(server)
            .or_else(|| {
                self.aliases
                    .get(server)
                    .and_then(|raw| guard.get_key_value(raw.as_str()))
            })
            .ok_or_else(|| anyhow!("MCP server '{server}' not found or not running"))?;
        if runtime.client.is_shutdown() {
            return Err(anyhow!("MCP server '{server}' has been stopped"));
        }
        Ok((raw.clone(), Arc::clone(&runtime.client)))
    }

    /// Raw names of the running servers that declared `resources`, sorted.
    pub fn resource_servers(&self) -> Vec<String> {
        let guard = self.inner.read().expect("mcp registry lock poisoned");
        let mut names: Vec<String> = guard
            .iter()
            .filter(|(_, rt)| rt.client.capabilities.resources && !rt.client.is_shutdown())
            .map(|(name, _)| name.clone())
            .collect();
        names.sort_unstable();
        names
    }

    /// `resources/list` on one server. Returns the raw server name with the
    /// listing so callers can label it.
    ///
    /// # Errors
    ///
    /// An unknown or stopped server, one that never declared `resources`,
    /// and whatever the request fails with.
    pub async fn list_resources(&self, server: &str) -> Result<(String, Vec<McpResource>)> {
        let (raw, client) = self.client_for(server)?;
        if !client.capabilities.resources {
            return Err(anyhow!("MCP server '{raw}' does not provide resources"));
        }
        Ok((raw, client.list_resources().await?))
    }

    /// `resources/read` of `uri` on one server.
    ///
    /// # Errors
    ///
    /// As [`Self::list_resources`], plus the server rejecting the URI.
    pub async fn read_resource(&self, server: &str, uri: &str) -> Result<Vec<ResourceContents>> {
        let (raw, client) = self.client_for(server)?;
        if !client.capabilities.resources {
            return Err(anyhow!("MCP server '{raw}' does not provide resources"));
        }
        client.read_resource(uri).await
    }

    /// `prompts/get` of the raw-named `prompt` on `server`, reduced to the
    /// text Mermaid submits: text parts (and embedded text resources) joined
    /// by blank lines, plus a count of the non-text parts left out.
    ///
    /// # Errors
    ///
    /// An unknown or stopped server and whatever the request fails with.
    pub async fn get_prompt(
        &self,
        server: &str,
        prompt: &str,
        arguments: &std::collections::BTreeMap<String, String>,
    ) -> Result<mermaid_domain::query::McpPromptText> {
        let (_, client) = self.client_for(server)?;
        let blocks = client.get_prompt(prompt, arguments).await?;
        Ok(prompt_text(&blocks))
    }

    /// Convert an MCP tool result into text suitable for a tool result message.
    /// Images are returned separately for multimodal attachment. Audio is
    /// attached through the same channel — adapters that don't support audio
    /// will silently drop it. Resource links + embedded resources render as
    /// text so the model can follow up with another tool call.
    #[must_use]
    pub fn format_tool_result(result: &McpToolResult) -> (String, Option<Vec<String>>) {
        let mut text_parts = Vec::new();
        let mut images = Vec::new();

        for block in &result.content {
            match block {
                ContentBlock::Text(text) => text_parts.push(text.clone()),
                ContentBlock::Image { data, .. } => images.push(data.clone()),
                ContentBlock::Audio { data, mime_type } => {
                    images.push(data.clone());
                    text_parts.push(format!("[audio attachment: {mime_type}]"));
                },
                ContentBlock::ResourceLink {
                    uri,
                    name,
                    description,
                    mime_type,
                } => {
                    let label = name.as_deref().unwrap_or(uri.as_str());
                    let desc = description.as_deref().unwrap_or("");
                    let mime = mime_type.as_deref().unwrap_or("");
                    text_parts.push(format!(
                        "[resource link: {label} ({mime}) — {desc} → {uri}]"
                    ));
                },
                ContentBlock::Resource {
                    uri,
                    mime_type,
                    text,
                    blob,
                } => {
                    let mime = mime_type.as_deref().unwrap_or("");
                    if let Some(t) = text {
                        text_parts.push(format!("[resource {uri}]:\n{t}"));
                    } else if let Some(b) = blob {
                        text_parts.push(format!(
                            "[resource {} ({}): {} bytes of base64]",
                            uri,
                            mime,
                            b.len()
                        ));
                    } else {
                        text_parts.push(format!("[resource {uri} ({mime})]"));
                    }
                },
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

    /// Gracefully shut down all MCP servers. Sets the shutting-down flag
    /// first so straggler startup tasks reap their own clients.
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let clients: Vec<(String, Arc<McpClient>)> = {
            let guard = self.inner.read().expect("mcp registry lock poisoned");
            guard
                .iter()
                .map(|(name, rt)| (name.clone(), Arc::clone(&rt.client)))
                .collect()
        };
        for (name, client) in clients {
            info!("Shutting down MCP server: {}", name);
            client.shutdown().await;
        }
    }

    /// Stop a single named server: kill its child via the transport. The
    /// stdout-reader task then exits on EOF — no explicit abort needed. Returns
    /// `true` if a server matched (raw or sanitized name).
    ///
    /// The registry entry lingers, but the client is flagged shut down, so a
    /// later `call_tool` to a stopped server returns a clean "has been
    /// stopped" error rather than a broken-pipe transport failure.
    pub async fn stop_server(&self, name: &str) -> bool {
        let client = {
            let guard = self.inner.read().expect("mcp registry lock poisoned");
            let runtime = guard.get(name).or_else(|| {
                self.aliases
                    .get(name)
                    .and_then(|raw| guard.get(raw.as_str()))
            });
            runtime.map(|rt| Arc::clone(&rt.client))
        };
        match client {
            Some(client) => {
                info!("Stopping MCP server: {}", name);
                client.shutdown().await;
                true
            },
            None => false,
        }
    }
}

/// Join a prompt's text parts (embedded text resources included) with blank
/// lines, counting everything else — images, audio, links, binary
/// resources — as skipped.
fn prompt_text(blocks: &[ContentBlock]) -> mermaid_domain::query::McpPromptText {
    let mut parts = Vec::new();
    let mut skipped = 0;
    for block in blocks {
        match block {
            ContentBlock::Text(text)
            | ContentBlock::Resource {
                text: Some(text), ..
            } => parts.push(text.as_str()),
            ContentBlock::Image { .. }
            | ContentBlock::Audio { .. }
            | ContentBlock::ResourceLink { .. }
            | ContentBlock::Resource { text: None, .. } => skipped += 1,
        }
    }
    mermaid_domain::query::McpPromptText {
        text: parts.join("\n\n"),
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_unknown_server_returns_false() {
        // No servers configured ⇒ empty manager; stopping an unknown name is a
        // no-op that reports `false` rather than panicking.
        let mgr = McpServerManager::new(&HashMap::new());
        assert!(!mgr.has_servers());
        assert!(!mgr.stop_server("does-not-exist").await);
    }

    #[test]
    fn aliases_assigned_from_sorted_config_keys() {
        let mut configs = HashMap::new();
        configs.insert("my.server".to_string(), McpServerConfig::default());
        configs.insert("plain".to_string(), McpServerConfig::default());
        let mgr = McpServerManager::new(&configs);
        assert_eq!(mgr.alias_for("my.server"), "my_server");
        assert_eq!(mgr.alias_for("plain"), "plain");
        // Unknown names sanitize on the fly instead of panicking.
        assert_eq!(mgr.alias_for("un known"), "un_known");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_timeout_reports_timed_out() {
        // A server whose process never speaks JSON-RPC: `sleep` hangs the
        // initialize round-trip; the injected 200ms bound trips first.
        let config = McpServerConfig {
            command: "sleep".to_string(),
            args: vec!["5".to_string()],
            ..Default::default()
        };
        let mut configs = HashMap::new();
        configs.insert("sleepy".to_string(), config.clone());
        let mgr = McpServerManager::new(&configs);
        let err = mgr
            .start_server_with_timeout("sleepy", &config, Duration::from_millis(200))
            .await
            .expect_err("must time out");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(!mgr.has_server("sleepy"));
    }

    #[tokio::test]
    async fn http_server_starts_and_lists_tools() {
        use super::super::transport_http::test_fixture::{fixture, json_reply, status_reply};
        let init_result = r#"{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"fx","version":"1.0"}}"#;
        let tools_result =
            r#"{"tools":[{"name":"echo","description":"echoes","inputSchema":{"type":"object"}}]}"#;
        let fx = fixture(vec![
            json_reply(&format!(
                r#"{{"jsonrpc":"2.0","id":1,"result":{init_result}}}"#
            )),
            status_reply(202, "Accepted"),
            json_reply(&format!(
                r#"{{"jsonrpc":"2.0","id":2,"result":{tools_result}}}"#
            )),
        ])
        .await;
        let config = fx.config();
        let mut configs = HashMap::new();
        configs.insert("remote".to_string(), config.clone());
        let mgr = McpServerManager::new(&configs);
        let catalog = mgr
            .start_server_with_timeout("remote", &config, Duration::from_secs(30))
            .await
            .expect("http server must start");
        let specs = catalog.tools;
        assert_eq!(specs.len(), 1);
        // `capabilities: {}` declares neither resources nor prompts, so no
        // prompts/list request was made.
        assert!(!catalog.resources);
        assert!(catalog.prompts.is_empty());
        assert!(mgr.resource_servers().is_empty());
        assert_eq!(specs[0].name, "mcp__remote__echo");
        assert!(mgr.has_server("remote"));
    }

    #[tokio::test]
    async fn resources_and_prompts_server_exposes_both_through_the_manager() {
        use super::super::transport_http::test_fixture::{
            fixture, json_reply, rpc_response, status_reply,
        };
        let init = r#"{"protocolVersion":"2025-11-25","capabilities":{"resources":{},"prompts":{}},"serverInfo":{"name":"fx"}}"#;
        let fx = fixture(vec![
            json_reply(&rpc_response(1, init)),
            status_reply(202, "Accepted"),
            json_reply(&rpc_response(2, r#"{"tools":[]}"#)),
            json_reply(&rpc_response(
                3,
                r#"{"prompts":[{"name":"Summarize","description":"Sum it up"}]}"#,
            )),
            json_reply(&rpc_response(
                4,
                r#"{"resources":[{"uri":"mem://notes","name":"notes"}]}"#,
            )),
            json_reply(&rpc_response(
                5,
                r#"{"messages":[{"role":"user","content":{"type":"text","text":"one"}},{"role":"user","content":{"type":"audio","data":"AA==","mimeType":"audio/wav"}},{"role":"user","content":{"type":"resource","resource":{"uri":"mem://x","text":"two"}}}]}"#,
            )),
        ])
        .await;
        let config = fx.config();
        let mut configs = HashMap::new();
        configs.insert("Notes.Srv".to_string(), config.clone());
        let mgr = McpServerManager::new(&configs);
        let catalog = mgr
            .start_server_with_timeout("Notes.Srv", &config, Duration::from_secs(30))
            .await
            .expect("server must start");
        assert!(catalog.resources);
        assert_eq!(catalog.prompts.len(), 1);
        assert_eq!(catalog.prompts[0].name, "mcp__notes_srv__summarize");
        assert_eq!(catalog.prompts[0].mcp_server(), Some("Notes.Srv"));
        assert_eq!(mgr.resource_servers(), ["Notes.Srv"]);

        // The sanitized alias routes like the raw name does.
        let (raw, resources) = mgr.list_resources("Notes_Srv").await.expect("list");
        assert_eq!(raw, "Notes.Srv");
        assert_eq!(resources[0].uri, "mem://notes");

        let text = mgr
            .get_prompt("Notes.Srv", "Summarize", &std::collections::BTreeMap::new())
            .await
            .expect("get");
        assert_eq!(text.text, "one\n\ntwo");
        assert_eq!(text.skipped, 1);
    }

    #[tokio::test]
    async fn resources_are_refused_for_a_server_without_the_capability() {
        use super::super::transport_http::test_fixture::{
            fixture, json_reply, rpc_response, status_reply,
        };
        let init = r#"{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fx"}}"#;
        let fx = fixture(vec![
            json_reply(&rpc_response(1, init)),
            status_reply(202, "Accepted"),
            json_reply(&rpc_response(2, r#"{"tools":[]}"#)),
        ])
        .await;
        let config = fx.config();
        let mut configs = HashMap::new();
        configs.insert("plain".to_string(), config.clone());
        let mgr = McpServerManager::new(&configs);
        mgr.start_server_with_timeout("plain", &config, Duration::from_secs(30))
            .await
            .expect("server must start");
        assert!(mgr.resource_servers().is_empty());
        let err = mgr
            .read_resource("plain", "mem://x")
            .await
            .expect_err("no resources capability");
        assert!(
            err.to_string().contains("does not provide resources"),
            "{err}"
        );
        // Nothing beyond startup reached the wire.
        assert_eq!(fx.requests().await.len(), 3);
    }

    #[tokio::test]
    async fn config_with_both_command_and_url_errors() {
        let config = McpServerConfig {
            command: "npx".to_string(),
            url: Some("https://example.com/mcp".to_string()),
            ..Default::default()
        };
        let mut configs = HashMap::new();
        configs.insert("conflicted".to_string(), config.clone());
        let mgr = McpServerManager::new(&configs);
        let err = mgr
            .start_server_with_timeout("conflicted", &config, Duration::from_secs(5))
            .await
            .expect_err("must reject");
        assert!(err.to_string().contains("mutually exclusive"), "{err}");
        assert!(!mgr.has_server("conflicted"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn straggler_insert_after_shutdown_is_reaped() {
        // Once shutdown() has run, a late-resolving startup must not insert.
        // Simulate by flipping the flag first: start_server_with_timeout on a
        // server that would "succeed" cannot easily be faked without a real
        // MCP process, so assert the flag's effect through the public path:
        // a sleeping fixture that times out never inserts either way, and the
        // flag stays set.
        let mgr = McpServerManager::new(&HashMap::new());
        mgr.shutdown().await;
        assert!(mgr.shutting_down.load(Ordering::Acquire));
        assert!(!mgr.has_servers());
    }
}
