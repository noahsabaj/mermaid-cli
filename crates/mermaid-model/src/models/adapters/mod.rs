//! Provider adapters module
//!
//! Contains implementations of the Model trait for Ollama, the
//! OpenAI-compatible long tail, Anthropic Claude, and Google Gemini.
//!
//! Each owns its wire format and nothing else: [`driver`] holds the read
//! loop they all used to carry a copy of.

/// The stream-accumulation rules (caps, bounds, usage, error bodies) every
/// adapter calls instead of carrying its own copy.
pub(super) mod accumulator;
pub mod anthropic;
/// Anthropic's computer toolset, mapped onto Mermaid's `computer` tool.
pub mod computer_toolset;
/// One test suite driven over recorded response bodies, one per provider.
/// In-crate rather than under `tests/` so it can reach the protocol
/// structs, which are wire-format detail and not public API.
#[cfg(test)]
mod conformance;
pub mod driver;
pub mod gemini;
/// Capability discovery by rejection: send optimistically, learn from a 400.
pub mod learning;
pub mod meta;
/// A scripted loopback HTTP provider for adapter tests.
#[cfg(test)]
mod mock_http;
/// The native tools end to end: declared, translated, replayed, refused.
#[cfg(test)]
mod native_tool_calls;
/// Anthropic's own text-editor and bash tools, mapped onto Mermaid's.
pub mod native_tools;
pub mod ollama;
pub mod ollama_sizing;
pub mod openai_compat;
/// OpenAI's `computer` tool, mapped onto Mermaid's `computer` tool.
mod openai_computer;
/// OpenAI's requests on the Responses API: kept reasoning, its own
/// `apply_patch` tool, server-side compaction.
mod openai_responses;
/// OpenAI on Responses end to end: kept reasoning, `apply_patch` translated
/// and replayed, refusals learned.
#[cfg(test)]
mod openai_responses_calls;
pub mod output_budget;
/// The Responses wire format the Meta and OpenAI adapters share.
pub(crate) mod responses;
/// Anthropic's server-side compaction, end to end: asked for, replayed,
/// and learned when refused.
#[cfg(test)]
mod server_compaction;
/// Tool-returned images for providers whose tool messages are text only.
mod tool_images;
/// Every adapter, end to end, against a model the catalog has never seen.
#[cfg(test)]
mod unknown_model;

/// A model's token limits as reported by its provider's models endpoint.
/// `None` means the provider didn't expose that limit — never a guess.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelLimits {
    pub max_context_tokens: Option<usize>,
    pub max_output_tokens: Option<usize>,
}
