//! Web tools: `web_search` and `web_fetch`.
//!
//! Both delegate to `web_client::WebSearchClient` — a thin HTTP
//! client for Ollama Cloud's web API (bearer-token path, via
//! `OLLAMA_API_KEY`). The wrapper's job is cancellation plumbing +
//! multi-query fan-out.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};

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
            Err(e) => return ToolOutcome::error(e, 0.0),
        };
        if queries.is_empty() {
            return ToolOutcome::error("web_search requires at least one query", 0.0);
        }
        if let Some(blocked) = super::policy_gate::gate_external(
            &ctx,
            "web_search",
            crate::runtime::ToolCategory::Web,
            format!("web_search ({} queries)", queries.len()),
            &args,
        )
        .await
        {
            return blocked;
        }

        let start = std::time::Instant::now();
        let mut combined = String::new();
        let mut result_count = 0usize;
        let mut sources = Vec::new();
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
                _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
                result = search => {
                    match result {
                        Ok(results) => {
                            result_count += results.len();
                            sources.extend(results.iter().map(|result| result.url.clone()));
                            let formatted = self.client.format_results(&results);
                            if queries.len() > 1 {
                                combined.push_str(&format!("=== query: {} ===\n{}\n\n", query, formatted));
                            } else {
                                combined = formatted;
                            }
                        },
                        Err(e) => {
                            return ToolOutcome::error(
                                format!("web_search({}): {}", query, e),
                                start.elapsed().as_secs_f64(),
                            );
                        },
                    }
                }
            }
        }

        let duration_secs = start.elapsed().as_secs_f64();
        let requested_count = queries.iter().map(|(_, count)| *count).sum();
        let query_texts = queries.iter().map(|(query, _)| query.clone()).collect();
        ToolOutcome::success(
            combined,
            format!(
                "{} {} returned",
                result_count,
                if result_count == 1 {
                    "result"
                } else {
                    "results"
                }
            ),
            duration_secs,
        )
        .with_metadata(ToolRunMetadata {
            detail: ToolMetadata::WebSearch {
                queries: query_texts,
                requested_count,
                result_count,
                sources,
            },
            result_count: Some(result_count),
            ..ToolRunMetadata::default()
        })
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
            return ToolOutcome::error("web_fetch requires 'url' (string)", 0.0);
        };
        if let Err(reason) = validate_fetch_url(url) {
            return ToolOutcome::error(format!("web_fetch: {reason}"), 0.0);
        }
        if let Some(blocked) = super::policy_gate::gate_external(
            &ctx,
            "web_fetch",
            crate::runtime::ToolCategory::Web,
            format!("web_fetch {}", url),
            &args,
        )
        .await
        {
            return blocked;
        }
        let start = std::time::Instant::now();
        let fetch = self.client.fetch_url(url);

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
            result = fetch => match result {
                Ok(page) => {
                    let output = format_fetch(url, &page);
                    let duration_secs = start.elapsed().as_secs_f64();
                    let line_count = output.lines().count();
                    let byte_count = output.len();
                    let title = if page.title.is_empty() {
                        None
                    } else {
                        Some(page.title)
                    };
                    ToolOutcome::success(
                        output,
                        format!("{} {} fetched", line_count, if line_count == 1 { "line" } else { "lines" }),
                        duration_secs,
                    )
                    .with_metadata(ToolRunMetadata {
                        detail: ToolMetadata::WebFetch {
                            url: url.to_string(),
                            title,
                            line_count,
                            byte_count,
                        },
                        line_count: Some(line_count),
                        byte_count: Some(byte_count),
                        ..ToolRunMetadata::default()
                    })
                },
                Err(e) => ToolOutcome::error(
                    format!("web_fetch({}): {}", url, e),
                    start.elapsed().as_secs_f64(),
                ),
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

/// Reject obviously-unsafe fetch URLs client-side before handing them to the
/// (server-side) Ollama fetch API: only `http`/`https`, and no loopback /
/// link-local / private / metadata hosts. Defense-in-depth against SSRF-style
/// abuse via model-supplied URLs.
fn validate_fetch_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {},
        other => {
            return Err(format!(
                "unsupported URL scheme '{other}' (only http/https allowed)"
            ));
        },
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    if is_blocked_host(host) {
        return Err(format!("refusing to fetch internal/loopback host '{host}'"));
    }
    Ok(())
}

fn is_blocked_host(host: &str) -> bool {
    let h = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::Ipv4Addr>() {
        return ip.is_loopback()      // 127.0.0.0/8
            || ip.is_private()       // 10/8, 172.16/12, 192.168/16
            || ip.is_link_local()    // 169.254/16 (incl. 169.254.169.254 metadata)
            || ip.is_unspecified()   // 0.0.0.0
            || ip.is_broadcast();
    }
    if let Ok(ip) = h.parse::<std::net::Ipv6Addr>() {
        return ip.is_loopback() || ip.is_unspecified();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_fetch_url_blocks_unsafe_targets() {
        // #9: scheme + internal-host guards.
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "http://localhost/admin",
            "http://127.0.0.1:8080",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://[::1]/",
            "not a url",
        ] {
            assert!(
                validate_fetch_url(bad).is_err(),
                "expected reject for {bad:?}",
            );
        }
        for good in [
            "https://example.com",
            "http://example.com/page?x=1",
            "https://docs.rs/serde",
        ] {
            assert!(
                validate_fetch_url(good).is_ok(),
                "expected accept for {good:?}",
            );
        }
    }

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
