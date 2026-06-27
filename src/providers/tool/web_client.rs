use crate::utils::{RetryConfig, retry_async};
use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Result from a web search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub full_content: String,
}

/// Result from a web fetch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchResult {
    pub title: String,
    pub content: String,
}

/// Ollama web search API response
#[derive(Debug, Deserialize)]
struct OllamaSearchResponse {
    results: Vec<OllamaSearchResult>,
}

#[derive(Debug, Deserialize)]
struct OllamaSearchResult {
    title: String,
    url: String,
    content: String,
}

/// Ollama web fetch API response
#[derive(Debug, Deserialize)]
struct OllamaFetchResponse {
    title: Option<String>,
    content: Option<String>,
}

const OLLAMA_API_BASE: &str = "https://ollama.com/api";

/// Web search client that uses Ollama's cloud API
#[derive(Clone)]
pub struct WebSearchClient {
    client: Client,
    api_key: String,
}

impl WebSearchClient {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
        }
    }

    /// Execute a search query
    pub async fn search_query(&self, query: &str, count: usize) -> Result<Vec<SearchResult>> {
        self.search(query, count).await
    }

    /// Execute search via Ollama Cloud API
    ///
    /// The web_search API already returns full page content per result,
    /// so no separate web_fetch calls are needed. Each result's content
    /// is truncated to prevent context bloat.
    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>> {
        // Validate count
        if count == 0 || count > 10 {
            return Err(anyhow!(
                "Result count must be between 1 and 10, got {}",
                count
            ));
        }

        // Query Ollama web search API with retry logic
        let retry_config = RetryConfig {
            max_attempts: 3,
            initial_delay_ms: 500,
            max_delay_ms: 5000,
            backoff_multiplier: 2.0,
        };

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let query_owned = query.to_string();
        // `count` is Copy (usize) — safe to capture by value across retries
        let ollama_response: OllamaSearchResponse = retry_async(
            || {
                let client = client.clone();
                let api_key = api_key.clone();
                let query = query_owned.clone();
                async move {
                    let response = client
                        .post(format!("{}/web_search", OLLAMA_API_BASE))
                        .header("Authorization", format!("Bearer {}", api_key))
                        .json(&serde_json::json!({
                            "query": query,
                            "max_results": count,
                        }))
                        .timeout(Duration::from_secs(30))
                        .send()
                        .await
                        .map_err(|e| anyhow!("Failed to reach Ollama web search API: {}", e))?;

                    if !response.status().is_success() {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        return Err(anyhow!(
                            "Ollama web search API returned error {}: {}",
                            status,
                            body
                        ));
                    }

                    let body =
                        read_body_capped(response, crate::constants::MAX_WEB_BODY_BYTES).await?;
                    serde_json::from_slice::<OllamaSearchResponse>(&body)
                        .map_err(|e| anyhow!("Failed to parse Ollama search response: {}", e))
                }
            },
            &retry_config,
        )
        .await?;

        // The web_search API returns full page content in each result's content field.
        // Truncate each to prevent context bloat.
        let search_results: Vec<SearchResult> = ollama_response
            .results
            .iter()
            .take(count)
            .map(|result| {
                let content = crate::utils::truncate_content(
                    &result.content,
                    crate::constants::WEB_CONTENT_MAX_CHARS,
                );
                SearchResult {
                    title: result.title.clone(),
                    url: result.url.clone(),
                    snippet: result.content.chars().take(200).collect(),
                    full_content: content,
                }
            })
            .collect();

        if search_results.is_empty() {
            return Err(anyhow!("No search results found for: {}", query));
        }

        Ok(search_results)
    }

    /// Fetch a URL's content via Ollama's web_fetch API
    pub async fn fetch_url(&self, url: &str) -> Result<WebFetchResult> {
        // Retry config for page fetches (2 attempts, shorter timeout)
        let retry_config = RetryConfig {
            max_attempts: 2,
            initial_delay_ms: 200,
            max_delay_ms: 2000,
            backoff_multiplier: 2.0,
        };

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let url_owned = url.to_string();
        let response: OllamaFetchResponse = retry_async(
            || {
                let client = client.clone();
                let api_key = api_key.clone();
                let url = url_owned.clone();
                async move {
                    let response = client
                        .post(format!("{}/web_fetch", OLLAMA_API_BASE))
                        .header("Authorization", format!("Bearer {}", api_key))
                        .json(&serde_json::json!({ "url": url }))
                        .timeout(Duration::from_secs(15))
                        .send()
                        .await
                        .map_err(|e| anyhow!("Failed to fetch {}: {}", url, e))?;

                    if !response.status().is_success() {
                        let status = response.status();
                        return Err(anyhow!("Failed to fetch {}: HTTP {}", url, status));
                    }

                    let body =
                        read_body_capped(response, crate::constants::MAX_WEB_BODY_BYTES).await?;
                    serde_json::from_slice::<OllamaFetchResponse>(&body)
                        .map_err(|e| anyhow!("Failed to parse fetch response: {}", e))
                }
            },
            &retry_config,
        )
        .await?;

        Ok(WebFetchResult {
            title: response.title.unwrap_or_default(),
            content: response.content.unwrap_or_default(),
        })
    }

    /// Format search results for model consumption
    ///
    /// Pure data -- no behavioral instructions. Citation rules live in the
    /// system prompt (src/prompts.rs), which is the SSOT for all model behavior.
    pub fn format_results(&self, results: &[SearchResult]) -> String {
        let mut formatted = String::from("[SEARCH_RESULTS]\n");

        for (i, result) in results.iter().enumerate() {
            formatted.push_str(&format!(
                "[{}] Title: {}\nURL: {}\nContent:\n{}\n---\n",
                i + 1,
                result.title,
                result.url,
                result.full_content
            ));
        }

        formatted.push_str("[/SEARCH_RESULTS]\n\n");

        // Source list for citation (behavior governed by system prompt)
        formatted.push_str("Sources:\n");
        for (i, result) in results.iter().enumerate() {
            formatted.push_str(&format!("{}. {} - {}\n", i + 1, result.title, result.url));
        }

        formatted
    }
}

/// Read a reqwest response body, refusing to buffer more than `max_bytes`.
/// `Response::json`/`bytes` buffer the whole body unbounded; a compromised or
/// misconfigured Ollama endpoint could return a multi-gigabyte body and OOM the
/// (long-lived) process. We reject early on an oversized `Content-Length` and
/// also enforce the cap while streaming (a lying or absent header can't bypass
/// it) (#28).
async fn read_body_capped(response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    use futures::StreamExt;
    if let Some(len) = response.content_length()
        && len as usize > max_bytes
    {
        return Err(anyhow!(
            "response body too large: {len} bytes exceeds {max_bytes} cap"
        ));
    }
    let mut stream = response.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow!("error reading response body: {e}"))?;
        if buf.len() + chunk.len() > max_bytes {
            return Err(anyhow!("response body exceeded {max_bytes} byte cap"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_web_search_client_creation() {
        let client = WebSearchClient::new("test-key".to_string());
        assert_eq!(client.api_key, "test-key");
    }

    #[test]
    fn test_format_results() {
        let client = WebSearchClient::new("test-key".to_string());
        let results = vec![SearchResult {
            title: "Test Article".to_string(),
            url: "https://example.com".to_string(),
            snippet: "This is a test".to_string(),
            full_content: "Full content here".to_string(),
        }];

        let formatted = client.format_results(&results);
        assert!(formatted.contains("[SEARCH_RESULTS]"));
        assert!(formatted.contains("Test Article"));
        assert!(formatted.contains("https://example.com"));
        assert!(formatted.contains("[/SEARCH_RESULTS]"));
    }
}
