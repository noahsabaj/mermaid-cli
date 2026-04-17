//! Model state management
//!
//! Handles LLM configuration and identity.

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::models::{Model, ModelConfig, ReasoningCapability, ReasoningLevel};

/// Model state - LLM configuration and identity
pub struct ModelState {
    pub model: Arc<RwLock<Box<dyn Model>>>,
    pub model_id: String,
    pub model_name: String,
    /// Vision support state:
    /// - Some(true) = model supports vision
    /// - Some(false) = model does not support vision (detected from error)
    /// - None = unknown (optimistic default)
    pub vision_supported: Option<bool>,
    /// Cached snapshot of the underlying model's `supports_reasoning`
    /// capability. Lives here so the sync render path doesn't have to
    /// `try_read()` the `tokio::sync::RwLock<Box<dyn Model>>` once per
    /// frame to compute snap-divergence (Step 5b). Refreshed on
    /// `/model` switch by the slash-command handler.
    pub supported_reasoning: ReasoningCapability,
    /// Base model configuration from app config. Used by build_config() to
    /// produce API-ready ModelConfig with runtime-only fields set.
    pub base_config: ModelConfig,
}

impl ModelState {
    pub fn new(model: Box<dyn Model>, model_id: String, base_config: ModelConfig) -> Self {
        let model_name = model.name().to_string();
        let supported_reasoning = model.capabilities().supports_reasoning.clone();
        Self {
            model: Arc::new(RwLock::new(model)),
            model_id,
            model_name,
            vision_supported: None,
            supported_reasoning,
            base_config,
        }
    }

    /// Get a reference to the model for reading
    pub fn model_ref(&self) -> &Arc<RwLock<Box<dyn Model>>> {
        &self.model
    }

    /// Cycle the reasoning level for the next chat call. Cycle order:
    /// `None → Low → Medium → High → Max → None`. `Minimal` and `XHigh`
    /// are omitted from the cycle (specialist tiers restricted to
    /// specific providers — OpenAI GPT-5 `minimal`, GPT-5.2/Opus-4.7
    /// `xhigh`). Both remain reachable via `/reasoning <level>`. Returns
    /// the new level so the caller can render a status message and persist.
    pub fn cycle_reasoning(&mut self) -> ReasoningLevel {
        let next = match self.base_config.reasoning {
            ReasoningLevel::None => ReasoningLevel::Low,
            ReasoningLevel::Low => ReasoningLevel::Medium,
            ReasoningLevel::Medium => ReasoningLevel::High,
            ReasoningLevel::High => ReasoningLevel::Max,
            ReasoningLevel::Max => ReasoningLevel::None,
            // `Minimal` isn't in the cycle, but if a slash command put us
            // there, the next Alt+T press lands on `Low` (rank+1).
            ReasoningLevel::Minimal => ReasoningLevel::Low,
            // `XHigh` isn't in the cycle either — if the user arrived via
            // `/reasoning xhigh`, Alt+T drops them to `None` to start the
            // standard cycle fresh rather than bouncing back up to Max.
            ReasoningLevel::XHigh => ReasoningLevel::None,
        };
        self.base_config.reasoning = next;
        next
    }

    /// Set the reasoning level explicitly (used by `/reasoning <level>`).
    pub fn set_reasoning(&mut self, level: ReasoningLevel) {
        self.base_config.reasoning = level;
    }

    /// Build a ModelConfig for API calls using current model state.
    /// Clones the base config and sets runtime-only fields.
    pub fn build_config(&self) -> ModelConfig {
        let mut config = self.base_config.clone();
        config.model = self.model_id.clone();
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stub model used to construct a `ModelState` in tests without
    /// needing a real adapter / network.
    struct StubModel;

    #[async_trait::async_trait]
    impl Model for StubModel {
        async fn chat(
            &self,
            _messages: &[crate::models::ChatMessage],
            _config: &ModelConfig,
            _stream_callback: Option<crate::models::StreamCallback>,
        ) -> crate::models::Result<crate::models::ModelResponse> {
            unimplemented!("stub")
        }
        fn name(&self) -> &str {
            "stub"
        }
        fn capabilities(&self) -> &crate::models::ModelCapabilities {
            use std::sync::OnceLock;
            static CAPS: OnceLock<crate::models::ModelCapabilities> = OnceLock::new();
            CAPS.get_or_init(crate::models::ModelCapabilities::ollama_default)
        }
        async fn list_models(&self) -> crate::models::Result<Vec<String>> {
            Ok(vec![])
        }
    }

    fn make_state(initial: ReasoningLevel) -> ModelState {
        let base = ModelConfig {
            reasoning: initial,
            ..Default::default()
        };
        ModelState::new(Box::new(StubModel), "stub/model".to_string(), base)
    }

    #[test]
    fn cycle_reasoning_visits_5_stops_starting_from_none() {
        let mut state = make_state(ReasoningLevel::None);
        assert_eq!(state.cycle_reasoning(), ReasoningLevel::Low);
        assert_eq!(state.cycle_reasoning(), ReasoningLevel::Medium);
        assert_eq!(state.cycle_reasoning(), ReasoningLevel::High);
        assert_eq!(state.cycle_reasoning(), ReasoningLevel::Max);
        assert_eq!(state.cycle_reasoning(), ReasoningLevel::None);
    }

    #[test]
    fn cycle_reasoning_after_minimal_lands_on_low() {
        // `Minimal` is reachable via `/reasoning minimal` but not part of
        // the cycle. The next Alt+T press should resume the cycle, not
        // panic or stay on Minimal.
        let mut state = make_state(ReasoningLevel::Minimal);
        assert_eq!(state.cycle_reasoning(), ReasoningLevel::Low);
    }

    #[test]
    fn cycle_reasoning_after_xhigh_lands_on_none() {
        // `XHigh` is reachable via `/reasoning xhigh` but skipped by the
        // cycle. From XHigh, Alt+T drops to None to start the standard
        // cycle fresh (matching the "exit specialist tier" intent).
        let mut state = make_state(ReasoningLevel::XHigh);
        assert_eq!(state.cycle_reasoning(), ReasoningLevel::None);
    }

    #[test]
    fn set_reasoning_updates_base_config() {
        let mut state = make_state(ReasoningLevel::Medium);
        state.set_reasoning(ReasoningLevel::Max);
        assert_eq!(state.base_config.reasoning, ReasoningLevel::Max);
    }

    #[test]
    fn build_config_propagates_reasoning_from_base() {
        let mut state = make_state(ReasoningLevel::High);
        let config = state.build_config();
        assert_eq!(config.reasoning, ReasoningLevel::High);
        // Cycling mutates only base_config; existing build_config clones
        // are unaffected (lock-free render path).
        state.cycle_reasoning();
        assert_eq!(config.reasoning, ReasoningLevel::High);
        assert_eq!(state.base_config.reasoning, ReasoningLevel::Max);
    }

    /// `supported_reasoning` is cached at construction so the sync render
    /// path doesn't hit the `tokio::sync::RwLock<Box<dyn Model>>` per
    /// frame. Verify it matches what the underlying model advertises.
    #[test]
    fn model_state_caches_supported_reasoning_at_construction() {
        let state = make_state(ReasoningLevel::Medium);
        // StubModel uses `ModelCapabilities::ollama_default()` which is
        // `ReasoningCapability::Binary` — verify the cached snapshot
        // matches.
        let expected = crate::models::ModelCapabilities::ollama_default().supports_reasoning;
        assert_eq!(state.supported_reasoning, expected);
    }
}
