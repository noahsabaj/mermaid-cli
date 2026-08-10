//! Provider adapters module
//!
//! Contains implementations of the Model trait for Ollama, the
//! OpenAI-compatible long tail, Anthropic Claude, and Google Gemini.
//!
//! Each owns its wire format and nothing else: [`driver`] holds the read
//! loop they all used to carry a copy of.

pub mod anthropic;
pub mod driver;
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
