use crate::utils::{RetryConfig, classify_host, retry_async_if, truncate_content};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::providers::ctx::WebByteBudget;

const MAX_REDIRECTS: usize = 5;

static EXTRACTION_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
static DOWNLOAD_LIMITER: OnceLock<DownloadLimiter> = OnceLock::new();

/// Result from a web search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub full_content: String,
}

/// Which transport produced a fetched page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchBackend {
    Native,
    OllamaCloud,
}

impl FetchBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::OllamaCloud => "ollama_cloud",
        }
    }
}

/// How response bytes became the content returned to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionMode {
    Readability,
    HtmlToMarkdown,
    PlainText,
    Markdown,
    Json,
    Xml,
    Cloud,
}

impl ExtractionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Readability => "readability",
            Self::HtmlToMarkdown => "html_to_markdown",
            Self::PlainText => "plain_text",
            Self::Markdown => "markdown",
            Self::Json => "json",
            Self::Xml => "xml",
            Self::Cloud => "cloud",
        }
    }
}

/// Result from a web fetch, including transport and provenance facts that must
/// survive into policy, UI, persistence, and model-visible formatting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchResult {
    pub requested_url: String,
    /// Final target URL when the backend can prove it. Cloud providers that do
    /// not expose redirect provenance leave this unknown rather than claiming
    /// the requested URL was final.
    pub final_url: Option<String>,
    /// Target response status. Ollama Cloud does not expose the target status.
    pub status: Option<u16>,
    pub media_type: Option<String>,
    pub charset: Option<String>,
    pub backend: FetchBackend,
    pub extraction: ExtractionMode,
    pub source_bytes: usize,
    pub output_bytes: usize,
    pub truncated: bool,
    pub title: String,
    pub content: String,
}

/// Stable transport/extraction failures retained across the provider boundary.
/// The tool layer can project these into structured telemetry without parsing
/// human-readable error strings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebFetchError {
    #[error("invalid web URL: {0}")]
    InvalidUrl(String),
    #[error("web destination denied: {0}")]
    DestinationDenied(String),
    #[error("web redirect denied: {0}")]
    RedirectDenied(String),
    #[error("HTTP {status} fetching {url}")]
    HttpStatus { status: u16, url: String },
    #[error("invalid response media type: {0}")]
    InvalidMedia(String),
    #[error("unsupported response media type '{0}'")]
    UnsupportedMedia(String),
    #[error("response body exceeded the {limit} byte per-request limit")]
    BodyTooLarge { limit: usize },
    #[error("response body exceeded the {limit} byte aggregate web budget for this turn")]
    TurnBudgetExceeded { limit: usize },
    #[error("unsupported response charset '{0}'")]
    UnsupportedCharset(String),
    #[error("response body is not valid {0} text")]
    Decode(String),
    #[error("web fetch returned no extractable content")]
    EmptyContent,
    #[error("web transport failed: {0}")]
    Transport(String),
    #[error("web extraction failed: {0}")]
    Extraction(String),
    #[error("web backend failed: {0}")]
    Backend(String),
}

impl WebFetchError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidUrl(_) => "invalid_url",
            Self::DestinationDenied(_) => "destination_denied",
            Self::RedirectDenied(_) => "redirect_denied",
            Self::HttpStatus { .. } => "http_status",
            Self::InvalidMedia(_) => "invalid_media",
            Self::UnsupportedMedia(_) => "unsupported_media",
            Self::BodyTooLarge { .. } => "body_too_large",
            Self::TurnBudgetExceeded { .. } => "turn_budget_exceeded",
            Self::UnsupportedCharset(_) => "unsupported_charset",
            Self::Decode(_) => "decode",
            Self::EmptyContent => "empty_content",
            Self::Transport(_) => "transport",
            Self::Extraction(_) => "extraction",
            Self::Backend(_) => "backend",
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            Self::HttpStatus { status, .. } => Some(*status),
            _ => None,
        }
    }
}

type FetchResult<T> = std::result::Result<T, WebFetchError>;

/// An HTTP(S) URL that has passed Mermaid's lexical web-destination policy.
/// Fragments are removed because they are not part of an HTTP request and may
/// contain sensitive client-side state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedWebUrl(reqwest::Url);

impl ValidatedWebUrl {
    pub fn parse(raw: &str) -> FetchResult<Self> {
        let url = reqwest::Url::parse(raw)
            .map_err(|error| WebFetchError::InvalidUrl(error.to_string()))?;
        Self::from_url(url)
    }

    fn from_url(url: reqwest::Url) -> FetchResult<Self> {
        Self::from_url_with_policy(url, is_blocked_web_host)
    }

    fn from_url_with_policy(
        mut url: reqwest::Url,
        is_blocked: fn(&str) -> bool,
    ) -> FetchResult<Self> {
        match url.scheme() {
            "http" | "https" => {},
            scheme => {
                return Err(WebFetchError::InvalidUrl(format!(
                    "unsupported scheme '{scheme}' (only http/https allowed)"
                )));
            },
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(WebFetchError::InvalidUrl(
                "userinfo credentials are not allowed".to_string(),
            ));
        }
        if url.as_str().len() > 8192 {
            return Err(WebFetchError::InvalidUrl(
                "URL exceeds the 8192 byte limit".to_string(),
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| WebFetchError::InvalidUrl("URL has no host".to_string()))?;
        if is_blocked(host) {
            return Err(WebFetchError::DestinationDenied(format!(
                "non-public or metadata host '{host}'"
            )));
        }
        url.set_fragment(None);
        Ok(Self(url))
    }

    #[cfg(test)]
    fn from_fixture_url(url: reqwest::Url) -> FetchResult<Self> {
        fn fixture_blocked(host: &str) -> bool {
            if classify_host(host).is_loopback() {
                return false;
            }
            is_blocked_web_host(host)
        }
        Self::from_url_with_policy(url, fixture_blocked)
    }

    pub fn as_url(&self) -> &reqwest::Url {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

const METADATA_HOSTNAMES: &[&str] = &[
    "metadata.google.internal",
    "metadata.goog",
    "metadata",
    "instance-data",
    "instance-data.ec2.internal",
];

fn is_blocked_web_host(host: &str) -> bool {
    let normalized = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    METADATA_HOSTNAMES.contains(&normalized.as_str()) || classify_host(host).is_internal()
}

/// A `web_search` backend: a query maps to ranked results. Implemented by
/// [`OllamaWebClient`] (Ollama Cloud) and [`SearxngClient`] (self-hosted).
#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(
        &self,
        query: &str,
        count: usize,
        budget: WebByteBudget,
    ) -> Result<Vec<SearchResult>>;
}

/// A `web_fetch` backend: a URL maps to readable page content. Implemented by
/// [`NativeFetchClient`] (in-process fetch) and [`OllamaWebClient`] (Ollama
/// Cloud's server-side fetch).
#[async_trait]
pub trait FetchProvider: Send + Sync {
    async fn fetch(&self, url: &str, budget: WebByteBudget) -> FetchResult<WebFetchResult>;
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
pub struct HttpStatusError {
    status: u16,
}

impl HttpStatusError {
    pub fn status(&self) -> u16 {
        self.status
    }
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
    if let Some(WebFetchError::Transport(_)) = e.downcast_ref::<WebFetchError>() {
        return true;
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
    pub fn new(api_key: String) -> Result<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .build()
            .map_err(|error| anyhow!("failed to build Ollama web client: {error}"))?;
        Ok(Self { client, api_key })
    }

    /// Execute search via Ollama Cloud API.
    ///
    /// The web_search API already returns full page content per result, so no
    /// separate web_fetch calls are needed. Each result's content is truncated
    /// to prevent context bloat.
    async fn search_impl(
        &self,
        query: &str,
        count: usize,
        budget: WebByteBudget,
    ) -> Result<Vec<SearchResult>> {
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
        let budget = budget.clone();
        // `count` is Copy (usize) — safe to capture by value across retries
        let ollama_response: OllamaSearchResponse = retry_async_if(
            || {
                let client = client.clone();
                let api_key = api_key.clone();
                let query = query_owned.clone();
                let budget = budget.clone();
                async move {
                    let endpoint = reqwest::Url::parse(&format!("{}/web_search", OLLAMA_API_BASE))
                        .map_err(|e| anyhow!("invalid Ollama web search endpoint: {e}"))?;
                    let download_permits = acquire_download_permits(&endpoint).await?;
                    let response = client
                        .post(endpoint)
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
                        let body = read_body_capped(
                            response,
                            crate::constants::MAX_WEB_BODY_BYTES.min(64 * 1024),
                            &budget,
                        )
                        .await
                        .map(|body| String::from_utf8_lossy(&body).into_owned())
                        .unwrap_or_else(|_| "<unavailable>".to_string());
                        let body = crate::utils::redact_secrets(&body);
                        return Err(anyhow::Error::new(HttpStatusError {
                            status: status.as_u16(),
                        })
                        .context(format!(
                            "Ollama web search API returned error {}: {}",
                            status, body
                        )));
                    }

                    let body =
                        read_body_capped(response, crate::constants::MAX_WEB_BODY_BYTES, &budget)
                            .await?;
                    drop(download_permits);
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
    async fn fetch_impl(&self, url: &str, budget: WebByteBudget) -> FetchResult<WebFetchResult> {
        let requested = ValidatedWebUrl::parse(url)?;
        let retry_config = RetryConfig {
            max_attempts: 2,
            initial_delay_ms: 200,
            max_delay_ms: 2000,
            backoff_multiplier: 2.0,
        };

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let url_owned = requested.as_str().to_string();
        let budget = budget.clone();
        let response: (OllamaFetchResponse, usize) = retry_async_if(
            || {
                let client = client.clone();
                let api_key = api_key.clone();
                let url = url_owned.clone();
                let budget = budget.clone();
                async move {
                    let safe_url = crate::utils::sanitize_url_for_display(&url);
                    let endpoint =
                        reqwest::Url::parse(&format!("{}/web_fetch", OLLAMA_API_BASE))
                            .map_err(|e| anyhow!("invalid Ollama web fetch endpoint: {e}"))?;
                    let download_permits = acquire_download_permits(&endpoint).await?;
                    let response = client
                        .post(endpoint)
                        .header("Authorization", format!("Bearer {}", api_key))
                        .json(&serde_json::json!({ "url": url }))
                        .timeout(Duration::from_secs(15))
                        .send()
                        .await
                        .map_err(|e| {
                            anyhow::Error::new(e).context(format!("Failed to fetch {safe_url}"))
                        })?;

                    if !response.status().is_success() {
                        let status = response.status();
                        return Err(anyhow::Error::new(HttpStatusError {
                            status: status.as_u16(),
                        })
                        .context(format!("Failed to fetch {safe_url}: HTTP {status}")));
                    }

                    let body =
                        read_body_capped(response, crate::constants::MAX_WEB_BODY_BYTES, &budget)
                            .await?;
                    drop(download_permits);
                    let source_bytes = body.len();
                    let parsed = serde_json::from_slice::<OllamaFetchResponse>(&body)
                        .map_err(|e| anyhow!("Failed to parse fetch response: {}", e))?;
                    Ok((parsed, source_bytes))
                }
            },
            &retry_config,
            web_error_is_retryable,
        )
        .await
        .map_err(|error| map_cloud_fetch_error(error, requested.as_str()))?;

        let source_bytes = response.1;
        let title = response.0.title.unwrap_or_default();
        let content = response.0.content.unwrap_or_default();
        if content.trim().is_empty() {
            return Err(WebFetchError::EmptyContent);
        }
        let output_bytes = content.len();
        Ok(WebFetchResult {
            requested_url: requested.as_str().to_string(),
            // Ollama's API does not expose redirect provenance.
            final_url: None,
            status: None,
            media_type: None,
            charset: None,
            backend: FetchBackend::OllamaCloud,
            extraction: ExtractionMode::Cloud,
            source_bytes,
            output_bytes,
            truncated: false,
            title,
            content,
        })
    }
}

#[async_trait]
impl SearchProvider for OllamaWebClient {
    async fn search(
        &self,
        query: &str,
        count: usize,
        budget: WebByteBudget,
    ) -> Result<Vec<SearchResult>> {
        self.search_impl(query, count, budget).await
    }
}

#[async_trait]
impl FetchProvider for OllamaWebClient {
    async fn fetch(&self, url: &str, budget: WebByteBudget) -> FetchResult<WebFetchResult> {
        self.fetch_impl(url, budget).await
    }
}

fn map_cloud_fetch_error(error: anyhow::Error, requested_url: &str) -> WebFetchError {
    if let Some(error) = error.downcast_ref::<WebFetchError>() {
        return error.clone();
    }
    if let Some(status) = error.downcast_ref::<HttpStatusError>() {
        return WebFetchError::HttpStatus {
            status: status.status(),
            url: crate::utils::sanitize_url_for_display(requested_url),
        };
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return WebFetchError::Transport(crate::utils::redact_secrets(&error.to_string()));
    }
    WebFetchError::Backend(crate::utils::redact_secrets(&format!("{error:#}")))
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
    pub fn new(base_url: String) -> Result<Self> {
        Self::build(base_url, false)
    }

    fn managed(base_url: String) -> Result<Self> {
        Self::build(base_url, true)
    }

    fn build(base_url: String, managed_local: bool) -> Result<Self> {
        let mut parsed = reqwest::Url::parse(&base_url)
            .map_err(|error| anyhow!("invalid SearXNG URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(anyhow!("SearXNG URL must use http or https"));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(anyhow!("SearXNG URL must not contain userinfo credentials"));
        }
        if parsed.query().is_some() {
            return Err(anyhow!("SearXNG base URL must not contain a query"));
        }
        parsed.set_fragment(None);
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .referer(false);
        if managed_local {
            // The owned localhost process must never be reached through an
            // ambient corporate or attacker-controlled HTTP proxy.
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|error| anyhow!("failed to build SearXNG client: {error}"))?;
        Ok(Self {
            client,
            base_url: parsed.as_str().trim_end_matches('/').to_string(),
        })
    }
}

#[async_trait]
impl SearchProvider for SearxngClient {
    async fn search(
        &self,
        query: &str,
        count: usize,
        budget: WebByteBudget,
    ) -> Result<Vec<SearchResult>> {
        let safe_base = crate::utils::sanitize_url_for_display(&self.base_url);
        let request_url = reqwest::Url::parse_with_params(
            &format!("{}/search", self.base_url),
            &[("q", query), ("format", "json")],
        )
        .map_err(|e| anyhow!("invalid SearXNG URL {safe_base}: {e}"))?;

        let download_permits = acquire_download_permits(&request_url).await?;

        let response = self
            .client
            .get(request_url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| {
                anyhow::Error::new(e).context(format!(
                    "Failed to reach SearXNG at {} — is it running?",
                    safe_base
                ))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(anyhow!(
                "SearXNG at {} returned {status}. A 403 usually means the JSON format is \
                 disabled — add `json` to `search.formats` in its settings.yml.",
                safe_base
            ));
        }

        let body =
            read_body_capped(response, crate::constants::MAX_WEB_BODY_BYTES, &budget).await?;
        drop(download_permits);
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

/// `web_search` backed by Mermaid's auto-managed local SearXNG bundle
/// (`crate::searxng`): starts the process lazily on the first search, reuses it,
/// and tears it down on exit. This is the zero-config default on platforms for
/// which Mermaid publishes a bundle.
pub struct ManagedSearxngBackend;

#[async_trait]
impl SearchProvider for ManagedSearxngBackend {
    async fn search(
        &self,
        query: &str,
        count: usize,
        budget: WebByteBudget,
    ) -> Result<Vec<SearchResult>> {
        let base_url = crate::searxng::manager().ensure_running().await?;
        SearxngClient::managed(base_url)?
            .search(query, count, budget)
            .await
    }
}

/// `web_fetch` performed in-process. Redirects are handled manually so every
/// destination is authorized before its request is issued.
#[derive(Clone)]
pub struct NativeFetchClient {
    client: Client,
}

impl NativeFetchClient {
    /// Build the hardened native client. Construction is fail-closed: losing
    /// the resolver, proxy policy, timeout, or redirect policy is an error.
    pub fn new() -> Result<Self> {
        let client = native_client_builder()
            .dns_resolver(std::sync::Arc::new(VettingResolver))
            .build()
            .map_err(|e| anyhow!("failed to build hardened native web client: {e}"))?;
        Ok(Self { client })
    }

    async fn fetch_validated(
        &self,
        requested: ValidatedWebUrl,
        budget: WebByteBudget,
    ) -> FetchResult<WebFetchResult> {
        self.fetch_with_validator(
            requested,
            ValidatedWebUrl::from_url,
            crate::constants::MAX_WEB_BODY_BYTES,
            budget,
        )
        .await
    }

    async fn fetch_with_validator(
        &self,
        requested: ValidatedWebUrl,
        validate: fn(reqwest::Url) -> FetchResult<ValidatedWebUrl>,
        max_body_bytes: usize,
        budget: WebByteBudget,
    ) -> FetchResult<WebFetchResult> {
        let requested_url = requested.as_str().to_string();
        let mut current = requested;

        for redirect_count in 0..=MAX_REDIRECTS {
            let download_permits = acquire_download_permits(current.as_url())
                .await
                .map_err(|error| WebFetchError::Transport(error.to_string()))?;
            let response = self
                .client
                .get(current.as_url().clone())
                .send()
                .await
                .map_err(|error| map_native_transport_error(error, current.as_str()))?;
            let status = response.status();
            if is_followable_redirect(status) {
                if redirect_count == MAX_REDIRECTS {
                    return Err(WebFetchError::RedirectDenied(format!(
                        "exceeded the {MAX_REDIRECTS}-redirect limit"
                    )));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .ok_or_else(|| {
                        WebFetchError::RedirectDenied(format!(
                            "redirect response {status} has no Location header"
                        ))
                    })?
                    .to_str()
                    .map_err(|_| {
                        WebFetchError::RedirectDenied(
                            "redirect Location header is not valid text".to_string(),
                        )
                    })?;
                current = validated_redirect_destination(&current, location, validate)?;
                continue;
            }

            if !status.is_success() {
                return Err(WebFetchError::HttpStatus {
                    status: status.as_u16(),
                    url: crate::utils::sanitize_url_for_display(current.as_str()),
                });
            }

            // Parse and authorize the media type before buffering the body.
            let media = ResponseMedia::from_headers(response.headers())?;
            let final_url = validate(response.url().clone())?;
            let body = read_body_capped(response, max_body_bytes, &budget).await?;
            drop(download_permits);

            let source_bytes = body.len();
            let extraction_permit = extraction_semaphore().acquire_owned().await.map_err(|_| {
                WebFetchError::Extraction("web extraction limiter is closed".to_string())
            })?;
            let final_url_for_extract = final_url.as_str().to_string();
            let media_for_extract = media.clone();
            let (title, content, extraction, charset) = tokio::task::spawn_blocking(move || {
                // Keep the permit inside the blocking task. Cancelling the
                // async caller drops its JoinHandle but cannot stop an already
                // running blocking parser; the task must remain accounted for.
                let _extraction_permit = extraction_permit;
                decode_and_extract(body, &final_url_for_extract, &media_for_extract)
            })
            .await
            .map_err(|error| {
                WebFetchError::Extraction(format!("content extraction task failed: {error}"))
            })??;

            let output_bytes = content.len();
            return Ok(WebFetchResult {
                requested_url,
                final_url: Some(final_url.as_str().to_string()),
                status: Some(status.as_u16()),
                media_type: media.media_type,
                charset: Some(charset),
                backend: FetchBackend::Native,
                extraction,
                source_bytes,
                output_bytes,
                truncated: false,
                title,
                content,
            });
        }

        Err(WebFetchError::RedirectDenied(
            "redirect state was exhausted".to_string(),
        ))
    }
}

fn native_client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .user_agent(NATIVE_FETCH_UA)
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        // A proxy resolves the target outside this resolver and would make the
        // proxy address, rather than the requested destination, the object being
        // vetted. Native fetches never inherit HTTP(S)_PROXY / ALL_PROXY.
        .no_proxy()
}

fn is_followable_redirect(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::MOVED_PERMANENTLY
            | reqwest::StatusCode::FOUND
            | reqwest::StatusCode::SEE_OTHER
            | reqwest::StatusCode::TEMPORARY_REDIRECT
            | reqwest::StatusCode::PERMANENT_REDIRECT
    )
}

fn validated_redirect_destination(
    current: &ValidatedWebUrl,
    location: &str,
    validate: fn(reqwest::Url) -> FetchResult<ValidatedWebUrl>,
) -> FetchResult<ValidatedWebUrl> {
    let next = current
        .as_url()
        .join(location)
        .map_err(|error| WebFetchError::RedirectDenied(error.to_string()))?;
    let next = validate(next)?;
    if current.as_url().scheme() == "https" && next.as_url().scheme() != "https" {
        return Err(WebFetchError::RedirectDenied(format!(
            "HTTPS-to-HTTP redirect from {} to {}",
            crate::utils::sanitize_url_for_display(current.as_str()),
            crate::utils::sanitize_url_for_display(next.as_str())
        )));
    }
    Ok(next)
}

#[async_trait]
impl FetchProvider for NativeFetchClient {
    async fn fetch(&self, url: &str, budget: WebByteBudget) -> FetchResult<WebFetchResult> {
        self.fetch_validated(ValidatedWebUrl::parse(url)?, budget)
            .await
    }
}

/// A reqwest DNS resolver that rejects the entire answer when any address is
/// not globally routable. The request connects to the exact vetted answer, so
/// a second DNS lookup cannot rebind the name after authorization.
struct VettingResolver;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct DestinationPolicyError(String);

impl reqwest::dns::Resolve for VettingResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            // Port 0 is a placeholder — reqwest overrides it with the URL's port.
            // We only need the resolved IPs in order to vet them.
            let addrs: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            vet_resolved_addresses(&host, &addrs)
                .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn vet_resolved_addresses(
    host: &str,
    addrs: &[std::net::SocketAddr],
) -> std::result::Result<(), DestinationPolicyError> {
    if addrs.is_empty() {
        return Err(DestinationPolicyError(format!(
            "'{host}' resolved to no addresses"
        )));
    }
    if addrs
        .iter()
        .any(|addr| classify_host(&addr.ip().to_string()).is_internal())
    {
        return Err(DestinationPolicyError(format!(
            "refusing to connect to '{host}' — its DNS answer contains a non-public address"
        )));
    }
    Ok(())
}

fn map_native_transport_error(error: reqwest::Error, requested_url: &str) -> WebFetchError {
    let mut source = std::error::Error::source(&error);
    while let Some(current) = source {
        if let Some(policy) = current.downcast_ref::<DestinationPolicyError>() {
            return WebFetchError::DestinationDenied(policy.0.clone());
        }
        source = current.source();
    }
    WebFetchError::Transport(crate::utils::redact_secrets(&format!(
        "{}: {error}",
        crate::utils::sanitize_url_for_display(requested_url)
    )))
}

struct DownloadPermits {
    _global: tokio::sync::OwnedSemaphorePermit,
    _origin: tokio::sync::OwnedSemaphorePermit,
}

async fn acquire_download_permits(url: &reqwest::Url) -> Result<DownloadPermits> {
    download_limiter().acquire(url).await
}

struct DownloadLimiter {
    global: Arc<Semaphore>,
    origins: Mutex<HashMap<String, Weak<Semaphore>>>,
    per_origin: usize,
}

impl DownloadLimiter {
    fn new(global: usize, per_origin: usize) -> Self {
        Self {
            global: Arc::new(Semaphore::new(global)),
            origins: Mutex::new(HashMap::new()),
            per_origin,
        }
    }

    async fn acquire(&self, url: &reqwest::Url) -> Result<DownloadPermits> {
        let origin = self
            .origin_semaphore(url)
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("web origin limiter is closed"))?;
        let global = self
            .global
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("web download limiter is closed"))?;
        Ok(DownloadPermits {
            _global: global,
            _origin: origin,
        })
    }

    fn origin_semaphore(&self, url: &reqwest::Url) -> Arc<Semaphore> {
        let key = format!(
            "{}://{}:{}",
            url.scheme(),
            url.host_str().unwrap_or_default().to_ascii_lowercase(),
            url.port_or_known_default().unwrap_or_default()
        );
        let mut semaphores = self
            .origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        semaphores.retain(|_, semaphore| semaphore.strong_count() > 0);
        if let Some(semaphore) = semaphores.get(&key).and_then(Weak::upgrade) {
            return semaphore;
        }
        let semaphore = Arc::new(Semaphore::new(self.per_origin));
        semaphores.insert(key, Arc::downgrade(&semaphore));
        semaphore
    }
}

fn download_limiter() -> &'static DownloadLimiter {
    DOWNLOAD_LIMITER.get_or_init(|| {
        DownloadLimiter::new(
            crate::constants::MAX_WEB_DOWNLOAD_CONCURRENCY,
            crate::constants::MAX_WEB_PER_ORIGIN_CONCURRENCY,
        )
    })
}

pub(super) fn extraction_semaphore() -> Arc<Semaphore> {
    EXTRACTION_SEMAPHORE
        .get_or_init(|| {
            Arc::new(Semaphore::new(
                crate::constants::MAX_WEB_EXTRACTION_CONCURRENCY,
            ))
        })
        .clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Html,
    Xhtml,
    PlainText,
    Markdown,
    Json,
    Xml,
}

#[derive(Debug, Clone)]
struct ResponseMedia {
    media_type: Option<String>,
    charset: Option<String>,
    kind: MediaKind,
}

impl ResponseMedia {
    fn from_headers(headers: &reqwest::header::HeaderMap) -> FetchResult<Self> {
        let Some(raw) = headers.get(reqwest::header::CONTENT_TYPE) else {
            return Ok(Self {
                media_type: None,
                charset: None,
                kind: MediaKind::PlainText,
            });
        };
        let raw = raw.to_str().map_err(|_| {
            WebFetchError::InvalidMedia("Content-Type header is not valid text".to_string())
        })?;
        let parsed: mime::Mime = raw.parse().map_err(|error| {
            WebFetchError::InvalidMedia(format!("Content-Type '{raw}': {error}"))
        })?;
        let media_type = parsed.essence_str().to_ascii_lowercase();
        let charset = parsed
            .get_param(mime::CHARSET)
            .map(|value| value.as_str().to_string());
        let kind = match media_type.as_str() {
            "text/html" => MediaKind::Html,
            "application/xhtml+xml" => MediaKind::Xhtml,
            "text/markdown" | "text/x-markdown" | "application/markdown" => MediaKind::Markdown,
            "application/json" | "text/json" => MediaKind::Json,
            "application/xml" | "text/xml" => MediaKind::Xml,
            _ if media_type.ends_with("+json") => MediaKind::Json,
            _ if media_type.ends_with("+xml") => MediaKind::Xml,
            _ if media_type.starts_with("text/") => MediaKind::PlainText,
            _ => {
                return Err(WebFetchError::UnsupportedMedia(media_type));
            },
        };
        Ok(Self {
            media_type: Some(media_type),
            charset,
            kind,
        })
    }
}

fn decode_and_extract(
    body: Vec<u8>,
    final_url: &str,
    media: &ResponseMedia,
) -> FetchResult<(String, String, ExtractionMode, String)> {
    if media.media_type.is_none() && body.contains(&0) {
        return Err(WebFetchError::UnsupportedMedia(
            "unlabeled binary data".to_string(),
        ));
    }
    let (decoded, charset) = decode_body(&body, media)?;
    let (title, content, extraction) = match media.kind {
        MediaKind::Html | MediaKind::Xhtml => extract_readable(&decoded, final_url)?,
        MediaKind::PlainText => (
            String::new(),
            decoded.trim().to_string(),
            ExtractionMode::PlainText,
        ),
        MediaKind::Markdown => (
            String::new(),
            decoded.trim().to_string(),
            ExtractionMode::Markdown,
        ),
        MediaKind::Json => {
            let _: serde_json::Value = serde_json::from_str(&decoded).map_err(|error| {
                WebFetchError::Extraction(format!("invalid JSON response body: {error}"))
            })?;
            (String::new(), decoded, ExtractionMode::Json)
        },
        MediaKind::Xml => (
            String::new(),
            decoded.trim().to_string(),
            ExtractionMode::Xml,
        ),
    };
    if content.trim().is_empty() {
        return Err(WebFetchError::EmptyContent);
    }
    Ok((title, content, extraction, charset))
}

fn decode_body(body: &[u8], media: &ResponseMedia) -> FetchResult<(String, String)> {
    // XML defines byte signatures for BOM-less UTF-16 and UTF-32. Check the
    // four-byte forms before encoding_rs's BOM helper: an UTF-32LE BOM starts
    // with the UTF-16LE BOM and would otherwise be misclassified.
    let signature_encoding = xml_signature_encoding(body, media.kind)?;
    let (encoding, bom_len) = if let Some(encoding) = signature_encoding {
        (encoding, 0)
    } else if let Some((encoding, bom_len)) = Encoding::for_bom(body) {
        (encoding, bom_len)
    } else {
        let declared = media.charset.as_deref().or_else(|| match media.kind {
            MediaKind::Html => find_ascii_assignment(body, b"charset"),
            MediaKind::Xhtml => find_ascii_assignment(body, b"charset")
                .or_else(|| find_ascii_assignment(body, b"encoding")),
            MediaKind::Xml => find_ascii_assignment(body, b"encoding"),
            _ => None,
        });
        let encoding = match declared {
            Some(label) => Encoding::for_label(label.as_bytes())
                .ok_or_else(|| WebFetchError::UnsupportedCharset(label.to_string()))?,
            None => UTF_8,
        };
        (encoding, 0)
    };
    let (decoded, had_errors) = encoding.decode_without_bom_handling(&body[bom_len..]);
    if had_errors {
        return Err(WebFetchError::Decode(encoding.name().to_string()));
    }
    Ok((decoded.into_owned(), encoding.name().to_ascii_lowercase()))
}

fn xml_signature_encoding(body: &[u8], kind: MediaKind) -> FetchResult<Option<&'static Encoding>> {
    let Some(prefix) = body.get(..4) else {
        return Ok(None);
    };
    // UTF-32 BOMs apply regardless of MIME and must be checked before the
    // UTF-16 BOM prefix they share.
    match prefix {
        [0x00, 0x00, 0xfe, 0xff] => {
            return Err(WebFetchError::UnsupportedCharset("utf-32be".to_string()));
        },
        [0xff, 0xfe, 0x00, 0x00] => {
            return Err(WebFetchError::UnsupportedCharset("utf-32le".to_string()));
        },
        _ => {},
    }
    if !matches!(kind, MediaKind::Xml | MediaKind::Xhtml) {
        return Ok(None);
    }
    match prefix {
        // UTF-32 is deliberately diagnosed rather than fed to UTF-8/UTF-16:
        // encoding_rs does not implement it.
        [0x00, 0x00, 0x00, 0x3c] => Err(WebFetchError::UnsupportedCharset("utf-32be".to_string())),
        [0x3c, 0x00, 0x00, 0x00] => Err(WebFetchError::UnsupportedCharset("utf-32le".to_string())),
        [0x00, 0x3c, 0x00, 0x3f] => Ok(Some(UTF_16BE)),
        [0x3c, 0x00, 0x3f, 0x00] => Ok(Some(UTF_16LE)),
        // XML's EBCDIC signature is recognizable but unsupported here.
        [0x4c, 0x6f, 0xa7, 0x94] => Err(WebFetchError::UnsupportedCharset("ebcdic".to_string())),
        _ => Ok(None),
    }
}

fn find_ascii_assignment<'a>(body: &'a [u8], name: &[u8]) -> Option<&'a str> {
    let prefix = &body[..body.len().min(4096)];
    let lower: Vec<u8> = prefix.iter().map(u8::to_ascii_lowercase).collect();
    let mut search_from = 0;
    while search_from + name.len() <= lower.len() {
        let Some(relative_start) = lower[search_from..]
            .windows(name.len())
            .position(|window| window == name)
        else {
            break;
        };
        let start = search_from + relative_start + name.len();
        let mut cursor = start;
        while lower.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if lower.get(cursor) != Some(&b'=') {
            search_from = start;
            continue;
        }
        cursor += 1;
        while lower.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        let quote = lower
            .get(cursor)
            .copied()
            .filter(|byte| matches!(byte, b'\'' | b'"'));
        if quote.is_some() {
            cursor += 1;
        }
        let end = lower[cursor..]
            .iter()
            .position(|byte| {
                quote.map_or_else(
                    || byte.is_ascii_whitespace() || matches!(byte, b';' | b'>' | b'/'),
                    |quote| *byte == quote,
                )
            })
            .map_or(lower.len(), |end| end + cursor);
        if end > cursor {
            return std::str::from_utf8(&prefix[cursor..end]).ok();
        }
        search_from = start;
    }
    None
}

/// Extract a page's title + main content (as markdown) from raw HTML. Uses a
/// readability pass to drop nav/boilerplate, then converts the isolated content
/// to markdown (preserving links). Falls back to whole-document conversion when
/// readability can't isolate useful content.
fn extract_readable(html: &str, url: &str) -> FetchResult<(String, String, ExtractionMode)> {
    use dom_smoothie::Readability;

    if let Ok(mut readability) = Readability::new(html, Some(url), None)
        && let Ok(article) = readability.parse()
    {
        let content_html = article.content.to_string();
        let markdown = htmd::convert(&content_html).map_err(|error| {
            WebFetchError::Extraction(format!(
                "failed to convert readable HTML to markdown: {error}"
            ))
        })?;
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
            return Ok((title, markdown.to_string(), ExtractionMode::Readability));
        }
    }

    let markdown = htmd::convert(html).map_err(|error| {
        WebFetchError::Extraction(format!("failed to convert HTML to markdown: {error}"))
    })?;
    let markdown = markdown.trim();
    if markdown.is_empty() {
        return Err(WebFetchError::EmptyContent);
    }
    Ok((
        fallback_title(html),
        markdown.to_string(),
        ExtractionMode::HtmlToMarkdown,
    ))
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
            let full_content = truncate_content(&content, crate::constants::WEB_CONTENT_MAX_CHARS);
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
        let url = crate::utils::sanitize_url_for_display(&result.url);
        formatted.push_str(&format!(
            "[{}] Title: {}\nURL: {}\nContent:\n{}\n---\n",
            i + 1,
            result.title,
            url,
            result.full_content
        ));
    }

    formatted.push_str("[/SEARCH_RESULTS]\n\n");

    // Source list for citation (behavior governed by system prompt)
    formatted.push_str("Sources:\n");
    for (i, result) in results.iter().enumerate() {
        let url = crate::utils::sanitize_url_for_display(&result.url);
        formatted.push_str(&format!("{}. {} - {}\n", i + 1, result.title, url));
    }

    formatted
}

/// Read a reqwest response body, refusing to buffer more than `max_bytes`.
/// `Response::json`/`bytes` buffer the whole body unbounded; a compromised or
/// misconfigured endpoint could return a multi-gigabyte body and OOM the
/// (long-lived) process. We reject early on an oversized `Content-Length` and
/// also enforce the cap while streaming (a lying or absent header can't bypass
/// it) (#28).
async fn read_body_capped(
    response: reqwest::Response,
    max_bytes: usize,
    budget: &WebByteBudget,
) -> FetchResult<Vec<u8>> {
    use futures::StreamExt;
    if let Some(len) = response.content_length()
        && len > max_bytes as u64
    {
        return Err(WebFetchError::BodyTooLarge { limit: max_bytes });
    }
    if let Some(len) = response.content_length()
        && usize::try_from(len).is_ok_and(|len| len > budget.remaining())
    {
        return Err(WebFetchError::TurnBudgetExceeded {
            limit: crate::constants::MAX_WEB_TURN_BYTES,
        });
    }
    if budget.remaining() == 0 {
        return Err(WebFetchError::TurnBudgetExceeded {
            limit: crate::constants::MAX_WEB_TURN_BYTES,
        });
    }
    let mut stream = response.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| WebFetchError::Transport(error.to_string()))?;
        // Charge every decoded chunk once it has crossed the transport
        // boundary, including the chunk that makes a single response fail its
        // own cap. Repeated oversized responses must not evade the turn total.
        if budget.charge(chunk.len()).is_err() {
            return Err(WebFetchError::TurnBudgetExceeded {
                limit: crate::constants::MAX_WEB_TURN_BYTES,
            });
        }
        if chunk.len() > max_bytes.saturating_sub(buf.len()) {
            return Err(WebFetchError::BodyTooLarge { limit: max_bytes });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    enum FixtureBodyMode {
        Fixed,
        Chunked,
        TruncatedChunked,
    }

    struct FixtureResponse {
        status: &'static str,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
        body_mode: FixtureBodyMode,
    }

    impl FixtureResponse {
        fn text(body: &str) -> Self {
            Self {
                status: "200 OK",
                headers: vec![("Content-Type", "text/plain; charset=utf-8".to_string())],
                body: body.as_bytes().to_vec(),
                body_mode: FixtureBodyMode::Fixed,
            }
        }

        fn chunked_text(body: &str) -> Self {
            Self {
                status: "200 OK",
                headers: vec![("Content-Type", "text/plain; charset=utf-8".to_string())],
                body: body.as_bytes().to_vec(),
                body_mode: FixtureBodyMode::Chunked,
            }
        }

        fn truncated_chunked() -> Self {
            Self {
                status: "200 OK",
                headers: vec![("Content-Type", "text/plain; charset=utf-8".to_string())],
                body: Vec::new(),
                body_mode: FixtureBodyMode::TruncatedChunked,
            }
        }

        fn redirect(location: String) -> Self {
            Self {
                status: "302 Found",
                headers: vec![("Location", location)],
                body: Vec::new(),
                body_mode: FixtureBodyMode::Fixed,
            }
        }

        fn status(status: &'static str) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: Vec::new(),
                body_mode: FixtureBodyMode::Fixed,
            }
        }

        fn gzip(body: Vec<u8>) -> Self {
            Self {
                status: "200 OK",
                headers: vec![
                    ("Content-Type", "text/plain; charset=utf-8".to_string()),
                    ("Content-Encoding", "gzip".to_string()),
                ],
                body,
                body_mode: FixtureBodyMode::Fixed,
            }
        }

        fn wire_bytes(self) -> Vec<u8> {
            let Self {
                status,
                headers,
                body,
                body_mode,
            } = self;
            let mut response = format!("HTTP/1.1 {status}\r\n");
            for (name, value) in headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            match body_mode {
                FixtureBodyMode::Fixed => {
                    response.push_str(&format!(
                        "Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    ));
                    let mut bytes = response.into_bytes();
                    bytes.extend_from_slice(&body);
                    bytes
                },
                FixtureBodyMode::Chunked => {
                    response.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
                    let mut bytes = response.into_bytes();
                    bytes.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
                    bytes.extend_from_slice(&body);
                    bytes.extend_from_slice(b"\r\n0\r\n\r\n");
                    bytes
                },
                FixtureBodyMode::TruncatedChunked => {
                    response.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
                    response.into_bytes()
                },
            }
        }
    }

    struct FixtureServer {
        base_url: String,
        task: tokio::task::JoinHandle<Vec<String>>,
    }

    impl FixtureServer {
        async fn spawn(responses: Vec<FixtureResponse>) -> Self {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind fixture listener");
            let address = listener.local_addr().expect("fixture address");
            let task = tokio::spawn(async move {
                let mut requests = Vec::with_capacity(responses.len());
                for response in responses {
                    let (mut stream, _) =
                        tokio::time::timeout(Duration::from_secs(5), listener.accept())
                            .await
                            .expect("fixture request timed out")
                            .expect("accept fixture request");
                    requests.push(read_request_headers(&mut stream).await);
                    stream
                        .write_all(&response.wire_bytes())
                        .await
                        .expect("write fixture response");
                    stream.shutdown().await.expect("close fixture response");
                }
                requests
            });
            Self {
                base_url: format!("http://{address}"),
                task,
            }
        }

        async fn requests(self) -> Vec<String> {
            tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .expect("fixture server did not finish")
                .expect("fixture server task panicked")
        }
    }

    async fn read_request_headers(stream: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
                .await
                .expect("request read timed out")
                .expect("read fixture request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            assert!(
                request.len() <= 64 * 1024,
                "fixture request headers too large"
            );
        }
        String::from_utf8(request).expect("HTTP request headers are UTF-8")
    }

    fn fixture_client() -> NativeFetchClient {
        NativeFetchClient {
            client: native_client_builder()
                .build()
                .expect("build fixture client"),
        }
    }

    async fn fixture_fetch(
        client: &NativeFetchClient,
        url: &str,
        max_body_bytes: usize,
    ) -> Result<WebFetchResult> {
        fixture_fetch_with_budget(client, url, max_body_bytes, WebByteBudget::isolated()).await
    }

    async fn fixture_fetch_with_budget(
        client: &NativeFetchClient,
        url: &str,
        max_body_bytes: usize,
        budget: WebByteBudget,
    ) -> Result<WebFetchResult> {
        let requested = ValidatedWebUrl::from_fixture_url(
            reqwest::Url::parse(url).expect("valid fixture URL"),
        )?;
        Ok(client
            .fetch_with_validator(
                requested,
                ValidatedWebUrl::from_fixture_url,
                max_body_bytes,
                budget,
            )
            .await?)
    }

    #[test]
    fn test_ollama_web_client_creation() {
        let client = OllamaWebClient::new("test-key".to_string()).unwrap();
        assert_eq!(client.api_key, "test-key");
    }

    #[test]
    fn native_client_builds_fail_closed_configuration() {
        NativeFetchClient::new().expect("hardened client should build");
    }

    #[test]
    fn configured_searxng_client_validates_its_trust_destination() {
        let client = SearxngClient::new("http://127.0.0.1:8080/base/#fragment".to_string())
            .expect("explicit self-hosted loopback is valid");
        assert_eq!(client.base_url, "http://127.0.0.1:8080/base");
        for invalid in [
            "file:///tmp/searxng",
            "https://user:password@example.test",
            "https://example.test/?token=secret",
            "not a URL",
        ] {
            assert!(
                SearxngClient::new(invalid.to_string()).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[tokio::test]
    async fn configured_searxng_never_follows_query_bearing_redirects() {
        let target = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let target_url = format!("http://{}", target.local_addr().unwrap());
        let target_seen = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_millis(250), target.accept())
                .await
                .is_ok()
        });
        let source = FixtureServer::spawn(vec![FixtureResponse::redirect(target_url)]).await;
        let client = SearxngClient::new(source.base_url.clone()).unwrap();

        let error = client
            .search("credential-shaped-query", 1, WebByteBudget::isolated())
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("302"));
        assert!(!target_seen.await.unwrap(), "redirect target was contacted");
        let requests = source.requests().await;
        assert!(requests[0].contains("credential-shaped-query"));
        assert!(!requests[0].to_ascii_lowercase().contains("referer:"));
    }

    #[tokio::test]
    async fn native_fetch_follows_manual_redirect_and_records_final_url() {
        let server = FixtureServer::spawn(vec![
            FixtureResponse::redirect("/final".to_string()),
            FixtureResponse::text("redirected content"),
        ])
        .await;
        let base_url = server.base_url.clone();
        let page = fixture_fetch(
            &fixture_client(),
            &format!("{base_url}/start#client-fragment"),
            crate::constants::MAX_WEB_BODY_BYTES,
        )
        .await
        .unwrap();

        assert_eq!(page.requested_url, format!("{base_url}/start"));
        assert_eq!(
            page.final_url.as_deref(),
            Some(format!("{base_url}/final").as_str())
        );
        assert_eq!(page.status, Some(200));
        assert_eq!(page.content, "redirected content");
        assert_eq!(page.backend, FetchBackend::Native);
        let requests = server.requests().await;
        assert!(requests[0].starts_with("GET /start HTTP/1.1"));
        assert!(requests[1].starts_with("GET /final HTTP/1.1"));
    }

    #[tokio::test]
    async fn native_fetch_rejects_private_redirect_before_connecting() {
        let server = FixtureServer::spawn(vec![FixtureResponse::redirect(
            "http://169.254.169.254/latest/meta-data".to_string(),
        )])
        .await;
        let start_url = format!("{}/start", server.base_url);
        let error = fixture_fetch(
            &fixture_client(),
            &start_url,
            crate::constants::MAX_WEB_BODY_BYTES,
        )
        .await
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("non-public"),
            "unexpected redirect error: {error:#}"
        );
        assert_eq!(server.requests().await.len(), 1);
    }

    #[tokio::test]
    async fn cross_origin_redirect_does_not_send_referer() {
        let destination = FixtureServer::spawn(vec![FixtureResponse::text("destination")]).await;
        let destination_url = destination.base_url.clone();
        let source = FixtureServer::spawn(vec![FixtureResponse::redirect(format!(
            "{destination_url}/final"
        ))])
        .await;
        let source_url = format!("{}/start?token=source-secret", source.base_url);

        let page = fixture_fetch(
            &fixture_client(),
            &source_url,
            crate::constants::MAX_WEB_BODY_BYTES,
        )
        .await
        .unwrap();
        assert_eq!(
            page.final_url.as_deref(),
            Some(format!("{destination_url}/final").as_str())
        );
        source.requests().await;
        let destination_requests = destination.requests().await;
        assert!(
            !destination_requests[0]
                .to_ascii_lowercase()
                .contains("\r\nreferer:"),
            "cross-origin request leaked Referer: {}",
            destination_requests[0]
        );
    }

    #[tokio::test]
    async fn native_client_ignores_ambient_proxy_configuration() {
        let target = FixtureServer::spawn(vec![FixtureResponse::text("direct")]).await;
        let target_url = format!("{}/resource", target.base_url);
        let proxy_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind poison proxy");
        let proxy_url = format!(
            "http://{}",
            proxy_listener.local_addr().expect("proxy address")
        );
        let (proxy_seen_tx, proxy_seen_rx) = tokio::sync::oneshot::channel();
        let proxy_task = tokio::spawn(async move {
            let (mut stream, _) = proxy_listener.accept().await.expect("accept proxy request");
            let request = read_request_headers(&mut stream).await;
            let _ = proxy_seen_tx.send(request);
            stream
                .write_all(&FixtureResponse::status("502 Bad Gateway").wire_bytes())
                .await
                .expect("write proxy response");
        });

        #[cfg(target_os = "windows")]
        let variables = vec![
            ("HTTP_PROXY", Some(proxy_url.as_str())),
            ("HTTPS_PROXY", Some(proxy_url.as_str())),
            ("ALL_PROXY", Some(proxy_url.as_str())),
            ("NO_PROXY", Some("")),
        ];
        #[cfg(not(target_os = "windows"))]
        let variables = vec![
            ("HTTP_PROXY", Some(proxy_url.as_str())),
            ("HTTPS_PROXY", Some(proxy_url.as_str())),
            ("ALL_PROXY", Some(proxy_url.as_str())),
            ("NO_PROXY", Some("")),
            ("http_proxy", Some(proxy_url.as_str())),
            ("https_proxy", Some(proxy_url.as_str())),
            ("all_proxy", Some(proxy_url.as_str())),
            ("no_proxy", Some("")),
        ];

        let page = temp_env::async_with_vars(variables, async {
            let client = fixture_client();
            fixture_fetch(&client, &target_url, crate::constants::MAX_WEB_BODY_BYTES).await
        })
        .await
        .unwrap();
        assert_eq!(page.content, "direct");
        assert!(
            tokio::time::timeout(Duration::from_millis(150), proxy_seen_rx)
                .await
                .is_err(),
            "native fetch unexpectedly connected to the ambient proxy"
        );
        proxy_task.abort();
        target.requests().await;
    }

    #[tokio::test]
    async fn native_fetch_preserves_typed_status_errors() {
        let server = FixtureServer::spawn(vec![FixtureResponse::status("404 Not Found")]).await;
        let url = format!("{}/missing", server.base_url);
        let error = fixture_fetch(
            &fixture_client(),
            &url,
            crate::constants::MAX_WEB_BODY_BYTES,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<WebFetchError>(),
            Some(WebFetchError::HttpStatus { status: 404, .. })
        ));
        assert!(format!("{error:#}").contains("HTTP 404"));
        server.requests().await;
    }

    #[tokio::test]
    async fn decoded_gzip_body_cannot_bypass_streaming_limit() {
        use base64::Engine as _;

        // 4096 ASCII 'a' bytes compressed to a small deterministic gzip member.
        let compressed = base64::engine::general_purpose::STANDARD
            .decode("H4sIAAAAAAAACu3BAQ0AAADCoKzvX8IeDigAAACgcwNz3JmcABAAAA==")
            .unwrap();
        assert!(compressed.len() < 1024);
        let server = FixtureServer::spawn(vec![FixtureResponse::gzip(compressed)]).await;
        let url = format!("{}/compressed", server.base_url);
        let budget = WebByteBudget::isolated();
        let error = fixture_fetch_with_budget(&fixture_client(), &url, 1024, budget.clone())
            .await
            .unwrap_err();

        assert!(
            matches!(
                error.downcast_ref::<WebFetchError>(),
                Some(WebFetchError::BodyTooLarge { limit: 1024 })
            ),
            "decoded body limit was not enforced: {error:#}"
        );
        assert!(
            crate::constants::MAX_WEB_TURN_BYTES - budget.remaining() > 1024,
            "the decoded overflow chunk was not charged to the turn budget"
        );
        server.requests().await;
    }

    #[tokio::test]
    async fn decoded_chunks_charge_the_shared_turn_budget_before_buffering() {
        let server = FixtureServer::spawn(vec![FixtureResponse::text("two bytes")]).await;
        let url = format!("{}/budget", server.base_url);
        let budget = WebByteBudget::isolated();
        budget
            .charge(crate::constants::MAX_WEB_TURN_BYTES - 1)
            .unwrap();

        let error = fixture_fetch_with_budget(
            &fixture_client(),
            &url,
            crate::constants::MAX_WEB_BODY_BYTES,
            budget,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<WebFetchError>(),
            Some(WebFetchError::TurnBudgetExceeded { .. })
        ));
        server.requests().await;
    }

    #[tokio::test]
    async fn turn_budget_overflow_prevents_polling_later_chunked_bodies() {
        let server = FixtureServer::spawn(vec![
            FixtureResponse::chunked_text("two bytes"),
            FixtureResponse::truncated_chunked(),
        ])
        .await;
        let url = format!("{}/budget", server.base_url);
        let budget = WebByteBudget::isolated();
        budget
            .charge(crate::constants::MAX_WEB_TURN_BYTES - 1)
            .unwrap();

        let first = fixture_fetch_with_budget(
            &fixture_client(),
            &url,
            crate::constants::MAX_WEB_BODY_BYTES,
            budget.clone(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            first.downcast_ref::<WebFetchError>(),
            Some(WebFetchError::TurnBudgetExceeded { .. })
        ));
        assert_eq!(budget.remaining(), 0);

        // The second response advertises chunked framing but closes before a
        // chunk arrives. A body poll would therefore produce a transport
        // error; the exhausted budget must win before the stream is polled.
        let second = fixture_fetch_with_budget(
            &fixture_client(),
            &url,
            crate::constants::MAX_WEB_BODY_BYTES,
            budget.clone(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                second.downcast_ref::<WebFetchError>(),
                Some(WebFetchError::TurnBudgetExceeded { .. })
            ),
            "later body was polled after budget exhaustion: {second:#}"
        );
        assert_eq!(budget.remaining(), 0);
        server.requests().await;
    }

    #[test]
    fn validated_url_normalizes_and_rejects_unsafe_destinations() {
        let url = ValidatedWebUrl::parse("https://example.com/path?q=1#private-state").unwrap();
        assert_eq!(url.as_str(), "https://example.com/path?q=1");

        for unsafe_url in [
            "file:///etc/passwd",
            "https://user:password@example.com/",
            "http://127.0.0.1/",
            "http://2130706433/",
            "http://0x7f000001/",
            "http://169.254.169.254/latest/meta-data/",
            "http://198.18.0.1/",
            "http://[::ffff:127.0.0.1]/",
            "http://metadata.google.internal/",
        ] {
            assert!(
                ValidatedWebUrl::parse(unsafe_url).is_err(),
                "{unsafe_url} must be rejected"
            );
        }
        assert!(matches!(
            ValidatedWebUrl::parse("http://127.0.0.1/"),
            Err(WebFetchError::DestinationDenied(_))
        ));
    }

    #[test]
    fn redirect_destination_is_revalidated_and_cannot_downgrade() {
        let https = ValidatedWebUrl::parse("https://example.com/a/start").unwrap();
        let relative =
            validated_redirect_destination(&https, "../final#section", ValidatedWebUrl::from_url)
                .unwrap();
        assert_eq!(relative.as_str(), "https://example.com/final");

        assert!(matches!(
            validated_redirect_destination(
                &https,
                "http://example.com/plaintext",
                ValidatedWebUrl::from_url,
            ),
            Err(WebFetchError::RedirectDenied(_))
        ));
        assert!(
            validated_redirect_destination(
                &https,
                "http://127.0.0.1/admin",
                ValidatedWebUrl::from_url,
            )
            .is_err()
        );
        assert!(
            validated_redirect_destination(
                &https,
                "http://169.254.169.254/latest",
                ValidatedWebUrl::from_url,
            )
            .is_err()
        );

        let http = ValidatedWebUrl::parse("http://example.com/start").unwrap();
        assert!(
            validated_redirect_destination(
                &http,
                "https://example.com/final",
                ValidatedWebUrl::from_url,
            )
            .is_ok()
        );
        assert!(is_followable_redirect(reqwest::StatusCode::FOUND));
        assert!(!is_followable_redirect(reqwest::StatusCode::NOT_MODIFIED));
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
    fn format_results_sanitizes_urls_without_mutating_results() {
        let raw = "https://user:hunter2@example.com/path?token=opaque-secret&ok=yes#fragment";
        let results = vec![SearchResult {
            title: "Sensitive link".to_string(),
            url: raw.to_string(),
            snippet: String::new(),
            full_content: "content".to_string(),
        }];

        let formatted = format_results(&results);
        assert_eq!(results[0].url, raw, "transport value must remain unchanged");
        for secret in ["user", "hunter2", "opaque-secret", "fragment"] {
            assert!(!formatted.contains(secret), "leaked {secret}: {formatted}");
        }
        assert!(formatted.contains("ok=yes"));
        assert!(formatted.contains("token=%5BREDACTED%5D"));
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
        let (title, md, mode) = extract_readable(html, "https://example.com/page").unwrap();
        assert!(!title.is_empty(), "title extracted, got {title:?}");
        assert!(matches!(
            mode,
            ExtractionMode::Readability | ExtractionMode::HtmlToMarkdown
        ));
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
        let (title, md, _) = extract_readable(html, "https://example.com").unwrap();
        assert_eq!(title, "Bare");
        assert!(md.contains("just a snippet"));
    }

    #[test]
    fn html_extraction_uses_final_url_as_relative_link_base() {
        let html = r#"<html><body><article><h1>Page</h1>
            <p>This article has enough useful prose for extraction and a
            <a href="../source">relative source link</a> that must be resolved
            against the final response URL after redirects.</p></article></body></html>"#;
        let (_, markdown, _) =
            extract_readable(html, "https://example.com/redirected/page").unwrap();
        assert!(
            markdown.contains("https://example.com/source"),
            "relative link was not resolved against final URL: {markdown}"
        );
    }

    fn response_media(raw: Option<&str>) -> FetchResult<ResponseMedia> {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(raw) = raw {
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_str(raw).unwrap(),
            );
        }
        ResponseMedia::from_headers(&headers)
    }

    #[test]
    fn content_type_dispatch_is_exact_and_rejects_binary() {
        assert_eq!(
            response_media(Some("text/html; charset=utf-8"))
                .unwrap()
                .kind,
            MediaKind::Html
        );
        assert_eq!(
            response_media(Some("application/problem+json"))
                .unwrap()
                .kind,
            MediaKind::Json
        );
        assert_eq!(
            response_media(Some("application/atom+xml")).unwrap().kind,
            MediaKind::Xml
        );
        assert_eq!(
            response_media(Some("text/markdown")).unwrap().kind,
            MediaKind::Markdown
        );
        assert!(response_media(Some("application/octet-stream")).is_err());
        assert!(response_media(Some("text/html-ish")).is_ok());
        assert_eq!(
            response_media(Some("text/html-ish")).unwrap().kind,
            MediaKind::PlainText
        );
    }

    #[test]
    fn declared_charset_is_decoded_without_lossy_replacement() {
        let media = response_media(Some("text/plain; charset=windows-1252")).unwrap();
        let (decoded, charset) = decode_body(b"caf\xe9", &media).unwrap();
        assert_eq!(decoded, "caf\u{e9}");
        assert_eq!(charset, "windows-1252");

        let utf8 = response_media(Some("text/plain")).unwrap();
        assert!(decode_body(&[0xff], &utf8).is_err());
    }

    #[test]
    fn html_meta_charset_is_honored_when_header_omits_it() {
        let media = response_media(Some("text/html")).unwrap();
        let body = b"<meta charset=\"windows-1252\"><p>caf\xe9</p>";
        let (decoded, charset) = decode_body(body, &media).unwrap();
        assert!(decoded.contains("caf\u{e9}"));
        assert_eq!(charset, "windows-1252");
    }

    #[test]
    fn xhtml_xml_declaration_charset_is_honored() {
        let media = response_media(Some("application/xhtml+xml")).unwrap();
        assert_eq!(media.kind, MediaKind::Xhtml);
        let body = b"<?xml version=\"1.0\" encoding=\"windows-1252\"?><html><p>caf\xe9</p></html>";
        let (decoded, charset) = decode_body(body, &media).unwrap();
        assert!(decoded.contains("caf\u{e9}"));
        assert_eq!(charset, "windows-1252");
    }

    #[test]
    fn bomless_xml_and_xhtml_utf16_signatures_are_decoded() {
        let source = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><root>café</root>";
        for media_type in ["application/xml", "application/xhtml+xml"] {
            let media = response_media(Some(media_type)).unwrap();
            for (big_endian, expected_charset) in [(false, "utf-16le"), (true, "utf-16be")] {
                let body = source
                    .encode_utf16()
                    .flat_map(|unit| {
                        if big_endian {
                            unit.to_be_bytes()
                        } else {
                            unit.to_le_bytes()
                        }
                    })
                    .collect::<Vec<_>>();
                let (decoded, charset) = decode_body(&body, &media).unwrap();
                assert_eq!(decoded, source);
                assert_eq!(charset, expected_charset);
            }
        }
    }

    #[test]
    fn xml_utf32_signatures_return_a_typed_unsupported_charset() {
        let media = response_media(Some("application/xml")).unwrap();
        for (body, expected) in [
            (vec![0x00, 0x00, 0x00, 0x3c], "utf-32be"),
            (vec![0x3c, 0x00, 0x00, 0x00], "utf-32le"),
            (vec![0x00, 0x00, 0xfe, 0xff], "utf-32be"),
            (vec![0xff, 0xfe, 0x00, 0x00], "utf-32le"),
        ] {
            assert_eq!(
                decode_body(&body, &media),
                Err(WebFetchError::UnsupportedCharset(expected.to_string()))
            );
        }
    }

    #[test]
    fn json_and_xml_are_not_interpreted_as_html() {
        let json_media = response_media(Some("application/json")).unwrap();
        let json_source = " \n{\"markup\":\"<h1>literal</h1>\",\"items\":[1,2]}\t ";
        let (_, json, mode, _) = decode_and_extract(
            json_source.as_bytes().to_vec(),
            "https://example.com/data",
            &json_media,
        )
        .unwrap();
        assert_eq!(mode, ExtractionMode::Json);
        assert_eq!(json, json_source);

        let invalid = decode_and_extract(
            br#"{"markup": "unterminated}"#.to_vec(),
            "https://example.com/data",
            &json_media,
        )
        .unwrap_err();
        assert!(matches!(invalid, WebFetchError::Extraction(_)));

        let xml_media = response_media(Some("application/xml")).unwrap();
        let xml_source = "<root><script>literal text</script></root>";
        let (_, xml, mode, _) = decode_and_extract(
            xml_source.as_bytes().to_vec(),
            "https://example.com/data",
            &xml_media,
        )
        .unwrap();
        assert_eq!(mode, ExtractionMode::Xml);
        assert_eq!(xml, xml_source);
    }

    #[test]
    fn empty_extractions_are_errors() {
        let plain = response_media(Some("text/plain")).unwrap();
        assert!(decode_and_extract(b"  \r\n".to_vec(), "https://example.com", &plain).is_err());
        let html = response_media(Some("text/html")).unwrap();
        assert!(
            decode_and_extract(
                b"<html><head></head><body></body></html>".to_vec(),
                "https://example.com",
                &html,
            )
            .is_err()
        );
        let unlabeled = response_media(None).unwrap();
        assert!(
            decode_and_extract(
                b"GIF89a\0binary".to_vec(),
                "https://example.com/image",
                &unlabeled,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn vetting_resolver_rejects_a_name_resolving_to_loopback() {
        use reqwest::dns::Resolve;
        use std::str::FromStr;
        // `localhost` resolves (via the hosts file, no network) to 127.0.0.1/::1
        // — both internal, so the connect-time resolver must fail closed. This is
        // the half of the DNS-rebinding guard a pre-flight resolve can't cover,
        // because it runs on the address the connection actually uses.
        let name = reqwest::dns::Name::from_str("localhost").expect("valid host name");
        assert!(
            VettingResolver.resolve(name).await.is_err(),
            "a name resolving to a loopback address must be rejected"
        );
    }

    #[test]
    fn resolved_address_vetting_rejects_empty_and_mixed_answers() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let public = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), 443);
        let second_public = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 443);
        let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 443);

        assert!(vet_resolved_addresses("example.com", &[]).is_err());
        assert!(vet_resolved_addresses("example.com", &[public, loopback]).is_err());
        assert!(vet_resolved_addresses("example.com", &[loopback, public]).is_err());
        assert!(vet_resolved_addresses("example.com", &[public, second_public]).is_ok());
    }

    #[tokio::test]
    async fn download_limiter_enforces_global_origin_and_cancellation_release() {
        let limiter = DownloadLimiter::new(
            crate::constants::MAX_WEB_DOWNLOAD_CONCURRENCY,
            crate::constants::MAX_WEB_PER_ORIGIN_CONCURRENCY,
        );

        let mut global_permits = Vec::new();
        for index in 0..crate::constants::MAX_WEB_DOWNLOAD_CONCURRENCY {
            let url = reqwest::Url::parse(&format!("https://origin-{index}.example.test/"))
                .expect("test URL");
            global_permits.push(limiter.acquire(&url).await.expect("global permit"));
        }
        let ninth_url = reqwest::Url::parse("https://ninth.example.test/").unwrap();
        let mut ninth = Box::pin(limiter.acquire(&ninth_url));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut ninth)
                .await
                .is_err(),
            "a ninth global download was admitted"
        );
        drop(global_permits.pop());
        let ninth_permit = tokio::time::timeout(Duration::from_secs(1), &mut ninth)
            .await
            .expect("global waiter was not released")
            .expect("global permit");
        drop(ninth_permit);
        drop(global_permits);

        let same_origin = reqwest::Url::parse("https://same.example.test/page").unwrap();
        let first = limiter.acquire(&same_origin).await.unwrap();
        let second = limiter.acquire(&same_origin).await.unwrap();
        let mut third = Box::pin(limiter.acquire(&same_origin));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut third)
                .await
                .is_err(),
            "a third same-origin download was admitted"
        );
        drop(first);
        let third = tokio::time::timeout(Duration::from_secs(1), &mut third)
            .await
            .expect("origin waiter was not released")
            .expect("origin permit");
        drop(third);
        drop(second);

        let cancellation_limiter = Arc::new(DownloadLimiter::new(1, 1));
        let held_url = reqwest::Url::parse("https://cancel.example.test/").unwrap();
        let holder_limiter = cancellation_limiter.clone();
        let holder_url = held_url.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let holder = tokio::spawn(async move {
            let _permit = holder_limiter.acquire(&holder_url).await.unwrap();
            let _ = ready_tx.send(());
            std::future::pending::<()>().await;
        });
        ready_rx.await.expect("holder acquired its permit");
        let mut waiter = Box::pin(cancellation_limiter.acquire(&held_url));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut waiter)
                .await
                .is_err()
        );
        holder.abort();
        let _ = holder.await;
        let released = tokio::time::timeout(Duration::from_secs(1), &mut waiter)
            .await
            .expect("cancelled holder leaked its permits")
            .expect("released permit");
        drop(released);
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
