//! Tool executors — one type per tool the model can call.
//!
//! The trait is small: `execute(args, ctx) -> ToolOutcome` for
//! dispatch, plus `schema() -> ToolDefinition` for advertising the
//! tool to the model. Everything else (cancellation, progress,
//! identity, workdir) rides inside `ExecContext`.
//!
//! Adding a tool:
//!   1. New file under `src/providers/tool/`.
//!   2. Impl `ToolExecutor` for a unit struct — both `execute` and
//!      `schema`.
//!   3. Register it in `ToolRegistry::build()` — the ONE factory production
//!      uses. There is no second registry to forget.
//!
//! Because `schema()` lives on the same trait as `execute()`, the
//! name + JSON schema the model sees cannot drift from the handler
//! that runs when the model calls it. Single source of truth.

pub mod apply_patch;
pub mod ask_user_question;
pub mod enter_plan_mode;
pub mod exec;
pub mod exit_plan_mode;
pub mod filesystem;
pub mod mcp;
pub mod memory;
pub mod path_lock;
pub mod path_safety;
pub mod policy_gate;
pub mod subagent;
pub mod tasks;
pub mod web;
pub mod web_client;
pub mod workspace;

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use mermaid_domain::{ToolDefinition, ToolOutcome};

use super::ctx::ExecContext;

/// Implemented by every tool that the model can call. All tools are
/// `Send + Sync` — they run across tokio `select!` branches inside
/// the effect runner.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Canonical name the model uses to call this tool. Matches
    /// `schema().name` exactly.
    fn name(&self) -> &'static str;

    /// JSON-schema description the model sees in the outgoing
    /// request. Adapters translate this into provider-native shape
    /// (Anthropic's `type: "custom"`, Gemini's `function_declarations`,
    /// OpenAI's flat `tools`, Ollama's function calling). The same
    /// `ToolDefinition` feeds all four.
    fn schema(&self) -> ToolDefinition;

    /// True for tools that exist for internal dispatch only and
    /// should NOT be advertised to the model (e.g. the MCP proxy
    /// router, which fronts every `mcp__server__tool` call — the
    /// individual MCP tools are advertised separately from
    /// `state.mcp.servers`). Default `false`.
    fn is_internal(&self) -> bool {
        false
    }

    /// Run the tool. The returned `ToolOutcome` is passed verbatim
    /// into `Msg::ToolFinished` — there's no error-to-outcome
    /// conversion happening outside this function.
    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome;
}

/// Registry of dispatchable tools. Single source of truth for what
/// the model sees AND what handles a call when the model issues it.
/// Built once at startup; read-only after that.
pub struct ToolRegistry {
    entries: HashMap<&'static str, Arc<dyn ToolExecutor>>,
    /// Teaching errors for tools that were CONSIDERED at build time and
    /// deliberately not registered (backend unavailable, network denied,
    /// filtered out of a child registry). Dispatch returns the reason when
    /// the model calls one, so the model learns why the tool is absent and
    /// what to do about it — a bare "unknown tool" reads as a schema bug
    /// and was observed driving models to fabricate results instead of
    /// reporting the gap.
    unavailable: HashMap<&'static str, String>,
    /// Startup-resolved web routing/viability shared by the parent registry,
    /// its provider-facing definitions, UI diagnostics, and every child
    /// registry. Keeping the backend clients here prevents credentials or
    /// environment changes from silently re-resolving a different route.
    web_capabilities: Option<Arc<web::WebCapabilities>>,
    /// Direct handle to the subagent spawner (also reachable through the
    /// `agent` tool entry, but `dyn ToolExecutor` can't be downcast). The
    /// effect layer uses it to service `Cmd::KillBackgroundAgent`. `None`
    /// in registries built without a spawner (child registries, tests).
    subagent_spawner: Option<Arc<subagent::SubagentSpawner>>,
}

/// An empty registry. Every session's registry comes from [`ToolRegistry::build`];
/// this exists for callers that assemble one by hand (stubs, the subagent
/// child registry) and for the `Default` convention, never as a second list
/// of built-in tools.
impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            unavailable: HashMap::new(),
            web_capabilities: None,
            subagent_spawner: None,
        }
    }

    #[must_use]
    pub fn web_capabilities(&self) -> Option<&web::WebCapabilities> {
        self.web_capabilities.as_deref()
    }

    #[must_use]
    pub fn subagent_spawner(&self) -> Option<&Arc<subagent::SubagentSpawner>> {
        self.subagent_spawner.as_ref()
    }

    pub fn register(&mut self, tool: Arc<dyn ToolExecutor>) {
        self.entries.insert(tool.name(), tool);
    }

    /// Record why a tool that could exist in this registry deliberately does
    /// not. The reason is model-facing: it must name the cause and the
    /// remediation, because it is returned verbatim when the model calls the
    /// absent tool.
    pub fn note_unavailable(&mut self, tool: &'static str, reason: impl Into<String>) {
        self.unavailable.insert(tool, reason.into());
    }

    #[must_use]
    pub fn unavailable_reason(&self, name: &str) -> Option<&str> {
        self.unavailable.get(name).map(String::as_str)
    }

    /// The outcome for a call this registry cannot dispatch: the recorded
    /// teaching error when the tool was deliberately omitted, else the plain
    /// unknown-tool error. `called_name` is the name the model used — for
    /// MCP calls it differs from the internal `mcp_proxy` routing key, and
    /// the model should see the name it actually wrote.
    #[must_use]
    pub fn unknown_tool_outcome(&self, tool_key: &str, called_name: &str) -> ToolOutcome {
        self.unavailable.get(tool_key).map_or_else(
            || ToolOutcome::error(format!("unknown tool: {called_name}"), 0.0),
            |reason| ToolOutcome::error(format!("{called_name} is not available: {reason}"), 0.0),
        )
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolExecutor>> {
        self.entries.get(name).cloned()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.entries.keys().copied()
    }

    /// Emit every user-facing tool's schema, for inclusion in an
    /// outgoing `ChatRequest.tools`. Effect runner calls this before
    /// dispatching `Cmd::CallModel` so the model always sees the
    /// same list the runner can dispatch. Internal routers (the MCP
    /// proxy) are filtered out.
    #[must_use]
    pub fn describe_all(&self) -> Vec<ToolDefinition> {
        self.entries
            .values()
            .filter(|t| !t.is_internal())
            .map(|t| t.schema())
            .collect()
    }
}

impl ToolRegistry {
    /// Config-aware factory. Always registers filesystem + exec +
    /// the MCP proxy + the subagent tool. Conditionally registers:
    ///
    ///   - Viable `web_fetch` and `web_search` capabilities resolved once by
    ///     `web::WebCapabilities`. Global network denial omits both.
    ///
    /// `providers` is the shared `ProviderFactory` that the effect
    /// runner also holds; the `SubagentSpawner` needs it so child
    /// reducer loops hit the same provider cache.
    ///
    /// Returns `Arc<Self>` so the effect runner can share a handle
    /// across turns without cloning the underlying `HashMap`.
    pub fn build(
        config: &mermaid_domain::Config,
        providers: Arc<crate::providers::ProviderFactory>,
    ) -> Arc<Self> {
        let mut r = Self::new();
        let web_capabilities = Arc::new(web::WebCapabilities::resolve(&config.web));
        r.register(Arc::new(filesystem::ReadFileTool));
        r.register(Arc::new(filesystem::WriteFileTool));
        r.register(Arc::new(filesystem::EditFileTool));
        r.register(Arc::new(apply_patch::ApplyPatchTool));
        r.register(Arc::new(filesystem::DeleteFileTool));
        r.register(Arc::new(filesystem::CreateDirectoryTool));
        r.register(Arc::new(exec::ExecuteCommandTool));
        r.register(Arc::new(memory::MemoryTool));
        r.register(Arc::new(ask_user_question::AskUserQuestionTool));
        r.register(Arc::new(enter_plan_mode::EnterPlanModeTool));
        r.register(Arc::new(exit_plan_mode::ExitPlanModeTool));
        r.register(Arc::new(tasks::TaskCreateTool));
        r.register(Arc::new(tasks::TaskUpdateTool));
        r.register(Arc::new(tasks::TaskListTool));
        r.register(Arc::new(mcp::McpToolProxy));

        // `safety.network = "deny"` is a global egress kill-switch, not only
        // a shell sandbox flag. Omit web capabilities entirely so adapters and
        // subagents cannot advertise or execute them — and record why, so a
        // model that calls one anyway is taught the cause instead of shown a
        // bare "unknown tool".
        if config.safety.network == mermaid_domain::NetworkPolicy::Allow {
            match web_capabilities.fetch_tool() {
                Some(tool) => r.register(Arc::new(tool)),
                None => r.note_unavailable(
                    "web_fetch",
                    web_capabilities.fetch.absence_reason("web_fetch"),
                ),
            }
            match web_capabilities.search_tool() {
                Some(tool) => r.register(Arc::new(tool)),
                None => r.note_unavailable(
                    "web_search",
                    web_capabilities.search.absence_reason("web_search"),
                ),
            }
        } else {
            for tool in ["web_fetch", "web_search"] {
                r.note_unavailable(
                    tool,
                    format!(
                        "{tool} is disabled: network access is off \
                         (safety.network = \"deny\" / --no-network)"
                    ),
                );
            }
        }

        // Subagents: always register. Depth + breadth caps live on
        // `SubagentSpawner`; the tool itself is harmless when nobody
        // calls it. Headless runs do register the agent — a CI prompt
        // may still delegate to subagents for batched work.
        let spawner = Arc::new(subagent::SubagentSpawner::new(
            providers,
            Arc::clone(&web_capabilities),
        ));
        r.register(Arc::new(subagent::SubagentTool::new(spawner.clone())));
        r.subagent_spawner = Some(spawner);
        r.web_capabilities = Some(web_capabilities);

        Arc::new(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production registry as a headless session sees it: `build()` with
    /// a default config and a stub provider factory. Tests go through the same
    /// factory production does, so a tool registered anywhere else does not
    /// exist as far as they are concerned.
    fn headless_registry() -> Arc<ToolRegistry> {
        let cfg = mermaid_domain::Config::default();
        let providers = Arc::new(crate::providers::ProviderFactory::new(cfg.clone()));
        ToolRegistry::build(&cfg, providers)
    }

    #[test]
    fn default_registry_has_builtin_tools() {
        let r = headless_registry();
        for name in &[
            "read_file",
            "write_file",
            "edit_file",
            "apply_patch",
            "delete_file",
            "create_directory",
            "execute_command",
            "memory",
        ] {
            assert!(r.get(name).is_some(), "missing: {name}");
        }
        assert!(r.get("not_a_tool").is_none());
        assert!(r.len() >= 6);
    }

    #[test]
    fn describe_all_returns_one_per_user_facing_tool() {
        let r = headless_registry();
        let schemas = r.describe_all();
        // mcp_proxy is registered but internal — filtered out of
        // describe_all. So len() includes it but schemas don't.
        let visible = r
            .names()
            .filter(|n| r.get(n).map(|t| !t.is_internal()).unwrap_or(false))
            .count();
        assert_eq!(schemas.len(), visible);
        for schema in &schemas {
            assert!(
                r.get(&schema.name).is_some(),
                "schema for unknown tool: {}",
                schema.name
            );
        }
    }

    #[test]
    fn mcp_proxy_is_registered_but_internal() {
        let r = headless_registry();
        let proxy = r.get("mcp_proxy").expect("mcp_proxy registered");
        assert!(proxy.is_internal());
        assert!(!r.describe_all().iter().any(|s| s.name == "mcp_proxy"));
    }

    #[test]
    fn schema_name_matches_executor_name() {
        let r = headless_registry();
        for name in r.names() {
            let tool = r.get(name).unwrap();
            assert_eq!(tool.name(), tool.schema().name.as_str());
        }
    }

    /// Serialization guard for tests that mutate the `OLLAMA_API_KEY`
    /// env var. Cargo's default test harness runs tests in parallel
    /// threads inside one process; without this mutex two env-touching
    /// tests would race and occasionally flip each other's expectations.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn build_registers_zero_config_web_tools_without_key() {
        // Both web tools register with no OLLAMA_API_KEY: web_fetch is native,
        // and web_search defaults to `auto`, which falls back to a managed local
        // SearXNG (the process starts lazily at call time, not here).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OLLAMA_API_KEY").ok();
        unsafe {
            std::env::remove_var("OLLAMA_API_KEY");
        }
        let cfg = mermaid_domain::Config::default();
        let providers = Arc::new(crate::providers::ProviderFactory::new(cfg.clone()));
        let r = ToolRegistry::build(&cfg, providers);
        assert!(
            r.get("web_fetch").is_some(),
            "native web_fetch registers without a key"
        );
        assert_eq!(
            r.get("web_search").is_some(),
            crate::searxng::managed_backend_viability().is_ok(),
            "auto web_search registers only when managed SearXNG is viable"
        );
        assert!(r.get("read_file").is_some());
        assert!(r.get("execute_command").is_some());
        let web = r
            .web_capabilities()
            .expect("config-aware registries retain the resolved web status");
        assert_eq!(web.fetch.backend, "native");
        assert_eq!(web.search.backend, "managed_searxng");
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("OLLAMA_API_KEY", v);
            }
        }
    }

    #[test]
    fn build_registers_ollama_web_search_with_key() {
        // Cloud routing is explicit: a key plus an explicit Ollama backend
        // registers search without changing the native fetch default.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OLLAMA_API_KEY").ok();
        unsafe {
            std::env::set_var("OLLAMA_API_KEY", "test-key-build");
        }
        let mut cfg = mermaid_domain::Config::default();
        cfg.web.search_backend = mermaid_domain::SearchBackend::Ollama;
        let providers = Arc::new(crate::providers::ProviderFactory::new(cfg.clone()));
        let r = ToolRegistry::build(&cfg, providers);
        assert!(r.get("web_search").is_some(), "web_search registered");
        assert!(r.get("web_fetch").is_some(), "web_fetch registered");
        unsafe {
            match prior {
                Some(v) => std::env::set_var("OLLAMA_API_KEY", v),
                None => std::env::remove_var("OLLAMA_API_KEY"),
            }
        }
    }

    #[test]
    fn auto_search_never_selects_cloud_just_because_a_key_exists() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OLLAMA_API_KEY").ok();
        unsafe {
            std::env::set_var("OLLAMA_API_KEY", "test-key-must-not-route");
        }
        let cfg = mermaid_domain::Config::default();
        let capabilities = web::WebCapabilities::resolve(&cfg.web);
        assert_eq!(capabilities.search.backend, "managed_searxng");
        assert_eq!(
            capabilities.search.available,
            crate::searxng::managed_backend_viability().is_ok()
        );
        unsafe {
            match prior {
                Some(value) => std::env::set_var("OLLAMA_API_KEY", value),
                None => std::env::remove_var("OLLAMA_API_KEY"),
            }
        }
    }

    #[test]
    fn auto_search_fallback_engages_only_when_opted_in_with_a_key() {
        // The opt-in flips exactly one case: auto + no viable bundle + key.
        // A viable sovereign default always wins over the fallback, and the
        // not-opted-in side is pinned by
        // `auto_search_never_selects_cloud_just_because_a_key_exists`.
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior = std::env::var("OLLAMA_API_KEY").ok();
        unsafe {
            std::env::set_var("OLLAMA_API_KEY", "test-key-fallback");
        }
        let mut cfg = mermaid_domain::Config::default();
        cfg.web.allow_ollama_search_fallback = true;
        let capabilities = web::WebCapabilities::resolve(&cfg.web);
        if crate::searxng::managed_backend_viability().is_ok() {
            assert_eq!(capabilities.search.backend, "managed_searxng");
            assert!(capabilities.search.available);
        } else {
            assert_eq!(capabilities.search.backend, "ollama_cloud");
            assert!(capabilities.search.available);
            assert_eq!(capabilities.search.egress, web::Egress::OffMachine);
            assert!(
                capabilities.search_tool().is_some(),
                "the fallback must produce a registrable tool"
            );
        }
        unsafe {
            match prior {
                Some(v) => std::env::set_var("OLLAMA_API_KEY", v),
                None => std::env::remove_var("OLLAMA_API_KEY"),
            }
        }
    }

    #[test]
    fn auto_search_fallback_without_a_key_reports_the_whole_chain() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior = std::env::var("OLLAMA_API_KEY").ok();
        unsafe {
            std::env::remove_var("OLLAMA_API_KEY");
        }
        let mut cfg = mermaid_domain::Config::default();
        cfg.web.allow_ollama_search_fallback = true;
        let capabilities = web::WebCapabilities::resolve(&cfg.web);
        if crate::searxng::managed_backend_viability().is_err() {
            assert!(!capabilities.search.available);
            assert_eq!(capabilities.search.backend, "ollama_cloud");
            let reason = capabilities.search.reason.as_deref().unwrap_or_default();
            assert!(reason.contains("managed bundle"), "{reason}");
            assert!(reason.contains("OLLAMA_API_KEY"), "{reason}");
        }
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("OLLAMA_API_KEY", v);
            }
        }
    }

    #[test]
    fn build_registers_searxng_web_search_without_key() {
        // The SearXNG search backend registers regardless of OLLAMA_API_KEY —
        // reachability is a call-time concern, not a registration one.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OLLAMA_API_KEY").ok();
        unsafe {
            std::env::remove_var("OLLAMA_API_KEY");
        }
        let mut cfg = mermaid_domain::Config::default();
        cfg.web.search_backend = mermaid_domain::SearchBackend::Searxng;
        let providers = Arc::new(crate::providers::ProviderFactory::new(cfg.clone()));
        let r = ToolRegistry::build(&cfg, providers);
        assert!(
            r.get("web_search").is_some(),
            "searxng web_search registers without a key"
        );
        assert!(
            r.get("web_fetch").is_some(),
            "native web_fetch still present"
        );
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("OLLAMA_API_KEY", v);
            }
        }
    }

    #[test]
    fn network_deny_omits_all_web_capabilities() {
        let mut cfg = mermaid_domain::Config::default();
        cfg.safety.network = mermaid_domain::NetworkPolicy::Deny;
        cfg.web.search_backend = mermaid_domain::SearchBackend::Searxng;
        let providers = Arc::new(crate::providers::ProviderFactory::new(cfg.clone()));
        let registry = ToolRegistry::build(&cfg, providers);
        assert!(registry.get("web_fetch").is_none());
        assert!(registry.get("web_search").is_none());
        assert!(registry.get("read_file").is_some());
        // Calling an omitted web tool is answered with the cause, not a bare
        // "unknown tool" — that reply was observed driving models to guess.
        for tool in ["web_fetch", "web_search"] {
            let outcome = registry.unknown_tool_outcome(tool, tool);
            let msg = outcome.error_message().unwrap_or_default();
            assert!(msg.contains("safety.network"), "{tool}: {msg}");
        }
        // Matched pair: a registered tool carries no absence note, and a name
        // never considered stays a plain unknown tool.
        assert!(registry.unavailable_reason("read_file").is_none());
        let outcome = registry.unknown_tool_outcome("frobnicate", "frobnicate");
        assert_eq!(
            outcome.error_message().unwrap_or_default(),
            "unknown tool: frobnicate"
        );
    }

    #[test]
    fn unavailable_search_backend_reason_reaches_the_model() {
        // The Windows field logs: `search_backend = "auto"` with no viable
        // managed bundle registered nothing, and calling `web_search` got
        // "unknown tool". The registry must instead carry the viability
        // reason plus the remediation. On hosts where the managed bundle IS
        // viable, the tool registers and no note exists — both sides pinned.
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior = std::env::var("OLLAMA_API_KEY").ok();
        unsafe {
            std::env::remove_var("OLLAMA_API_KEY");
        }
        let cfg = mermaid_domain::Config::default();
        let providers = Arc::new(crate::providers::ProviderFactory::new(cfg.clone()));
        let registry = ToolRegistry::build(&cfg, providers);
        match crate::searxng::managed_backend_viability() {
            Ok(_) => {
                assert!(registry.get("web_search").is_some());
                assert!(registry.unavailable_reason("web_search").is_none());
            },
            Err(viability_reason) => {
                assert!(registry.get("web_search").is_none());
                let reason = registry
                    .unavailable_reason("web_search")
                    .expect("absence reason recorded");
                assert!(
                    reason.contains(&viability_reason),
                    "must carry the real cause: {reason}"
                );
                assert!(
                    reason.contains("search_backend"),
                    "must carry the remediation: {reason}"
                );
            },
        }
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("OLLAMA_API_KEY", v);
            }
        }
    }
}
