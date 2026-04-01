//! Stdio JSON-RPC 2.0 transport for MCP servers.
//!
//! Spawns an MCP server as a child process and communicates via
//! newline-delimited JSON-RPC messages over stdin/stdout.
//! Server stderr is piped to tracing (not parsed as JSON-RPC).

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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
            .stderr(std::process::Stdio::piped());

        for (key, value) in env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "Failed to spawn MCP server: {} {}",
                command,
                args.join(" ")
            )
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
                    tracing::warn!("MCP: unparseable stdout line: {}", &line[..line.len().min(200)]);
                    continue;
                };

                // Check if this is a response (has "id" and either "result" or "error")
                if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
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
            .map_err(|_| anyhow!("MCP request timed out after {}s: {}", REQUEST_TIMEOUT_SECS, method))?
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

    /// Gracefully shut down the MCP server process.
    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        // Try graceful kill first
        let _ = child.kill().await;
    }
}
