//! Web tools: `web_search` and `web_fetch`.
//!
//! Each tool holds a pluggable backend (`web_client::SearchProvider` /
//! `FetchProvider`) selected from `[web]` config: `web_fetch` defaults to a
//! native in-process fetch (no key), while `web_search = "auto"` uses the
//! managed local SearXNG bundle. Cloud routing is selected explicitly. This
//! tool layer owns cancellation plumbing, snapshots, and multi-query fan-out;
//! the backend owns the transport and destination policy.

use mermaid_domain::ProgressEvent;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use futures::{StreamExt, stream};

use mermaid_domain::{FetchBackend, SearchBackend, WebConfig};
use mermaid_domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};

use super::super::ctx::ExecContext;
use super::ToolExecutor;
use super::web_client::{
    FetchProvider, ManagedSearxngBackend, NativeFetchClient, OllamaWebClient, SearchProvider,
    SearxngClient, ValidatedWebUrl, WebFetchError, WebFetchResult, format_results,
};

/// Where a backend's traffic terminates. `trust_destination` says this in
/// prose for humans; this says it in a form callers can branch on, so
/// disclosure decisions never depend on matching English strings that a later
/// wording change would silently break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Egress {
    /// Terminates on this machine: direct sockets, or a local child process.
    OnMachine,
    /// Reaches a third party, or a host that cannot be proven to be loopback.
    /// Configured-endpoint backends land here even when the operator points
    /// them at localhost — over-disclosing is the safe direction to err.
    OffMachine,
}

/// User-visible availability for one web capability. This contains no
/// credentials and is safe for doctor/UI diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebCapabilityStatus {
    pub available: bool,
    pub backend: &'static str,
    pub trust_destination: &'static str,
    pub egress: Egress,
    pub reason: Option<String>,
}

/// Single source of truth for web backend selection and viability. Registry,
/// doctor, provider adapters, and child registries consume this result instead
/// of independently guessing from an Ollama environment variable.
pub struct WebCapabilities {
    pub fetch: WebCapabilityStatus,
    pub search: WebCapabilityStatus,
    fetch_backend: Option<Arc<dyn FetchProvider>>,
    search_backend: Option<Arc<dyn SearchProvider>>,
}

impl WebCapabilities {
    pub fn resolve(web: &WebConfig) -> Self {
        let needs_ollama_key = web.fetch_backend == FetchBackend::Ollama
            || web.search_backend == SearchBackend::Ollama;
        let ollama_key = needs_ollama_key
            .then(|| mermaid_model::utils::resolve_provider_key("ollama", "OLLAMA_API_KEY", None))
            .flatten();

        let (fetch, fetch_backend): (_, Option<Arc<dyn FetchProvider>>) = match web.fetch_backend {
            FetchBackend::Native => match NativeFetchClient::new() {
                Ok(client) => (
                    available("native", "direct from this machine", Egress::OnMachine),
                    Some(Arc::new(client)),
                ),
                Err(error) => (
                    unavailable(
                        "native",
                        "direct from this machine",
                        Egress::OnMachine,
                        error.to_string(),
                    ),
                    None,
                ),
            },
            FetchBackend::Ollama => {
                let (status, client) = ollama_cloud_backend(
                    ollama_key.clone(),
                    "Ollama Cloud (target redirects are provider-managed; final URL is not disclosed)",
                );
                (status, client.map(|c| c as Arc<dyn FetchProvider>))
            },
        };

        let (search, search_backend): (_, Option<Arc<dyn SearchProvider>>) =
            match web.search_backend {
                // Auto is deliberately sovereign-only. Merely having a cloud
                // credential must not silently change the egress destination;
                // users opt into Ollama Cloud with `search_backend = "ollama"`.
                SearchBackend::Auto => match crate::searxng::managed_backend_viability() {
                    Ok(_) => (
                        available(
                            "managed_searxng",
                            "local managed process",
                            Egress::OnMachine,
                        ),
                        Some(Arc::new(ManagedSearxngBackend)),
                    ),
                    Err(reason) => (
                        unavailable(
                            "managed_searxng",
                            "local managed process",
                            Egress::OnMachine,
                            reason,
                        ),
                        None,
                    ),
                },
                SearchBackend::Ollama => {
                    let (status, client) = ollama_cloud_backend(ollama_key, "Ollama Cloud");
                    (status, client.map(|c| c as Arc<dyn SearchProvider>))
                },
                // A configured endpoint may well be loopback, but proving that
                // means parsing an operator-supplied URL. Disclose instead.
                SearchBackend::Searxng => match SearxngClient::new(web.searxng_url.clone()) {
                    Ok(client) => (
                        available("searxng", "configured SearXNG instance", Egress::OffMachine),
                        Some(Arc::new(client)),
                    ),
                    Err(error) => (
                        unavailable(
                            "searxng",
                            "configured SearXNG instance",
                            Egress::OffMachine,
                            error.to_string(),
                        ),
                        None,
                    ),
                },
            };

        Self {
            fetch,
            search,
            fetch_backend,
            search_backend,
        }
    }

    /// Assemble a capability set from statuses alone, with no backends behind
    /// them. Lets disclosure tests assert formatting for viability combinations
    /// the test host cannot actually produce (e.g. a missing SearXNG bundle).
    #[cfg(test)]
    pub fn from_statuses_for_test(fetch: WebCapabilityStatus, search: WebCapabilityStatus) -> Self {
        Self {
            fetch,
            search,
            fetch_backend: None,
            search_backend: None,
        }
    }

    pub fn fetch_tool(&self) -> Option<WebFetchTool> {
        self.fetch_backend
            .clone()
            .map(|backend| WebFetchTool::new(backend, self.fetch.backend))
    }

    pub fn search_tool(&self) -> Option<WebSearchTool> {
        self.search_backend.clone().map(|backend| WebSearchTool {
            backend,
            backend_name: self.search.backend,
        })
    }
}

/// Build the shared Ollama Cloud client. The fetch and search backends resolve
/// the same credential into the same client and differ only in how they
/// describe the destination they trust, so the failure arms live here once.
fn ollama_cloud_backend(
    key: Option<String>,
    trust_destination: &'static str,
) -> (WebCapabilityStatus, Option<Arc<OllamaWebClient>>) {
    match key {
        Some(key) => match OllamaWebClient::new(key) {
            Ok(client) => (
                available("ollama_cloud", trust_destination, Egress::OffMachine),
                Some(Arc::new(client)),
            ),
            Err(error) => (
                unavailable(
                    "ollama_cloud",
                    "Ollama Cloud",
                    Egress::OffMachine,
                    error.to_string(),
                ),
                None,
            ),
        },
        None => (
            unavailable(
                "ollama_cloud",
                "Ollama Cloud",
                Egress::OffMachine,
                "OLLAMA_API_KEY is not configured",
            ),
            None,
        ),
    }
}

fn available(
    backend: &'static str,
    trust_destination: &'static str,
    egress: Egress,
) -> WebCapabilityStatus {
    WebCapabilityStatus {
        available: true,
        backend,
        trust_destination,
        egress,
        reason: None,
    }
}

fn unavailable(
    backend: &'static str,
    trust_destination: &'static str,
    egress: Egress,
    reason: impl Into<String>,
) -> WebCapabilityStatus {
    WebCapabilityStatus {
        available: false,
        backend,
        trust_destination,
        egress,
        reason: Some(reason.into()),
    }
}

/// `web_search` — query the configured search backend. Accepts a single
/// `{query, max_results}` OR a list of `{queries: [{query, max_results}]}` for
/// parallel fan-out.
pub struct WebSearchTool {
    backend: Arc<dyn SearchProvider>,
    backend_name: &'static str,
}

const MAX_WEB_SEARCH_FAILURE_BYTES: usize = 1024;

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
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
                    "query": { "type": "string", "minLength": 1, "maxLength": 2048 },
                    "max_results": { "type": "integer", "minimum": 1, "maximum": 10, "default": 5 },
                    "queries": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": mermaid_model::constants::MAX_BATCH_TOOL_ITEMS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string", "minLength": 1, "maxLength": 2048 },
                                "max_results": { "type": "integer", "minimum": 1, "maximum": 10, "default": 5 }
                            },
                            "required": ["query"],
                            "additionalProperties": false
                        }
                    }
                },
                "oneOf": [
                    { "required": ["query"], "not": { "required": ["queries"] } },
                    { "required": ["queries"], "not": { "required": ["query"] } }
                ],
                "additionalProperties": false
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
            mermaid_runtime::ToolCategory::Web,
            format!("web_search ({} queries)", queries.len()),
            &args,
        )
        .await
        {
            return blocked;
        }

        let start = std::time::Instant::now();
        let jobs = stream::iter(queries.iter().cloned().enumerate())
            .map(|(idx, (query, count))| {
                let backend = self.backend.clone();
                let progress = ctx.progress.clone();
                let budget = ctx.web_budget();
                let total = queries.len();
                async move {
                    let display_query = mermaid_model::utils::redact_secrets(&query);
                    let _ = progress
                        .send(ProgressEvent::Status(format!(
                            "searching {}/{}: {}",
                            idx + 1,
                            total,
                            display_query
                        )))
                        .await;
                    let result = backend.search(&query, count, budget).await;
                    (idx, query, result)
                }
            })
            .buffer_unordered(mermaid_model::constants::MAX_WEB_SEARCH_CONCURRENCY)
            .collect::<Vec<_>>();
        let mut completed = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            completed = jobs => completed,
        };
        completed.sort_by_key(|(idx, _, _)| *idx);

        let mut combined = String::new();
        let mut result_count = 0usize;
        let mut sources = Vec::new();
        let mut errors: Vec<mermaid_domain::WebSearchFailure> = Vec::new();
        for (idx, query, result) in completed {
            let display_query = mermaid_model::utils::redact_secrets(&query);
            // A single query returning nothing or erroring does NOT abort the
            // batch — record it and carry on so the other queries' results
            // survive (a partial answer beats none).
            let section =
                match result {
                    Ok(results) => {
                        result_count += results.len();
                        sources.extend(results.iter().map(|result| {
                            mermaid_model::utils::sanitize_url_for_display(&result.url)
                        }));
                        if results.is_empty() {
                            "[SEARCH_RESULTS]\n(no results found)\n[/SEARCH_RESULTS]\n".to_string()
                        } else {
                            format_results(&results)
                        }
                    },
                    Err(e) => {
                        let safe_error = mermaid_model::utils::truncate_middle_bytes(
                            &mermaid_model::utils::redact_secrets(&format!("{e:#}")),
                            MAX_WEB_SEARCH_FAILURE_BYTES,
                        );
                        errors.push(mermaid_domain::WebSearchFailure {
                            query_index: idx,
                            error: safe_error.clone(),
                        });
                        format!("(search failed: {safe_error})\n")
                    },
                };
            if queries.len() > 1 {
                combined.push_str(&format!("=== query: {display_query} ===\n{section}\n\n"));
            } else {
                combined = section;
            }
        }

        // Only a total failure — every query hit a backend error — is a tool
        // error. An empty-but-reachable search, or a partial success, returns
        // normally so the model sees what did come back.
        if errors.len() == queries.len() {
            let summary = errors
                .iter()
                .map(|failure| format!("query {}: {}", failure.query_index + 1, failure.error))
                .collect::<Vec<_>>()
                .join("; ");
            let message = format!("web_search via {} failed: {summary}", self.backend_name);
            let message = mermaid_model::utils::truncate_middle_bytes(
                &message,
                mermaid_model::constants::WEB_SEARCH_AGGREGATE_MAX_BYTES
                    .saturating_sub("Error: ".len()),
            );
            return ToolOutcome::error(message, start.elapsed().as_secs_f64()).with_metadata(
                ToolRunMetadata {
                    detail: ToolMetadata::WebSearch {
                        queries: queries.iter().map(|(query, _)| query.clone()).collect(),
                        requested_count: queries.iter().map(|(_, count)| *count).sum(),
                        result_count: 0,
                        sources: Vec::new(),
                        backend: self.backend_name.to_string(),
                        succeeded_queries: 0,
                        failed_queries: errors.len(),
                        partial: false,
                        truncated: false,
                        failures: errors,
                    },
                    result_count: Some(0),
                    ..ToolRunMetadata::default()
                },
            );
        }

        // Cap the aggregate output. Per-result content is already truncated to
        // WEB_CONTENT_MAX_CHARS, but many results across many queries can still
        // bloat context (and memory) past what any single result's cap bounds (#28).
        let truncated = combined.len() > mermaid_model::constants::WEB_SEARCH_AGGREGATE_MAX_BYTES;
        let combined = mermaid_model::utils::truncate_middle_bytes(
            &combined,
            mermaid_model::constants::WEB_SEARCH_AGGREGATE_MAX_BYTES,
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
                backend: self.backend_name.to_string(),
                succeeded_queries: queries.len() - errors.len(),
                failed_queries: errors.len(),
                partial: !errors.is_empty(),
                truncated,
                failures: errors,
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
    backend_name: &'static str,
    snapshots: Arc<Mutex<FetchSnapshotStore>>,
}

impl WebFetchTool {
    fn new(backend: Arc<dyn FetchProvider>, backend_name: &'static str) -> Self {
        Self {
            backend,
            backend_name,
            snapshots: global_fetch_snapshot_store(),
        }
    }

    #[cfg(test)]
    fn new_with_test_snapshots(
        backend: Arc<dyn FetchProvider>,
        backend_name: &'static str,
    ) -> Self {
        Self {
            backend,
            backend_name,
            snapshots: Arc::new(Mutex::new(FetchSnapshotStore::default())),
        }
    }
}

fn fetch_failure_outcome(
    error: &WebFetchError,
    requested_url: &str,
    backend: &str,
    duration_secs: f64,
    pattern: Option<String>,
    context_lines: usize,
) -> ToolOutcome {
    let requested_url = mermaid_model::utils::sanitize_url_for_display(requested_url);
    let message = mermaid_model::utils::redact_secrets(&format!(
        "web_fetch({requested_url}) via {backend}: {error}"
    ));
    let pattern_context = pattern.as_ref().map(|_| context_lines);
    ToolOutcome::error(message, duration_secs).with_metadata(ToolRunMetadata {
        detail: ToolMetadata::WebFetch {
            url: requested_url,
            final_url: None,
            status: error.status(),
            error_kind: Some(error.kind().to_string()),
            media_type: None,
            charset: None,
            backend: backend.to_string(),
            extraction: String::new(),
            title: None,
            line_count: 0,
            byte_count: 0,
            source_byte_count: 0,
            output_byte_count: 0,
            truncated: false,
            pattern,
            context_lines: pattern_context,
            match_count: None,
            snapshot_id: None,
        },
        line_count: Some(0),
        byte_count: Some(0),
        ..ToolRunMetadata::default()
    })
}

const MAX_FETCH_SNAPSHOTS: usize = 4;
const MAX_FETCH_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
const MAX_SNAPSHOT_TITLE_BYTES: usize = 300;
const MAX_SNAPSHOT_URL_BYTES: usize = 8 * 1024;
const MAX_SNAPSHOT_MEDIA_TYPE_BYTES: usize = 256;
const MAX_SNAPSHOT_CHARSET_BYTES: usize = 64;

static FETCH_SNAPSHOT_STORE: OnceLock<Arc<Mutex<FetchSnapshotStore>>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
struct FetchSnapshotScope {
    session_id: Option<String>,
    task_id: Option<String>,
    fallback_turn: Option<u64>,
}

impl FetchSnapshotScope {
    fn from_context(ctx: &ExecContext) -> Self {
        let has_owner = ctx.session_id.is_some() || ctx.task_id.is_some();
        Self {
            session_id: ctx.session_id.as_deref().map(compact_string),
            task_id: ctx.task_id.as_deref().map(compact_string),
            // A context with neither durable owner must fail closed across
            // turns: otherwise every anonymous caller would share one cache.
            fallback_turn: (!has_owner).then_some(ctx.turn.0),
        }
    }

    fn retained_string_bytes(&self) -> usize {
        option_string_capacity(&self.session_id)
            .saturating_add(option_string_capacity(&self.task_id))
    }
}

#[derive(Clone)]
struct FetchSnapshot {
    id: String,
    scope: FetchSnapshotScope,
    page: Arc<WebFetchResult>,
    retained_bytes: usize,
}

#[derive(Default)]
struct FetchSnapshotStore {
    entries: VecDeque<FetchSnapshot>,
    bytes: usize,
    next_id: u64,
}

impl FetchSnapshotStore {
    fn insert(
        &mut self,
        scope: FetchSnapshotScope,
        page: WebFetchResult,
    ) -> Result<(String, Arc<WebFetchResult>), String> {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let mut id = format!("web-{}", self.next_id);
        id.shrink_to_fit();

        let fixed_bytes = id.capacity().saturating_add(scope.retained_string_bytes());
        if fixed_bytes >= MAX_FETCH_SNAPSHOT_BYTES {
            return Err("web_fetch: snapshot owner identity exceeds the cache budget".to_string());
        }
        let page = Arc::new(bound_snapshot_page(
            page,
            MAX_FETCH_SNAPSHOT_BYTES - fixed_bytes,
        ));
        let retained_bytes = fixed_bytes.saturating_add(page_retained_string_bytes(&page));
        if retained_bytes > MAX_FETCH_SNAPSHOT_BYTES {
            return Err("web_fetch: snapshot metadata exceeds the cache budget".to_string());
        }

        while !self.entries.is_empty()
            && (self.entries.len() >= MAX_FETCH_SNAPSHOTS
                || self.bytes.saturating_add(retained_bytes) > MAX_FETCH_SNAPSHOT_BYTES)
        {
            if let Some(removed) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(removed.retained_bytes);
            }
        }
        self.bytes = self.bytes.saturating_add(retained_bytes);
        self.entries.push_back(FetchSnapshot {
            id: id.clone(),
            scope,
            page: page.clone(),
            retained_bytes,
        });
        Ok((id, page))
    }

    fn get(&self, scope: &FetchSnapshotScope, id: &str) -> Option<Arc<WebFetchResult>> {
        self.entries
            .iter()
            .find(|entry| entry.id == id && &entry.scope == scope)
            .map(|entry| Arc::clone(&entry.page))
    }
}

fn global_fetch_snapshot_store() -> Arc<Mutex<FetchSnapshotStore>> {
    FETCH_SNAPSHOT_STORE
        .get_or_init(|| Arc::new(Mutex::new(FetchSnapshotStore::default())))
        .clone()
}

fn bound_snapshot_page(mut page: WebFetchResult, max_retained_bytes: usize) -> WebFetchResult {
    page.title = bounded_title(&page.title);
    page.title.shrink_to_fit();
    bound_owned_string(&mut page.requested_url, MAX_SNAPSHOT_URL_BYTES);
    if let Some(final_url) = page.final_url.as_mut() {
        bound_owned_string(final_url, MAX_SNAPSHOT_URL_BYTES);
    }
    bound_optional_string(&mut page.media_type, MAX_SNAPSHOT_MEDIA_TYPE_BYTES);
    bound_optional_string(&mut page.charset, MAX_SNAPSHOT_CHARSET_BYTES);

    let metadata_bytes = page_retained_string_bytes_without_content(&page);
    let content_budget = max_retained_bytes.saturating_sub(metadata_bytes);
    if page.content.len() > content_budget {
        let cut = page.content.floor_char_boundary(content_budget);
        page.content.truncate(cut);
        page.truncated = true;
    }
    page.content.shrink_to_fit();
    page
}

fn compact_string(value: &str) -> String {
    let mut value = value.to_string();
    value.shrink_to_fit();
    value
}

fn bound_owned_string(value: &mut String, max_bytes: usize) {
    if value.len() > max_bytes {
        value.truncate(value.floor_char_boundary(max_bytes));
    }
    value.shrink_to_fit();
}

fn bound_optional_string(value: &mut Option<String>, max_bytes: usize) {
    if let Some(value) = value {
        bound_owned_string(value, max_bytes);
    }
}

fn option_string_capacity(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, String::capacity)
}

fn page_retained_string_bytes_without_content(page: &WebFetchResult) -> usize {
    page.requested_url
        .capacity()
        .saturating_add(option_string_capacity(&page.final_url))
        .saturating_add(option_string_capacity(&page.media_type))
        .saturating_add(option_string_capacity(&page.charset))
        .saturating_add(page.title.capacity())
}

fn page_retained_string_bytes(page: &WebFetchResult) -> usize {
    page_retained_string_bytes_without_content(page).saturating_add(page.content.capacity())
}

async fn run_snapshot_blocking<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    run_snapshot_blocking_with(super::web_client::extraction_semaphore(), work).await
}

async fn run_snapshot_blocking_with<T, F>(
    limiter: Arc<tokio::sync::Semaphore>,
    work: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = limiter
        .acquire_owned()
        .await
        .map_err(|_| "web snapshot renderer is closed".to_string())?;
    tokio::task::spawn_blocking(move || {
        // Dropping the JoinHandle cannot stop blocking work. Keep its permit in
        // the closure so cancellation never creates an unaccounted renderer.
        let _permit = permit;
        work()
    })
    .await
    .map_err(|error| format!("web snapshot renderer failed: {error}"))
}

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
#[async_trait]
impl ToolExecutor for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".to_string(),
            description: "Fetch a public HTTP(S) URL into a bounded session snapshot, or inspect \
                          a prior snapshot without refetching. Use pattern for case-insensitive \
                          matching, or start_line + line_count for stable continuation."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "format": "uri",
                        "maxLength": 8192,
                        "description": "Public HTTP(S) URL to fetch"
                    },
                    "snapshot_id": {
                        "type": "string",
                        "pattern": "^web-[0-9]+$",
                        "description": "Snapshot returned by an earlier web_fetch call"
                    },
                    "pattern": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 1024,
                        "description": "Case-insensitive substring to find in the page (not a regex)"
                    },
                    "context_lines": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 10,
                        "default": 2,
                        "description": "Context lines around each match (default 2, max 10)"
                    },
                    "start_line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "First 1-based snapshot line to return"
                    },
                    "line_count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 500,
                        "default": 200,
                        "description": "Maximum snapshot lines to return"
                    }
                },
                "oneOf": [
                    { "required": ["url"], "not": { "required": ["snapshot_id"] } },
                    { "required": ["snapshot_id"], "not": { "required": ["url"] } }
                ],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let request = match parse_fetch_args(&args) {
            Ok(request) => request,
            Err(error) => return ToolOutcome::error(error, 0.0),
        };
        let start = std::time::Instant::now();
        let snapshot_scope = FetchSnapshotScope::from_context(&ctx);
        let (page, snapshot_id) = match &request.target {
            FetchTarget::Snapshot(snapshot_id) => {
                let page = self
                    .snapshots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&snapshot_scope, snapshot_id);
                let Some(page) = page else {
                    return ToolOutcome::error(
                        format!(
                            "web_fetch: snapshot '{snapshot_id}' is unavailable or was evicted"
                        ),
                        start.elapsed().as_secs_f64(),
                    );
                };
                (page, snapshot_id.to_string())
            },
            FetchTarget::Url(url) => {
                let safe_url = mermaid_model::utils::sanitize_url_for_display(url.as_str());
                if let Some(blocked) = super::policy_gate::gate_external(
                    &ctx,
                    "web_fetch",
                    mermaid_runtime::ToolCategory::Web,
                    format!("web_fetch via {} {safe_url}", self.backend_name),
                    &args,
                )
                .await
                {
                    return blocked;
                }
                let fetch = self.backend.fetch(url.as_str(), ctx.web_budget());
                let page = tokio::select! {
                    biased;
                    _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
                    result = fetch => match result {
                        Ok(page) => page,
                        Err(error) => {
                            return fetch_failure_outcome(
                                &error,
                                url.as_str(),
                                self.backend_name,
                                start.elapsed().as_secs_f64(),
                                request.pattern.clone(),
                                request.context_lines,
                            );
                        },
                    },
                };
                let inserted = self
                    .snapshots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(snapshot_scope, page);
                match inserted {
                    Ok((snapshot_id, page)) => (page, snapshot_id),
                    Err(error) => {
                        return ToolOutcome::error(error, start.elapsed().as_secs_f64());
                    },
                }
            },
        };

        let render_page = Arc::clone(&page);
        let render_snapshot_id = snapshot_id.clone();
        let render_pattern = request.pattern.clone();
        let render_context_lines = request.context_lines;
        let render_start_line = request.start_line;
        let render_line_count = request.line_count;
        let render = run_snapshot_blocking(move || {
            format_fetch(
                &render_page,
                &render_snapshot_id,
                render_pattern.as_deref(),
                render_context_lines,
                render_start_line,
                render_line_count,
            )
        });
        let formatted = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            result = render => match result {
                Ok(formatted) => formatted,
                Err(error) => {
                    return ToolOutcome::error(error, start.elapsed().as_secs_f64());
                },
            },
        };
        let duration_secs = start.elapsed().as_secs_f64();
        let line_count = formatted.output.lines().count();
        let byte_count = formatted.output.len();
        let title = (!page.title.is_empty()).then(|| bounded_title(&page.title));
        let requested_url = mermaid_model::utils::sanitize_url_for_display(&page.requested_url);
        let final_url = page
            .final_url
            .as_deref()
            .map(mermaid_model::utils::sanitize_url_for_display);
        let pattern_context = request.pattern.as_ref().map(|_| request.context_lines);
        ToolOutcome::success(
            formatted.output,
            format!(
                "{} {} fetched via {}",
                line_count,
                if line_count == 1 { "line" } else { "lines" },
                page.backend.as_str()
            ),
            duration_secs,
        )
        .with_metadata(ToolRunMetadata {
            detail: ToolMetadata::WebFetch {
                url: requested_url,
                final_url,
                status: page.status,
                error_kind: None,
                media_type: page.media_type.clone(),
                charset: page.charset.clone(),
                backend: page.backend.as_str().to_string(),
                extraction: page.extraction.as_str().to_string(),
                title,
                line_count,
                byte_count,
                source_byte_count: page.source_bytes,
                output_byte_count: page.output_bytes,
                truncated: page.truncated || formatted.truncated,
                pattern: request.pattern,
                context_lines: pattern_context,
                match_count: formatted.match_count,
                snapshot_id: Some(snapshot_id),
            },
            line_count: Some(line_count),
            byte_count: Some(byte_count),
            ..ToolRunMetadata::default()
        })
    }
}

/// Exactly one of the two ways to name a page. Modelled as an enum rather
/// than two `Option`s so "both" and "neither" are unrepresentable past the
/// parser — `execute` reads the target without re-checking the invariant.
enum FetchTarget {
    Url(ValidatedWebUrl),
    Snapshot(String),
}

struct ParsedFetchArgs {
    target: FetchTarget,
    pattern: Option<String>,
    context_lines: usize,
    start_line: Option<usize>,
    line_count: usize,
}

fn parse_fetch_args(args: &serde_json::Value) -> Result<ParsedFetchArgs, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "web_fetch arguments must be an object".to_string())?;
    for key in obj.keys() {
        if !matches!(
            key.as_str(),
            "url" | "snapshot_id" | "pattern" | "context_lines" | "start_line" | "line_count"
        ) {
            return Err(format!("web_fetch: unknown argument '{key}'"));
        }
    }

    let url = match obj.get("url") {
        None => None,
        Some(value) => {
            let raw = value
                .as_str()
                .ok_or_else(|| "web_fetch: 'url' must be a string".to_string())?
                .trim();
            if raw.len() > 8192 {
                return Err("web_fetch: URL exceeds 8192 bytes".to_string());
            }
            Some(ValidatedWebUrl::parse(raw).map_err(|error| format!("web_fetch: {error}"))?)
        },
    };
    let snapshot_id = match obj.get("snapshot_id") {
        None => None,
        Some(value) => {
            let id = value
                .as_str()
                .ok_or_else(|| "web_fetch: 'snapshot_id' must be a string".to_string())?;
            let valid = id.strip_prefix("web-").is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
            });
            if !valid {
                return Err("web_fetch: invalid snapshot id".to_string());
            }
            Some(id.to_string())
        },
    };
    let target = match (url, snapshot_id) {
        (Some(url), None) => FetchTarget::Url(url),
        (None, Some(id)) => FetchTarget::Snapshot(id),
        _ => {
            return Err("web_fetch requires exactly one of 'url' or 'snapshot_id'".to_string());
        },
    };

    let pattern = match obj.get("pattern") {
        None => None,
        Some(value) => {
            let pattern = value
                .as_str()
                .ok_or_else(|| "web_fetch: 'pattern' must be a string".to_string())?
                .trim();
            if pattern.is_empty() {
                return Err("web_fetch: 'pattern' must not be empty".to_string());
            }
            if pattern.contains(['\r', '\n']) {
                return Err("web_fetch: 'pattern' must be a single line".to_string());
            }
            if pattern.chars().count() > 1024 {
                return Err("web_fetch: 'pattern' exceeds 1024 characters".to_string());
            }
            Some(pattern.to_string())
        },
    };
    let context_lines = parse_bounded_usize(obj, "context_lines", 2, 0, 10)?;
    if pattern.is_none() && obj.contains_key("context_lines") {
        return Err("web_fetch: 'context_lines' requires 'pattern'".to_string());
    }
    let has_range = obj.contains_key("start_line") || obj.contains_key("line_count");
    if pattern.is_some() && has_range {
        return Err("web_fetch: use either 'pattern' or a line range, not both".to_string());
    }
    let start_line = has_range
        .then(|| parse_bounded_usize(obj, "start_line", 1, 1, usize::MAX))
        .transpose()?;
    let line_count = parse_bounded_usize(obj, "line_count", 200, 1, 500)?;

    Ok(ParsedFetchArgs {
        target,
        pattern,
        context_lines,
        start_line,
        line_count,
    })
}

fn parse_bounded_usize(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, String> {
    let Some(value) = obj.get(key) else {
        return Ok(default);
    };
    let value = value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| format!("web_fetch: '{key}' must be an integer"))?;
    if value < min || value > max {
        return Err(format!("web_fetch: '{key}' must be from {min} to {max}"));
    }
    Ok(value)
}

/// Cap on the complete model-visible `web_fetch` result, including provenance
/// headers and truncation marker.
const WEB_FETCH_MAX_BYTES: usize = mermaid_model::constants::WEB_SEARCH_AGGREGATE_MAX_BYTES;
const FETCH_TRUNCATION_SUFFIX: &str = "\n\n...[content truncated]\n[/WEB_FETCH]";

struct FormattedFetch {
    output: String,
    truncated: bool,
    match_count: Option<usize>,
}

/// Cap on find-in-page match BLOCKS per call (merged context windows).
/// Matched lines beyond the included blocks are summarized as a
/// `(+N more matches)` tail so the model knows the page has more.
const MAX_PATTERN_MATCHES: usize = 20;

fn format_fetch(
    page: &WebFetchResult,
    snapshot_id: &str,
    pattern: Option<&str>,
    ctx_lines: usize,
    start_line: Option<usize>,
    line_count: usize,
) -> FormattedFetch {
    let title = if page.title.trim().is_empty() {
        "(no title)".to_string()
    } else {
        bounded_title(&page.title)
    };
    let requested_url = bounded_url(&page.requested_url);
    let final_url = page
        .final_url
        .as_deref()
        .map(bounded_url)
        .unwrap_or_else(|| "(not disclosed by backend)".to_string());
    let status = page
        .status
        .map(|status| status.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let media = page.media_type.as_deref().unwrap_or("unknown");
    let charset = page.charset.as_deref().unwrap_or("unknown");

    let (body, match_count) = if let Some(pattern) = pattern {
        match extract_matches(&page.content, pattern, ctx_lines, MAX_PATTERN_MATCHES) {
            Some((report, count)) => (report, Some(count)),
            None => (format!("No matching lines for \"{pattern}\"."), Some(0)),
        }
    } else if let Some(start_line) = start_line {
        (
            format_line_range(&page.content, start_line, line_count),
            None,
        )
    } else {
        (page.content.clone(), None)
    };

    let mut output = format!(
        "[WEB_FETCH]\nTitle: {title}\nRequested URL: {requested_url}\nFinal URL: {final_url}\nStatus: {status}\nMedia-Type: {media}\nCharset: {charset}\nBackend: {}\nExtraction: {}\nSnapshot: {snapshot_id}\nSource bytes: {}\nExtracted bytes: {}\nSnapshot bytes: {}\nSource lines: {}\n\nContent:\n{body}\n[/WEB_FETCH]",
        page.backend.as_str(),
        page.extraction.as_str(),
        page.source_bytes,
        page.output_bytes,
        page.content.len(),
        page.content.lines().count(),
    );
    let truncated = output.len() > WEB_FETCH_MAX_BYTES;
    if truncated {
        let budget = WEB_FETCH_MAX_BYTES.saturating_sub(FETCH_TRUNCATION_SUFFIX.len());
        let cut = output.floor_char_boundary(budget);
        output.truncate(cut);
        output.push_str(FETCH_TRUNCATION_SUFFIX);
    }
    FormattedFetch {
        output,
        truncated,
        match_count,
    }
}

fn bounded_title(title: &str) -> String {
    let mut bounded = String::with_capacity(title.len().min(MAX_SNAPSHOT_TITLE_BYTES));
    let content_budget = MAX_SNAPSHOT_TITLE_BYTES.saturating_sub(3);
    let mut truncated = false;

    for word in title.split_whitespace() {
        let separator_bytes = usize::from(!bounded.is_empty());
        if bounded
            .len()
            .saturating_add(separator_bytes)
            .saturating_add(word.len())
            <= MAX_SNAPSHOT_TITLE_BYTES
        {
            if separator_bytes != 0 {
                bounded.push(' ');
            }
            bounded.push_str(word);
            continue;
        }

        if bounded.len() > content_budget {
            bounded.truncate(bounded.floor_char_boundary(content_budget));
        }
        if bounded.len() < content_budget {
            if separator_bytes != 0 && bounded.len() < content_budget {
                bounded.push(' ');
            }
            let remaining = content_budget.saturating_sub(bounded.len());
            let cut = word.floor_char_boundary(remaining);
            bounded.push_str(&word[..cut]);
        }
        truncated = true;
        break;
    }

    if truncated {
        bounded.push_str("...");
    }
    bounded
}

fn bounded_url(url: &str) -> String {
    const MAX_DISPLAY_URL_BYTES: usize = 2048;
    let url = mermaid_model::utils::sanitize_url_for_display(url);
    if url.len() <= MAX_DISPLAY_URL_BYTES {
        return url;
    }
    let cut = url.floor_char_boundary(MAX_DISPLAY_URL_BYTES.saturating_sub(3));
    format!("{}...", &url[..cut])
}

fn format_line_range(content: &str, start_line: usize, line_count: usize) -> String {
    let total = content.lines().count();
    if start_line > total {
        return format!("Requested line {start_line}, but the snapshot contains {total} lines.");
    }
    let mut output = format!(
        "Lines {start_line}-{} of {total}:\n",
        start_line
            .saturating_add(line_count)
            .saturating_sub(1)
            .min(total)
    );
    for (offset, line) in content
        .lines()
        .skip(start_line.saturating_sub(1))
        .take(line_count)
        .enumerate()
    {
        output.push_str(&format!("L{}: {line}\n", start_line + offset));
    }
    output
}

/// Canonical caseless form for substring matching. NFD before and after the
/// full Unicode fold follows the Unicode canonical-caseless algorithm: it
/// equates composed/decomposed text and handles multi-character folds such as
/// `ß` -> `ss`, while leaving the original line untouched for display.
fn normalized_case_fold(value: &str) -> String {
    use caseless::Caseless;
    use unicode_normalization::UnicodeNormalization;

    value.nfd().default_case_fold().nfd().collect()
}

/// Find-in-page core: canonical Unicode-caseless SUBSTRING match per line (not
/// a regex — model-supplied metacharacters must mean themselves), each match
/// reported as a `L<n>:`-prefixed context block. Overlapping or adjacent
/// windows merge into one block; blocks are separated by `---` lines and capped
/// at `max_blocks`, with a `(+N more matches)` tail counting the matched lines
/// that didn't fit. Returns `None` when nothing matches.
fn extract_matches(
    content: &str,
    pattern: &str,
    context_lines: usize,
    max_blocks: usize,
) -> Option<(String, usize)> {
    let needle = normalized_case_fold(pattern);
    let lines: Vec<&str> = content.lines().collect();
    let matched: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| normalized_case_fold(line).contains(&needle))
        .map(|(i, _)| i)
        .collect();
    if matched.is_empty() {
        return None;
    }

    // Merge each match's [i-ctx, i+ctx] window with overlapping/adjacent
    // neighbors; count how many matched lines the included blocks cover.
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for &i in &matched {
        let start = i.saturating_sub(context_lines);
        let end = (i + context_lines).min(lines.len() - 1);
        match blocks.last_mut() {
            Some((_, last_end)) if start <= *last_end + 1 => *last_end = (*last_end).max(end),
            _ => blocks.push((start, end)),
        }
    }
    let included = &blocks[..blocks.len().min(max_blocks)];
    let cutoff = included.last().map(|&(_, end)| end).unwrap_or(0);
    let dropped = matched.iter().filter(|&&i| i > cutoff).count();

    let mut out = format!(
        "{} match{} for \"{}\":\n",
        matched.len(),
        if matched.len() == 1 { "" } else { "es" },
        pattern
    );
    for (bi, &(start, end)) in included.iter().enumerate() {
        if bi > 0 {
            out.push_str("---\n");
        }
        for (offset, line) in lines[start..=end].iter().enumerate() {
            // 1-based line numbers, matching how editors and grep report.
            out.push_str(&format!("L{}: {}\n", start + offset + 1, line));
        }
    }
    if dropped > 0 {
        out.push_str(&format!(
            "(+{dropped} more match{})\n",
            if dropped == 1 { "" } else { "es" }
        ));
    }
    let match_count = matched.len();
    Some((out, match_count))
}

fn parse_queries(args: &serde_json::Value) -> Result<Vec<(String, usize)>, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "web_search arguments must be an object".to_string())?;
    for key in obj.keys() {
        if !matches!(key.as_str(), "query" | "max_results" | "queries") {
            return Err(format!("web_search: unknown argument '{key}'"));
        }
    }
    if obj.contains_key("query") && obj.contains_key("queries") {
        return Err("web_search accepts either 'query' or 'queries', not both".to_string());
    }

    if let Some(value) = obj.get("queries") {
        let Some(arr) = value.as_array() else {
            return Err("web_search: 'queries' must be an array".to_string());
        };
        if arr.is_empty() {
            return Err("web_search: 'queries' must contain at least one entry".to_string());
        }
        if arr.len() > mermaid_model::constants::MAX_BATCH_TOOL_ITEMS {
            return Err(format!(
                "web_search: too many queries ({}); cap is {} per call — split the request",
                arr.len(),
                mermaid_model::constants::MAX_BATCH_TOOL_ITEMS
            ));
        }
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            let Some(obj) = v.as_object() else {
                return Err(
                    "web_search: 'queries' must be an array of {query, max_results}".to_string(),
                );
            };
            for key in obj.keys() {
                if !matches!(key.as_str(), "query" | "max_results") {
                    return Err(format!("web_search: unknown query argument '{key}'"));
                }
            }
            out.push(parse_query_entry(obj)?);
        }
        return Ok(out);
    }
    if obj.contains_key("query") {
        return Ok(vec![parse_query_entry(obj)?]);
    }
    Err("web_search requires 'query' (string) or 'queries' (array)".to_string())
}

fn parse_query_entry(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<(String, usize), String> {
    let query = obj
        .get("query")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "web_search: each query needs 'query' (string)".to_string())?
        .trim();
    if query.is_empty() {
        return Err("web_search: query must not be empty".to_string());
    }
    if query.contains(['\r', '\n', '\0']) {
        return Err("web_search: query must be a single text line".to_string());
    }
    if query.chars().count() > 2048 {
        return Err("web_search: query exceeds 2048 characters".to_string());
    }
    let count = match obj.get("max_results") {
        None => 5,
        Some(value) => {
            let count = value.as_u64().ok_or_else(|| {
                "web_search: 'max_results' must be an integer from 1 to 10".to_string()
            })?;
            if !(1..=10).contains(&count) {
                return Err("web_search: 'max_results' must be from 1 to 10".to_string());
            }
            count as usize
        },
    };
    Ok((query.to_string(), count))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn page(content: impl Into<String>) -> WebFetchResult {
        let content = content.into();
        WebFetchResult {
            requested_url: "https://example.com/start".to_string(),
            final_url: Some("https://example.com/final".to_string()),
            status: Some(200),
            media_type: Some("text/html".to_string()),
            charset: Some("utf-8".to_string()),
            backend: super::super::web_client::FetchBackend::Native,
            extraction: super::super::web_client::ExtractionMode::Readability,
            source_bytes: content.len(),
            output_bytes: content.len(),
            truncated: false,
            title: "T".to_string(),
            content,
        }
    }

    fn scope(session_id: &str) -> FetchSnapshotScope {
        FetchSnapshotScope {
            session_id: Some(compact_string(session_id)),
            task_id: None,
            fallback_turn: None,
        }
    }

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
    fn format_fetch_caps_long_content() {
        // F46: a huge page body must be truncated with a marker, not dumped whole.
        let big = "z".repeat(WEB_FETCH_MAX_BYTES * 2);
        let big_page = page(big);
        let out = format_fetch(&big_page, "web-1", None, 2, None, 200);
        assert!(
            out.output.len() <= WEB_FETCH_MAX_BYTES,
            "content must be capped, got {} bytes",
            out.output.len()
        );
        assert!(
            out.output.contains("truncated"),
            "expected truncation marker"
        );
        assert!(out.truncated);

        // A short page is emitted intact, with no marker.
        let small = page("hello world");
        let out = format_fetch(&small, "web-1", None, 2, None, 200);
        assert!(out.output.contains("hello world"));
        assert!(!out.output.contains("truncated"));
    }

    #[test]
    fn format_fetch_caps_the_complete_envelope_and_sanitizes_provenance() {
        let mut page = page("body");
        page.title = format!("  {}\n{}  ", "title ".repeat(100), "tail");
        page.requested_url = format!(
            "https://alice:hunter2@example.com/page?token=opaque-secret&q={}",
            "x".repeat(10_000)
        );
        page.final_url = Some(page.requested_url.clone());

        let out = format_fetch(&page, "web-1", None, 2, None, 200);
        assert!(out.output.len() <= WEB_FETCH_MAX_BYTES);
        assert!(
            !out.output.contains("alice"),
            "userinfo leaked: {}",
            out.output
        );
        assert!(
            !out.output.contains("hunter2"),
            "password leaked: {}",
            out.output
        );
        assert!(!out.output.contains("opaque-secret"), "query secret leaked");
        let title = out
            .output
            .lines()
            .find_map(|line| line.strip_prefix("Title: "))
            .expect("title header");
        assert!(title.len() <= 300);
    }

    #[test]
    fn complete_output_budget_holds_for_multibyte_boundary_sizes() {
        for unit in ["a", "é", "界"] {
            for units in [0, 1, 14_900, 15_000, 15_100, 40_000] {
                let mut candidate = page(unit.repeat(units));
                candidate.title = unit.repeat(1_000);
                let formatted = format_fetch(&candidate, "web-99", None, 2, None, 200);
                assert!(
                    formatted.output.len() <= WEB_FETCH_MAX_BYTES,
                    "{} bytes escaped the complete-result cap",
                    formatted.output.len()
                );
                assert!(std::str::from_utf8(formatted.output.as_bytes()).is_ok());
                assert!(formatted.output.ends_with("[/WEB_FETCH]"));
            }
        }
    }

    #[test]
    fn snapshot_store_accounts_for_and_bounds_every_retained_string() {
        let original_content = "é".repeat(MAX_FETCH_SNAPSHOT_BYTES / 2 + 1_000);
        let original_output_bytes = original_content.len();
        let mut oversized = page(original_content);
        oversized.output_bytes = original_output_bytes;
        oversized.title = "title ".repeat(10_000);
        oversized.requested_url = format!("https://example.com/{}", "r".repeat(20_000));
        oversized.final_url = Some(format!("https://example.com/{}", "f".repeat(20_000)));
        oversized.media_type = Some("m".repeat(1_000));
        oversized.charset = Some("c".repeat(1_000));

        let owner = scope("session-retained-size");
        let mut store = FetchSnapshotStore::default();
        let (id, bounded) = store.insert(owner.clone(), oversized).unwrap();
        let entry = store.entries.back().expect("snapshot entry");
        let expected = entry
            .id
            .capacity()
            .saturating_add(entry.scope.retained_string_bytes())
            .saturating_add(page_retained_string_bytes(&entry.page));

        assert_eq!(entry.id, id);
        assert_eq!(entry.retained_bytes, expected);
        assert_eq!(store.bytes, expected);
        assert!(store.bytes <= MAX_FETCH_SNAPSHOT_BYTES);
        assert_eq!(bounded.output_bytes, original_output_bytes);
        assert!(bounded.truncated);
        assert!(bounded.title.len() <= MAX_SNAPSHOT_TITLE_BYTES);
        assert!(bounded.requested_url.len() <= MAX_SNAPSHOT_URL_BYTES);
        assert!(bounded.final_url.as_ref().unwrap().len() <= MAX_SNAPSHOT_URL_BYTES);
        assert!(bounded.media_type.as_ref().unwrap().len() <= MAX_SNAPSHOT_MEDIA_TYPE_BYTES);
        assert!(bounded.charset.as_ref().unwrap().len() <= MAX_SNAPSHOT_CHARSET_BYTES);
        assert!(std::str::from_utf8(bounded.content.as_bytes()).is_ok());
    }

    #[test]
    fn snapshot_store_isolates_session_and_task_owners() {
        let owner = FetchSnapshotScope {
            session_id: Some(compact_string("session-a")),
            task_id: Some(compact_string("task-a")),
            fallback_turn: None,
        };
        let mut store = FetchSnapshotStore::default();
        let (id, _) = store.insert(owner.clone(), page("private page")).unwrap();

        assert!(store.get(&owner, &id).is_some());
        for outsider in [
            FetchSnapshotScope {
                session_id: Some(compact_string("session-b")),
                task_id: Some(compact_string("task-a")),
                fallback_turn: None,
            },
            FetchSnapshotScope {
                session_id: Some(compact_string("session-a")),
                task_id: Some(compact_string("task-b")),
                fallback_turn: None,
            },
        ] {
            assert!(store.get(&outsider, &id).is_none());
        }
    }

    #[test]
    fn snapshot_store_evicts_the_oldest_entry_at_the_count_limit() {
        let mut store = FetchSnapshotStore::default();
        let owner = scope("session-eviction");
        let mut ids = Vec::new();
        for index in 0..=MAX_FETCH_SNAPSHOTS {
            ids.push(
                store
                    .insert(owner.clone(), page(format!("page {index}")))
                    .unwrap()
                    .0,
            );
        }
        assert!(
            store.get(&owner, &ids[0]).is_none(),
            "oldest snapshot was not evicted"
        );
        assert!(store.get(&owner, ids.last().unwrap()).is_some());
        assert_eq!(store.entries.len(), MAX_FETCH_SNAPSHOTS);
    }

    #[test]
    fn extract_matches_finds_case_insensitive_with_context() {
        let content = "line one\nline two\nTARGET here\nline four\nline five";
        let (out, count) = extract_matches(content, "target", 1, 20).unwrap();
        assert_eq!(count, 1);
        assert!(out.starts_with("1 match for \"target\":"));
        assert!(out.contains("L2: line two"));
        assert!(out.contains("L3: TARGET here"));
        assert!(out.contains("L4: line four"));
        assert!(!out.contains("L1:"), "context clipped to 1 line: {out}");
        assert!(!out.contains("L5:"));
    }

    #[test]
    fn extract_matches_merges_overlapping_windows() {
        // Matches on adjacent lines must merge into ONE block (no separator).
        let content = "a\nhit one\nhit two\nb\nc\nd\ne\nf\ng\nhit three\nz";
        let (out, count) = extract_matches(content, "hit", 1, 20).unwrap();
        assert_eq!(count, 3);
        assert!(out.starts_with("3 matches"));
        assert_eq!(out.matches("---").count(), 1, "two blocks: {out}");
        // No duplicated lines from the merged windows.
        assert_eq!(out.matches("hit one").count(), 1);
    }

    #[test]
    fn extract_matches_caps_blocks_and_reports_tail() {
        // 25 matches spaced far apart -> 25 blocks, capped at 20 + tail note.
        let content = (0..25)
            .map(|i| format!("match {i}\nx\nx\nx\nx\nx"))
            .collect::<Vec<_>>()
            .join("\n");
        let (out, count) = extract_matches(&content, "match", 0, 20).unwrap();
        assert_eq!(count, 25);
        assert!(out.starts_with("25 matches"));
        assert_eq!(out.matches("---").count(), 19, "20 blocks: {out}");
        assert!(out.contains("(+5 more matches)"), "tail note: {out}");
    }

    #[test]
    fn extract_matches_none_and_multibyte() {
        assert!(extract_matches("nothing here", "absent", 2, 20).is_none());
        // Multibyte content must not panic and must match case-insensitively.
        let content = "voil\u{e0} un r\u{e9}sultat\nplain line";
        let (out, count) = extract_matches(content, "R\u{c9}SULTAT", 0, 20).unwrap();
        assert_eq!(count, 1);
        assert!(out.contains("L1: voil\u{e0} un r\u{e9}sultat"));
        // Context 0 keeps only the matching line.
        assert!(!out.contains("plain line"));
    }

    #[test]
    fn extract_matches_uses_full_unicode_case_folding() {
        let content = "Die Straße ist lang\nSTRASSE in capitals\nother";
        let (out, count) = extract_matches(content, "strasse", 0, 20).unwrap();
        assert!(out.starts_with("2 matches for \"strasse\":"), "{out}");
        assert!(out.contains("L1: Die Straße ist lang"), "{out}");
        assert!(out.contains("L2: STRASSE in capitals"), "{out}");
        assert_eq!(count, 2);
    }

    #[test]
    fn extract_matches_normalizes_composed_and_decomposed_text() {
        let content = "Café noir\nCafe\u{301} blanc\nplain";
        let decomposed_pattern = "CAFE\u{301}";
        let (out, count) = extract_matches(content, decomposed_pattern, 0, 20).unwrap();
        assert!(out.starts_with("2 matches"), "{out}");
        assert!(out.contains("L1: Café noir"), "{out}");
        assert!(out.contains("L2: Cafe\u{301} blanc"), "{out}");
        assert_eq!(count, 2);
    }

    #[test]
    fn format_fetch_pattern_paths() {
        let page = page("alpha\nbeta\ngamma");
        // Match -> report replaces the body.
        let out = format_fetch(&page, "web-1", Some("beta"), 1, None, 200);
        assert!(out.output.contains("1 match for \"beta\""));
        assert!(out.output.contains("L2: beta"));
        // No match is explicit and does not unexpectedly dump the full body.
        let out = format_fetch(&page, "web-1", Some("nope"), 1, None, 200);
        assert!(out.output.contains("No matching lines for \"nope\"."));
        assert!(!out.output.contains("alpha"));
    }

    #[test]
    fn find_in_page_runs_before_the_cap() {
        // A match in the tail of a page longer than the cap must still be
        // found (matching runs pre-cap; only the report is capped).
        let mut content = "x\n".repeat(WEB_FETCH_MAX_BYTES / 2);
        content.push_str("needle in the tail\n");
        let page = page(content);
        let out = format_fetch(&page, "web-1", Some("needle"), 1, None, 200);
        assert!(
            out.output.contains("1 match for \"needle\""),
            "tail match found"
        );
        assert!(out.output.contains("needle in the tail"));
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
            {"query": "b", "max_results": 5},
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
    fn parse_queries_rejects_out_of_range_count() {
        let args = serde_json::json!({"query": "q", "max_results": 999});
        assert!(parse_queries(&args).is_err());
        let args = serde_json::json!({"query": "q", "max_results": 0});
        assert!(parse_queries(&args).is_err());
        let args = serde_json::json!({"query": "q", "max_results": "5"});
        assert!(parse_queries(&args).is_err());
    }

    #[test]
    fn parse_queries_rejects_ambiguous_and_unknown_arguments() {
        assert!(
            parse_queries(&serde_json::json!({"query":"a", "queries":[{"query":"b"}]})).is_err()
        );
        assert!(parse_queries(&serde_json::json!({"query":"a", "extra":true})).is_err());
        assert!(parse_queries(&serde_json::json!({"query":"   "})).is_err());
        assert!(parse_queries(&serde_json::json!({"query":"safe\n=== injected ==="})).is_err());
    }

    #[test]
    fn parse_queries_rejects_excess_fan_out() {
        // #90: a single call can't request unbounded fan-out.
        let many: Vec<_> = (0..mermaid_model::constants::MAX_BATCH_TOOL_ITEMS + 1)
            .map(|i| serde_json::json!({"query": format!("q{i}")}))
            .collect();
        let args = serde_json::json!({ "queries": many });
        assert!(parse_queries(&args).is_err());

        // Exactly at the cap is still accepted.
        let at_cap: Vec<_> = (0..mermaid_model::constants::MAX_BATCH_TOOL_ITEMS)
            .map(|i| serde_json::json!({"query": format!("q{i}")}))
            .collect();
        let args = serde_json::json!({ "queries": at_cap });
        assert_eq!(
            parse_queries(&args).unwrap().len(),
            mermaid_model::constants::MAX_BATCH_TOOL_ITEMS
        );
    }

    #[test]
    fn parse_fetch_args_is_strict_and_rejects_credentialed_urls() {
        for invalid in [
            serde_json::json!({}),
            serde_json::json!({"url": "https://example.com", "snapshot_id": "web-1"}),
            serde_json::json!({"url": "https://user:password@example.com"}),
            serde_json::json!({"url": "http://127.0.0.1/private"}),
            serde_json::json!({"snapshot_id": "bad"}),
            serde_json::json!({"snapshot_id": "web-1", "context_lines": 2}),
            serde_json::json!({"snapshot_id": "web-1", "pattern": "x", "start_line": 1}),
            serde_json::json!({"snapshot_id": "web-1", "unknown": true}),
        ] {
            assert!(parse_fetch_args(&invalid).is_err(), "accepted {invalid}");
        }

        let parsed = parse_fetch_args(&serde_json::json!({
            "url": "https://example.com/page#fragment",
            "start_line": 4,
            "line_count": 2
        }))
        .unwrap();
        let FetchTarget::Url(url) = &parsed.target else {
            panic!("expected a URL target");
        };
        assert_eq!(url.as_str(), "https://example.com/page");
        assert_eq!(parsed.start_line, Some(4));
        assert_eq!(parsed.line_count, 2);
    }

    #[tokio::test]
    async fn web_fetch_failure_retains_typed_backend_provenance() {
        use crate::providers::ctx::test_exec_context;
        use mermaid_domain::{ToolCallId, ToolStatus, TurnId};

        struct FailingFetch;

        #[async_trait]
        impl FetchProvider for FailingFetch {
            async fn fetch(
                &self,
                url: &str,
                _budget: crate::providers::ctx::WebByteBudget,
            ) -> Result<WebFetchResult, WebFetchError> {
                Err(WebFetchError::HttpStatus {
                    status: 503,
                    url: url.to_string(),
                })
            }
        }

        let tool = WebFetchTool::new_with_test_snapshots(Arc::new(FailingFetch), "mock");
        let (ctx, _rx) =
            test_exec_context(TurnId(9), ToolCallId(9), std::path::PathBuf::from("/tmp"));
        let outcome = tool
            .execute(serde_json::json!({"url": "https://example.com/page"}), ctx)
            .await;

        assert_eq!(outcome.status, ToolStatus::Error);
        match &outcome.metadata.detail {
            ToolMetadata::WebFetch {
                status,
                error_kind,
                backend,
                final_url,
                ..
            } => {
                assert_eq!(*status, Some(503));
                assert_eq!(error_kind.as_deref(), Some("http_status"));
                assert_eq!(backend, "mock");
                assert!(final_url.is_none());
            },
            other => panic!("expected web metadata, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_search_progress_redacts_without_changing_transport_query() {
        use crate::providers::ctx::test_exec_context;
        use mermaid_domain::{ToolCallId, ToolStatus, TurnId};

        struct RecordingSearch {
            seen: Arc<Mutex<Option<String>>>,
        }

        #[async_trait]
        impl SearchProvider for RecordingSearch {
            async fn search(
                &self,
                query: &str,
                _count: usize,
                _budget: crate::providers::ctx::WebByteBudget,
            ) -> anyhow::Result<Vec<crate::providers::tool::web_client::SearchResult>> {
                *self
                    .seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(query.to_string());
                Ok(Vec::new())
            }
        }

        let seen = Arc::new(Mutex::new(None));
        let tool = WebSearchTool {
            backend: Arc::new(RecordingSearch { seen: seen.clone() }),
            backend_name: "mock",
        };
        let (ctx, mut progress) =
            test_exec_context(TurnId(91), ToolCallId(91), std::path::PathBuf::from("/tmp"));
        let query = "research OPENAI_API_KEY=abc";
        let outcome = tool.execute(serde_json::json!({"query": query}), ctx).await;
        assert_eq!(outcome.status, ToolStatus::Success);
        assert_eq!(
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_deref(),
            Some(query),
            "redaction must not alter the transport query"
        );
        let ProgressEvent::Status(status) = progress.recv().await.expect("search progress") else {
            panic!("expected search status progress");
        };
        assert!(
            !status.contains("abc"),
            "progress leaked query secret: {status}"
        );
        assert!(status.contains("OPENAI_API_KEY=[REDACTED]"));
    }

    #[tokio::test]
    async fn snapshot_line_ranges_do_not_refetch_mutable_pages() {
        use crate::providers::ctx::test_exec_context;
        use mermaid_domain::{ToolCallId, ToolStatus, TurnId};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct MockFetch {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl FetchProvider for MockFetch {
            async fn fetch(
                &self,
                url: &str,
                _budget: crate::providers::ctx::WebByteBudget,
            ) -> Result<WebFetchResult, WebFetchError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let mut result = page("one\ntwo\nthree");
                result.requested_url = url.to_string();
                result.final_url = Some(url.to_string());
                Ok(result)
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let tool = WebFetchTool::new_with_test_snapshots(
            Arc::new(MockFetch {
                calls: calls.clone(),
            }),
            "mock",
        );
        let (mut ctx, _rx) =
            test_exec_context(TurnId(10), ToolCallId(10), std::path::PathBuf::from("/tmp"));
        ctx.session_id = Some("session-a".to_string());
        let first = tool
            .execute(serde_json::json!({"url": "https://example.com/page"}), ctx)
            .await;
        assert_eq!(first.status, ToolStatus::Success);
        let snapshot_id = match &first.metadata.detail {
            ToolMetadata::WebFetch { snapshot_id, .. } => snapshot_id.clone().expect("snapshot id"),
            other => panic!("expected web metadata, got {other:?}"),
        };

        let (mut foreign_ctx, _rx) =
            test_exec_context(TurnId(11), ToolCallId(11), std::path::PathBuf::from("/tmp"));
        foreign_ctx.session_id = Some("session-b".to_string());
        let foreign = tool
            .execute(
                serde_json::json!({
                    "snapshot_id": snapshot_id.clone(),
                    "start_line": 2,
                    "line_count": 1
                }),
                foreign_ctx,
            )
            .await;
        assert_eq!(foreign.status, ToolStatus::Error);
        assert!(foreign.output().contains("unavailable or was evicted"));

        let (mut ctx, _rx) =
            test_exec_context(TurnId(12), ToolCallId(12), std::path::PathBuf::from("/tmp"));
        ctx.session_id = Some("session-a".to_string());
        let continuation = tool
            .execute(
                serde_json::json!({
                    "snapshot_id": snapshot_id,
                    "start_line": 2,
                    "line_count": 1
                }),
                ctx,
            )
            .await;
        assert_eq!(continuation.status, ToolStatus::Success);
        assert!(continuation.output().contains("L2: two"));
        assert!(!continuation.output().contains("L1: one"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "snapshot triggered a refetch"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshot_blocking_work_respects_the_global_extractor_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut jobs = Vec::new();
        for _ in 0..8 {
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            jobs.push(tokio::spawn(async move {
                run_snapshot_blocking(move || {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    active.fetch_sub(1, Ordering::SeqCst);
                })
                .await
                .unwrap();
            }));
        }
        for job in jobs {
            job.await.unwrap();
        }

        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(
            peak.load(Ordering::SeqCst) <= mermaid_model::constants::MAX_WEB_EXTRACTION_CONCURRENCY
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_snapshot_waiter_keeps_its_permit_until_blocking_work_finishes() {
        let limiter = Arc::new(tokio::sync::Semaphore::new(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let worker_limiter = limiter.clone();
        let worker = tokio::spawn(async move {
            run_snapshot_blocking_with(worker_limiter, move || {
                let _ = started_tx.send(());
                release_rx.recv().expect("test releases blocking worker");
            })
            .await
        });
        started_rx.await.expect("blocking worker started");
        worker.abort();
        let _ = worker.await;
        assert_eq!(
            limiter.available_permits(),
            0,
            "cancelling the async waiter released a still-running blocking job"
        );

        release_tx.send(()).expect("release blocking worker");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while limiter.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("blocking worker did not release its permit");
    }

    #[tokio::test]
    async fn web_search_batch_survives_empty_and_failed_queries() {
        use crate::providers::ctx::test_exec_context;
        use crate::providers::tool::web_client::SearchResult;
        use async_trait::async_trait;
        use mermaid_domain::{ToolCallId, ToolStatus, TurnId};
        use std::sync::Arc;

        struct Mock;
        #[async_trait]
        impl SearchProvider for Mock {
            async fn search(
                &self,
                query: &str,
                _count: usize,
                _budget: crate::providers::ctx::WebByteBudget,
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
            backend_name: "mock",
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
        match &out.metadata.detail {
            ToolMetadata::WebSearch {
                partial, failures, ..
            } => {
                assert!(*partial);
                assert_eq!(failures.len(), 1);
                assert_eq!(failures[0].query_index, 2);
                assert!(failures[0].error.contains("backend down"));
            },
            other => panic!("expected web search metadata, got {other:?}"),
        }

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
        match &out.metadata.detail {
            ToolMetadata::WebSearch {
                failed_queries,
                failures,
                ..
            } => {
                assert_eq!(*failed_queries, 2);
                assert_eq!(failures.len(), 2);
            },
            other => panic!("expected web search metadata, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_search_batch_caps_concurrency_and_restores_input_order() {
        use crate::providers::ctx::test_exec_context;
        use crate::providers::tool::web_client::SearchResult;
        use mermaid_domain::{ToolCallId, ToolStatus, TurnId};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ConcurrencyMock {
            active: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl SearchProvider for ConcurrencyMock {
            async fn search(
                &self,
                query: &str,
                _count: usize,
                _budget: crate::providers::ctx::WebByteBudget,
            ) -> anyhow::Result<Vec<SearchResult>> {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
                let delay = 5 + (6 - query.parse::<u64>().unwrap_or(0)) * 5;
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(vec![SearchResult {
                    title: format!("result {query}"),
                    url: format!("https://example.com/{query}"),
                    snippet: String::new(),
                    full_content: format!("content {query}"),
                }])
            }
        }

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tool = WebSearchTool {
            backend: Arc::new(ConcurrencyMock {
                active: active.clone(),
                peak: peak.clone(),
            }),
            backend_name: "mock",
        };
        let (ctx, _rx) =
            test_exec_context(TurnId(20), ToolCallId(20), std::path::PathBuf::from("/tmp"));
        let queries: Vec<_> = (0..6)
            .map(|index| serde_json::json!({"query": index.to_string()}))
            .collect();
        let outcome = tool
            .execute(serde_json::json!({"queries": queries}), ctx)
            .await;
        assert_eq!(outcome.status, ToolStatus::Success);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(
            peak.load(Ordering::SeqCst),
            mermaid_model::constants::MAX_WEB_SEARCH_CONCURRENCY
        );
        let mut cursor = 0;
        for index in 0..6 {
            let marker = format!("=== query: {index} ===");
            let position = outcome.output()[cursor..]
                .find(&marker)
                .map(|offset| cursor + offset)
                .expect("ordered query section");
            assert!(position >= cursor);
            cursor = position + marker.len();
        }
    }

    #[tokio::test]
    async fn web_search_complete_output_budget_is_byte_exact_for_multibyte_text() {
        use crate::providers::ctx::test_exec_context;
        use crate::providers::tool::web_client::SearchResult;
        use mermaid_domain::{ToolCallId, ToolStatus, TurnId};

        struct MultibyteMock;

        #[async_trait]
        impl SearchProvider for MultibyteMock {
            async fn search(
                &self,
                _query: &str,
                _count: usize,
                _budget: crate::providers::ctx::WebByteBudget,
            ) -> anyhow::Result<Vec<SearchResult>> {
                Ok(vec![SearchResult {
                    title: "界".repeat(5_000),
                    url: "https://example.com/result".to_string(),
                    snippet: String::new(),
                    full_content: "界".repeat(20_000),
                }])
            }
        }

        let tool = WebSearchTool {
            backend: Arc::new(MultibyteMock),
            backend_name: "mock",
        };
        let (ctx, _rx) =
            test_exec_context(TurnId(21), ToolCallId(21), std::path::PathBuf::from("/tmp"));
        let outcome = tool
            .execute(serde_json::json!({"query": "multibyte"}), ctx)
            .await;

        assert_eq!(outcome.status, ToolStatus::Success);
        assert!(
            outcome.output().len() <= mermaid_model::constants::WEB_SEARCH_AGGREGATE_MAX_BYTES,
            "{} bytes escaped the complete search-result cap",
            outcome.output().len()
        );
        assert!(std::str::from_utf8(outcome.output().as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn web_search_total_failure_and_structured_errors_are_byte_bounded() {
        use crate::providers::ctx::test_exec_context;
        use mermaid_domain::{ToolCallId, ToolStatus, TurnId};

        struct LargeFailure;

        #[async_trait]
        impl SearchProvider for LargeFailure {
            async fn search(
                &self,
                _query: &str,
                _count: usize,
                _budget: crate::providers::ctx::WebByteBudget,
            ) -> anyhow::Result<Vec<crate::providers::tool::web_client::SearchResult>> {
                Err(anyhow::anyhow!("{}", "界".repeat(20_000)))
            }
        }

        let tool = WebSearchTool {
            backend: Arc::new(LargeFailure),
            backend_name: "mock",
        };
        let (ctx, _rx) =
            test_exec_context(TurnId(22), ToolCallId(22), std::path::PathBuf::from("/tmp"));
        let outcome = tool
            .execute(
                serde_json::json!({"queries": [{"query": "one"}, {"query": "two"}]}),
                ctx,
            )
            .await;

        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(
            outcome.output().len() <= mermaid_model::constants::WEB_SEARCH_AGGREGATE_MAX_BYTES,
            "{} bytes escaped the complete search error cap",
            outcome.output().len()
        );
        let ToolMetadata::WebSearch { failures, .. } = &outcome.metadata.detail else {
            panic!("expected web search metadata");
        };
        assert_eq!(failures.len(), 2);
        assert!(
            failures
                .iter()
                .all(|failure| failure.error.len() <= MAX_WEB_SEARCH_FAILURE_BYTES)
        );
    }
}
