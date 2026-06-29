//! Provider adapters module
//!
//! Contains implementations of the Model trait for Ollama, the
//! OpenAI-compatible long tail, Anthropic Claude, and Google Gemini.

pub mod anthropic;
pub mod gemini;
pub mod ollama;
pub mod ollama_sizing;
pub mod openai_compat;
