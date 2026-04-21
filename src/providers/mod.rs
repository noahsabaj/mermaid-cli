//! Provider-facing traits: `ModelProvider` + `ToolExecutor`.
//!
//! This module is the boundary between "inert data the reducer
//! produces" and "real I/O the effect runner performs." Concrete
//! provider adapters (Ollama, Anthropic, Gemini, OpenAI-compat) live
//! under `model/`; tool implementations live under `tool/`.
//!
//! The goal is that adding a provider or tool touches exactly one
//! file — no observer plumbing, no dispatch wiring, no adapter-
//! specific retry logic. The traits carry the full surface; the
//! effect runner just looks up an impl and calls it.
//!
//! C3 ships the trait definitions + Ollama as the proof-of-pattern
//! `ModelProvider` + `ReadFileTool`/`WriteFileTool` as the
//! proof-of-pattern `ToolExecutor`. C4 ports the remaining three
//! provider adapters; C5 ports the remaining tools.

pub mod capabilities;
pub mod ctx;
pub mod factory;
pub mod model;
pub mod tool;

pub use capabilities::Capabilities;
pub use ctx::{
    ExecContext, FinalResponse, ProgressEvent, StreamContext, StreamEvent, clone_messages,
    test_exec_context, test_stream_context,
};
pub use factory::ProviderFactory;
pub use model::{
    AnthropicProvider, GeminiProvider, ModelProvider, OllamaProvider, OpenAICompatProvider,
};
pub use tool::{ToolExecutor, ToolRegistry};

use std::sync::Arc;

/// Registry of available `ModelProvider`s keyed by model ID
/// (`provider/name`). C4 wires the full lookup; for now this is an
/// empty shell so the effect runner can consult it once the full
/// ecosystem is in.
pub struct Providers {
    entries: std::collections::HashMap<String, Arc<dyn ModelProvider>>,
}

impl Providers {
    pub fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
        }
    }

    pub fn register(&mut self, key: impl Into<String>, provider: Arc<dyn ModelProvider>) {
        self.entries.insert(key.into(), provider);
    }

    pub fn get(&self, key: &str) -> Option<Arc<dyn ModelProvider>> {
        self.entries.get(key).cloned()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for Providers {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_registry_is_empty() {
        let p = Providers::default();
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        assert!(p.get("anything").is_none());
    }
}
