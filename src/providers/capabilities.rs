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

use crate::models::{ModelCapabilities, ReasoningCapability};

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
    /// Does the provider emit a verifiable "thinking signature" that
    /// must round-trip on the next request (Anthropic's extended
    /// thinking)? When true, the reducer preserves the signature on
    /// the assistant message via `thinking_signature`.
    pub emits_thinking_signature: bool,
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
            // Only Anthropic emits signatures; we can't tell from
            // `ModelCapabilities` alone. Adapters set this explicitly.
            emits_thinking_signature: false,
        }
    }

    /// Builder: mark that this provider round-trips thinking signatures.
    pub fn with_thinking_signature(mut self) -> Self {
        self.emits_thinking_signature = true;
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
        };
        let caps = Capabilities::from_legacy(&legacy);
        assert!(caps.supports_tools);
        assert!(caps.supports_vision);
        assert_eq!(caps.max_context_tokens, Some(32_000));
        assert!(!caps.emits_thinking_signature);
    }

    #[test]
    fn with_thinking_signature_sets_flag() {
        let legacy = ModelCapabilities::ollama_default();
        let caps = Capabilities::from_legacy(&legacy).with_thinking_signature();
        assert!(caps.emits_thinking_signature);
    }
}
