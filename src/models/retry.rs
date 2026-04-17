//! Transient-failure retry policy for provider HTTP calls.
//!
//! All four adapters (Ollama, OpenAI-compat, Anthropic, Gemini) POST to
//! an upstream /chat-like endpoint and occasionally get 5xx responses,
//! 429 rate-limits, or connect-level failures from the provider — see
//! the real-world Ollama Cloud 500 that motivated this module
//! (`{"error":"Internal Server Error (ref: ...)"}` mid-agent-loop). The
//! `UserFacingError.recoverable` flag has long advertised that these
//! were safe to retry, but nothing actually retried.
//!
//! This module wraps the initial request in a short retry loop:
//! - **Retry on** response status 429 or 5xx, or `BackendError::
//!   ConnectionFailed` (reqwest connect/DNS/TLS failures).
//! - **Do not retry** 4xx other than 429 (caller error), 2xx/3xx
//!   (success / redirect), `ParseError`, `Authentication`, or any error
//!   classification that isn't transient.
//!
//! The closure is invoked fresh on each attempt because
//! `reqwest::RequestBuilder::send` consumes `self`. Streaming is safe
//! to retry here because we only check `response.status()` before the
//! body is consumed — if the status is 2xx the response is returned
//! to the adapter which then starts streaming as usual. Mid-stream
//! failures become `StreamError` and are NOT retried (partial content
//! has already reached the caller).

use std::time::Duration;

use super::error::{BackendError, ModelError, Result};

/// Total attempts (initial + retries). 3 attempts means up to 2 retries
/// on top of the first request, which costs at most ~1.5s of extra
/// latency on the worst path (500ms + 1000ms backoff). Keeps the
/// user-visible wait bounded.
pub const DEFAULT_MAX_ATTEMPTS: usize = 3;

/// Initial backoff before the first retry. Doubles each attempt,
/// capped at `MAX_DELAY_MS`.
const DEFAULT_INITIAL_DELAY_MS: u64 = 500;

/// Upper bound on per-attempt backoff so a burst of retries doesn't
/// wedge the request for more than a few seconds.
const MAX_DELAY_MS: u64 = 3_000;

/// Retry a build+send closure on transient upstream failures. Returns
/// the first non-transient response (success OR terminal non-retryable
/// failure), or the last result after `max_attempts` is exhausted.
///
/// The build step is inside the closure because `reqwest::RequestBuilder
/// ::send` consumes the builder — each attempt needs a fresh one. The
/// closure does NOT need to clone the request body itself; callers
/// typically reference the body from their adapter state.
pub async fn retry_transient_http<F, Fut>(mut build_and_send: F) -> Result<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response>>,
{
    retry_transient_http_with(
        build_and_send_attempts(DEFAULT_MAX_ATTEMPTS),
        &mut build_and_send,
    )
    .await
}

/// Internal wrapper so `retry_transient_http` can be called with the
/// default policy, while tests can call `retry_transient_http_with` and
/// override the attempt count. Keeps the public surface minimal.
async fn retry_transient_http_with<F, Fut>(
    policy: RetryPolicy,
    build_and_send: &mut F,
) -> Result<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response>>,
{
    let mut attempt: usize = 1;
    let mut delay_ms = DEFAULT_INITIAL_DELAY_MS;

    loop {
        let result = build_and_send().await;
        let transient = classify(&result);

        if !transient.is_transient() || attempt >= policy.max_attempts {
            if transient.is_transient() {
                tracing::warn!(
                    attempts = attempt,
                    reason = transient.reason(),
                    "transient upstream failure — retries exhausted"
                );
            }
            return result;
        }

        tracing::warn!(
            attempt,
            max = policy.max_attempts,
            delay_ms,
            reason = transient.reason(),
            "retrying transient upstream failure"
        );
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        attempt += 1;
        delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);
    }
}

#[derive(Debug, Clone, Copy)]
struct RetryPolicy {
    max_attempts: usize,
}

fn build_and_send_attempts(max_attempts: usize) -> RetryPolicy {
    RetryPolicy { max_attempts }
}

/// Classification of a single attempt's outcome for retry decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transience {
    /// 2xx/3xx — return to caller.
    Success,
    /// 4xx other than 429, parse errors, auth errors — return to caller,
    /// no retry.
    Terminal,
    /// 5xx, 429, or connection failure — safe to retry.
    Retryable(&'static str),
}

impl Transience {
    fn is_transient(self) -> bool {
        matches!(self, Transience::Retryable(_))
    }

    fn reason(self) -> &'static str {
        match self {
            Transience::Success => "success",
            Transience::Terminal => "terminal",
            Transience::Retryable(reason) => reason,
        }
    }
}

fn classify(result: &Result<reqwest::Response>) -> Transience {
    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == 429 {
                Transience::Retryable("http_429")
            } else if (500..=599).contains(&status) {
                Transience::Retryable("http_5xx")
            } else {
                Transience::Success
            }
        },
        Err(ModelError::Backend(BackendError::ConnectionFailed { .. })) => {
            Transience::Retryable("connection_failed")
        },
        Err(_) => Transience::Terminal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Build a fake response with an arbitrary status code. `reqwest`
    /// doesn't expose a public constructor for `Response`, so we mock by
    /// driving a tiny in-process HTTP server. Keeping the tests hermetic
    /// this way — we don't talk to the internet.
    async fn fake_response(status: u16) -> reqwest::Response {
        use std::net::SocketAddr;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("local_addr");

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                // Drain the request line + headers so the client side
                // doesn't EPIPE on us.
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = format!(
                    "HTTP/1.1 {status} X\r\n\
                     Content-Length: 0\r\n\
                     Connection: close\r\n\r\n"
                );
                let _ = sock.write_all(body.as_bytes()).await;
            }
        });

        let url = format!("http://{}/x", addr);
        reqwest::get(url).await.expect("send")
    }

    #[tokio::test]
    async fn success_returns_on_first_attempt() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let result = retry_transient_http(move || {
            let c = Arc::clone(&calls_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(fake_response(200).await)
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status().as_u16(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_5xx_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let result = retry_transient_http_with(RetryPolicy { max_attempts: 3 }, &mut move || {
            let c = Arc::clone(&calls_clone);
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                // First two attempts return 500, third returns 200.
                let status = if n < 2 { 500 } else { 200 };
                Ok(fake_response(status).await)
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status().as_u16(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn exhausts_retries_on_persistent_5xx() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let result = retry_transient_http_with(RetryPolicy { max_attempts: 3 }, &mut move || {
            let c = Arc::clone(&calls_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(fake_response(500).await)
            }
        })
        .await;
        // Final response is returned as Ok(_) with 500 — the adapter
        // wraps it into BackendError::HttpError downstream. Retries
        // exhausted, not an Err here.
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status().as_u16(), 500);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_4xx_client_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let result = retry_transient_http_with(RetryPolicy { max_attempts: 3 }, &mut move || {
            let c = Arc::clone(&calls_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(fake_response(400).await)
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status().as_u16(), 400);
        // 400 is terminal — only the initial attempt runs.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_429_rate_limit() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let result = retry_transient_http_with(RetryPolicy { max_attempts: 2 }, &mut move || {
            let c = Arc::clone(&calls_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(fake_response(429).await)
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status().as_u16(), 429);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retries_connection_failed_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let result = retry_transient_http_with(RetryPolicy { max_attempts: 3 }, &mut move || {
            let c = Arc::clone(&calls_clone);
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(ModelError::Backend(BackendError::ConnectionFailed {
                        backend: "test".to_string(),
                        url: "http://nope".to_string(),
                        reason: "dns".to_string(),
                    }))
                } else {
                    Ok(fake_response(200).await)
                }
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_parse_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let result = retry_transient_http_with(RetryPolicy { max_attempts: 3 }, &mut move || {
            let c = Arc::clone(&calls_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(ModelError::ParseError {
                    message: "bad json".to_string(),
                    raw: None,
                })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
