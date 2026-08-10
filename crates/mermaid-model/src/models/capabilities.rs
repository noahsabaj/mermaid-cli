//! Per-model capability metadata: the STATIC baseline an adapter advertises.
//!
//! Adapters expose `ModelCapabilities` via `Model::capabilities()` so the
//! rest of the codebase can ask facts like "does this model support tool
//! calls?" without per-provider string matching scattered through the
//! codebase.
//!
//! This is the WEAKEST of three capability sources, and deliberately so.
//! Precedence, strongest first:
//!
//!   1. **Live probes.** The provider wrapper's `resolve_context_window`
//!      (provider `/models` metadata, Ollama `/api/show`, cached in the
//!      runtime store's `provider_probes`), the vision probe surfaced as
//!      `Msg::ProviderVisionResolved`, and the Ollama placement check.
//!      Fresh truth from the running provider always wins.
//!   2. **The catalog** (`super::catalog`). The static per-model table for
//!      facts no provider API exposes: thinking wire shapes, effort
//!      ceilings, temperature support, and the vision markers the
//!      OpenAI-compatible long tail consults here.
//!   3. **These constructors.** The per-provider statics: the two
//!      decisions an adapter can make with no model name in hand -- does
//!      this provider family accept image input, and which reasoning enum
//!      does it speak.
//!
//! `max_context_tokens` / `max_output_tokens` stay `None` at this level on
//! purpose -- static pins rot (see `catalog.rs`'s header for the full
//! argument); live discovery fills them. The Meta adapter is the one
//! deliberate exception: its documented muse-spark family limits ride on
//! the struct because no Meta endpoint exposes them.

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
    /// The statically advertised baseline: tool calling on, context and
    /// output windows unknown (live discovery fills them -- source 1 in the
    /// module doc), no continuation state. The two parameters are the only
    /// decisions an adapter makes at this level; every adapter constructor
    /// routes through here so "windows stay `None`" is a property of the
    /// type, not a convention repeated per provider.
    #[must_use]
    pub fn advertised(supports_vision: bool, supports_reasoning: ReasoningCapability) -> Self {
        Self {
            supports_tools: true,
            supports_vision,
            supports_reasoning,
            max_context_tokens: None,
            max_output_tokens: None,
            emits_provider_continuation: false,
        }
    }

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
    #[must_use]
    pub fn ollama_default() -> Self {
        Self::advertised(false, ReasoningCapability::Binary)
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
    fn advertised_baseline_leaves_windows_to_live_discovery() {
        // Source 3 of 3: the static baseline must never pin a window --
        // that is live discovery's job (source 1), and a static pin here
        // would silently outrank nothing and rot quietly.
        let caps = ModelCapabilities::advertised(true, ReasoningCapability::Binary);
        assert!(caps.supports_tools, "every adapter advertises tool calling");
        assert!(caps.supports_vision);
        assert!(caps.max_context_tokens.is_none());
        assert!(caps.max_output_tokens.is_none());
        assert!(!caps.emits_provider_continuation);
        assert!(
            ModelCapabilities::advertised(false, ReasoningCapability::Binary)
                .with_provider_continuation()
                .emits_provider_continuation
        );
    }

    #[test]
    fn capabilities_are_cloneable() {
        let caps = ModelCapabilities::ollama_default();
        let cloned = caps.clone();
        assert_eq!(cloned.supports_tools, caps.supports_tools);
    }
}
