//! A scripted OpenAI-compatible HTTP endpoint for the offline eval tier.
//!
//! [`super::super::stub_model`] stands in at the `ModelProvider` seam, inside
//! the test process. The evals need the seam one layer further out: they drive
//! the real `mermaid` binary, so the model has to be something that binary can
//! reach over the wire. This is that: a `/chat/completions` endpoint on
//! loopback that answers each call with the next scripted turn, streamed as
//! Chat Completions SSE. Everything from the HTTP client inwards (the real
//! adapter, reducer, effect runner, tools) is the production path.
//!
//! It accepts every request parameter and honours none of them, which is also
//! exactly the quirk the `ignored-parameter` eval exists to cover: many
//! OpenAI-compatible and local servers take a field they do not support and
//! silently drop it rather than returning a 400. Every request body is kept,
//! so a check can confirm the parameter really was sent.
//!
//! Running off the end of the script answers 400 rather than hanging, and is
//! recorded: it means the run took a path the reference did not describe.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};

/// One model call's worth of scripted output.
#[derive(Debug, Clone)]
pub enum MockTurn {
    /// Ask for these tool calls, in order, in one turn.
    Tools(Vec<(String, Value)>),
    /// Answer with this text and stop.
    Say(String),
}

#[derive(Default)]
struct Shared {
    script: VecDeque<MockTurn>,
    /// Every `/chat/completions` body received, oldest first.
    requests: Vec<Value>,
    /// Calls that arrived after the script ran out.
    overruns: usize,
}

/// A running mock endpoint. Stops accepting when dropped.
pub struct MockProvider {
    port: u16,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
}

impl MockProvider {
    /// Bind a loopback port and serve `script` from a background thread.
    ///
    /// # Panics
    ///
    /// When no loopback port can be bound.
    #[must_use]
    pub fn start(script: Vec<MockTurn>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the mock provider");
        let port = listener.local_addr().expect("mock provider address").port();
        let shared = Arc::new(Mutex::new(Shared {
            script: script.into(),
            ..Shared::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_shared, thread_stop) = (Arc::clone(&shared), Arc::clone(&stop));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let shared = Arc::clone(&thread_shared);
                std::thread::spawn(move || {
                    let _ = serve(stream, &shared);
                });
            }
        });
        Self { port, shared, stop }
    }

    /// The `base_url` a `[providers.*]` entry points at.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// Every chat request body the endpoint received, oldest first.
    #[must_use]
    pub fn requests(&self) -> Vec<Value> {
        self.lock().requests.clone()
    }

    /// Scripted turns never asked for.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.lock().script.len()
    }

    /// Calls that arrived after the script ran out.
    #[must_use]
    pub fn overruns(&self) -> usize {
        self.lock().overruns
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking `accept` so the thread sees the flag.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// Answer one HTTP/1.1 request, then close the connection.
fn serve(stream: TcpStream, shared: &Mutex<Shared>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;

    let mut out = stream;
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    if path.ends_with("/models") {
        // Limit discovery. An empty listing is the "provider exposes nothing"
        // case, which Mermaid handles with its static fallback.
        return respond_json(&mut out, 200, &json!({ "object": "list", "data": [] }));
    }
    if !path.ends_with("/chat/completions") {
        return respond_json(
            &mut out,
            404,
            &json!({ "error": { "message": "not found" } }),
        );
    }

    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let streaming = request["stream"].as_bool().unwrap_or(false);
    let (turn, call) = {
        let mut shared = shared.lock().unwrap_or_else(PoisonError::into_inner);
        shared.requests.push(request);
        let call = shared.requests.len();
        let turn = shared.script.pop_front();
        if turn.is_none() {
            shared.overruns += 1;
        }
        drop(shared);
        (turn, call)
    };
    let Some(turn) = turn else {
        // 400, not 5xx: the adapter retries server errors, and a retry here
        // would only find the script just as empty.
        return respond_json(
            &mut out,
            400,
            &json!({ "error": { "message": format!(
                "eval mock provider: the reference script has no turn for call {call}"
            ) } }),
        );
    };
    if streaming {
        respond_sse(&mut out, &turn, call)
    } else {
        respond_json(&mut out, 200, &completion(&turn, call))
    }
}

fn respond_json(out: &mut TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    let body = body.to_string();
    let reason = if status == 200 { "OK" } else { "Error" };
    write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    out.flush()
}

/// Stream `turn` as Chat Completions chunks. No `Content-Length`: the body ends
/// when the connection closes, which `Connection: close` announces.
fn respond_sse(out: &mut TcpStream, turn: &MockTurn, call: usize) -> std::io::Result<()> {
    write!(
        out,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
    )?;
    let mut frames = Vec::new();
    let finish = match turn {
        MockTurn::Say(text) => {
            // Several chunks, so the adapter's accumulation is exercised.
            for piece in text.split_inclusive(' ') {
                frames.push(json!({ "choices": [{ "index": 0, "delta": { "content": piece } }] }));
            }
            "stop"
        },
        MockTurn::Tools(calls) => {
            for (index, (name, args)) in calls.iter().enumerate() {
                frames.push(
                    json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                    "index": index,
                    "id": format!("call_{call}_{index}"),
                    "type": "function",
                    "function": { "name": name, "arguments": args.to_string() },
                }] } }] }),
                );
            }
            "tool_calls"
        },
    };
    frames.push(json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": finish }] }));
    frames.push(json!({ "choices": [], "usage": usage() }));
    for frame in frames {
        write!(out, "data: {frame}\n\n")?;
    }
    write!(out, "data: [DONE]\n\n")?;
    out.flush()
}

/// The same turn as a non-streaming completion.
fn completion(turn: &MockTurn, call: usize) -> Value {
    let (message, finish) = match turn {
        MockTurn::Say(text) => (json!({ "role": "assistant", "content": text }), "stop"),
        MockTurn::Tools(calls) => {
            let calls: Vec<Value> = calls
                .iter()
                .enumerate()
                .map(|(index, (name, args))| {
                    json!({
                        "id": format!("call_{call}_{index}"),
                        "type": "function",
                        "function": { "name": name, "arguments": args.to_string() },
                    })
                })
                .collect();
            (
                json!({ "role": "assistant", "content": null, "tool_calls": calls }),
                "tool_calls",
            )
        },
    };
    json!({
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": usage(),
    })
}

/// Fixed, small usage numbers: the evals report tokens, and a scripted run
/// should report the same count every time.
fn usage() -> Value {
    json!({ "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120 })
}
