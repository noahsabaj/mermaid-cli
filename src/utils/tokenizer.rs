use anyhow::Result;

/// Token counting utility using character-based estimation
/// Uses ~4 characters per token as a reasonable approximation
pub struct Tokenizer {
    /// Base model name (provider prefix stripped at construction time)
    base_model_name: String,
}

impl Tokenizer {
    /// Create a new tokenizer for the given model
    pub fn new(model_name: &str) -> Self {
        let base = if let Some(idx) = model_name.find('/') {
            // Safe: '/' is ASCII, so byte offset == char offset
            &model_name[idx + 1..]
        } else {
            model_name
        };
        Self {
            base_model_name: base.to_lowercase(),
        }
    }

    /// Count tokens in a single text string (~4 chars per token)
    pub fn count_tokens(&self, text: &str) -> Result<usize> {
        Ok(text.len().div_ceil(4))
    }

    /// Count tokens in a chat message format
    pub fn count_chat_tokens(&self, messages: &[(String, String)]) -> Result<usize> {
        let total_chars: usize = messages
            .iter()
            .map(|(role, content)| role.len() + content.len() + 4) // +4 for message overhead
            .sum();
        Ok(total_chars.div_ceil(4))
    }

    /// Get the maximum context window for a model (in tokens).
    ///
    /// Focused on Ollama model families — the actual models Mermaid supports.
    /// These are conservative defaults; Ollama may use different context sizes
    /// depending on the specific model variant and user's num_ctx setting.
    pub fn get_max_tokens(&self) -> usize {
        let model_name = &self.base_model_name;

        // Large-context models (128k+)
        if model_name.contains("qwen3-coder")
            || model_name.contains("qwen2.5-coder")
            || model_name.contains("deepseek-v3")
            || model_name.contains("deepseek-r1")
            || model_name.contains("kimi")
        {
            131072
        }
        // 64k context models
        else if model_name.contains("deepseek-coder") || model_name.contains("command-r") {
            65536
        }
        // 32k context models
        else if model_name.contains("qwen")
            || model_name.contains("mistral")
            || model_name.contains("mixtral")
            || model_name.contains("gemma2")
        {
            32768
        }
        // 16k context models
        else if model_name.contains("codellama") || model_name.contains("phi") {
            16384
        }
        // 8k context models (llama3 default)
        else if model_name.contains("llama3")
            || model_name.contains("llama-3")
            || model_name.contains("gemma")
        {
            8192
        }
        // 4k context models (older)
        else if model_name.contains("llama2")
            || model_name.contains("llama-2")
            || model_name.contains("tinyllama")
        {
            4096
        } else {
            8192 // Conservative default for unknown models
        }
    }

    /// Calculate remaining tokens in context window
    pub fn remaining_tokens(&self, used_tokens: usize) -> usize {
        let max_tokens = self.get_max_tokens();
        max_tokens.saturating_sub(used_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_counting() {
        let tokenizer = Tokenizer::new("gpt-3.5-turbo");
        let text = "Hello, world! This is a test message.";
        let count = tokenizer.count_tokens(text).unwrap();
        assert!(count > 0);
        assert!(count < text.len());
    }

    #[test]
    fn test_model_name_extraction() {
        let tokenizer = Tokenizer::new("ollama/gpt-4");
        assert_eq!(tokenizer.base_model_name, "gpt-4");

        let tokenizer = Tokenizer::new("unknown-model");
        assert_eq!(tokenizer.base_model_name, "unknown-model");
    }

    #[test]
    fn test_max_tokens() {
        let tokenizer = Tokenizer::new("ollama/qwen3-coder:30b");
        assert_eq!(tokenizer.get_max_tokens(), 131072);

        let tokenizer = Tokenizer::new("ollama/llama3:8b");
        assert_eq!(tokenizer.get_max_tokens(), 8192);

        let tokenizer = Tokenizer::new("tinyllama");
        assert_eq!(tokenizer.get_max_tokens(), 4096);

        let tokenizer = Tokenizer::new("ollama/mistral");
        assert_eq!(tokenizer.get_max_tokens(), 32768);

        let tokenizer = Tokenizer::new("unknown-model");
        assert_eq!(tokenizer.get_max_tokens(), 8192);
    }
}
