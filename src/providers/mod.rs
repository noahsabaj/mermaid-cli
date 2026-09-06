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

pub mod approval;
pub mod auto_classifier;
pub mod ctx;
pub mod discovery;
pub mod factory;
pub mod model;
pub mod questions;
pub mod tasks;
pub mod tool;

pub use approval::{ApprovalBroker, ApprovalDecision, allowlist_key, is_domain_allowed};
pub use auto_classifier::{AutoClassifier, ModelAutoClassifier, VetRequest, VetVerdict};
pub use ctx::{ExecContext, FinalResponse, StreamContext, StreamEvent, clone_messages};
/// Test-only context builders. Behind a feature rather than `#[cfg(test)]`
/// because the integration tests under `tests/` link the library without
/// `cfg(test)`; the self dev-dependency in `Cargo.toml` turns the feature on
/// for them, and a release build has no trace of these.
#[cfg(any(test, feature = "test-support"))]
pub use ctx::{test_exec_context, test_stream_context};
pub use discovery::{
    ConfiguredProvider, ProviderCatalog, ProviderProblem, configured_remote_providers,
    provider_catalogs, provider_problems,
};
pub use factory::{ProviderFactory, model_provider_resolves};
pub use model::{
    AnthropicProvider, GeminiProvider, MetaProvider, ModelProvider, OllamaProvider,
    OpenAICompatProvider,
};
pub use questions::QuestionBroker;
pub use tasks::TaskBroker;
pub use tool::{ToolExecutor, ToolRegistry};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trait_is_object_safe() {
        // Compile-time guard: `dyn ModelProvider` must stay
        // object-safe so `ProviderFactory` can hand out
        // `Arc<dyn ModelProvider>`.
        fn _assert(_: &dyn ModelProvider) {}
    }
}
