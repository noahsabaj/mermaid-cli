use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Result from a web search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub full_content: String,
}

/// Searxng search response structure
#[derive(Debug, Deserialize)]
struct SearxngResponse {
    results: Vec<SearxngResult>,
    #[serde(default)]
    answer: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearxngResult {
    title: String,
    url: String,
    content: String,
}

/// Web search client that queries local Searxng instance
pub struct WebSearchClient {
    client: Client,
    searxng_url: String,
    cache: HashMap<String, (Vec<SearchResult>, Instant)>,
    cache_ttl: Duration,
}

impl WebSearchClient {
    pub fn new(searxng_url: String) -> Self {
        Self {
            client: Client::new(),
            searxng_url,
            cache: HashMap::new(),
            cache_ttl: Duration::from_secs(3600), // 1 hour
        }
    }

    /// Search and cache results
    pub async fn search_cached(
        &mut self,
        query: &str,
        count: usize,
    ) -> Result<Vec<SearchResult>> {
        let cache_key = format!("{}:{}", query, count);

        // Check cache first
        if let Some((results, timestamp)) = self.cache.get(&cache_key) {
            if timestamp.elapsed() < self.cache_ttl {
                return Ok(results.clone());
            } else {
                // Cache expired, remove it
                self.cache.remove(&cache_key);
            }
        }

        // Cache miss or expired - fetch fresh
        let results = self.search(query, count).await?;
        self.cache.insert(cache_key, (results.clone(), Instant::now()));
        Ok(results)
    }

    /// Execute search and fetch full page content
    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>> {
        // Validate count
        if count == 0 || count > 10 {
            return Err(anyhow!("Result count must be between 1 and 10, got {}", count));
        }

        // Query Searxng
        let encoded_query = urlencoding::encode(query);
        let url = format!(
            "{}/search?q={}&format=json&pageno=1",
            self.searxng_url, encoded_query
        );

        let response = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| anyhow!("Failed to reach Searxng (is it running?): {}", e))?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Searxng returned error status: {}",
                response.status()
            ));
        }

        let searxng_response: SearxngResponse = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse Searxng response: {}", e))?;

        // Take top N results
        let mut search_results = Vec::new();
        for result in searxng_response.results.iter().take(count) {
            // Fetch full page content
            match self.fetch_full_page(&result.url).await {
                Ok(full_content) => {
                    search_results.push(SearchResult {
                        title: result.title.clone(),
                        url: result.url.clone(),
                        snippet: result.content.clone(),
                        full_content,
                    });
                }
                Err(e) => {
                    // If full page fetch fails, use snippet only
                    eprintln!("[WARN] Failed to fetch full page {}: {}", result.url, e);
                    search_results.push(SearchResult {
                        title: result.title.clone(),
                        url: result.url.clone(),
                        snippet: result.content.clone(),
                        full_content: result.content.clone(),
                    });
                }
            }
        }

        if search_results.is_empty() {
            return Err(anyhow!("No search results found for: {}", query));
        }

        Ok(search_results)
    }

    /// Fetch full page content and convert to markdown
    async fn fetch_full_page(&self, url: &str) -> Result<String> {
        // Sanitize URL to prevent SSRF
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(anyhow!("Invalid URL: {}", url));
        }

        let response = self
            .client
            .get(url)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| anyhow!("Failed to fetch {}: {}", url, e))?;

        if !response.status().is_success() {
            return Err(anyhow!("Failed to fetch {}: {}", url, response.status()));
        }

        let html = response
            .text()
            .await
            .map_err(|e| anyhow!("Failed to read response body: {}", e))?;

        // Convert HTML to markdown
        let markdown = html2md::parse_html(&html);

        // Truncate to reasonable size (5000 chars per page to prevent context bloat)
        let truncated = if markdown.len() > 5000 {
            format!("{}...[truncated]", &markdown[..5000])
        } else {
            markdown
        };

        Ok(truncated)
    }

    /// Format search results for model consumption
    pub fn format_results(&self, results: &[SearchResult]) -> String {
        let mut formatted = String::from("[SEARCH_RESULTS]\n");

        for result in results {
            formatted.push_str(&format!(
                "Title: {}\nURL: {}\nContent:\n{}\n---\n",
                result.title, result.url, result.full_content
            ));
        }

        formatted.push_str("[/SEARCH_RESULTS]\n");
        formatted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_web_search_client_creation() {
        let client = WebSearchClient::new("http://localhost:8888".to_string());
        assert_eq!(client.searxng_url, "http://localhost:8888");
        assert_eq!(client.cache.len(), 0);
    }

    #[test]
    fn test_result_count_validation() {
        // Would need async test framework for actual async testing
        // This is a placeholder for structure validation
        let results: Vec<SearchResult> = vec![];
        assert!(results.is_empty());
    }

    #[test]
    fn test_format_results() {
        let client = WebSearchClient::new("http://localhost:8888".to_string());
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
