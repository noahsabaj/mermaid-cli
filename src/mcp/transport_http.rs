//! Streamable HTTP JSON-RPC 2.0 transport for MCP servers (spec 2025-11-25).
//!
//! Every client message is POSTed to the single MCP endpoint; the server
//! answers a request with either a plain JSON body or an SSE stream that
//! eventually carries the response. Session state (`MCP-Session-Id`) and the
//! negotiated protocol version (`MCP-Protocol-Version`) ride as headers on
//! every subsequent request. Shutdown is an HTTP DELETE of the session.
//!
//! Out of scope (v1): OAuth flows, the GET server-listening stream, and the
//! deprecated two-endpoint HTTP+SSE transport. SSE resumption (GET +
//! `Last-Event-ID`) is implemented only for the SEP-1699 polling case: a
//! server that closes the connection mid-request after assigning event IDs.

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::{Duration, timeout};

use super::transport::{
    REQUEST_TIMEOUT_SECS, extract_jsonrpc_result, is_response, parse_response_id,
};
use crate::app::{McpServerConfig, TransportKind};
use crate::utils::{HostClass, classify_host, drain_sse_events};

/// TCP connect budget. Separate from the response budget: a dead host should
/// fail in seconds, not eat the whole 30s control-call window.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Budget for a POSTed notification/response (202-class round-trip) — no
/// server compute is expected, only delivery.
const NOTIFICATION_TIMEOUT_SECS: u64 = 10;

/// Budget for the session DELETE at shutdown. Best-effort; shutdown never
/// blocks on a slow server.
const DELETE_TIMEOUT_SECS: u64 = 5;

/// Cap on SEP-1699 SSE reconnects for a single request. The outer per-request
/// timeout still bounds total wall time; this stops a server that keeps
/// closing the stream without ever delivering the response.
const MAX_SSE_RECONNECTS: u32 = 5;

/// Max bytes of an error-response body echoed into an error message. Redacted
/// before use — bodies can quote request headers.
const ERROR_BODY_SNIPPET_BYTES: usize = 200;

// `HeaderName::from_static` requires lowercase. HTTP header names are
// case-insensitive on the wire; the spec currently spells these
// `MCP-Session-Id` / `MCP-Protocol-Version` / `Last-Event-ID`.
const SESSION_HEADER: HeaderName = HeaderName::from_static("mcp-session-id");
const PROTOCOL_VERSION_HEADER: HeaderName = HeaderName::from_static("mcp-protocol-version");
const LAST_EVENT_ID_HEADER: HeaderName = HeaderName::from_static("last-event-id");

/// Accept values: POSTs must advertise both response shapes; SSE resume GETs
/// only ever get a stream.
const ACCEPT_POST: &str = "application/json, text/event-stream";
const ACCEPT_SSE: &str = "text/event-stream";

/// Streamable HTTP transport for a single remote MCP server.
///
/// # Concurrency model
///
/// Each request is an independent HTTP round-trip correlated by JSON-RPC id
/// (`next_id` is atomic), so concurrent callers don't serialize the way stdio
/// writes do. `session_id` / `protocol_version` are `RwLock`s written once
/// early in the session (initialize) and read on every request; the locks are
/// never held across an `.await`.
pub(super) struct HttpTransport {
    client: reqwest::Client,
    url: reqwest::Url,
    /// Literal config headers, validated at construction.
    static_headers: HeaderMap,
    /// Header name -> env var name, resolved per request so a rotated secret
    /// is picked up without restart. A missing var is skipped.
    env_headers: Vec<(HeaderName, String)>,
    /// `MCP-Session-Id` captured from response headers; echoed on every
    /// subsequent request once set.
    session_id: RwLock<Option<HeaderValue>>,
    /// Negotiated protocol version, set from the initialize result; sent as
    /// `MCP-Protocol-Version` on everything after.
    protocol_version: RwLock<Option<HeaderValue>>,
    /// Monotonic request ID counter.
    next_id: AtomicU64,
}

impl HttpTransport {
    /// Build a transport for `config` (which must be url-shaped). Fails fast on
    /// an invalid url/scheme or an unparseable header NAME — error messages
    /// never include header values, which are secrets.
    pub fn new(config: &McpServerConfig) -> Result<Self> {
        // Re-validate rather than trust the caller: enforces url-presence and
        // the https-or-loopback scheme rule on every construction path.
        if config.transport_kind()? != TransportKind::Http {
            bail!("MCP server config is not url-shaped");
        }
        let url_str = config
            .url
            .as_deref()
            .expect("transport_kind checked url presence");
        let url = reqwest::Url::parse(url_str)
            .map_err(|e| anyhow!("invalid MCP server url '{url_str}': {e}"))?;

        // reqwest connects to IP-literal hosts directly, never consulting the
        // dns_resolver below — vet literals here so the private-network policy
        // also holds for `url = "https://192.168.1.5/mcp"`. DNS names classify
        // as Public and are vetted at connect time by McpVettingResolver.
        let host = url.host_str().unwrap_or_default();
        let blocked = match classify_host(host) {
            HostClass::Loopback | HostClass::Public => false,
            _ => !config.allow_private_network,
        };
        if blocked {
            bail!(
                "refusing to connect MCP server '{host}': it is a private/internal \
                 address (set allow_private_network = true for this server to permit it)"
            );
        }

        let mut static_headers = HeaderMap::new();
        for (name, value) in &config.headers {
            let n = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| anyhow!("invalid MCP header name '{name}'"))?;
            // Never echo the value: it is a secret.
            let v = HeaderValue::from_str(value)
                .map_err(|_| anyhow!("invalid value for MCP header '{name}'"))?;
            static_headers.insert(n, v);
        }
        let mut env_headers = Vec::new();
        for (name, var) in &config.env_headers {
            let n = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| anyhow!("invalid MCP header name '{name}'"))?;
            env_headers.push((n, var.clone()));
        }

        let client = reqwest::Client::builder()
            .user_agent(format!("mermaid/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            // Following a redirect would forward Authorization headers to a
            // possibly different origin.
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(std::sync::Arc::new(McpVettingResolver {
                allow_private: config.allow_private_network,
            }))
            .build()
            .context("failed to build MCP HTTP client")?;

        Ok(Self {
            client,
            url,
            static_headers,
            env_headers,
            session_id: RwLock::new(None),
            protocol_version: RwLock::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    /// Send a JSON-RPC request and wait for the response under the default
    /// control-call timeout.
    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        self.send_request_with_timeout(method, params, REQUEST_TIMEOUT_SECS)
            .await
    }

    /// Like [`Self::send_request`], but with an explicit budget bounding the
    /// WHOLE round-trip: POST, SSE drain, and any SEP-1699 reconnects.
    pub async fn send_request_with_timeout(
        &self,
        method: &str,
        params: Value,
        response_timeout_secs: u64,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        timeout(
            Duration::from_secs(response_timeout_secs),
            self.request_roundtrip(method, id, &request),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "MCP request timed out after {}s: {}",
                response_timeout_secs,
                method
            )
        })?
    }

    /// Send a JSON-RPC notification. The server answers a client notification
    /// (or response) with 202 Accepted; any 2xx is tolerated.
    pub async fn send_notification(&self, method: &str, params: Value) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.post_message(&notification)
            .await
            .with_context(|| format!("MCP notification failed (method: {method})"))
    }

    /// Record the protocol version negotiated during `initialize`; sent as the
    /// `MCP-Protocol-Version` header on every subsequent request.
    pub fn set_protocol_version(&self, version: &str) {
        match HeaderValue::from_str(version) {
            Ok(v) => {
                *self
                    .protocol_version
                    .write()
                    .expect("mcp protocol_version lock poisoned") = Some(v);
            },
            // A version that can't be a header value can't go on the wire;
            // requests then omit the header (server assumes 2025-03-26).
            Err(_) => tracing::warn!(
                "MCP: server negotiated protocol version {:?} is not a valid header value; \
                 omitting MCP-Protocol-Version",
                version
            ),
        }
    }

    /// End the session: DELETE with the session id, bounded to 5s. A server
    /// MAY respond 405 (it does not allow client-initiated termination) —
    /// tolerated, like every other failure here. No session id = nothing to do.
    pub async fn shutdown(&self) {
        if self
            .session_id
            .read()
            .expect("mcp session_id lock poisoned")
            .is_none()
        {
            return;
        }
        let Ok(headers) = self.request_headers(None) else {
            return;
        };
        let result = timeout(
            Duration::from_secs(DELETE_TIMEOUT_SECS),
            self.client.delete(self.url.clone()).headers(headers).send(),
        )
        .await;
        match result {
            Ok(Ok(resp)) if resp.status() == StatusCode::METHOD_NOT_ALLOWED => {
                tracing::debug!("MCP: server does not allow client session termination (405)");
            },
            Ok(Err(e)) => tracing::debug!("MCP: session DELETE failed: {}", e),
            Err(_) => tracing::debug!("MCP: session DELETE timed out"),
            Ok(Ok(_)) => {},
        }
    }

    /// One request round-trip: POST, then dispatch on the response
    /// Content-Type (plain JSON object vs SSE stream).
    async fn request_roundtrip(&self, method: &str, id: u64, request: &Value) -> Result<Value> {
        let response = self
            .client
            .post(self.url.clone())
            .headers(self.request_headers(Some(ACCEPT_POST))?)
            .json(request)
            .send()
            .await
            .with_context(|| format!("MCP HTTP request failed (method: {method})"))?;
        self.capture_session(response.headers());

        let status = response.status();
        if status == StatusCode::ACCEPTED {
            // 202 is the notification/response acknowledgement; a request
            // needs an actual JSON-RPC response.
            bail!("MCP server returned 202 Accepted to a request (method: {method})");
        }
        if !status.is_success() {
            return Err(self.status_error(method, status, response).await);
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            let msg = self.drain_sse_for_response(response, method, id).await?;
            extract_jsonrpc_result(msg)
        } else if content_type.starts_with("application/json") {
            let msg: Value = response
                .json()
                .await
                .with_context(|| format!("MCP response was not valid JSON (method: {method})"))?;
            if !is_response(&msg) || msg.get("id").and_then(parse_response_id) != Some(id) {
                bail!("MCP JSON response did not answer request {id} (method: {method})");
            }
            extract_jsonrpc_result(msg)
        } else {
            bail!(
                "MCP server returned unsupported Content-Type '{content_type}' \
                 (method: {method}; expected application/json or text/event-stream)"
            );
        }
    }

    /// Drain an SSE response stream until the response to request `id`
    /// arrives. Handles SEP-1699 polling: if the server assigned event IDs and
    /// the connection closes before our response, honor any `retry` delay and
    /// resume via GET + `Last-Event-ID` (resumption is always a GET, even for
    /// a POST-originated stream).
    async fn drain_sse_for_response(
        &self,
        response: reqwest::Response,
        method: &str,
        id: u64,
    ) -> Result<Value> {
        let mut meta = SseMeta::default();
        let mut response = response;
        let mut reconnects = 0u32;
        loop {
            if let Some(msg) = self.drain_one_stream(response, id, &mut meta).await? {
                return Ok(msg);
            }
            // Stream ended without our response. Without event IDs there is
            // nothing to resume from — the server just dropped the request.
            let Some(last_id) = meta.last_event_id.clone() else {
                bail!("MCP SSE stream ended without a response (method: {method})");
            };
            reconnects += 1;
            if reconnects > MAX_SSE_RECONNECTS {
                bail!(
                    "MCP SSE stream did not deliver a response after {MAX_SSE_RECONNECTS} \
                     reconnects (method: {method})"
                );
            }
            // The client MUST respect a server-provided retry delay.
            if let Some(ms) = meta.retry_ms {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            let mut headers = self.request_headers(Some(ACCEPT_SSE))?;
            headers.insert(
                LAST_EVENT_ID_HEADER,
                HeaderValue::from_str(&last_id)
                    .map_err(|_| anyhow!("MCP SSE event id is not a valid header value"))?,
            );
            let resumed = self
                .client
                .get(self.url.clone())
                .headers(headers)
                .send()
                .await
                .with_context(|| format!("MCP SSE resume failed (method: {method})"))?;
            self.capture_session(resumed.headers());
            if !resumed.status().is_success() {
                let status = resumed.status();
                return Err(self.status_error(method, status, resumed).await);
            }
            response = resumed;
        }
    }

    /// Drain one SSE HTTP response. Returns `Ok(Some(msg))` when the response
    /// to `id` arrives, `Ok(None)` on a disconnect that may be resumable.
    async fn drain_one_stream(
        &self,
        response: reqwest::Response,
        id: u64,
        meta: &mut SseMeta,
    ) -> Result<Option<Value>> {
        meta.start_stream();
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    // A mid-stream error after the server assigned event IDs
                    // is indistinguishable from a deliberate SEP-1699 close;
                    // let the caller resume. Without IDs it is fatal.
                    if meta.last_event_id.is_some() {
                        tracing::debug!("MCP: SSE stream broke, will resume: {}", e);
                        return Ok(None);
                    }
                    return Err(anyhow!(e)).context("MCP SSE stream read failed");
                },
            };
            meta.feed(&chunk);
            buf.extend_from_slice(&chunk);
            for payload in drain_sse_events(&mut buf) {
                let Ok(msg) = serde_json::from_str::<Value>(&payload) else {
                    tracing::warn!("MCP: unparseable SSE event payload");
                    continue;
                };
                if is_response(&msg) {
                    if msg.get("id").and_then(parse_response_id) == Some(id) {
                        return Ok(Some(msg));
                    }
                    // A response to some other request on this stream —
                    // nothing of ours to complete.
                    continue;
                }
                if msg.get("method").is_some() {
                    match msg.get("id") {
                        // Server-initiated request: it blocks on a reply, so
                        // POST one back rather than stalling the server (F79).
                        Some(rid) if !rid.is_null() => {
                            let rid = rid.clone();
                            self.answer_server_request(&msg, &rid).await;
                        },
                        // Notification: no reply expected. Ignore it.
                        _ => tracing::trace!(
                            method = msg.get("method").and_then(|m| m.as_str()).unwrap_or(""),
                            "MCP: ignoring server notification on SSE stream"
                        ),
                    }
                }
            }
        }
        Ok(None)
    }

    /// Answer a server-initiated request arriving on an SSE stream: `ping`
    /// gets a proper empty result (spec lifecycle utility); anything else gets
    /// JSON-RPC -32601. Best-effort — a failed reply is the server's stall,
    /// not ours.
    async fn answer_server_request(&self, msg: &Value, rid: &Value) {
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let reply = if method == "ping" {
            json!({ "jsonrpc": "2.0", "id": rid, "result": {} })
        } else {
            json!({
                "jsonrpc": "2.0",
                "id": rid,
                "error": { "code": -32601, "message": "method not supported" },
            })
        };
        if let Err(e) = self.post_message(&reply).await {
            tracing::debug!(
                "MCP: failed to answer server-initiated request '{}': {}",
                method,
                e
            );
        }
    }

    /// POST a message that expects no JSON-RPC response (a notification or a
    /// response of ours). Any 2xx acknowledges it; non-2xx = server rejected.
    async fn post_message(&self, message: &Value) -> Result<()> {
        let fut = async {
            let response = self
                .client
                .post(self.url.clone())
                .headers(self.request_headers(Some(ACCEPT_POST))?)
                .json(message)
                .send()
                .await
                .context("MCP HTTP post failed")?;
            self.capture_session(response.headers());
            let status = response.status();
            if !status.is_success() {
                return Err(self.status_error("(notification)", status, response).await);
            }
            Ok(())
        };
        timeout(Duration::from_secs(NOTIFICATION_TIMEOUT_SECS), fut)
            .await
            .map_err(|_| anyhow!("MCP notification timed out after {NOTIFICATION_TIMEOUT_SECS}s"))?
    }

    /// The header set for one request: static config headers, env-resolved
    /// headers, then session id / protocol version / Accept. Env values are
    /// re-read per request; a missing var is skipped, an un-headerable value
    /// errors naming only the header and the var.
    fn request_headers(&self, accept: Option<&'static str>) -> Result<HeaderMap> {
        let mut headers = self.static_headers.clone();
        for (name, var) in &self.env_headers {
            if let Ok(value) = std::env::var(var) {
                let v = HeaderValue::from_str(&value).map_err(|_| {
                    anyhow!("invalid value in env var '{var}' for MCP header '{name}'")
                })?;
                headers.insert(name.clone(), v);
            }
        }
        if let Some(accept) = accept {
            headers.insert(ACCEPT, HeaderValue::from_static(accept));
        }
        if let Some(sid) = self
            .session_id
            .read()
            .expect("mcp session_id lock poisoned")
            .clone()
        {
            headers.insert(SESSION_HEADER, sid);
        }
        if let Some(pv) = self
            .protocol_version
            .read()
            .expect("mcp protocol_version lock poisoned")
            .clone()
        {
            headers.insert(PROTOCOL_VERSION_HEADER, pv);
        }
        Ok(headers)
    }

    /// Capture `MCP-Session-Id` from any response's headers (the normative
    /// source is the InitializeResult response; accepting it from any response
    /// is a safe superset). Lookup is case-insensitive by HeaderMap contract.
    fn capture_session(&self, headers: &HeaderMap) {
        if let Some(v) = headers.get(&SESSION_HEADER) {
            *self
                .session_id
                .write()
                .expect("mcp session_id lock poisoned") = Some(v.clone());
        }
    }

    /// Build the error for a non-success status: the 404-with-session case is
    /// session expiry (spec says the client MUST re-initialize; we surface it
    /// instead of silently re-initializing mid-run — server-side tool state is
    /// gone, so the user should restart the connection deliberately). Other
    /// statuses carry a short redacted body snippet.
    async fn status_error(
        &self,
        method: &str,
        status: StatusCode,
        response: reqwest::Response,
    ) -> anyhow::Error {
        if status == StatusCode::NOT_FOUND
            && self
                .session_id
                .read()
                .expect("mcp session_id lock poisoned")
                .is_some()
        {
            return anyhow!(
                "MCP session expired (HTTP 404); a new session must be initialized — \
                 restart the server connection (method: {method})"
            );
        }
        let body = response.text().await.unwrap_or_default();
        let snippet = crate::utils::redact_secrets(&body);
        let end = snippet.floor_char_boundary(ERROR_BODY_SNIPPET_BYTES.min(snippet.len()));
        anyhow!(
            "MCP server returned HTTP {} (method: {}): {}",
            status,
            method,
            &snippet[..end]
        )
    }
}

/// Tracks the SSE `id:` and `retry:` fields, which [`drain_sse_events`]
/// deliberately discards. Fed the same raw bytes as the event buffer; only
/// complete lines are inspected, so a field split across chunks still parses.
#[derive(Default)]
struct SseMeta {
    line_buf: Vec<u8>,
    /// `id:` of the event currently being parsed. Promoted to
    /// `last_event_id` only at the event's terminating blank line — WHATWG
    /// SSE commits the cursor at dispatch, not at field parse, so a
    /// disconnect mid-event must not advance the resume point past the very
    /// event that was cut off.
    pending_event_id: Option<String>,
    /// Last fully-delivered event id — the SEP-1699 resumption cursor.
    last_event_id: Option<String>,
    /// Server-requested reconnection delay in milliseconds.
    retry_ms: Option<u64>,
}

impl SseMeta {
    /// Reset per-stream parse state before draining a (re)connected stream.
    /// The resumption cursor and retry delay survive reconnects; a partial
    /// line cut off by the disconnect must not prefix the resumed stream's
    /// first line and corrupt id/retry parsing.
    fn start_stream(&mut self) {
        self.line_buf.clear();
        self.pending_event_id = None;
    }

    fn feed(&mut self, chunk: &[u8]) {
        self.line_buf.extend_from_slice(chunk);
        while let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.line_buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches(['\n', '\r']);
            if text.is_empty() {
                // Event boundary: the event was fully delivered.
                if let Some(id) = self.pending_event_id.take() {
                    self.last_event_id = Some(id);
                }
            } else if let Some(v) = text.strip_prefix("id:") {
                // Per SSE spec, one leading space after the colon is ignored,
                // and an empty id (the SEP-1699 priming event) still counts as
                // "this stream has IDs" — record it.
                let v = v.strip_prefix(' ').unwrap_or(v);
                self.pending_event_id = Some(v.to_string());
            } else if let Some(v) = text.strip_prefix("retry:")
                && let Ok(ms) = v.trim().parse::<u64>()
            {
                self.retry_ms = Some(ms);
            }
        }
    }
}

/// Connect-time DNS vetting for MCP endpoints. Unlike web_fetch's resolver,
/// loopback is always allowed (local MCP servers are a first-class case);
/// private / link-local / CGNAT / metadata addresses are blocked unless the
/// config opts in with `allow_private_network` — plugin bundles ship MCP
/// configs, so a malicious bundle must not reach 169.254.169.254 or the LAN.
struct McpVettingResolver {
    allow_private: bool,
}

impl reqwest::dns::Resolve for McpVettingResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow_private = self.allow_private;
        Box::pin(async move {
            let host = name.as_str().to_string();
            // Port 0 is a placeholder — reqwest overrides it with the URL's
            // port. We only need the resolved IPs in order to vet them.
            let addrs: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            for addr in &addrs {
                let class = classify_host(&addr.ip().to_string());
                let blocked = match class {
                    HostClass::Loopback | HostClass::Public => false,
                    _ => !allow_private,
                };
                if blocked {
                    return Err(format!(
                        "refusing to connect MCP server '{host}': it resolves to a \
                         private/internal address (set allow_private_network = true \
                         for this server to permit it)"
                    )
                    .into());
                }
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Canned-HTTP test fixture (project precedent: ollama/daemon tests), shared
/// with the server_manager tests.
///
/// A tokio TcpListener that serves one canned reply per accepted connection,
/// in order, recording each raw request. Every reply carries
/// `Connection: close`, so reqwest opens a fresh connection per request and
/// the accept loop stays strictly sequential.
#[cfg(test)]
pub(super) mod test_fixture {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Mutex;

    pub enum Reply {
        Raw(String),
        /// Accept and read the request, then never respond.
        Hang,
    }

    pub struct Fixture {
        pub url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl Fixture {
        pub async fn requests(&self) -> Vec<String> {
            self.requests.lock().await.clone()
        }

        pub fn config(&self) -> McpServerConfig {
            McpServerConfig {
                url: Some(self.url.clone()),
                ..Default::default()
            }
        }

        pub fn transport(&self) -> HttpTransport {
            HttpTransport::new(&self.config()).expect("transport")
        }
    }

    pub async fn fixture(replies: Vec<Reply>) -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let raw = read_http_request(&mut sock).await;
                recorded.lock().await.push(raw);
                match reply {
                    Reply::Raw(bytes) => {
                        let _ = sock.write_all(bytes.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    },
                    Reply::Hang => {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    },
                }
            }
        });
        Fixture {
            url: format!("http://127.0.0.1:{}/mcp", addr.port()),
            requests,
        }
    }

    /// Read one HTTP/1.1 request: headers through `\r\n\r\n`, then a body of
    /// exactly `Content-Length` bytes.
    async fn read_http_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                let content_length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let total = pos + 4 + content_length;
                while buf.len() < total {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                return String::from_utf8_lossy(&buf[..total.min(buf.len())]).into_owned();
            }
            match sock.read(&mut tmp).await {
                Ok(0) | Err(_) => return String::from_utf8_lossy(&buf).into_owned(),
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
    }

    pub fn json_reply_with_headers(body: &str, extra_headers: &str) -> Reply {
        Reply::Raw(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
    }

    pub fn json_reply(body: &str) -> Reply {
        json_reply_with_headers(body, "")
    }

    pub fn sse_reply(events: &str) -> Reply {
        Reply::Raw(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
            events.len()
        ))
    }

    pub fn status_reply(code: u16, reason: &str) -> Reply {
        Reply::Raw(format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))
    }

    pub fn rpc_response(id: u64, result: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#)
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::McpClient;
    use super::test_fixture::*;
    use super::*;

    #[tokio::test]
    async fn plain_json_response_round_trips() {
        let fx = fixture(vec![json_reply(&rpc_response(1, r#"{"ok":true}"#))]).await;
        let t = fx.transport();
        let result = t.send_request("ping", json!({})).await.expect("result");
        assert_eq!(result["ok"], true);
        let reqs = fx.requests().await;
        assert_eq!(reqs.len(), 1);
        // POST with the Accept pair, JSON body carrying our method.
        assert!(reqs[0].starts_with("POST /mcp"), "{}", reqs[0]);
        assert!(
            reqs[0]
                .to_ascii_lowercase()
                .contains("accept: application/json, text/event-stream"),
            "{}",
            reqs[0]
        );
        assert!(reqs[0].contains(r#""method":"ping""#), "{}", reqs[0]);
    }

    #[tokio::test]
    async fn sse_response_drains_notifications_until_our_id() {
        let events = format!(
            "data: {}\n\ndata: {}\n\n",
            r#"{"jsonrpc":"2.0","method":"notifications/message","params":{}}"#,
            rpc_response(1, r#"{"via":"sse"}"#)
        );
        let fx = fixture(vec![sse_reply(&events)]).await;
        let t = fx.transport();
        let result = t
            .send_request("tools/list", json!({}))
            .await
            .expect("result");
        assert_eq!(result["via"], "sse");
    }

    #[tokio::test]
    async fn session_id_is_captured_and_echoed_including_delete() {
        let fx = fixture(vec![
            // Old-spelling header on purpose: capture must be case-insensitive.
            json_reply_with_headers(&rpc_response(1, "{}"), "Mcp-Session-Id: sess-123\r\n"),
            json_reply(&rpc_response(2, "{}")),
            status_reply(200, "OK"), // DELETE
        ])
        .await;
        let t = fx.transport();
        t.send_request("initialize", json!({})).await.expect("init");
        t.send_request("tools/list", json!({})).await.expect("list");
        t.shutdown().await;
        let reqs = fx.requests().await;
        assert_eq!(reqs.len(), 3);
        assert!(
            !reqs[0].to_ascii_lowercase().contains("mcp-session-id"),
            "no session id before the server assigns one: {}",
            reqs[0]
        );
        assert!(
            reqs[1]
                .to_ascii_lowercase()
                .contains("mcp-session-id: sess-123"),
            "{}",
            reqs[1]
        );
        assert!(reqs[2].starts_with("DELETE /mcp"), "{}", reqs[2]);
        assert!(
            reqs[2]
                .to_ascii_lowercase()
                .contains("mcp-session-id: sess-123"),
            "{}",
            reqs[2]
        );
    }

    #[tokio::test]
    async fn shutdown_without_session_sends_nothing() {
        let fx = fixture(vec![]).await;
        let t = fx.transport();
        t.shutdown().await;
        assert!(fx.requests().await.is_empty());
    }

    #[tokio::test]
    async fn delete_405_is_tolerated() {
        let fx = fixture(vec![
            json_reply_with_headers(&rpc_response(1, "{}"), "MCP-Session-Id: s1\r\n"),
            status_reply(405, "Method Not Allowed"),
        ])
        .await;
        let t = fx.transport();
        t.send_request("initialize", json!({})).await.expect("init");
        // Must not panic or error — shutdown is infallible by design.
        t.shutdown().await;
        assert_eq!(fx.requests().await.len(), 2);
    }

    #[tokio::test]
    async fn http_404_with_active_session_reports_expiry() {
        let fx = fixture(vec![
            json_reply_with_headers(&rpc_response(1, "{}"), "MCP-Session-Id: s1\r\n"),
            status_reply(404, "Not Found"),
        ])
        .await;
        let t = fx.transport();
        t.send_request("initialize", json!({})).await.expect("init");
        let err = t
            .send_request("tools/list", json!({}))
            .await
            .expect_err("expired");
        assert!(err.to_string().contains("session expired"), "{err}");
    }

    #[tokio::test]
    async fn static_and_env_headers_reach_the_wire() {
        // Unique env var name — nextest runs tests in parallel processes but
        // in-process parallelism still forbids reuse across tests.
        const VAR: &str = "MERMAID_TEST_MCP_HTTP_ENV_HEADER_A";
        unsafe { std::env::set_var(VAR, "from-env") };
        let fx = fixture(vec![json_reply(&rpc_response(1, "{}"))]).await;
        let mut config = fx.config();
        config
            .headers
            .insert("X-Static-Token".to_string(), "static-secret".to_string());
        config
            .env_headers
            .insert("X-Env-Token".to_string(), VAR.to_string());
        // A missing env var is skipped, not an error.
        config.env_headers.insert(
            "X-Missing".to_string(),
            "MERMAID_TEST_MCP_HTTP_ENV_HEADER_MISSING".to_string(),
        );
        let t = HttpTransport::new(&config).expect("transport");
        t.send_request("ping", json!({})).await.expect("result");
        let req = fx.requests().await.remove(0).to_ascii_lowercase();
        assert!(req.contains("x-static-token: static-secret"), "{req}");
        assert!(req.contains("x-env-token: from-env"), "{req}");
        assert!(!req.contains("x-missing"), "{req}");
    }

    #[tokio::test]
    async fn invalid_header_name_errors_at_construction_without_value() {
        let mut config = McpServerConfig {
            url: Some("https://example.com/mcp".to_string()),
            ..Default::default()
        };
        config
            .headers
            .insert("bad header".to_string(), "secret-value".to_string());
        // map to () — HttpTransport has no Debug (it holds secret headers).
        let err = HttpTransport::new(&config)
            .map(|_| ())
            .expect_err("bad name");
        assert!(err.to_string().contains("bad header"), "{err}");
        assert!(!err.to_string().contains("secret-value"), "{err}");
    }

    #[tokio::test]
    async fn notification_accepts_202_and_rejects_500() {
        let fx = fixture(vec![
            status_reply(202, "Accepted"),
            Reply::Raw(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops"
                    .to_string(),
            ),
        ])
        .await;
        let t = fx.transport();
        t.send_notification("notifications/initialized", json!({}))
            .await
            .expect("202 ok");
        let err = t
            .send_notification("notifications/initialized", json!({}))
            .await
            .expect_err("500 err");
        // The status lives in the context chain; `:#` renders the whole chain.
        let rendered = format!("{err:#}");
        assert!(rendered.contains("500"), "{rendered}");
        assert!(
            rendered.contains("oops"),
            "body snippet expected: {rendered}"
        );
    }

    #[tokio::test]
    async fn http_202_on_a_request_is_a_protocol_error() {
        let fx = fixture(vec![status_reply(202, "Accepted")]).await;
        let t = fx.transport();
        let err = t
            .send_request("tools/list", json!({}))
            .await
            .expect_err("202 on request");
        assert!(err.to_string().contains("202"), "{err}");
    }

    #[tokio::test]
    async fn sse_eof_without_response_and_without_ids_errors() {
        let events =
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\n";
        let fx = fixture(vec![sse_reply(events)]).await;
        let t = fx.transport();
        let err = t
            .send_request("tools/list", json!({}))
            .await
            .expect_err("eof");
        assert!(
            err.to_string().contains("ended without a response"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn sse_disconnect_with_event_id_resumes_via_get_last_event_id() {
        // SEP-1699 polling: stream 1 sends a priming event (id, empty data)
        // plus a retry hint, then closes without the response. The client must
        // wait and resume with GET + Last-Event-ID; stream 2 delivers the
        // response.
        let priming = "id: ev-7\nretry: 10\ndata:\n\n";
        let resumed = format!(
            "id: ev-8\ndata: {}\n\n",
            rpc_response(1, r#"{"resumed":true}"#)
        );
        let fx = fixture(vec![sse_reply(priming), sse_reply(&resumed)]).await;
        let t = fx.transport();
        let result = t
            .send_request("tools/call", json!({}))
            .await
            .expect("resumed");
        assert_eq!(result["resumed"], true);
        let reqs = fx.requests().await;
        assert_eq!(reqs.len(), 2);
        assert!(reqs[1].starts_with("GET /mcp"), "{}", reqs[1]);
        let get = reqs[1].to_ascii_lowercase();
        assert!(get.contains("last-event-id: ev-7"), "{}", reqs[1]);
        assert!(get.contains("accept: text/event-stream"), "{}", reqs[1]);
    }

    #[tokio::test]
    async fn server_request_on_sse_stream_gets_error_reply_posted_back() {
        let events = format!(
            "data: {}\n\ndata: {}\n\n",
            r#"{"jsonrpc":"2.0","id":99,"method":"sampling/createMessage","params":{}}"#,
            rpc_response(1, "{}")
        );
        let fx = fixture(vec![sse_reply(&events), status_reply(202, "Accepted")]).await;
        let t = fx.transport();
        t.send_request("tools/list", json!({}))
            .await
            .expect("result");
        let reqs = fx.requests().await;
        assert_eq!(reqs.len(), 2, "the -32601 reply must be POSTed back");
        assert!(reqs[1].contains("-32601"), "{}", reqs[1]);
        assert!(reqs[1].contains(r#""id":99"#), "{}", reqs[1]);
    }

    #[tokio::test]
    async fn server_ping_on_sse_stream_gets_result_not_32601() {
        let events = format!(
            "data: {}\n\ndata: {}\n\n",
            r#"{"jsonrpc":"2.0","id":42,"method":"ping"}"#,
            rpc_response(1, "{}")
        );
        let fx = fixture(vec![sse_reply(&events), status_reply(202, "Accepted")]).await;
        let t = fx.transport();
        t.send_request("tools/list", json!({}))
            .await
            .expect("result");
        let reqs = fx.requests().await;
        assert_eq!(reqs.len(), 2);
        assert!(reqs[1].contains(r#""result":{}"#), "{}", reqs[1]);
        assert!(!reqs[1].contains("-32601"), "{}", reqs[1]);
    }

    #[tokio::test]
    async fn injected_short_timeout_is_enforced() {
        let fx = fixture(vec![Reply::Hang]).await;
        let t = fx.transport();
        let start = std::time::Instant::now();
        let err = t
            .send_request_with_timeout("tools/list", json!({}), 1)
            .await
            .expect_err("must time out");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn protocol_version_header_sent_after_initialize() {
        let init_result = r#"{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"fx","version":"1.0"}}"#;
        let fx = fixture(vec![
            json_reply_with_headers(&rpc_response(1, init_result), "MCP-Session-Id: s9\r\n"),
            status_reply(202, "Accepted"), // notifications/initialized
            json_reply(&rpc_response(2, r#"{"tools":[]}"#)),
        ])
        .await;
        let mut client = McpClient::new(fx.transport().into());
        client.initialize().await.expect("initialize");
        client.list_tools().await.expect("list");
        let reqs = fx.requests().await;
        assert_eq!(reqs.len(), 3);
        assert!(
            !reqs[0]
                .to_ascii_lowercase()
                .contains("mcp-protocol-version"),
            "initialize itself carries no version header: {}",
            reqs[0]
        );
        // Everything after initialize — including notifications/initialized —
        // carries the negotiated version and the session id.
        for req in &reqs[1..] {
            let low = req.to_ascii_lowercase();
            assert!(low.contains("mcp-protocol-version: 2025-11-25"), "{req}");
            assert!(low.contains("mcp-session-id: s9"), "{req}");
        }
    }

    #[tokio::test]
    async fn resolver_allows_loopback_blocks_private_honors_flag() {
        use reqwest::dns::Resolve;
        use std::str::FromStr;
        let name = |s: &str| reqwest::dns::Name::from_str(s).expect("name");
        // Loopback is always allowed — local MCP servers are first-class.
        let default = McpVettingResolver {
            allow_private: false,
        };
        assert!(default.resolve(name("localhost")).await.is_ok());
        // A private address is blocked by default...
        assert!(default.resolve(name("192.168.1.5")).await.is_err());
        // ...and allowed with the explicit opt-in.
        let opted_in = McpVettingResolver {
            allow_private: true,
        };
        assert!(opted_in.resolve(name("192.168.1.5")).await.is_ok());
    }

    #[test]
    fn sse_meta_tracks_id_and_retry_across_chunk_splits() {
        let mut meta = SseMeta::default();
        // Field split across chunks: only complete lines are inspected.
        meta.feed(b"id: ev");
        assert_eq!(meta.last_event_id, None);
        meta.feed(b"-1\nretry: 250\ndata: x\n\n");
        assert_eq!(meta.last_event_id.as_deref(), Some("ev-1"));
        assert_eq!(meta.retry_ms, Some(250));
        // A later id supersedes; CRLF line endings parse too.
        meta.feed(b"id: ev-2\r\ndata: y\r\n\r\n");
        assert_eq!(meta.last_event_id.as_deref(), Some("ev-2"));
    }

    #[test]
    fn sse_meta_commits_id_only_at_event_boundary() {
        let mut meta = SseMeta::default();
        meta.feed(b"id: ev-1\ndata: x\n\n");
        assert_eq!(meta.last_event_id.as_deref(), Some("ev-1"));
        // An event cut off before its terminating blank line must NOT advance
        // the cursor — resuming after it would skip its redelivery.
        meta.feed(b"id: ev-2\ndata: {\"jsonr");
        assert_eq!(meta.last_event_id.as_deref(), Some("ev-1"));
        // The resumed stream starts fresh: the partial line is discarded and
        // must not swallow the first field of the new stream.
        meta.start_stream();
        meta.feed(b"id: ev-2\ndata: y\n\n");
        assert_eq!(meta.last_event_id.as_deref(), Some("ev-2"));
    }

    #[test]
    fn new_vets_ip_literal_hosts() {
        // reqwest never consults the dns_resolver for IP literals, so new()
        // must apply the private-network policy itself.
        let private = McpServerConfig {
            url: Some("https://192.168.1.5/mcp".to_string()),
            ..Default::default()
        };
        // No expect_err: HttpTransport deliberately has no Debug impl (it
        // holds secret-bearing headers).
        let err = match HttpTransport::new(&private) {
            Ok(_) => panic!("private literal must be rejected"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("allow_private_network"), "{err}");
        let metadata = McpServerConfig {
            url: Some("https://[::ffff:169.254.169.254]/mcp".to_string()),
            ..Default::default()
        };
        assert!(HttpTransport::new(&metadata).is_err());
        let opted_in = McpServerConfig {
            url: Some("https://192.168.1.5/mcp".to_string()),
            allow_private_network: true,
            ..Default::default()
        };
        assert!(HttpTransport::new(&opted_in).is_ok());
        // Loopback literals stay first-class.
        let loopback = McpServerConfig {
            url: Some("http://127.0.0.1:9099/mcp".to_string()),
            ..Default::default()
        };
        assert!(HttpTransport::new(&loopback).is_ok());
    }

    #[test]
    fn new_rejects_config_without_url() {
        let config = McpServerConfig {
            command: "npx".to_string(),
            ..Default::default()
        };
        assert!(HttpTransport::new(&config).is_err());
    }
}
