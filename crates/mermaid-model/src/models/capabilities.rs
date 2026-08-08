//! Per-model capability metadata.
//!
//! Adapters expose `ModelCapabilities` via `Model::capabilities()` so the
//! rest of the codebase can ask facts like "does this model support tool
//! calls?" or "what reasoning levels does it accept?" without per-provider
//! string matching scattered through the codebase. This is the same
//! pattern Roo Code uses on its `ModelInfo` struct (`supports_reasoning_*`
//! flags) and Codex CLI uses on `ModelPreset.supported_reasoning_efforts`.
//!
//! For Step 1 the values are hardcoded conservative defaults. A future
//! step can add per-model lookup (similar to <https://models.dev>) or
//! runtime probing (Ollama `/api/show`).

use super::reasoning::ReasoningCapability;

/// Capability flags advertised by a model adapter.
#[derive(Debug, Clone)]
pub struct ModelCapabilities {
    /// Model accepts tool/function-calling requests in the chat API.
    pub supports_tools: bool,
    /// Model accepts image inputs in messages (vision-capable).
    pub supports_vision: bool,
    /// Reasoning controls the model exposes — see `ReasoningCapability`.
    pub supports_reasoning: ReasoningCapability,
    /// Maximum context window in tokens, if known.
    pub max_context_tokens: Option<usize>,
    /// The model's per-response output ceiling in tokens, if known (from
    /// `/models` metadata or a documented per-model table).
    pub max_output_tokens: Option<usize>,
    /// Does the provider emit opaque continuation data that must round-trip on
    /// the next request (Anthropic thinking, Meta encrypted reasoning)?
    ///
    /// Lived on a near-identical `providers::Capabilities` that wrapped this
    /// struct field-for-field just to carry it. Adapters default it to `false`
    /// and opt in via `with_provider_continuation()`.
    pub emits_provider_continuation: bool,
}

impl ModelCapabilities {
    /// Builder: mark that this provider round-trips continuation state.
    #[must_use]
    pub fn with_provider_continuation(mut self) -> Self {
        self.emits_provider_continuation = true;
        self
    }
}

impl ModelCapabilities {
    /// Conservative defaults for an Ollama-served model. We assume tool
    /// calling (every modern Ollama-supported model the project targets
    /// has it), assume no vision (the safer static default — real vision
    /// support is probed from the `/api/show` `capabilities` array by
    /// `OllamaAdapter::vision_supported` and refreshed into the runtime
    /// snapshot via `Msg::ProviderVisionResolved`), and treat reasoning as
    /// binary on/off (matches the `think: bool` semantics for everything
    /// except gpt-oss).
    pub fn ollama_default() -> Self {
        Self {
            supports_tools: true,
            supports_vision: false,
            supports_reasoning: ReasoningCapability::Binary,
            max_context_tokens: None,
            max_output_tokens: None,
            emits_provider_continuation: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_default_is_conservative() {
        let caps = ModelCapabilities::ollama_default();
        assert!(caps.supports_tools);
        assert!(!caps.supports_vision);
        assert_eq!(caps.supports_reasoning, ReasoningCapability::Binary);
        assert!(caps.max_context_tokens.is_none());
    }

    #[test]
    fn capabilities_are_cloneable() {
        let caps = ModelCapabilities::ollama_default();
        let cloned = caps.clone();
        assert_eq!(cloned.supports_tools, caps.supports_tools);
    }
}
