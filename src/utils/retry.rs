use anyhow::Result;
use std::time::Duration;
use tracing::debug;

/// Retry configuration
pub struct RetryConfig {
    pub max_attempts: usize,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay_ms: 100,
            max_delay_ms: 10_000,
            backoff_multiplier: 2.0,
        }
    }
}

/// Retry an async operation with exponential backoff, retrying on ANY error.
/// For selective retries (skip terminal errors like HTTP 4xx) use
/// [`retry_async_if`].
pub async fn retry_async<F, Fut, T>(operation: F, config: &RetryConfig) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    retry_async_if(operation, config, |_| true).await
}

/// Retry an async operation with exponential backoff, but only while
/// `is_retryable` returns true for the error. A terminal error (e.g. an HTTP
/// 4xx on a non-idempotent POST) is surfaced immediately instead of being
/// retried `max_attempts` times (#85).
pub async fn retry_async_if<F, Fut, T, P>(
    operation: F,
    config: &RetryConfig,
    is_retryable: P,
) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
    P: Fn(&anyhow::Error) -> bool,
{
    let mut attempt = 0;
    let mut delay_ms = config.initial_delay_ms;

    loop {
        attempt += 1;

        match operation().await {
            Ok(result) => return Ok(result),
            // Terminal error, or attempts exhausted: stop retrying.
            Err(e) if attempt >= config.max_attempts || !is_retryable(&e) => {
                if attempt >= config.max_attempts {
                    return Err(anyhow::anyhow!(
                        "Operation failed after {} attempts: {}",
                        config.max_attempts,
                        e
                    ));
                }
                // Non-retryable: surface the original error unwrapped.
                return Err(e);
            },
            Err(e) => {
                debug!(
                    attempt = attempt,
                    max_attempts = config.max_attempts,
                    delay_ms = delay_ms,
                    "Retry attempt failed: {}",
                    e
                );

                // Sleep with jittered exponential backoff (the jitter de-syncs
                // concurrent clients so they don't retry in lockstep).
                tokio::time::sleep(Duration::from_millis(jitter(delay_ms))).await;

                // Calculate next delay
                delay_ms = ((delay_ms as f64) * config.backoff_multiplier) as u64;
                delay_ms = delay_ms.min(config.max_delay_ms);
            },
        }
    }
}

/// Apply ±20% jitter to `delay_ms` using real entropy so concurrent clients —
/// and processes restarting at the same time — don't retry in lockstep (a
/// thundering herd). `pub(crate)` so the effect-layer retry middleware shares
/// this single impl rather than duplicating a weaker clock-based one (#87).
pub(crate) fn jitter(delay_ms: u64) -> u64 {
    let span = delay_ms / 5;
    if span == 0 {
        return delay_ms;
    }
    let mut bytes = [0u8; 8];
    let entropy = match getrandom::fill(&mut bytes) {
        Ok(()) => u64::from_le_bytes(bytes),
        // getrandom shouldn't fail on supported targets; degrade to the
        // unjittered delay rather than panic.
        Err(_) => return delay_ms,
    };
    let offset = entropy % (2 * span + 1);
    delay_ms - span + offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn test_retry_async_success_on_first_try() {
        let config = RetryConfig::default();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = Arc::clone(&call_count);

        let result = retry_async(
            move || {
                let count = Arc::clone(&call_count_clone);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, anyhow::Error>(42)
                }
            },
            &config,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_async_success_on_second_try() {
        let config = RetryConfig {
            max_attempts: 3,
            initial_delay_ms: 10,
            ..Default::default()
        };
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = Arc::clone(&call_count);

        let result = retry_async(
            move || {
                let count = Arc::clone(&call_count_clone);
                async move {
                    let current = count.fetch_add(1, Ordering::SeqCst) + 1;
                    if current < 2 {
                        Err(anyhow::anyhow!("Temporary error"))
                    } else {
                        Ok(42)
                    }
                }
            },
            &config,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_retry_async_fails_after_max_attempts() {
        let config = RetryConfig {
            max_attempts: 3,
            initial_delay_ms: 10,
            ..Default::default()
        };
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = Arc::clone(&call_count);

        let result = retry_async(
            move || {
                let count = Arc::clone(&call_count_clone);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err::<i32, _>(anyhow::anyhow!("Persistent error"))
                }
            },
            &config,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_async_if_skips_nonretryable_errors() {
        // #85: a non-retryable error returns immediately (one attempt), not
        // after max_attempts.
        let config = RetryConfig {
            max_attempts: 5,
            initial_delay_ms: 1,
            ..Default::default()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&calls);
        let result: Result<i32> = retry_async_if(
            move || {
                let c = Arc::clone(&cc);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::anyhow!("terminal"))
                }
            },
            &config,
            |_| false,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn jitter_stays_within_band() {
        // ±20% band: jitter(1000) ∈ [800, 1200].
        for _ in 0..100 {
            let j = jitter(1000);
            assert!((800..=1200).contains(&j), "jitter out of band: {j}");
        }
        // Tiny delays (span 0) pass through unchanged.
        assert_eq!(jitter(3), 3);
    }
}
