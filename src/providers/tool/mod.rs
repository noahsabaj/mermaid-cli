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

pub mod exec;
pub mod filesystem;
pub mod mcp;
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
        // MCP proxy is the dispatcher for every mcp__server__tool
        // call; it's internal (not advertised) but MUST be registered
        // so runtime lookups succeed.
        r.register(Arc::new(mcp::McpToolProxy));
        r
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
        ] {
            assert!(r.get(name).is_some(), "missing: {}", name);
        }
        assert!(r.get("not_a_tool").is_none());
        assert!(r.len() >= 6);
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
}
