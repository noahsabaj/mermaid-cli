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

/// Retry an async operation with exponential backoff
pub async fn retry_async<F, Fut, T>(operation: F, config: &RetryConfig) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0;
    let mut delay_ms = config.initial_delay_ms;

    loop {
        attempt += 1;

        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) if attempt >= config.max_attempts => {
                return Err(anyhow::anyhow!(
                    "Operation failed after {} attempts: {}",
                    config.max_attempts,
                    e
                ));
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

/// Apply ±20% jitter to `delay_ms` from a cheap time source (no rng dependency)
/// so concurrent clients don't retry in lockstep (thundering herd).
fn jitter(delay_ms: u64) -> u64 {
    let span = delay_ms / 5;
    if span == 0 {
        return delay_ms;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let offset = nanos % (2 * span + 1);
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
}
