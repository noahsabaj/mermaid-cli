use crate::utils::{RetryConfig, classify_host, retry_async_if, truncate_middle};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
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

/// A `web_search` backend: a query maps to ranked results. Implemented by
/// [`OllamaWebClient`] (Ollama Cloud) and [`SearxngClient`] (self-hosted).
#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>>;
}

/// A `web_fetch` backend: a URL maps to readable page content. Implemented by
/// [`NativeFetchClient`] (in-process fetch) and [`OllamaWebClient`] (Ollama
/// Cloud's server-side fetch).
#[async_trait]
pub trait FetchProvider: Send + Sync {
    async fn fetch(&self, url: &str) -> Result<WebFetchResult>;
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

/// SearXNG JSON search response (`/search?format=json`). Only the fields we use
/// are modelled; the rest of the (large) payload is ignored.
#[derive(Debug, Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Debug, Deserialize)]
struct SearxngResult {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    content: String,
}

const OLLAMA_API_BASE: &str = "https://ollama.com/api";

/// User-Agent for native (`fetch_backend = "native"`) fetches. Some sites 403 a
/// blank UA; a descriptive one is polite and identifies the client.
const NATIVE_FETCH_UA: &str =
    "Mozilla/5.0 (compatible; MermaidBot/1.0; +https://github.com/noahsabaj/mermaid-cli)";

/// Carries the HTTP status of a non-success response so the retry classifier can
/// tell retryable (5xx / 429) from terminal (4xx) responses without
/// string-matching the error message (#85).
#[derive(Debug)]
struct HttpStatusError {
    status: u16,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.status)
    }
}

impl std::error::Error for HttpStatusError {}

/// Retry only transient web-API failures: network timeout/connect errors and
/// 5xx / 429 responses. Terminal 4xx (auth, bad request) and parse errors are
/// surfaced immediately rather than retried `max_attempts` times (#85). The
/// typed errors are found through anyhow's `.context()` layers via downcast.
fn web_error_is_retryable(e: &anyhow::Error) -> bool {
    if let Some(re) = e.downcast_ref::<reqwest::Error>() {
        return re.is_timeout() || re.is_connect();
    }
    if let Some(h) = e.downcast_ref::<HttpStatusError>() {
        return h.status == 429 || (500..600).contains(&h.status);
    }
    false
}

/// Web client backed by Ollama Cloud's `/api/web_search` + `/api/web_fetch`,
/// authenticated with a bearer token (`OLLAMA_API_KEY`).
#[derive(Clone)]
pub struct OllamaWebClient {
    client: Client,
    api_key: String,
}

impl OllamaWebClient {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
        }
    }

    /// Execute search via Ollama Cloud API.
    ///
    /// The web_search API already returns full page content per result, so no
    /// separate web_fetch calls are needed. Each result's content is truncated
    /// to prevent context bloat.
    async fn search_impl(&self, query: &str, count: usize) -> Result<Vec<SearchResult>> {
        if count == 0 || count > 10 {
            return Err(anyhow!(
                "Result count must be between 1 and 10, got {}",
                count
            ));
        }

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
        let ollama_response: OllamaSearchResponse = retry_async_if(
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
                        .map_err(|e| {
                            anyhow::Error::new(e).context("Failed to reach Ollama web search API")
                        })?;

                    if !response.status().is_success() {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        return Err(anyhow::Error::new(HttpStatusError {
                            status: status.as_u16(),
                        })
                        .context(format!(
                            "Ollama web search API returned error {}: {}",
                            status, body
                        )));
                    }

                    let body =
                        read_body_capped(response, crate::constants::MAX_WEB_BODY_BYTES).await?;
                    serde_json::from_slice::<OllamaSearchResponse>(&body)
                        .map_err(|e| anyhow!("Failed to parse Ollama search response: {}", e))
                }
            },
            &retry_config,
            web_error_is_retryable,
        )
        .await?;

        let search_results = map_search_results(
            ollama_response
                .results
                .into_iter()
                .map(|r| (r.title, r.url, r.content)),
            count,
        );

        // Empty is a valid outcome (no matches), not an error.
        Ok(search_results)
    }

    /// Fetch a URL's content via Ollama's web_fetch API.
    async fn fetch_impl(&self, url: &str) -> Result<WebFetchResult> {
        let retry_config = RetryConfig {
            max_attempts: 2,
            initial_delay_ms: 200,
            max_delay_ms: 2000,
            backoff_multiplier: 2.0,
        };

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let url_owned = url.to_string();
        let response: OllamaFetchResponse = retry_async_if(
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
                        .map_err(|e| {
                            anyhow::Error::new(e).context(format!("Failed to fetch {}", url))
                        })?;

                    if !response.status().is_success() {
                        let status = response.status();
                        return Err(anyhow::Error::new(HttpStatusError {
                            status: status.as_u16(),
                        })
                        .context(format!("Failed to fetch {}", url)));
                    }

                    let body =
                        read_body_capped(response, crate::constants::MAX_WEB_BODY_BYTES).await?;
                    serde_json::from_slice::<OllamaFetchResponse>(&body)
                        .map_err(|e| anyhow!("Failed to parse fetch response: {}", e))
                }
            },
            &retry_config,
            web_error_is_retryable,
        )
        .await?;

        Ok(WebFetchResult {
            title: response.title.unwrap_or_default(),
            content: response.content.unwrap_or_default(),
        })
    }
}

#[async_trait]
impl SearchProvider for OllamaWebClient {
    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>> {
        self.search_impl(query, count).await
    }
}

#[async_trait]
impl FetchProvider for OllamaWebClient {
    async fn fetch(&self, url: &str) -> Result<WebFetchResult> {
        self.fetch_impl(url).await
    }
}

/// `web_search` backed by a self-hosted SearXNG instance's JSON API. Keyless —
/// the instance itself queries upstream engines; Mermaid only talks to the
/// user's local SearXNG.
#[derive(Clone)]
pub struct SearxngClient {
    client: Client,
    base_url: String,
}

impl SearxngClient {
    pub fn new(base_url: String) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

#[async_trait]
impl SearchProvider for SearxngClient {
    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>> {
        let request_url = reqwest::Url::parse_with_params(
            &format!("{}/search", self.base_url),
            &[("q", query), ("format", "json")],
        )
        .map_err(|e| anyhow!("invalid SearXNG URL {}: {e}", self.base_url))?;

        let response = self
            .client
            .get(request_url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| {
                anyhow::Error::new(e).context(format!(
                    "Failed to reach SearXNG at {} — is it running?",
                    self.base_url
                ))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(anyhow!(
                "SearXNG at {} returned {status}. A 403 usually means the JSON format is \
                 disabled — add `json` to `search.formats` in its settings.yml.",
                self.base_url
            ));
        }

        let body = read_body_capped(response, crate::constants::MAX_WEB_BODY_BYTES).await?;
        let parsed: SearxngResponse = serde_json::from_slice(&body).map_err(|e| {
            anyhow!("Failed to parse SearXNG response (is `format=json` enabled?): {e}")
        })?;

        let results = map_search_results(
            parsed
                .results
                .into_iter()
                .map(|r| (r.title, r.url, r.content)),
            count,
        );

        // Empty is a valid outcome (no matches), not an error — the tool layer
        // reports "no results" and keeps any sibling queries' results.
        Ok(results)
    }
}

/// `web_search` backed by an auto-managed local SearXNG container
/// (`crate::searxng`): starts the container lazily on the first search, reuses
/// it after, and mermaid tears it down on exit. This is the zero-config default
/// when no Ollama key is configured — the user installs and configures nothing.
pub struct ManagedSearxngBackend;

#[async_trait]
impl SearchProvider for ManagedSearxngBackend {
    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>> {
        let base_url = crate::searxng::manager().ensure_running().await?;
        SearxngClient::new(base_url).search(query, count).await
    }
}

/// `web_fetch` performed in-process: fetch the URL directly and convert its
/// HTML to markdown. No API key, no third party. Because the request leaves
/// from this machine, the SSRF guards in `web.rs` (lexical) plus
/// [`guard_resolved_ips`] (resolved-address) are the boundary here — not a
/// remote service's.
pub struct NativeFetchClient {
    client: Client,
}

impl Default for NativeFetchClient {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeFetchClient {
    pub fn new() -> Self {
        let client = Client::builder()
            .user_agent(NATIVE_FETCH_UA)
            .timeout(Duration::from_secs(20))
            // Re-validate every redirect hop. reqwest follows 3xx internally, so a
            // public URL that 302s to an internal host (127.0.0.1, localhost,
            // 169.254.169.254, RFC-1918) would sail past the one-shot pre-send
            // `guard_resolved_ips`. Re-check each hop's target host lexically.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= MAX_FETCH_REDIRECTS {
                    return attempt.error("too many redirects");
                }
                // Own the host string before moving `attempt` into follow()/error().
                let host = attempt.url().host_str().map(str::to_owned);
                if redirect_allowed(host.as_deref()) {
                    attempt.follow()
                } else {
                    attempt.error(format!(
                        "refusing redirect to internal host '{}'",
                        host.as_deref().unwrap_or("<none>")
                    ))
                }
            }))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { client }
    }
}

#[async_trait]
impl FetchProvider for NativeFetchClient {
    async fn fetch(&self, url: &str) -> Result<WebFetchResult> {
        // Native fetch makes THIS process the fetcher, so re-check that the
        // host doesn't resolve to an internal address (defense-in-depth on top
        // of the lexical guard `web.rs` already applied).
        guard_resolved_ips(url).await?;

        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| anyhow::Error::new(e).context(format!("Failed to fetch {url}")))?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(anyhow::Error::new(HttpStatusError {
                status: status.as_u16(),
            })
            .context(format!("Failed to fetch {url}")));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        let body = read_body_capped(response, crate::constants::MAX_WEB_BODY_BYTES).await?;

        // Only convert markup/text. A binary content-type (pdf, image, zip)
        // can't become useful markdown — reject with a clear reason. An absent
        // content-type is allowed (treated as text).
        let looks_textual = content_type.is_empty()
            || content_type.contains("html")
            || content_type.contains("xml")
            || content_type.contains("json")
            || content_type.starts_with("text/");
        if !looks_textual {
            return Err(anyhow!(
                "web_fetch: unsupported content-type '{content_type}' (only html/text pages)"
            ));
        }

        let html = String::from_utf8_lossy(&body).into_owned();
        let url_owned = url.to_string();
        // HTML parsing + readability + markdown conversion is CPU-bound; keep it
        // off the async runtime's worker threads.
        let (title, content) =
            tokio::task::spawn_blocking(move || extract_readable(&html, &url_owned))
                .await
                .map_err(|e| anyhow!("content extraction failed: {e}"))?;

        Ok(WebFetchResult { title, content })
    }
}

/// Maximum redirect hops the native fetch client follows (mirrors the previous
/// `Policy::limited(5)`); the fetch errors after this many.
const MAX_FETCH_REDIRECTS: usize = 5;

/// Whether a redirect to `target_host` may be followed: refuse when the host is
/// missing or classifies as internal (loopback / link-local / private / CGNAT /
/// unspecified). Factored out so the per-hop SSRF re-check is unit-testable
/// without a live redirecting server.
fn redirect_allowed(target_host: Option<&str>) -> bool {
    match target_host {
        Some(host) => !classify_host(host).is_internal(),
        None => false,
    }
}

/// Reject a native fetch whose host resolves to a loopback / private /
/// link-local / metadata address. Best-effort: the eventual connect re-resolves
/// (a TOCTOU gap a no-network check can't close), but this stops the obvious
/// "public name pointing at 127.0.0.1 / 169.254.169.254" aim.
async fn guard_resolved_ips(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|e| anyhow!("invalid URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("URL has no host"))?;
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| anyhow!("DNS resolution failed for {host}: {e}"))?;
    for addr in addrs {
        if classify_host(&addr.ip().to_string()).is_internal() {
            return Err(anyhow!(
                "refusing to fetch '{host}' — it resolves to an internal address"
            ));
        }
    }
    Ok(())
}

/// Extract a page's title + main content (as markdown) from raw HTML. Uses a
/// readability pass to drop nav/boilerplate, then converts the isolated content
/// to markdown (preserving links). Falls back to whole-document conversion when
/// readability can't parse the page.
fn extract_readable(html: &str, url: &str) -> (String, String) {
    use dom_smoothie::Readability;

    if let Ok(mut readability) = Readability::new(html, Some(url), None)
        && let Ok(article) = readability.parse()
    {
        let content_html = article.content.to_string();
        let markdown = htmd::convert(&content_html).unwrap_or_default();
        let markdown = markdown.trim();
        // Readability can succeed yet strip a page down to nothing (SPA shells,
        // pages it misjudges). Only trust a non-empty extraction; otherwise
        // fall through to whole-document conversion.
        if !markdown.is_empty() {
            let title = if article.title.trim().is_empty() {
                fallback_title(html)
            } else {
                article.title
            };
            return (title, markdown.to_string());
        }
    }

    let markdown = htmd::convert(html).unwrap_or_default();
    (fallback_title(html), markdown.trim().to_string())
}

/// Crude `<title>` scrape for the fallback path (readability normally supplies
/// the title; this only runs when it couldn't parse the document).
fn fallback_title(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(open) = lower.find("<title") else {
        return String::new();
    };
    let after_tag = match html[open..].find('>') {
        Some(gt) => &html[open + gt + 1..],
        None => return String::new(),
    };
    match after_tag.to_ascii_lowercase().find("</title>") {
        Some(end) => after_tag[..end].trim().to_string(),
        None => String::new(),
    }
}

/// Map raw `(title, url, content)` search hits to [`SearchResult`], truncating
/// each hit's content to bound model context. Shared by every search backend.
fn map_search_results(
    hits: impl Iterator<Item = (String, String, String)>,
    count: usize,
) -> Vec<SearchResult> {
    hits.take(count)
        .map(|(title, url, content)| {
            let full_content = truncate_middle(&content, crate::constants::WEB_CONTENT_MAX_CHARS);
            let snippet = content.chars().take(200).collect();
            SearchResult {
                title,
                url,
                snippet,
                full_content,
            }
        })
        .collect()
}

/// Format search results for model consumption.
///
/// Pure data -- no behavioral instructions. Citation rules live in the system
/// prompt (src/prompts.rs), which is the SSOT for all model behavior.
pub fn format_results(results: &[SearchResult]) -> String {
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

/// Read a reqwest response body, refusing to buffer more than `max_bytes`.
/// `Response::json`/`bytes` buffer the whole body unbounded; a compromised or
/// misconfigured endpoint could return a multi-gigabyte body and OOM the
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
    fn test_ollama_web_client_creation() {
        let client = OllamaWebClient::new("test-key".to_string());
        assert_eq!(client.api_key, "test-key");
    }

    #[test]
    fn redirect_to_internal_host_is_refused() {
        // The classic SSRF-via-redirect aim: a public URL 302-ing to an internal
        // literal. Every internal target (and a hostless one) must be refused.
        for internal in [
            "127.0.0.1",
            "localhost",
            "169.254.169.254",
            "10.0.0.5",
            "192.168.1.1",
            "[::1]",
        ] {
            assert!(
                !redirect_allowed(Some(internal)),
                "{internal} redirect must be refused"
            );
        }
        assert!(
            !redirect_allowed(None),
            "a hostless redirect must be refused"
        );
        assert!(
            redirect_allowed(Some("example.com")),
            "a public host redirect must be allowed"
        );
    }

    #[test]
    fn test_format_results() {
        let results = vec![SearchResult {
            title: "Test Article".to_string(),
            url: "https://example.com".to_string(),
            snippet: "This is a test".to_string(),
            full_content: "Full content here".to_string(),
        }];

        let formatted = format_results(&results);
        assert!(formatted.contains("[SEARCH_RESULTS]"));
        assert!(formatted.contains("Test Article"));
        assert!(formatted.contains("https://example.com"));
        assert!(formatted.contains("[/SEARCH_RESULTS]"));
    }

    #[test]
    fn map_search_results_truncates_and_caps_count() {
        let hits = (0..5).map(|i| {
            (
                format!("t{i}"),
                format!("https://e{i}.com"),
                "x".repeat(crate::constants::WEB_CONTENT_MAX_CHARS * 2),
            )
        });
        let out = map_search_results(hits, 3);
        assert_eq!(out.len(), 3, "count cap applied");
        assert!(
            out[0].full_content.len() <= crate::constants::WEB_CONTENT_MAX_CHARS + 64,
            "content truncated"
        );
        assert!(out[0].snippet.chars().count() <= 200);
    }

    #[test]
    fn searxng_response_parses_results() {
        let json = serde_json::json!({
            "results": [
                {"title": "A", "url": "https://a.com", "content": "alpha"},
                {"url": "https://b.com"},
            ]
        })
        .to_string();
        let parsed: SearxngResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.results.len(), 2);
        assert_eq!(parsed.results[0].url, "https://a.com");
        // Missing title/content default to empty, not a parse error.
        assert_eq!(parsed.results[1].title, "");
        assert_eq!(parsed.results[1].content, "");
    }

    #[test]
    fn extract_readable_produces_markdown() {
        let html = r#"<html><head><title>My Page</title></head>
            <body><article><h1>Heading</h1><p>Hello <a href="https://x.com">link</a>.</p>
            <p>More text to satisfy the readability length heuristic so this block is
            treated as the main article content rather than boilerplate chrome.</p>
            </article></body></html>"#;
        let (title, md) = extract_readable(html, "https://example.com/page");
        assert!(!title.is_empty(), "title extracted, got {title:?}");
        assert!(
            md.contains("Hello"),
            "content converted to markdown: {md:?}"
        );
        assert!(md.contains("](https://x.com)"), "links preserved: {md:?}");
    }

    #[test]
    fn extract_readable_fallback_title_on_unparseable() {
        // A bare fragment with no article structure still yields a title + body.
        let html = "<title>Bare</title><p>just a snippet</p>";
        let (title, md) = extract_readable(html, "https://example.com");
        assert_eq!(title, "Bare");
        assert!(md.contains("just a snippet"));
    }

    #[test]
    fn web_error_is_retryable_classifies_status() {
        // #85: 5xx / 429 retry; 4xx and untyped (parse) errors are terminal.
        assert!(web_error_is_retryable(&anyhow::Error::new(
            HttpStatusError { status: 500 }
        )));
        assert!(web_error_is_retryable(&anyhow::Error::new(
            HttpStatusError { status: 429 }
        )));
        assert!(!web_error_is_retryable(&anyhow::Error::new(
            HttpStatusError { status: 404 }
        )));
        assert!(!web_error_is_retryable(&anyhow::Error::new(
            HttpStatusError { status: 401 }
        )));
        assert!(!web_error_is_retryable(&anyhow!("parse failed")));
        // Production wraps the status error with .context(); downcast must still
        // find it through the context layer.
        let wrapped = anyhow::Error::new(HttpStatusError { status: 503 }).context("upstream");
        assert!(web_error_is_retryable(&wrapped));
    }
}
