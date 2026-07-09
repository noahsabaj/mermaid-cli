//! Web tools: `web_search` and `web_fetch`.
//!
//! Each tool holds a pluggable backend (`web_client::SearchProvider` /
//! `FetchProvider`) selected from `[web]` config: `web_fetch` defaults to a
//! native in-process fetch (no key), `web_search` to Ollama Cloud or a
//! self-hosted SearXNG. This tool layer owns cancellation plumbing, the SSRF
//! guard, and multi-query fan-out; the backend owns the transport.

use std::sync::Arc;

use async_trait::async_trait;

use crate::app::{FetchBackend, SearchBackend, WebConfig};
use crate::domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};

use super::super::ctx::{ExecContext, ProgressEvent};
use super::ToolExecutor;
use super::web_client::{
    FetchProvider, ManagedSearxngBackend, NativeFetchClient, OllamaWebClient, SearchProvider,
    SearxngClient, WebFetchResult, format_results,
};

/// Build the `web_fetch` tool for the configured backend. `native` always
/// yields a tool; `ollama` yields one only when `OLLAMA_API_KEY` resolves
/// (otherwise the tool would 401 on every call, so we don't register it).
pub fn web_fetch_tool(web: &WebConfig) -> Option<WebFetchTool> {
    match web.fetch_backend {
        FetchBackend::Native => Some(WebFetchTool::native()),
        FetchBackend::Ollama => {
            crate::utils::resolve_api_key("OLLAMA_API_KEY", None).map(WebFetchTool::ollama)
        },
    }
}

/// Build the `web_search` tool for the configured backend. `auto` (the default)
/// and `searxng` always yield a tool; `ollama` yields one only when
/// `OLLAMA_API_KEY` resolves.
pub fn web_search_tool(web: &WebConfig) -> Option<WebSearchTool> {
    match web.search_backend {
        // Zero-config default: Ollama Cloud when a key is present, otherwise an
        // auto-managed local SearXNG (started lazily on the first search).
        SearchBackend::Auto => Some(
            crate::utils::resolve_api_key("OLLAMA_API_KEY", None)
                .map(WebSearchTool::ollama)
                .unwrap_or_else(WebSearchTool::managed_searxng),
        ),
        SearchBackend::Ollama => {
            crate::utils::resolve_api_key("OLLAMA_API_KEY", None).map(WebSearchTool::ollama)
        },
        SearchBackend::Searxng => Some(WebSearchTool::searxng(web.searxng_url.clone())),
    }
}

/// `web_search` — query the configured search backend. Accepts a single
/// `{query, max_results}` OR a list of `{queries: [{query, max_results}]}` for
/// parallel fan-out.
pub struct WebSearchTool {
    backend: Arc<dyn SearchProvider>,
}

impl WebSearchTool {
    /// Search via Ollama Cloud (bearer `OLLAMA_API_KEY`).
    pub fn ollama(api_key: String) -> Self {
        Self {
            backend: Arc::new(OllamaWebClient::new(api_key)),
        }
    }

    /// Search via a self-hosted SearXNG instance at `base_url`.
    pub fn searxng(base_url: String) -> Self {
        Self {
            backend: Arc::new(SearxngClient::new(base_url)),
        }
    }

    /// Search via an auto-managed local SearXNG container (zero-config default).
    pub fn managed_searxng() -> Self {
        Self {
            backend: Arc::new(ManagedSearxngBackend),
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
                "Search the web. Takes either a single `query` + `max_results`, or an array of `queries` for parallel fan-out."
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
        let mut errors: Vec<String> = Vec::new();
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

            let search = self.backend.search(query, *count);
            let result = tokio::select! {
                biased;
                _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
                result = search => result,
            };
            // A single query returning nothing or erroring does NOT abort the
            // batch — record it and carry on so the other queries' results
            // survive (a partial answer beats none).
            let section = match result {
                Ok(results) => {
                    result_count += results.len();
                    sources.extend(results.iter().map(|result| result.url.clone()));
                    if results.is_empty() {
                        "[SEARCH_RESULTS]\n(no results found)\n[/SEARCH_RESULTS]\n".to_string()
                    } else {
                        format_results(&results)
                    }
                },
                Err(e) => {
                    errors.push(format!("{query}: {e}"));
                    format!("(search failed: {e})\n")
                },
            };
            if queries.len() > 1 {
                combined.push_str(&format!("=== query: {query} ===\n{section}\n\n"));
            } else {
                combined = section;
            }
        }

        // Only a total failure — every query hit a backend error — is a tool
        // error. An empty-but-reachable search, or a partial success, returns
        // normally so the model sees what did come back.
        if errors.len() == queries.len() {
            return ToolOutcome::error(
                format!("web_search failed: {}", errors.join("; ")),
                start.elapsed().as_secs_f64(),
            );
        }

        // Cap the aggregate output. Per-result content is already truncated to
        // WEB_CONTENT_MAX_CHARS, but many results across many queries can still
        // bloat context (and memory) past what any single result's cap bounds (#28).
        let combined = crate::utils::truncate_middle(
            &combined,
            crate::constants::WEB_SEARCH_AGGREGATE_MAX_CHARS,
        );

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

/// `web_fetch` — retrieve a URL's readable content as markdown. Single URL,
/// single response. Native by default (fetches + converts in-process, no key);
/// can be backed by Ollama Cloud instead.
pub struct WebFetchTool {
    backend: Arc<dyn FetchProvider>,
}

impl WebFetchTool {
    /// Fetch the URL in-process and convert its HTML to markdown (no API key).
    pub fn native() -> Self {
        Self {
            backend: Arc::new(NativeFetchClient::new()),
        }
    }

    /// Fetch via Ollama Cloud's server-side `/api/web_fetch`.
    pub fn ollama(api_key: String) -> Self {
        Self {
            backend: Arc::new(OllamaWebClient::new(api_key)),
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
            description: "Retrieve a single URL's main content as markdown.".to_string(),
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
        let fetch = self.backend.fetch(url);

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

/// Cap on a single `web_fetch` body (#F46). The raw fetch is bounded only by the
/// 16 MB HTTP body limit, so without this one URL could dump megabytes into model
/// context. A full page warrants more room than a `web_search` snippet
/// (`WEB_CONTENT_MAX_CHARS`), so this mirrors web_search's per-call aggregate
/// budget. Applied as a byte cap; truncation is char-boundary safe.
const WEB_FETCH_MAX_CHARS: usize = crate::constants::WEB_SEARCH_AGGREGATE_MAX_CHARS;

/// Truncate a fetched page body to `WEB_FETCH_MAX_CHARS` bytes, char-boundary
/// safe, appending a marker — consistent with how `web_search` bounds the
/// content it returns (#F46). Borrows when no truncation is needed.
fn cap_fetch_content(content: &str) -> std::borrow::Cow<'_, str> {
    if content.len() <= WEB_FETCH_MAX_CHARS {
        return std::borrow::Cow::Borrowed(content);
    }
    let cut = content.floor_char_boundary(WEB_FETCH_MAX_CHARS);
    std::borrow::Cow::Owned(format!("{}\n\n...[content truncated]", &content[..cut]))
}

fn format_fetch(url: &str, page: &WebFetchResult) -> String {
    let title = if page.title.is_empty() {
        "(no title)"
    } else {
        page.title.as_str()
    };
    let content = cap_fetch_content(&page.content);
    format!("# {}\n\nURL: {}\n\n{}", title, url, content)
}

fn parse_queries(args: &serde_json::Value) -> Result<Vec<(String, usize)>, String> {
    if let Some(arr) = args.get("queries").and_then(|v| v.as_array()) {
        if arr.len() > crate::constants::MAX_BATCH_TOOL_ITEMS {
            return Err(format!(
                "web_search: too many queries ({}); cap is {} per call — split the request",
                arr.len(),
                crate::constants::MAX_BATCH_TOOL_ITEMS
            ));
        }
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

/// Reject obviously-unsafe fetch URLs before the backend runs: only
/// `http`/`https`, and no loopback / link-local / private / metadata hosts.
/// For the native backend this is the primary SSRF boundary (the request
/// leaves from this process, so `web_client::guard_resolved_ips` also checks
/// the resolved addresses); for the Ollama backend it's defense-in-depth ahead
/// of Ollama's own server-side fetch. Guards against model-supplied URLs.
/// Reject anything that isn't a plain `http(s)` URL. A `file:`, `javascript:`,
/// `data:`, or otherwise exotic scheme has no business reaching an HTTP fetch or
/// an OS browser launcher. Returns the parsed URL so callers can inspect the
/// host without re-parsing. Note: this deliberately does NOT block loopback —
/// `open_url` legitimately opens a just-started local dev server.
pub(crate) fn require_http_scheme(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        other => Err(format!(
            "unsupported URL scheme '{other}' (only http/https allowed)"
        )),
    }
}

fn validate_fetch_url(url: &str) -> Result<(), String> {
    let parsed = require_http_scheme(url)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    if is_blocked_host(host) {
        return Err(format!("refusing to fetch internal/loopback host '{host}'"));
    }
    Ok(())
}

/// Cloud-metadata DNS hostnames that resolve (inside the relevant cloud) to a
/// link-local metadata IP — `169.254.169.254` and friends — but are LEXICALLY
/// public, so the IP-only `classify_host` waves them through as
/// [`crate::utils::HostClass::Public`]. We block them by name as well. Matched
/// after lowercasing + trimming any surrounding `[]` and trailing FQDN dot.
const METADATA_HOSTNAMES: &[&str] = &[
    "metadata.google.internal",   // GCP (canonical)
    "metadata.goog",              // GCP (alternate)
    "metadata",                   // GCP/Azure short name (http://metadata/ responds)
    "instance-data",              // AWS (cloud-init alias)
    "instance-data.ec2.internal", // AWS
];

/// F57: client-side SSRF denylist for `web_fetch`.
///
/// For the NATIVE backend the request originates from this process, so this
/// lexical check plus `web_client::guard_resolved_ips` (which classifies the
/// resolved addresses) is the authoritative boundary — modulo the
/// DNS-rebinding TOCTOU that no pre-connect check can fully close.
///
/// For the OLLAMA backend the URL is POSTed to Ollama's server-side
/// `/api/web_fetch` and the in-process client only ever connects to
/// `ollama.com`; there the authoritative boundary is server-side (Ollama) and
/// this check is defense-in-depth so a model can't trivially aim the server at
/// an obvious internal target.
///
/// Either way we reject:
/// - every non-public IP form via the shared `classify_host` (loopback,
///   RFC-1918/ULA, link-local incl. `169.254.169.254`, CGNAT, unspecified
///   `0.0.0.0`/`::`, plus the IPv4-mapped-IPv6 / ULA / link-local-IPv6 / `[::1]`
///   forms a hand-rolled IPv4 check would miss); and
/// - the well-known cloud-metadata HOSTNAMES (`metadata.google.internal`, …)
///   that are lexically public but front a metadata service.
fn is_blocked_host(host: &str) -> bool {
    let normalized = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if METADATA_HOSTNAMES.contains(&normalized.as_str()) {
        return true;
    }
    crate::utils::classify_host(host).is_internal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_http_scheme_accepts_http_rejects_exotic() {
        // http/https pass — including loopback, since `open_url` legitimately
        // opens a just-started local dev server (so this must NOT block localhost).
        for good in [
            "http://example.com",
            "https://example.com/path?a=1&b=2",
            "http://localhost:3000",
            "http://127.0.0.1:8080",
        ] {
            assert!(require_http_scheme(good).is_ok(), "{good} should pass");
        }
        // Non-http(s) schemes and unparseable input are rejected.
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,<script>",
            "ftp://example.com",
            "not a url",
        ] {
            assert!(
                require_http_scheme(bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }

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
            // #27/#80: IPv6/CGNAT bypasses the old IPv4-centric blocklist missed.
            "http://[::ffff:169.254.169.254]/latest/meta-data/",
            "http://[fc00::1]/",
            "http://[fe80::1]/",
            "http://100.100.100.200/",
            // F57: cloud-metadata hostnames are lexically public (IP-only
            // classify_host waves them through) but front a metadata service.
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://metadata.goog/",
            "http://metadata/",
            "http://instance-data/latest/meta-data/",
            "https://METADATA.GOOGLE.INTERNAL./",
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
    fn is_blocked_host_covers_metadata_names_and_ip_forms() {
        // F57: cloud-metadata HOSTNAMES (lexically public, IP-only
        // classify_host misses them) are blocked, case/dot-insensitively.
        for h in [
            "metadata.google.internal",
            "metadata.google.internal.", // trailing FQDN dot
            "Metadata.Google.Internal",  // case-insensitive
            "metadata.goog",
            "metadata",
            "instance-data",
            "instance-data.ec2.internal",
        ] {
            assert!(is_blocked_host(h), "metadata host {h:?} must be blocked");
        }
        // Non-public IP forms still go through classify_host (incl. the ones
        // the task lists as examples that must already be covered).
        for h in ["0.0.0.0", "::1", "169.254.169.254", "127.0.0.1"] {
            assert!(is_blocked_host(h), "internal IP {h:?} must be blocked");
        }
        // Legitimate public hosts are NOT blocked — including a real `.goog`
        // domain that merely is not the metadata alias.
        for h in ["example.com", "docs.rs", "abc.goog", "8.8.8.8"] {
            assert!(!is_blocked_host(h), "public host {h:?} must be allowed");
        }
    }

    #[test]
    fn format_fetch_caps_long_content() {
        // F46: a huge page body must be truncated with a marker, not dumped whole.
        let big = "z".repeat(WEB_FETCH_MAX_CHARS * 2);
        let page = WebFetchResult {
            title: "T".to_string(),
            content: big,
        };
        let out = format_fetch("https://example.com", &page);
        assert!(
            out.len() < WEB_FETCH_MAX_CHARS + 256,
            "content must be capped, got {} bytes",
            out.len()
        );
        assert!(out.contains("truncated"), "expected truncation marker");

        // A short page is emitted intact, with no marker.
        let small = WebFetchResult {
            title: "T".to_string(),
            content: "hello world".to_string(),
        };
        let out = format_fetch("https://example.com", &small);
        assert!(out.contains("hello world"));
        assert!(!out.contains("truncated"));
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

    #[test]
    fn parse_queries_rejects_excess_fan_out() {
        // #90: a single call can't request unbounded fan-out.
        let many: Vec<_> = (0..crate::constants::MAX_BATCH_TOOL_ITEMS + 1)
            .map(|i| serde_json::json!({"query": format!("q{i}")}))
            .collect();
        let args = serde_json::json!({ "queries": many });
        assert!(parse_queries(&args).is_err());

        // Exactly at the cap is still accepted.
        let at_cap: Vec<_> = (0..crate::constants::MAX_BATCH_TOOL_ITEMS)
            .map(|i| serde_json::json!({"query": format!("q{i}")}))
            .collect();
        let args = serde_json::json!({ "queries": at_cap });
        assert_eq!(
            parse_queries(&args).unwrap().len(),
            crate::constants::MAX_BATCH_TOOL_ITEMS
        );
    }

    #[tokio::test]
    async fn web_search_batch_survives_empty_and_failed_queries() {
        use crate::domain::{ToolCallId, ToolStatus, TurnId};
        use crate::providers::ctx::test_exec_context;
        use crate::providers::tool::web_client::SearchResult;
        use async_trait::async_trait;
        use std::sync::Arc;

        struct Mock;
        #[async_trait]
        impl SearchProvider for Mock {
            async fn search(
                &self,
                query: &str,
                _count: usize,
            ) -> anyhow::Result<Vec<SearchResult>> {
                match query {
                    "boom" => Err(anyhow::anyhow!("backend down")),
                    "empty" => Ok(Vec::new()),
                    _ => Ok(vec![SearchResult {
                        title: "Title".to_string(),
                        url: "https://example.com".to_string(),
                        snippet: "snip".to_string(),
                        full_content: "content".to_string(),
                    }]),
                }
            }
        }

        let mk = || WebSearchTool {
            backend: Arc::new(Mock),
        };
        let tmp = std::path::PathBuf::from("/tmp");

        // Partial: one good, one empty, one erroring -> success, good kept.
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), tmp.clone());
        let out = mk()
            .execute(
                serde_json::json!({"queries": [{"query":"good"},{"query":"empty"},{"query":"boom"}]}),
                ctx,
            )
            .await;
        assert_eq!(
            out.status,
            ToolStatus::Success,
            "a partial batch must not abort"
        );
        assert!(
            out.output().contains("https://example.com"),
            "keeps the good result"
        );

        // A single empty query is "no results", not a hard error.
        let (ctx, _rx) = test_exec_context(TurnId(2), ToolCallId(2), tmp.clone());
        let out = mk()
            .execute(serde_json::json!({"query": "empty"}), ctx)
            .await;
        assert_eq!(out.status, ToolStatus::Success, "empty is not an error");
        assert!(out.output().contains("no results"));

        // Every query failing IS a tool error.
        let (ctx, _rx) = test_exec_context(TurnId(3), ToolCallId(3), tmp);
        let out = mk()
            .execute(
                serde_json::json!({"queries": [{"query":"boom"},{"query":"boom"}]}),
                ctx,
            )
            .await;
        assert_eq!(out.status, ToolStatus::Error, "total failure is an error");
    }
}
