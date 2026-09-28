//! A scripted HTTP provider for adapter tests: one `TcpListener`, a handler
//! that sees each request, and a log of what was sent.
//!
//! Deliberately tiny. It speaks just enough HTTP/1.1 for reqwest (a
//! `Content-Length` request body in, one `Connection: close` response out)
//! and no more, so a test reads as "the provider said this" rather than as
//! mock-server configuration.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One request the provider received.
#[derive(Debug, Clone)]
pub(crate) struct Received {
    pub(crate) path: String,
    /// Header names lowercased, in arrival order.
    pub(crate) headers: Vec<(String, String)>,
    /// The JSON body, or `Value::Null` for a bodyless request.
    pub(crate) body: Value,
}

impl Received {
    /// The value of header `name` (case-insensitive), if it was sent.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// What the provider answers.
#[derive(Debug, Clone)]
pub(crate) struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
}

impl Reply {
    /// A 200 carrying a recorded stream body.
    pub(crate) fn stream(content_type: &'static str, body: &str) -> Self {
        Self {
            status: 200,
            content_type,
            body: body.to_string(),
        }
    }

    /// A 200 JSON body.
    pub(crate) fn json(body: &Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.to_string(),
        }
    }

    /// A rejection: `status` with a JSON error body.
    pub(crate) fn error(status: u16, body: &Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string(),
        }
    }
}

type Handler = dyn Fn(&Received) -> Reply + Send + Sync;

/// The running provider. Dropping it stops accepting connections.
pub(crate) struct MockProvider {
    pub(crate) url: String,
    received: Arc<Mutex<Vec<Received>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockProvider {
    /// Serve `handler` on a fresh loopback port.
    pub(crate) async fn start(
        handler: impl Fn(&Received) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let received = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let log = Arc::clone(&received);
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                let log = Arc::clone(&log);
                tokio::spawn(async move { serve(socket, &*handler, &log).await });
            }
        });
        Self {
            url,
            received,
            task,
        }
    }

    /// Every request so far, in arrival order.
    pub(crate) fn received(&self) -> Vec<Received> {
        self.received.lock().expect("log").clone()
    }

    /// The bodies of every request to a path ending in `suffix`.
    pub(crate) fn bodies_to(&self, suffix: &str) -> Vec<Value> {
        self.received()
            .into_iter()
            .filter(|r| {
                r.path
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .ends_with(suffix)
            })
            .map(|r| r.body)
            .collect()
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(mut socket: TcpStream, handler: &Handler, log: &Mutex<Vec<Received>>) {
    let Some(request) = read_request(&mut socket).await else {
        return;
    };
    log.lock().expect("log").push(request.clone());
    let reply = handler(&request);
    let head = format!(
        "HTTP/1.1 {} MOCK\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(reply.body.as_bytes()).await;
    let _ = socket.shutdown().await;
}

async fn read_request(socket: &mut TcpStream) -> Option<Received> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let path = head.lines().next()?.split_whitespace().nth(1)?.to_string();
    let headers: Vec<(String, String)> = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < header_end + length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = serde_json::from_slice(&buf[header_end..]).unwrap_or(Value::Null);
    Some(Received {
        path,
        headers,
        body,
    })
}
