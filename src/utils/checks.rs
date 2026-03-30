//! Preemptive checks for service availability
//!
//! These checks run BEFORE operations to provide clear, early error messages
//! rather than cryptic failures during execution.

use std::time::Duration;

use super::retry::{RetryConfig, retry_async};

/// Check result with actionable error message
#[derive(Debug)]
#[must_use]
pub struct CheckResult {
    pub available: bool,
    pub message: String,
}

impl CheckResult {
    pub fn ok() -> Self {
        Self {
            available: true,
            message: String::new(),
        }
    }

    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            available: false,
            message: message.into(),
        }
    }
}

/// Check if Ollama is running and responding
///
/// Returns early with a clear message if Ollama isn't available,
/// rather than letting the connection fail later with a timeout.
/// Uses retry logic since Ollama may be starting up.
pub async fn check_ollama_available(host: &str, port: u16) -> CheckResult {
    let url = format!("http://{}:{}/api/tags", host, port);
    let host_owned = host.to_string();
    let port_owned = port;

    // Use a short timeout for the health check - we just want to know if it's running
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return CheckResult::fail(format!("Failed to create HTTP client: {}", e));
        },
    };

    // Retry config: 3 attempts with quick backoff (Ollama might be starting)
    let retry_config = RetryConfig {
        max_attempts: 3,
        initial_delay_ms: 500,
        max_delay_ms: 2000,
        backoff_multiplier: 2.0,
    };

    let result = retry_async(
        || {
            let client = client.clone();
            let url = url.clone();
            async move {
                let response = client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;

                if response.status().is_success() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("HTTP {}", response.status()))
                }
            }
        },
        &retry_config,
    )
    .await;

    match result {
        Ok(()) => CheckResult::ok(),
        Err(e) => {
            let error_str = e.to_string();
            let message = if error_str.contains("Connection refused") {
                format!(
                    "Ollama is not running at {}:{}\n\n\
                    Start Ollama with:\n\
                      ollama serve\n\n\
                    Or if using systemd:\n\
                      systemctl start ollama",
                    host_owned, port_owned
                )
            } else if error_str.contains("timed out") {
                format!(
                    "Ollama at {}:{} is not responding (timed out)\n\n\
                    Ollama may be overloaded or starting up. Check:\n\
                      ollama ps        # See running models\n\
                      ollama list      # See available models",
                    host_owned, port_owned
                )
            } else {
                format!(
                    "Cannot connect to Ollama at {}:{}\n\n\
                    Error: {}\n\n\
                    Make sure Ollama is installed and running:\n\
                      curl -fsSL https://ollama.com/install.sh | sh\n\
                      ollama serve",
                    host_owned, port_owned, error_str
                )
            };
            CheckResult::fail(message)
        },
    }
}

/// Check if a specific model is available in Ollama
pub async fn check_ollama_model(host: &str, port: u16, model_name: &str) -> CheckResult {
    let url = format!("http://{}:{}/api/tags", host, port);
    let model_name_owned = model_name.to_string();

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return CheckResult::fail(format!("Failed to create HTTP client: {}", e));
        },
    };

    // Retry config: 2 attempts (model listing should be quick)
    let retry_config = RetryConfig {
        max_attempts: 2,
        initial_delay_ms: 300,
        max_delay_ms: 1000,
        backoff_multiplier: 2.0,
    };

    let result = retry_async(
        || {
            let client = client.clone();
            let url = url.clone();
            async move {
                let response = client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("Cannot connect to Ollama: {}", e))?;

                if !response.status().is_success() {
                    return Err(anyhow::anyhow!(
                        "Ollama responded with error: {}",
                        response.status()
                    ));
                }

                response
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to parse Ollama response: {}", e))
            }
        },
        &retry_config,
    )
    .await;

    match result {
        Ok(json) => {
            if let Some(models) = json.get("models").and_then(|m| m.as_array()) {
                let model_names: Vec<&str> = models
                    .iter()
                    .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                    .collect();

                // Check for exact match or prefix match (e.g., "llama3" matches "llama3:latest")
                let found = model_names.iter().any(|name| {
                    *name == model_name_owned
                        || name.starts_with(&format!("{}:", model_name_owned))
                        || model_name_owned.starts_with(&format!("{}:", name))
                });

                if found {
                    CheckResult::ok()
                } else {
                    let available = if model_names.is_empty() {
                        "No models installed".to_string()
                    } else {
                        model_names.join(", ")
                    };
                    CheckResult::fail(format!(
                        "Model '{}' not found in Ollama\n\n\
                        Available models: {}\n\n\
                        Pull the model with:\n\
                          ollama pull {}",
                        model_name_owned, available, model_name_owned
                    ))
                }
            } else {
                CheckResult::fail("Invalid response from Ollama: missing models list")
            }
        },
        Err(e) => CheckResult::fail(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_result_ok() {
        let result = CheckResult::ok();
        assert!(result.available);
        assert!(result.message.is_empty());
    }

    #[test]
    fn test_check_result_fail() {
        let result = CheckResult::fail("test error");
        assert!(!result.available);
        assert_eq!(result.message, "test error");
    }

    #[tokio::test]
    async fn test_check_ollama_available_completes() {
        // Just verify the check completes without panicking
        // Ollama may or may not be running during tests
        let result = check_ollama_available("localhost", 11434).await;
        assert!(result.available || !result.message.is_empty());
    }

    #[tokio::test]
    async fn test_check_ollama_error_message_is_helpful() {
        // When Ollama is not running, error message should be helpful
        // Use a port that's definitely not Ollama
        let result = check_ollama_available("localhost", 59999).await;
        if !result.available {
            assert!(
                result.message.contains("ollama serve") || result.message.contains("not running"),
                "Error should include actionable advice"
            );
        }
    }
}
