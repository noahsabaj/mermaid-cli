//! Cross-cutting wrappers over effect handlers.
//!
//! Retry-on-5xx, tracing, rate-limiting — all the concerns that used
//! to be re-implemented per-adapter in the old `src/models/adapters/`
//! tree. In the v0.7 architecture they live here and wrap any effect
//! handler once, uniformly.
//!
//! The retry policy is lifted verbatim from the v0.6 `src/models/
//! retry.rs` (which gets deleted in commit 10). Same classification
//! function, same 500ms→3s exponential backoff, same 3-attempt cap.
//! Moving it to middleware rather than keeping it inline in each
//! adapter means future providers get retry for free; and the test
//! suite doesn't need an in-process HTTP server per adapter.

use std::time::Duration;

use crate::models::{BackendError, ModelError, Result};

/// Total attempts (initial + retries). 3 attempts means up to 2
/// retries on top of the first request, costing at most ~1.5s of
/// extra latency on the worst path (500ms + 1000ms backoff).
pub const DEFAULT_MAX_ATTEMPTS: usize = 3;

const DEFAULT_INITIAL_DELAY_MS: u64 = 500;
const MAX_DELAY_MS: u64 = 3_000;

/// Retry a closure whose output is `Result<reqwest::Response>`
/// whenever the response was a transient upstream failure (5xx / 429
/// / connection failed). Returns the first non-transient response, or
/// the last result after attempts are exhausted.
///
/// The closure takes nothing and must rebuild the request internally
/// because `reqwest::RequestBuilder::send` consumes the builder — so
/// each attempt needs a fresh one.
pub async fn retry_transient_http<F, Fut>(mut build_and_send: F) -> Result<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response>>,
{
    retry_transient_http_with(
        RetryPolicy {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        },
        &mut build_and_send,
    )
    .await
}

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
        let transience = classify(&result);

        if !transience.is_transient() || attempt >= policy.max_attempts {
            if transience.is_transient() {
                tracing::warn!(
                    attempts = attempt,
                    reason = transience.reason(),
                    "middleware: transient upstream failure — retries exhausted"
                );
            }
            return result;
        }

        tracing::warn!(
            attempt,
            max = policy.max_attempts,
            delay_ms,
            reason = transience.reason(),
            "middleware: retrying transient upstream failure"
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transience {
    Success,
    Terminal,
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
            Transience::Retryable(r) => r,
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn fake_response(status: u16) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(body.as_bytes()).await;
            }
        });

        let url = format!("http://{}/x", addr);
        reqwest::get(url).await.expect("send")
    }

    #[tokio::test]
    async fn retries_5xx_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&calls);
        let result = retry_transient_http_with(RetryPolicy { max_attempts: 3 }, &mut move || {
            let c = Arc::clone(&cc);
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
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
    async fn does_not_retry_4xx_client_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&calls);
        let result = retry_transient_http_with(RetryPolicy { max_attempts: 3 }, &mut move || {
            let c = Arc::clone(&cc);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(fake_response(400).await)
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status().as_u16(), 400);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_429() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&calls);
        let result = retry_transient_http_with(RetryPolicy { max_attempts: 2 }, &mut move || {
            let c = Arc::clone(&cc);
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
        let cc = Arc::clone(&calls);
        let result = retry_transient_http_with(RetryPolicy { max_attempts: 3 }, &mut move || {
            let c = Arc::clone(&cc);
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
}
