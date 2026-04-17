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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::time::{Duration, timeout};

/// Default timeout for JSON-RPC request/response round-trips
const REQUEST_TIMEOUT_SECS: u64 = 30;

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

        for (key, value) in env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().with_context(|| {
            format!("Failed to spawn MCP server: {} {}", command, args.join(" "))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to capture MCP server stdin"))?;
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
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
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

                // Check if this is a response (has "id" and either "result" or "error").
                // JSON-RPC 2.0 allows the id to be a string OR a number; we always
                // SEND integers (next_id is u64), but spec-conformant servers may
                // echo back the id in either shape, so accept both.
                if let Some(id) = msg.get("id").and_then(parse_response_id) {
                    let mut pending = pending_clone.lock().await;
                    if let Some(sender) = pending.remove(&id) {
                        let _ = sender.send(msg);
                    }
                }
                // Notifications (no "id") are silently ignored for now.
                // Future: handle notifications/tools/list_changed etc.
            }
        });

        // Background task: read stderr for logging (not JSON-RPC)
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!("MCP stderr: {}", line);
            }
        });

        Ok(Self {
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            next_id: AtomicU64::new(1),
            child: Arc::new(Mutex::new(child)),
            _reader_task: reader_task,
        })
    }

    /// Send a JSON-RPC request and wait for the response.
    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        // Register pending response before sending (avoid race)
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }

        // Write request as newline-delimited JSON
        let msg = format!("{}\n", serde_json::to_string(&request)?);
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(msg.as_bytes()).await.with_context(|| {
                format!("Failed to write to MCP server stdin (method: {})", method)
            })?;
            stdin.flush().await?;
        }

        // Wait for response with timeout
        let response = timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), rx)
            .await
            .map_err(|_| {
                anyhow!(
                    "MCP request timed out after {}s: {}",
                    REQUEST_TIMEOUT_SECS,
                    method
                )
            })?
            .map_err(|_| anyhow!("MCP response channel closed unexpectedly"))?;

        // Check for JSON-RPC error
        if let Some(error) = response.get("error") {
            let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            let message = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown error");
            return Err(anyhow!("MCP error (code {}): {}", code, message));
        }

        // Extract result
        response
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("MCP response missing 'result' field"))
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
        stdin.write_all(msg.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Gracefully shut down the MCP server process per MCP spec guidance:
    /// close stdin (signals EOF) → brief grace period → SIGTERM → SIGKILL.
    /// Most servers exit cleanly on stdin close; the terminate/kill steps
    /// are a safety net for misbehaving servers.
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

        // Step 3: SIGTERM-equivalent via `start_kill` (doesn't await).
        // Give another brief grace for the signal handler to run.
        if let Err(e) = child.start_kill() {
            tracing::debug!("MCP: start_kill failed: {}", e);
        }
        if tokio::time::timeout(Duration::from_secs(1), child.wait())
            .await
            .is_ok()
        {
            return;
        }

        // Step 4: last resort — force kill and reap. `kill()` on tokio
        // is equivalent to `start_kill() + wait()`, so this also reaps.
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

/// Extract a u64 request id from a JSON-RPC `id` field, accepting either an
/// integer (`{"id": 5}`) or a string-encoded integer (`{"id": "5"}`). The
/// JSON-RPC 2.0 spec permits both shapes and a strict server may echo back
/// the id as a different JSON type than we sent.
fn parse_response_id(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
}

#[cfg(test)]
mod tests {
    use super::parse_response_id;
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
}
