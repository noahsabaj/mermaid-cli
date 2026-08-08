//! Stdio JSON-RPC 2.0 transport for MCP servers.
//!
//! Spawns an MCP server as a child process and communicates via
//! newline-delimited JSON-RPC messages over stdin/stdout.
//! Server stderr is piped to tracing (not parsed as JSON-RPC).

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::time::{Duration, timeout};

use mermaid_model::constants::MAX_MCP_FRAME_BYTES;
use mermaid_model::utils::{CappedLine, read_line_capped};

/// Default timeout for JSON-RPC request/response round-trips (discovery,
/// initialize, ping — all fast control calls). Shared with the HTTP transport.
pub(super) const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Timeout for a `tools/call` round-trip. Tool invocations do real work —
/// browser automation, web research, large builds — so the fast-control 30s
/// budget spuriously killed legitimate long-running tools. This is the ceiling;
/// the turn's own cancellation still aborts a genuinely stuck call sooner.
/// Shared with the HTTP transport.
pub(super) const TOOL_CALL_TIMEOUT_SECS: u64 = 300;

/// Timeout for a single stdin write+flush. Separate from (and shorter than)
/// `REQUEST_TIMEOUT_SECS`: this bounds local pipe backpressure — a child that
/// never drains its stdin — not server compute time. Healthy frame writes
/// complete in microseconds; a multi-second stall means the child is wedged,
/// so we fail fast instead of waiting the full response budget (#37).
const WRITE_TIMEOUT_SECS: u64 = 10;

/// Minimal, non-secret environment variables passed through to an MCP server
/// child after `env_clear()`. Deliberately excludes every provider API key /
/// cloud credential Mermaid holds — only locale, terminal, and path basics
/// survive (plus the server's own declared `env`, added separately, and any
/// `LC_*` matched by prefix). See F48.
#[cfg(not(windows))]
const SAFE_ENV_PASSTHROUGH: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TERM", "TMPDIR",
];
#[cfg(windows)]
const SAFE_ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TERM",
    "TMPDIR",
    "SYSTEMROOT",
    "SystemRoot",
    "APPDATA",
    "USERPROFILE",
    "PATHEXT",
    "COMSPEC",
];

/// Stdio transport for a single MCP server process.
///
/// Manages the child process lifecycle and provides request/response
/// correlation over the JSON-RPC 2.0 protocol.
///
/// # Concurrency model
///
/// The `stdin` mutex serializes outbound writes so that JSON-RPC messages
/// are never interleaved. The `pending` mutex maps request IDs to oneshot
/// channels, allowing multiple in-flight requests (the ID counter is
/// atomic). In practice, because `send_request` holds the `stdin` lock
/// for the entire write+flush, concurrent callers queue behind it. This
/// is correct: stdio is a byte stream with no framing guarantees beyond
/// newlines, so concurrent writes could produce corrupt JSON.
pub struct StdioTransport {
    /// Stdin writer for sending messages to the server
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    /// Pending requests waiting for a response: id → oneshot sender
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    /// Monotonic request ID counter
    next_id: AtomicU64,
    /// Child process handle (for shutdown)
    child: Arc<Mutex<Child>>,
    /// Background reader task handle
    _reader_task: tokio::task::JoinHandle<()>,
}

impl StdioTransport {
    /// Spawn an MCP server process and set up the transport.
    ///
    /// The command is executed with piped stdin/stdout. Stderr is read
    /// in a background task and forwarded to tracing.
    #[expect(
        clippy::too_many_lines,
        reason = "predates the lint; see .github/baselines/expect_budget.txt"
    )]
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // If `Child` is dropped without `shutdown` being called (e.g. the
            // outer task panics or we abort during init), tokio kills and
            // reaps the process for us. `shutdown` does the same explicitly
            // for the normal path; this is just a belt-and-braces fallback
            // so we never leak MCP server processes.
            .kill_on_drop(true);

        // Don't hand an MCP server child Mermaid's entire environment — that
        // would leak every provider API key / cloud credential we hold to
        // third-party (including curated, no-confirmation) server code. Start
        // from an empty environment and re-add only a minimal, non-secret
        // allowlist, plus any `LC_*` locale overrides, plus the server's own
        // declared `env` vars (which intentionally take precedence). See F48.
        cmd.env_clear();
        for key in SAFE_ENV_PASSTHROUGH {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        for (key, value) in std::env::vars_os() {
            if key.to_str().is_some_and(|k| k.starts_with("LC_")) {
                cmd.env(&key, &value);
            }
        }

        for (key, value) in env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().with_context(|| {
            // Redact args — they can carry secrets (e.g. `--api-key=…`) (#93).
            format!(
                "Failed to spawn MCP server: {} {}",
                command,
                mermaid_model::utils::redact_secrets(&args.join(" "))
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to capture MCP server stdin"))?;
        // Wrap stdin now (rather than at the end) so the stdout reader task can
        // share it and reply to server-initiated requests we don't support (F79).
        let stdin = Arc::new(Mutex::new(stdin));
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to capture MCP server stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to capture MCP server stderr"))?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Background task: read stdout for JSON-RPC responses
        let pending_clone = Arc::clone(&pending);
        let stdin_for_reader = Arc::clone(&stdin);
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let line = match read_line_capped(&mut reader, MAX_MCP_FRAME_BYTES).await {
                    Ok(CappedLine::Line(bytes)) => match String::from_utf8(bytes) {
                        Ok(s) => s,
                        // A non-UTF-8 frame can't be a JSON-RPC message — JSON is
                        // UTF-8 by RFC 8259 §8.1 and serde_json needs a &str — so it
                        // carries nothing to route. Skip it and resync on the next
                        // frame instead of tearing down the reader (#36).
                        Err(_) => {
                            tracing::warn!("MCP: dropping non-UTF-8 stdout frame");
                            continue;
                        },
                    },
                    Ok(CappedLine::TooLong) => {
                        tracing::warn!("MCP: dropping oversize stdout frame (exceeds cap)");
                        continue;
                    },
                    Ok(CappedLine::Eof) | Err(_) => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    // Slice on a char boundary — MCP stdout may contain
                    // non-ASCII text, and a raw byte slice at offset 200
                    // could fall inside a multi-byte codepoint and panic.
                    let end = line.floor_char_boundary(200);
                    tracing::warn!("MCP: unparseable stdout line: {}", &line[..end]);
                    continue;
                };

                // Only a true JSON-RPC *response* (an id, no method) completes a
                // pending request. A server-initiated *request* also carries an id
                // but ALSO a method; its id can collide with one of ours, and
                // routing it to `pending` would wrongly complete a caller's oneshot
                // (#89). Notifications (method, no id) are likewise not responses.
                if is_response(&msg) {
                    if let Some(id) = msg.get("id").and_then(parse_response_id) {
                        let mut pending = pending_clone.lock().await;
                        if let Some(sender) = pending.remove(&id) {
                            let _ = sender.send(msg);
                        }
                    }
                } else if msg.get("method").is_some() {
                    // Has a `method`: either a server-initiated REQUEST (also
                    // carries an `id`) or a NOTIFICATION (no `id`). We implement
                    // neither, but they need different handling.
                    match msg.get("id") {
                        // Server request: it BLOCKS until it gets a response. Reply
                        // with a JSON-RPC "method not supported" error rather than
                        // dropping it and letting the server stall forever (F79).
                        Some(id) if !id.is_null() => {
                            tracing::debug!(
                                method = msg.get("method").and_then(|m| m.as_str()).unwrap_or(""),
                                "MCP: replying method-not-supported to unsupported server request"
                            );
                            reply_method_not_supported(&stdin_for_reader, id).await;
                        },
                        // Notification: no `id`, no reply expected. Ignore it.
                        _ => {
                            tracing::trace!(
                                method = msg.get("method").and_then(|m| m.as_str()).unwrap_or(""),
                                "MCP: ignoring server notification (no reply expected)"
                            );
                        },
                    }
                } else {
                    // Neither a response nor a request/notification we recognize
                    // (e.g. an `id` that isn't a parseable number, with no method).
                    tracing::trace!("MCP: ignoring unrecognized stdout message");
                }
            }

            // Reader loop exited: stdout hit EOF (child closed it) or a fatal read
            // error. No further responses can arrive, so fail every still-pending
            // request NOW rather than letting each caller burn REQUEST_TIMEOUT_SECS.
            // Dropping each oneshot::Sender closes its channel; the caller's
            // `timeout(.., rx)` then resolves immediately via its channel-closed arm
            // (#94). `clear()` drops all senders.
            pending_clone.lock().await.clear();
        });

        // Background task: read stderr for logging (not JSON-RPC)
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            loop {
                match read_line_capped(&mut reader, MAX_MCP_FRAME_BYTES).await {
                    Ok(CappedLine::Line(bytes)) => {
                        // Redact — servers sometimes echo secrets on stderr (#93).
                        let line = String::from_utf8_lossy(&bytes);
                        tracing::debug!(
                            "MCP stderr: {}",
                            mermaid_model::utils::redact_secrets(&line)
                        );
                    },
                    Ok(CappedLine::TooLong) => continue,
                    Ok(CappedLine::Eof) | Err(_) => break,
                }
            }
        });

        Ok(Self {
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            child: Arc::new(Mutex::new(child)),
            _reader_task: reader_task,
        })
    }

    /// Send a JSON-RPC request and wait for the response under the default
    /// control-call timeout (`REQUEST_TIMEOUT_SECS`).
    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        self.send_request_with_timeout(method, params, REQUEST_TIMEOUT_SECS)
            .await
    }

    /// Like [`Self::send_request`], but with an explicit response-wait budget. Used by
    /// `tools/call`, whose work can legitimately outrun the fast-control budget.
    pub async fn send_request_with_timeout(
        &self,
        method: &str,
        params: Value,
        response_timeout_secs: u64,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        // Serialize BEFORE registering the pending entry so a (theoretical)
        // serialization failure can't leak it.
        let msg = format!("{}\n", serde_json::to_string(&request)?);

        // Register pending response before sending (avoid the race where the
        // reader sees the response before we've inserted the sender).
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }

        // Write the request under a timeout. On any failure — including a wedged
        // child that never drains its stdin — drop the pending entry first so a
        // flaky server can't slowly grow the map with dead senders. Error context
        // carries only `method`, never `params` (which may hold secrets) (#37).
        {
            let mut stdin = self.stdin.lock().await;
            match timeout(
                Duration::from_secs(WRITE_TIMEOUT_SECS),
                stdin.write_all(msg.as_bytes()),
            )
            .await
            {
                Ok(Ok(())) => {},
                Ok(Err(e)) => {
                    drop(stdin);
                    self.pending.lock().await.remove(&id);
                    return Err(e).with_context(|| {
                        format!("Failed to write to MCP server stdin (method: {method})")
                    });
                },
                Err(_) => {
                    drop(stdin);
                    self.pending.lock().await.remove(&id);
                    return Err(anyhow!(
                        "Timed out after {WRITE_TIMEOUT_SECS}s writing to MCP server stdin (method: {method})"
                    ));
                },
            }
            match timeout(Duration::from_secs(WRITE_TIMEOUT_SECS), stdin.flush()).await {
                Ok(Ok(())) => {},
                Ok(Err(e)) => {
                    drop(stdin);
                    self.pending.lock().await.remove(&id);
                    return Err(e).with_context(|| {
                        format!("Failed to flush MCP server stdin (method: {method})")
                    });
                },
                Err(_) => {
                    drop(stdin);
                    self.pending.lock().await.remove(&id);
                    return Err(anyhow!(
                        "Timed out after {WRITE_TIMEOUT_SECS}s flushing MCP server stdin (method: {method})"
                    ));
                },
            }
        }

        // Wait for the response, removing the pending entry on EVERY non-success
        // exit (timeout or channel-closed) so it can't leak. On success the
        // reader task has already removed it.
        let response = match timeout(Duration::from_secs(response_timeout_secs), rx).await {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                return Err(anyhow!("MCP response channel closed unexpectedly"));
            },
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(anyhow!(
                    "MCP request timed out after {response_timeout_secs}s: {method}"
                ));
            },
        };

        extract_jsonrpc_result(response)
    }

    /// Send a JSON-RPC notification (no response expected).
    pub async fn send_notification(&self, method: &str, params: Value) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        let msg = format!("{}\n", serde_json::to_string(&notification)?);
        let mut stdin = self.stdin.lock().await;
        // Bound the write+flush like send_request (#37): a child that never
        // drains its stdin must not hang the caller forever.
        timeout(
            Duration::from_secs(WRITE_TIMEOUT_SECS),
            stdin.write_all(msg.as_bytes()),
        )
        .await
        .map_err(|_| anyhow!("Timed out writing MCP notification (method: {method})"))??;
        timeout(Duration::from_secs(WRITE_TIMEOUT_SECS), stdin.flush())
            .await
            .map_err(|_| anyhow!("Timed out flushing MCP notification (method: {method})"))??;
        Ok(())
    }

    /// Gracefully shut down the MCP server process per MCP spec guidance:
    /// close stdin (signals EOF) → brief grace → SIGTERM → SIGKILL. Most servers
    /// exit cleanly on stdin close; the SIGTERM/SIGKILL steps are a safety net
    /// for misbehaving servers.
    ///
    /// On Unix, SIGTERM is delivered by `terminate` (shelling out to `kill`,
    /// matching `runtime::client::kill_pid`) so a well-behaved server can run its
    /// signal handler before we escalate to SIGKILL via `start_kill`. On non-Unix
    /// there is no SIGTERM, so we go straight to `start_kill` (the platform's
    /// terminate).
    ///
    /// `kill_on_drop(true)` set in `spawn` is the panic-path fallback;
    /// this explicit path is the normal one.
    pub async fn shutdown(&self) {
        // Step 1: close stdin. Many MCP servers treat stdin EOF as a
        // shutdown signal and exit on their own.
        //
        // Note: we can't drop `self.stdin` because it's behind an Arc —
        // but `ChildStdin::shutdown()` flushes + closes the write side,
        // which has the same effect for the child's reader.
        {
            let mut stdin = self.stdin.lock().await;
            let _ = stdin.shutdown().await;
        }

        let mut child = self.child.lock().await;

        // Step 2: short grace period (2s) for the server to exit on its own.
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_ok()
        {
            return;
        }

        // Step 3: SIGTERM (Unix only) — ask the server to terminate cleanly and
        // give its handler a brief grace. No-op on non-Unix, which has no
        // SIGTERM; that falls through to `start_kill` below.
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            terminate(pid).await;
            if tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .is_ok()
            {
                return;
            }
        }

        // Step 4: SIGKILL via `start_kill` (doesn't await), then a brief grace.
        // On non-Unix this is the first forced-terminate step.
        if let Err(e) = child.start_kill() {
            tracing::debug!("MCP: start_kill failed: {}", e);
        }
        if tokio::time::timeout(Duration::from_secs(1), child.wait())
            .await
            .is_ok()
        {
            return;
        }

        // Step 5: last resort — force kill and reap. `kill()` on tokio
        // is equivalent to `start_kill() + wait()`, so this also reaps.
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

/// The transport an [`super::client::McpClient`] talks through: a spawned
/// child process (stdio) or a remote Streamable HTTP endpoint. An enum rather
/// than a trait object: async trait objects need the `async-trait` crate and
/// generics would ripple through the manager and effect layers.
pub(super) enum Transport {
    Stdio(StdioTransport),
    // Boxed: HttpTransport (client + url + header maps) is ~8x the stdio
    // variant, and clients live behind an Arc anyway (large_enum_variant).
    Http(Box<super::transport_http::HttpTransport>),
}

impl From<StdioTransport> for Transport {
    fn from(t: StdioTransport) -> Self {
        Transport::Stdio(t)
    }
}

impl From<super::transport_http::HttpTransport> for Transport {
    fn from(t: super::transport_http::HttpTransport) -> Self {
        Transport::Http(Box::new(t))
    }
}

impl Transport {
    /// The response-wait budget a `tools/call` should use — longer than the
    /// fast-control default because tool work (browser automation, research,
    /// builds) legitimately takes minutes.
    pub fn tool_call_timeout_secs() -> u64 {
        TOOL_CALL_TIMEOUT_SECS
    }

    /// Send a JSON-RPC request and wait for the response under the default
    /// control-call timeout.
    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        match self {
            Transport::Stdio(t) => t.send_request(method, params).await,
            Transport::Http(t) => t.send_request(method, params).await,
        }
    }

    /// Like [`Self::send_request`], but with an explicit response-wait budget.
    pub async fn send_request_with_timeout(
        &self,
        method: &str,
        params: Value,
        response_timeout_secs: u64,
    ) -> Result<Value> {
        match self {
            Transport::Stdio(t) => {
                t.send_request_with_timeout(method, params, response_timeout_secs)
                    .await
            },
            Transport::Http(t) => {
                t.send_request_with_timeout(method, params, response_timeout_secs)
                    .await
            },
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    pub async fn send_notification(&self, method: &str, params: Value) -> Result<()> {
        match self {
            Transport::Stdio(t) => t.send_notification(method, params).await,
            Transport::Http(t) => t.send_notification(method, params).await,
        }
    }

    /// Record the protocol version negotiated at `initialize`. HTTP sends it
    /// as the `MCP-Protocol-Version` header on every subsequent request;
    /// stdio has no per-message headers, so this is a no-op there.
    pub fn set_protocol_version(&self, version: &str) {
        match self {
            Transport::Stdio(_) => {},
            Transport::Http(t) => t.set_protocol_version(version),
        }
    }

    /// Gracefully shut the transport down (kill the child / end the session).
    pub async fn shutdown(&self) {
        match self {
            Transport::Stdio(t) => t.shutdown().await,
            Transport::Http(t) => t.shutdown().await,
        }
    }
}

/// Turn a full JSON-RPC response object into its `result`, mapping a JSON-RPC
/// `error` member to an `Err`. Shared by both transports.
pub(super) fn extract_jsonrpc_result(response: Value) -> Result<Value> {
    if let Some(error) = response.get("error") {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown error");
        return Err(anyhow!("MCP error (code {code}): {message}"));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("MCP response missing 'result' field"))
}

/// Extract a u64 request id from a JSON-RPC `id` field, accepting either an
/// integer (`{"id": 5}`) or a string-encoded integer (`{"id": "5"}`). The
/// JSON-RPC 2.0 spec permits both shapes and a strict server may echo back
/// the id as a different JSON type than we sent.
pub(super) fn parse_response_id(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
}

/// True only for a JSON-RPC *response* (a reply to a request we sent), as
/// opposed to a server-initiated *request* or a *notification*. Per JSON-RPC
/// 2.0 a Response has an `id` and never a `method`; a server Request has an
/// `id` AND a `method`; a Notification has a `method` and no `id`. Routing a
/// message that has a `method` to `pending` would wrongly complete a caller's
/// oneshot — see #89.
pub(super) fn is_response(msg: &Value) -> bool {
    msg.get("id").and_then(parse_response_id).is_some() && msg.get("method").is_none()
}

/// Reply to a server-initiated JSON-RPC *request* we don't implement with a
/// JSON-RPC error, so the server isn't left blocking on a response that would
/// otherwise never arrive (F79). The request `id` is echoed back verbatim
/// (number or string), as the spec requires. Best-effort and bounded by
/// `WRITE_TIMEOUT_SECS`: a write failure or a wedged child just means no reply
/// gets out, which the reader loop will soon observe as EOF anyway.
async fn reply_method_not_supported(stdin: &Mutex<tokio::process::ChildStdin>, id: &Value) {
    // -32601 is the JSON-RPC 2.0 "Method not found" code.
    let reply = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.clone(),
        "error": { "code": -32601, "message": "method not supported" },
    });
    let Ok(serialized) = serde_json::to_string(&reply) else {
        return;
    };
    let mut line = serialized;
    line.push('\n');
    let mut guard = stdin.lock().await;
    let _ = timeout(
        Duration::from_secs(WRITE_TIMEOUT_SECS),
        guard.write_all(line.as_bytes()),
    )
    .await;
    let _ = timeout(Duration::from_secs(WRITE_TIMEOUT_SECS), guard.flush()).await;
}

/// Send SIGTERM to `pid` by shelling out to `kill`, mirroring
/// `runtime::client::kill_pid`. Best-effort: failure (process already gone, or
/// `kill` missing) is ignored — the caller escalates to SIGKILL next. Positive
/// pid only: the MCP child is not a process-group leader, so we signal the
/// child itself, not a group.
#[cfg(unix)]
async fn terminate(pid: u32) {
    let _ = Command::new("kill")
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

#[cfg(test)]
mod tests {
    use super::{is_response, parse_response_id};
    use serde_json::json;

    /// The warn-log truncation used above must not panic when byte 200 lands
    /// inside a multi-byte UTF-8 codepoint. This reproduces the scenario with
    /// 199 ASCII bytes followed by a 3-byte CJK character that straddles the
    /// 200 cutoff.
    #[test]
    fn truncation_respects_char_boundary() {
        let line = format!("{}你好", "a".repeat(199));
        // Byte 200 falls inside the first codepoint of "你好".
        let end = line.floor_char_boundary(200);
        let truncated = &line[..end]; // must not panic
        assert!(end <= 200);
        assert!(truncated.is_char_boundary(end));
    }

    #[test]
    fn parse_response_id_accepts_integer() {
        assert_eq!(parse_response_id(&json!(5)), Some(5));
        assert_eq!(parse_response_id(&json!(0)), Some(0));
    }

    #[test]
    fn parse_response_id_accepts_string_integer() {
        // JSON-RPC 2.0 allows the id to be a string; spec-conformant servers
        // may echo back `"id": "5"` even though we sent `"id": 5`.
        assert_eq!(parse_response_id(&json!("5")), Some(5));
        assert_eq!(parse_response_id(&json!("0")), Some(0));
    }

    #[test]
    fn parse_response_id_rejects_non_numeric() {
        assert_eq!(parse_response_id(&json!("abc")), None);
        assert_eq!(parse_response_id(&json!(null)), None);
        assert_eq!(parse_response_id(&json!({})), None);
        assert_eq!(parse_response_id(&json!(-1)), None);
    }

    #[test]
    fn is_response_true_for_result_or_error() {
        assert!(is_response(
            &json!({"jsonrpc": "2.0", "id": 1, "result": {}})
        ));
        assert!(is_response(
            &json!({"jsonrpc": "2.0", "id": "7", "error": {"code": -1, "message": "x"}})
        ));
    }

    #[test]
    fn is_response_false_for_server_request() {
        // Server-initiated request: has BOTH id and method. Must NOT be routed to
        // a pending oneshot — that is bug #89 (and its id can collide with ours).
        assert!(!is_response(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "sampling/createMessage", "params": {}})
        ));
    }

    #[test]
    fn is_response_false_for_notification() {
        // Notification: method, no id.
        assert!(!is_response(
            &json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {}})
        ));
    }

    #[test]
    fn is_response_false_without_id() {
        assert!(!is_response(&json!({"jsonrpc": "2.0", "result": {}})));
    }

    #[test]
    fn invalid_utf8_frame_is_not_valid_json() {
        // #36 safety claim: a non-UTF-8 frame can never be a JSON-RPC message, so
        // skipping it (rather than tearing down the reader) loses nothing.
        let bad = [0xff, 0xfe, b'{', b'}'];
        assert!(String::from_utf8(bad.to_vec()).is_err());
        assert!(serde_json::from_slice::<serde_json::Value>(&bad).is_err());
    }

    // The server reads our request, then closes stdout while keeping stdin open
    // and staying alive — so the write+flush succeed and resolution comes from
    // the #94 EOF drain (channel-closed), not a BrokenPipe or the 30s wait.
    // Pre-#94 this blocks ~REQUEST_TIMEOUT_SECS.
    #[cfg(unix)]
    #[tokio::test]
    async fn pending_request_fails_fast_when_server_closes_stdout() {
        use super::StdioTransport;
        use std::collections::HashMap;
        let t = StdioTransport::spawn(
            "sh",
            &["-c".to_string(), "read x; exec 1>&-; sleep 2".to_string()],
            &HashMap::new(),
        )
        .await
        .expect("spawn");

        let start = std::time::Instant::now();
        let res = t.send_request("ping", json!({})).await;
        assert!(
            res.is_err(),
            "request must fail once the server closes stdout"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "must fail fast via the EOF drain, not wait REQUEST_TIMEOUT_SECS"
        );
        // `t` drops here → kill_on_drop reaps the sleeping child.
    }

    // F48: the MCP child must NOT inherit Mermaid's whole environment (which
    // holds every provider API key). The child echoes back, as a JSON-RPC
    // response, a declared env var (must pass through), whether PATH is present
    // (allowlisted), and CARGO — a var the parent has under `cargo test` that is
    // NOT allowlisted, so `env_clear()` must drop it.
    #[cfg(unix)]
    #[tokio::test]
    async fn child_env_is_cleared_except_allowlist_and_declared() {
        use super::StdioTransport;
        use std::collections::HashMap;
        let script = r#"read line
printf '{"jsonrpc":"2.0","id":1,"result":{"declared":"%s","path":"%s","cargo":"%s"}}\n' "$DECLARED_TOKEN" "$([ -n "$PATH" ] && echo yes || echo no)" "$CARGO""#;
        let mut env = HashMap::new();
        env.insert("DECLARED_TOKEN".to_string(), "passed-through".to_string());

        let t = StdioTransport::spawn("sh", &["-c".to_string(), script.to_string()], &env)
            .await
            .expect("spawn");
        let res = t.send_request("ping", json!({})).await.expect("response");
        assert_eq!(
            res["declared"], "passed-through",
            "declared env vars must still pass through"
        );
        assert_eq!(res["path"], "yes", "PATH must be in the allowlist");
        assert_eq!(
            res["cargo"], "",
            "a non-allowlisted inherited var must be cleared by env_clear()"
        );
    }

    // F79: a server-initiated request (id + method) we don't support must get a
    // JSON-RPC error reply so the server isn't left blocking. The fake server
    // reads our ping, emits a server request, then reads our reply and reports —
    // via the ping response — whether that reply carried the -32601 error code.
    // The read/write alternation makes ordering deterministic.
    #[cfg(unix)]
    #[tokio::test]
    async fn server_initiated_request_gets_method_not_supported_reply() {
        use super::StdioTransport;
        use std::collections::HashMap;
        let script = r#"read ping
printf '{"jsonrpc":"2.0","id":99,"method":"sampling/createMessage","params":{}}\n'
read reply
case "$reply" in
  *-32601*) printf '{"jsonrpc":"2.0","id":1,"result":{"replied":true}}\n' ;;
  *) printf '{"jsonrpc":"2.0","id":1,"result":{"replied":false}}\n' ;;
esac"#;
        let t = StdioTransport::spawn(
            "sh",
            &["-c".to_string(), script.to_string()],
            &HashMap::new(),
        )
        .await
        .expect("spawn");
        let res = t
            .send_request("ping", json!({}))
            .await
            .expect("ping response");
        assert_eq!(
            res["replied"], true,
            "transport must reply with a -32601 error to a server-initiated request, \
             not silently drop it"
        );
    }

    // terminate() must deliver a real SIGTERM (signal 15), not SIGKILL (signal 9)
    // (#92). Assert on the delivered signal directly rather than relying on a
    // TERM trap running, which would race trap-installation against the signal.
    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_sends_real_sigterm() {
        use std::os::unix::process::ExitStatusExt;
        use tokio::process::Command;
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 10")
            .spawn()
            .expect("spawn sh");
        let pid = child.id().expect("pid");
        super::terminate(pid).await;
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .expect("child should die from the signal")
            .expect("wait");
        // 15 = SIGTERM (POSIX; identical on Linux/macOS). SIGKILL would be 9.
        assert_eq!(
            status.signal(),
            Some(15),
            "terminate() must send SIGTERM, not SIGKILL; got {status:?}"
        );
    }
}
