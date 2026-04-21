//! Tool executors — one type per tool the model can call.
//!
//! The trait is intentionally tiny: `execute(args, ctx) -> ToolOutcome`.
//! Everything else (cancellation, progress, identity, workdir) rides
//! inside `ExecContext`. A new tool is ~50 lines of state-free code;
//! no boilerplate for plumbing in observers or dispatch wiring.
//!
//! Adding a tool:
//!   1. New file under `src/providers/tool/`.
//!   2. Impl `ToolExecutor` for a unit struct.
//!   3. Register it in the `tool::registry()` assembly.
//!   4. Add a `ToolDefinition` entry for the outgoing request schema.

pub mod filesystem;

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::ToolOutcome;

use super::ctx::ExecContext;

/// Implemented by every tool that the model can call. All tools are
/// `Send + Sync` — they run across tokio `select!` branches inside
/// the effect runner.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Canonical name the model uses to call this tool. Must match
    /// the `ToolDefinition.name` shipped in the outgoing request.
    fn name(&self) -> &'static str;

    /// Run the tool. The returned `ToolOutcome` is passed verbatim
    /// into `Msg::ToolFinished` — there's no error-to-outcome
    /// conversion happening outside this function.
    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome;
}

/// Small registry the effect runner consults to dispatch a tool
/// call. Built once at startup; read-only after that.
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
}

impl Default for ToolRegistry {
    fn default() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(filesystem::ReadFileTool));
        r.register(Arc::new(filesystem::WriteFileTool));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_has_read_and_write() {
        let r = ToolRegistry::default();
        assert!(r.get("read_file").is_some());
        assert!(r.get("write_file").is_some());
        assert!(r.get("not_a_tool").is_none());
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn registry_names_iterator() {
        let r = ToolRegistry::default();
        let names: Vec<_> = r.names().collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
    }
}
