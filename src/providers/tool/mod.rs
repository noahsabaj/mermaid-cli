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
//!   3. Register it in `ToolRegistry::default()`.
//!
//! Because `schema()` lives on the same trait as `execute()`, the
//! name + JSON schema the model sees cannot drift from the handler
//! that runs when the model calls it. Single source of truth.

pub mod computer_use;
pub mod exec;
pub mod filesystem;
pub mod mcp;
pub mod memory;
pub mod path_safety;
pub mod policy_gate;
pub mod subagent;
pub mod web;
pub mod web_client;

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::{ToolDefinition, ToolOutcome};

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
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn ToolExecutor>) {
        self.entries.insert(tool.name(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ToolExecutor>> {
        self.entries.get(name).cloned()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

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
    pub fn describe_all(&self) -> Vec<ToolDefinition> {
        self.entries
            .values()
            .filter(|t| !t.is_internal())
            .map(|t| t.schema())
            .collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(filesystem::ReadFileTool));
        r.register(Arc::new(filesystem::WriteFileTool));
        r.register(Arc::new(filesystem::EditFileTool));
        r.register(Arc::new(filesystem::DeleteFileTool));
        r.register(Arc::new(filesystem::CreateDirectoryTool));
        r.register(Arc::new(exec::ExecuteCommandTool));
        r.register(Arc::new(memory::MemoryTool));
        // MCP proxy is the dispatcher for every mcp__server__tool
        // call; it's internal (not advertised) but MUST be registered
        // so runtime lookups succeed.
        r.register(Arc::new(mcp::McpToolProxy));
        r
    }
}

/// Whether the host mermaid process is running interactively (TUI)
/// or headlessly (one-shot `mermaid run <prompt>` / CI). Controls
/// which tools get registered: headless mode never advertises
/// GUI / computer-use tools even when a display probes alive, because
/// a CI job has no user to watch the screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    Interactive,
    Headless,
}

impl ToolRegistry {
    /// Register the computer-use tools the given backend can actually drive.
    /// Screenshot works on every usable backend; the five input tools need
    /// pointer/keyboard injection (X11/Wayland); `list_windows` is X11-only.
    /// Advertising a tool the backend can't drive means the model is told it
    /// has a capability that `bail!`s at call time (#35).
    fn register_computer_use_tools(&mut self, backend: computer_use::Backend) {
        let driver = Arc::new(computer_use::ComputerUseDriver::new(backend));
        self.register(Arc::new(computer_use::ScreenshotTool::new(driver.clone())));
        if backend.supports_input_injection() {
            self.register(Arc::new(computer_use::ClickTool::new(driver.clone())));
            self.register(Arc::new(computer_use::TypeTextTool::new(driver.clone())));
            self.register(Arc::new(computer_use::PressKeyTool::new(driver.clone())));
            self.register(Arc::new(computer_use::ScrollTool::new(driver.clone())));
            self.register(Arc::new(computer_use::MouseMoveTool::new(driver.clone())));
        }
        if backend.supports_window_listing() {
            self.register(Arc::new(computer_use::ListWindowsTool::new(driver.clone())));
        }
    }

    /// Config-aware factory. Always registers filesystem + exec +
    /// the MCP proxy + the subagent tool. Conditionally registers:
    ///
    ///   - `web_search` + `web_fetch` iff `OLLAMA_API_KEY` resolves
    ///     (via `utils::resolve_api_key`). Without a key, the tools
    ///     would error on every call — so we don't advertise them at
    ///     all.
    ///   - The computer-use tools the detected backend can drive (see
    ///     `register_computer_use_tools`) iff `mode == Interactive` AND
    ///     `computer_use::probe()` returns a usable backend.
    ///
    /// `providers` is the shared `ProviderFactory` that the effect
    /// runner also holds; the `SubagentSpawner` needs it so child
    /// reducer loops hit the same provider cache.
    ///
    /// Returns `Arc<Self>` so the effect runner can share a handle
    /// across turns without cloning the underlying HashMap.
    pub fn build(
        _config: &crate::app::Config,
        mode: TuiMode,
        providers: Arc<crate::providers::ProviderFactory>,
    ) -> Arc<Self> {
        let mut r = Self::new();
        r.register(Arc::new(filesystem::ReadFileTool));
        r.register(Arc::new(filesystem::WriteFileTool));
        r.register(Arc::new(filesystem::EditFileTool));
        r.register(Arc::new(filesystem::DeleteFileTool));
        r.register(Arc::new(filesystem::CreateDirectoryTool));
        r.register(Arc::new(exec::ExecuteCommandTool));
        r.register(Arc::new(memory::MemoryTool));
        r.register(Arc::new(mcp::McpToolProxy));

        if let Some(key) = crate::utils::resolve_api_key("OLLAMA_API_KEY", None) {
            r.register(Arc::new(web::WebSearchTool::new(key.clone())));
            r.register(Arc::new(web::WebFetchTool::new(key)));
        }

        // Computer-use tools only register when (a) the process runs
        // interactively (Headless CI has no user to watch a screenshot)
        // AND (b) a display backend passes the startup probe. Failed
        // probe → tools aren't advertised → model can't call them.
        if mode == TuiMode::Interactive {
            let backend = computer_use::probe();
            if backend.is_usable() {
                r.register_computer_use_tools(backend);
            }
        }

        // Subagents: always register. Depth + breadth caps live on
        // `SubagentSpawner`; the tool itself is harmless when nobody
        // calls it. Headless runs do register the agent — a CI prompt
        // may still delegate to subagents for batched work.
        let spawner = Arc::new(subagent::SubagentSpawner::new(providers));
        r.register(Arc::new(subagent::SubagentTool::new(spawner)));

        Arc::new(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_has_builtin_tools() {
        let r = ToolRegistry::default();
        for name in &[
            "read_file",
            "write_file",
            "edit_file",
            "delete_file",
            "create_directory",
            "execute_command",
            "memory",
        ] {
            assert!(r.get(name).is_some(), "missing: {}", name);
        }
        assert!(r.get("not_a_tool").is_none());
        assert!(r.len() >= 6);
    }

    #[test]
    fn computer_use_registration_is_selective_per_backend() {
        use computer_use::Backend;
        let reg = |b: Backend| {
            let mut r = ToolRegistry::new();
            r.register_computer_use_tools(b);
            r
        };

        // macOS: only screenshot — the input verbs + list_windows bail at
        // runtime, so advertising them just wastes the model's turns (#35).
        let mac = reg(Backend::MacOS);
        assert!(mac.get("screenshot").is_some());
        for t in [
            "click",
            "type_text",
            "press_key",
            "scroll",
            "mouse_move",
            "list_windows",
        ] {
            assert!(mac.get(t).is_none(), "macOS must not advertise {t}");
        }

        // Wayland: input tools, but no list_windows (no portable enumeration).
        let way = reg(Backend::Wayland);
        assert!(way.get("click").is_some());
        assert!(way.get("list_windows").is_none());

        // X11: all seven.
        let x11 = reg(Backend::X11);
        for t in [
            "screenshot",
            "click",
            "type_text",
            "press_key",
            "scroll",
            "mouse_move",
            "list_windows",
        ] {
            assert!(x11.get(t).is_some(), "X11 missing {t}");
        }
    }

    #[test]
    fn describe_all_returns_one_per_user_facing_tool() {
        let r = ToolRegistry::default();
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
        let r = ToolRegistry::default();
        let proxy = r.get("mcp_proxy").expect("mcp_proxy registered");
        assert!(proxy.is_internal());
        assert!(!r.describe_all().iter().any(|s| s.name == "mcp_proxy"));
    }

    #[test]
    fn schema_name_matches_executor_name() {
        let r = ToolRegistry::default();
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
    fn build_registers_web_tools_when_key_present() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OLLAMA_API_KEY").ok();
        unsafe {
            std::env::set_var("OLLAMA_API_KEY", "test-key-build");
        }
        let cfg = crate::app::Config::default();
        let providers = Arc::new(crate::providers::ProviderFactory::new(cfg.clone()));
        let r = ToolRegistry::build(&cfg, TuiMode::Interactive, providers);
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
    fn build_skips_web_tools_without_key() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OLLAMA_API_KEY").ok();
        unsafe {
            std::env::remove_var("OLLAMA_API_KEY");
        }
        let cfg = crate::app::Config::default();
        let providers = Arc::new(crate::providers::ProviderFactory::new(cfg.clone()));
        let r = ToolRegistry::build(&cfg, TuiMode::Headless, providers);
        assert!(r.get("web_search").is_none(), "web_search skipped");
        assert!(r.get("web_fetch").is_none(), "web_fetch skipped");
        assert!(r.get("read_file").is_some());
        assert!(r.get("execute_command").is_some());
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("OLLAMA_API_KEY", v);
            }
        }
    }
}
