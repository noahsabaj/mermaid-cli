/// Backend implementations for different model serving platforms
///
/// Each backend provides the same ModelBackend trait interface but handles
/// different underlying services (Ollama, vLLM, etc).

pub mod ollama;
pub mod vllm;

pub use ollama::OllamaBackend;
pub use vllm::VLLMBackend;

