use anyhow::Result;
use std::time::Duration;

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
pub async fn retry_async<F, Fut, T>(
    operation: F,
    config: &RetryConfig,
) -> Result<T>
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
            }
            Err(e) => {
                eprintln!(
                    "[RETRY] Attempt {}/{} failed: {}. Retrying in {}ms...",
                    attempt, config.max_attempts, e, delay_ms
                );

                // Sleep with exponential backoff
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;

                // Calculate next delay
                delay_ms = ((delay_ms as f64) * config.backoff_multiplier) as u64;
                delay_ms = delay_ms.min(config.max_delay_ms);
            }
        }
    }
}

/// Retry a synchronous operation with exponential backoff
pub fn retry_sync<F, T>(operation: F, config: &RetryConfig) -> Result<T>
where
    F: Fn() -> Result<T>,
{
    let mut attempt = 0;
    let mut delay_ms = config.initial_delay_ms;

    loop {
        attempt += 1;

        match operation() {
            Ok(result) => return Ok(result),
            Err(e) if attempt >= config.max_attempts => {
                return Err(anyhow::anyhow!(
                    "Operation failed after {} attempts: {}",
                    config.max_attempts,
                    e
                ));
            }
            Err(e) => {
                eprintln!(
                    "[RETRY] Attempt {}/{} failed: {}. Retrying in {}ms...",
                    attempt, config.max_attempts, e, delay_ms
                );

                // Sleep with exponential backoff
                std::thread::sleep(Duration::from_millis(delay_ms));

                // Calculate next delay
                delay_ms = ((delay_ms as f64) * config.backoff_multiplier) as u64;
                delay_ms = delay_ms.min(config.max_delay_ms);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_retry_async_success_on_first_try() {
        let config = RetryConfig::default();
        let mut call_count = 0;

        let result = retry_async(
            || async {
                call_count += 1;
                Ok::<_, anyhow::Error>(42)
            },
            &config,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count, 1);
    }

    #[tokio::test]
    async fn test_retry_async_success_on_second_try() {
        let config = RetryConfig {
            max_attempts: 3,
            initial_delay_ms: 10,
            ..Default::default()
        };
        let mut call_count = 0;

        let result = retry_async(
            || async {
                call_count += 1;
                if call_count < 2 {
                    Err(anyhow::anyhow!("Temporary error"))
                } else {
                    Ok(42)
                }
            },
            &config,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count, 2);
    }

    #[tokio::test]
    async fn test_retry_async_fails_after_max_attempts() {
        let config = RetryConfig {
            max_attempts: 3,
            initial_delay_ms: 10,
            ..Default::default()
        };
        let mut call_count = 0;

        let result = retry_async(
            || async {
                call_count += 1;
                Err::<i32, _>(anyhow::anyhow!("Persistent error"))
            },
            &config,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(call_count, 3);
    }
}