//! Cross-cutting wrappers over effect handlers.
//!
//! Retry-on-5xx, tracing, rate-limiting — all concerns that would
//! otherwise be re-implemented per-adapter. Living here means any
//! new effect handler picks them up uniformly; 500ms→3s exponential
//! backoff, 3-attempt cap, same classification function for every
//! provider.

use std::time::Duration;

use crate::models::{BackendError, ModelError, Result};

/// Total attempts (initial + retries). 3 attempts means up to 2
/// retries on top of the first request, costing at most ~1.5s of
/// extra latency on the worst path (500ms + 1000ms backoff).
pub const DEFAULT_MAX_ATTEMPTS: usize = 3;

const DEFAULT_INITIAL_DELAY_MS: u64 = 500;
const MAX_DELAY_MS: u64 = 3_000;
/// Upper bound on how long we'll wait, even if a server's `Retry-After` asks
/// for more — a hostile or misconfigured value mustn't hang the turn.
const MAX_RETRY_AFTER_MS: u64 = 60_000;

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

        // A server-provided `Retry-After` is the authoritative wait for ANY
        // retryable status — a 503 (or other 5xx) can carry it just like a 429,
        // and must be honored rather than retried sooner under our own backoff
        // (F26). Read it while we still hold the response; a connection-failure
        // error carries no response, so `.ok()` yields `None` and we fall back
        // to the jittered backoff below.
        let retry_after_ms = if transience.is_transient() {
            result
                .as_ref()
                .ok()
                .and_then(|r| parse_retry_after_ms(r.headers()))
        } else {
            None
        };

        if !transience.is_transient() || attempt >= policy.max_attempts {
            if transience.is_transient() {
                tracing::warn!(
                    attempts = attempt,
                    reason = transience.reason(),
                    "middleware: transient upstream failure — retries exhausted"
                );
                // A persistent 429 becomes a typed `RateLimit` so the UI can show
                // a rate-limit affordance instead of a generic HTTP error.
                if transience.reason() == "http_429" && result.is_ok() {
                    return Err(ModelError::RateLimit {
                        retry_after: retry_after_ms.map(|ms| ms / 1000),
                    });
                }
            }
            return result;
        }

        // Honor `Retry-After` when present (capped so a hostile/huge value can't
        // hang the turn); otherwise a jittered exponential backoff that avoids
        // synchronized retries across concurrent clients.
        let sleep_ms = crate::utils::jitter(delay_ms)
            .max(retry_after_ms.unwrap_or(0))
            .min(MAX_RETRY_AFTER_MS);
        tracing::warn!(
            attempt,
            max = policy.max_attempts,
            sleep_ms,
            reason = transience.reason(),
            "middleware: retrying transient upstream failure"
        );
        // No explicit cancel race here (#42): this retry only runs inside a
        // provider's `chat`, which the model wrapper drives under
        // `select! { ctx.token.cancelled() => …, chat_fut => … }` (the
        // model/mod.rs cancellation invariant). A cancel drops `chat_fut`, and
        // with it this in-flight sleep, so the backoff is already promptly
        // cancellable — threading a token down the Model trait would add surface
        // for no behavioral gain.
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        attempt += 1;
        delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);
    }
}

/// Parse a `Retry-After` header into milliseconds. Handles the integer
/// delta-seconds form (what OpenAI / Anthropic send); the rare HTTP-date form
/// falls through to `None` and we use the backoff instead.
fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    raw.trim()
        .parse::<u64>()
        .ok()
        .map(|secs| secs.saturating_mul(1000))
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

    /// Like `fake_response`, but the response carries a `Retry-After` header
    /// (delta-seconds form) so we can exercise the F26 5xx + Retry-After path.
    async fn fake_response_with_retry_after(status: u16, retry_after_secs: u64) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = format!(
                    "HTTP/1.1 {status} X\r\nRetry-After: {retry_after_secs}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(body.as_bytes()).await;
            }
        });

        let url = format!("http://{}/x", addr);
        reqwest::get(url).await.expect("send")
    }

    #[test]
    fn parse_retry_after_handles_integer_seconds_and_ignores_dates() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(parse_retry_after_ms(&headers), Some(2_000));
        // The rare HTTP-date form is not parsed (delta-seconds only) → None.
        let mut dated = HeaderMap::new();
        dated.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after_ms(&dated), None);
        // Absent header → None.
        assert_eq!(parse_retry_after_ms(&HeaderMap::new()), None);
    }

    #[tokio::test]
    async fn honors_retry_after_on_503() {
        // F26: a 503 carrying `Retry-After` must drive the wait, not the
        // (shorter) jittered exponential backoff. `Retry-After: 1` ⇒ the retry
        // sleeps ~1000ms; the attempt-1 backoff alone is jitter(500) ∈
        // [400,600]ms, so an elapsed ≥ 850ms proves the header was honored.
        let calls = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&calls);
        let start = std::time::Instant::now();
        let result = retry_transient_http_with(RetryPolicy { max_attempts: 2 }, &mut move || {
            let c = Arc::clone(&cc);
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Ok(fake_response_with_retry_after(503, 1).await)
                } else {
                    Ok(fake_response(200).await)
                }
            }
        })
        .await;
        let elapsed = start.elapsed();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status().as_u16(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            elapsed >= Duration::from_millis(850),
            "expected Retry-After (1s) to drive the 503 wait, waited only {:?}",
            elapsed
        );
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
    async fn retries_429_then_surfaces_rate_limit() {
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
        // A persistent 429 retries, then surfaces as a typed RateLimit (#2).
        assert!(matches!(result, Err(ModelError::RateLimit { .. })));
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
