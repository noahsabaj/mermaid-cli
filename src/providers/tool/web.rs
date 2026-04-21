//! Web tools: `web_search` and `web_fetch`.
//!
//! Both delegate to `web_client::WebSearchClient` — a thin HTTP
//! client for Ollama Cloud's web API (bearer-token path, via
//! `OLLAMA_API_KEY`). The wrapper's job is cancellation plumbing +
//! multi-query fan-out.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::{ToolDefinition, ToolOutcome};

use super::super::ctx::{ExecContext, ProgressEvent};
use super::ToolExecutor;
use super::web_client::{WebFetchResult, WebSearchClient};

/// `web_search` — query Ollama Cloud's web-search endpoint. Accepts a
/// single `{query, max_results}` OR a list of `{queries: [{query,
/// max_results}]}` for parallel fan-out.
pub struct WebSearchTool {
    client: Arc<WebSearchClient>,
}

impl WebSearchTool {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Arc::new(WebSearchClient::new(api_key)),
        }
    }
}

#[async_trait]
impl ToolExecutor for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".to_string(),
            description:
                "Search the web via Ollama Cloud's search API. Takes either a single `query` + `max_results`, or an array of `queries` for parallel fan-out."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "max_results": { "type": "integer", "minimum": 1, "maximum": 10, "default": 5 },
                    "queries": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string" },
                                "max_results": { "type": "integer", "minimum": 1, "maximum": 10 }
                            },
                            "required": ["query"]
                        }
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let queries = match parse_queries(&args) {
            Ok(q) => q,
            Err(e) => {
                return ToolOutcome::Error {
                    error: e,
                    duration_secs: 0.0,
                };
            },
        };
        if queries.is_empty() {
            return ToolOutcome::Error {
                error: "web_search requires at least one query".to_string(),
                duration_secs: 0.0,
            };
        }

        let start = std::time::Instant::now();
        let mut combined = String::new();
        for (idx, (query, count)) in queries.iter().enumerate() {
            let _ = ctx
                .progress
                .send(ProgressEvent::Status(format!(
                    "searching {}/{}: {}",
                    idx + 1,
                    queries.len(),
                    query
                )))
                .await;

            let search = self.client.search_query(query, *count);
            tokio::select! {
                biased;
                _ = ctx.token.cancelled() => return ToolOutcome::Cancelled,
                result = search => {
                    match result {
                        Ok(results) => {
                            let formatted = self.client.format_results(&results);
                            if queries.len() > 1 {
                                combined.push_str(&format!("=== query: {} ===\n{}\n\n", query, formatted));
                            } else {
                                combined = formatted;
                            }
                        },
                        Err(e) => {
                            return ToolOutcome::Error {
                                error: format!("web_search({}): {}", query, e),
                                duration_secs: start.elapsed().as_secs_f64(),
                            };
                        },
                    }
                }
            }
        }

        ToolOutcome::Finished {
            output: combined,
            images: None,
            duration_secs: start.elapsed().as_secs_f64(),
        }
    }
}

/// `web_fetch` — retrieve a URL's readable content (Ollama Cloud's
/// fetch endpoint). Single URL, single response.
pub struct WebFetchTool {
    client: Arc<WebSearchClient>,
}

impl WebFetchTool {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Arc::new(WebSearchClient::new(api_key)),
        }
    }
}

#[async_trait]
impl ToolExecutor for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".to_string(),
            description: "Retrieve a single URL's main content as text (Ollama Cloud fetch API)."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
            return ToolOutcome::Error {
                error: "web_fetch requires 'url' (string)".to_string(),
                duration_secs: 0.0,
            };
        };
        let start = std::time::Instant::now();
        let fetch = self.client.fetch_url(url);

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::Cancelled,
            result = fetch => match result {
                Ok(page) => ToolOutcome::Finished {
                    output: format_fetch(url, &page),
                    images: None,
                    duration_secs: start.elapsed().as_secs_f64(),
                },
                Err(e) => ToolOutcome::Error {
                    error: format!("web_fetch({}): {}", url, e),
                    duration_secs: start.elapsed().as_secs_f64(),
                },
            },
        }
    }
}

fn format_fetch(url: &str, page: &WebFetchResult) -> String {
    let title = if page.title.is_empty() {
        "(no title)"
    } else {
        page.title.as_str()
    };
    format!("# {}\n\nURL: {}\n\n{}", title, url, page.content)
}

fn parse_queries(args: &serde_json::Value) -> Result<Vec<(String, usize)>, String> {
    if let Some(arr) = args.get("queries").and_then(|v| v.as_array()) {
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            let Some(obj) = v.as_object() else {
                return Err(
                    "web_search: 'queries' must be an array of {query, max_results}".to_string(),
                );
            };
            let Some(query) = obj.get("query").and_then(|x| x.as_str()) else {
                return Err("web_search: each query entry needs 'query' (string)".to_string());
            };
            let count = obj
                .get("max_results")
                .or_else(|| obj.get("result_count"))
                .and_then(|x| x.as_u64())
                .unwrap_or(5)
                .clamp(1, 10) as usize;
            out.push((query.to_string(), count));
        }
        return Ok(out);
    }
    if let Some(query) = args.get("query").and_then(|v| v.as_str()) {
        let count = args
            .get("max_results")
            .or_else(|| args.get("result_count"))
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 10) as usize;
        return Ok(vec![(query.to_string(), count)]);
    }
    Err("web_search requires 'query' (string) or 'queries' (array)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_queries_single_form() {
        let args = serde_json::json!({"query": "rust async", "max_results": 3});
        let q = parse_queries(&args).unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].0, "rust async");
        assert_eq!(q[0].1, 3);
    }

    #[test]
    fn parse_queries_array_form() {
        let args = serde_json::json!({"queries": [
            {"query": "a", "max_results": 2},
            {"query": "b", "result_count": 5},
        ]});
        let q = parse_queries(&args).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[1].1, 5);
    }

    #[test]
    fn parse_queries_missing_errors() {
        let args = serde_json::json!({});
        assert!(parse_queries(&args).is_err());
    }

    #[test]
    fn parse_queries_clamps_count() {
        let args = serde_json::json!({"query": "q", "max_results": 999});
        let q = parse_queries(&args).unwrap();
        assert_eq!(q[0].1, 10);
        let args = serde_json::json!({"query": "q", "max_results": 0});
        let q = parse_queries(&args).unwrap();
        assert_eq!(q[0].1, 1);
    }
}
