//! Provider adapters module
//!
//! Contains implementations of the Model trait for Ollama, the
//! OpenAI-compatible long tail, Anthropic Claude, and Google Gemini.

pub mod anthropic;
pub mod gemini;
pub mod ollama;
pub mod ollama_sizing;
pub mod openai_compat;
pub mod output_budget;

/// A model's token limits as reported by its provider's models endpoint.
/// `None` means the provider didn't expose that limit — never a guess.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelLimits {
    pub max_context_tokens: Option<usize>,
    pub max_output_tokens: Option<usize>,
}
