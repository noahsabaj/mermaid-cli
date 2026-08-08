//! Per-provider capability flags.
//!
//! Providers advertise what they support via `Capabilities`. The
//! reducer uses this to decide (e.g.) whether to show the reasoning
//! slider, whether an MCP tool call shape works, or whether vision
//! inputs are accepted. Future replacements for models.dev
//! (static-lookup) or `/api/show` (runtime probe) plug in here.
//!
//! `Capabilities` is a superset of the adapter-layer
//! `ModelCapabilities`. Adapters translate via `from_legacy` so the
//! provider layer doesn't duplicate capability logic.

use mermaid_model::models::{ModelCapabilities, ReasoningCapability};

/// Capabilities of a model as advertised by its provider adapter.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// Native tool/function calling supported.
    pub supports_tools: bool,
    /// Vision (image inputs on user messages) supported.
    pub supports_vision: bool,
    /// Reasoning controls exposed — binary, enumerated, or numeric.
    pub supports_reasoning: ReasoningCapability,
    /// Maximum context window in tokens, if the adapter knows.
    pub max_context_tokens: Option<usize>,
    /// The model's per-response output ceiling in tokens, if known.
    pub max_output_tokens: Option<usize>,
    /// Does the provider emit opaque continuation data that must round-trip on
    /// the next request (Anthropic thinking or Meta encrypted reasoning)?
    pub emits_provider_continuation: bool,
}

impl Capabilities {
    /// Construct from the adapter-layer `ModelCapabilities`. Used by
    /// provider wrappers so we don't duplicate adapter-side capability
    /// logic.
    pub fn from_legacy(caps: &ModelCapabilities) -> Self {
        Self {
            supports_tools: caps.supports_tools,
            supports_vision: caps.supports_vision,
            supports_reasoning: caps.supports_reasoning.clone(),
            max_context_tokens: caps.max_context_tokens,
            max_output_tokens: caps.max_output_tokens,
            // Provider wrappers opt into continuation explicitly.
            emits_provider_continuation: false,
        }
    }

    /// Builder: mark that this provider round-trips continuation state.
    pub fn with_provider_continuation(mut self) -> Self {
        self.emits_provider_continuation = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_legacy_preserves_flags() {
        let legacy = ModelCapabilities {
            supports_tools: true,
            supports_vision: true,
            supports_reasoning: ReasoningCapability::Binary,
            max_context_tokens: Some(32_000),
            max_output_tokens: Some(8_192),
        };
        let caps = Capabilities::from_legacy(&legacy);
        assert!(caps.supports_tools);
        assert!(caps.supports_vision);
        assert_eq!(caps.max_context_tokens, Some(32_000));
        assert_eq!(caps.max_output_tokens, Some(8_192));
        assert!(!caps.emits_provider_continuation);
    }

    #[test]
    fn with_provider_continuation_sets_flag() {
        let legacy = ModelCapabilities::ollama_default();
        let caps = Capabilities::from_legacy(&legacy).with_provider_continuation();
        assert!(caps.emits_provider_continuation);
    }
}
