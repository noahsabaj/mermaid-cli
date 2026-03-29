use anyhow::Result;

/// Token counting utility using character-based estimation
/// Uses ~4 characters per token as a reasonable approximation
pub struct Tokenizer {
    model_name: String,
}

impl Tokenizer {
    /// Create a new tokenizer for the given model
    pub fn new(model_name: &str) -> Self {
        Self {
            model_name: model_name.to_string(),
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

    /// Get the maximum tokens for a model
    pub fn get_max_tokens(&self) -> usize {
        let model_name = self.get_base_model_name();

        if model_name.contains("gpt-4o")
            || model_name.contains("gpt-4-turbo")
            || model_name.contains("gpt-4-1106")
        {
            128000
        } else if model_name.contains("gpt-4-32k") {
            32768
        } else if model_name.contains("gpt-4") {
            8192
        } else if model_name.contains("gpt-3.5-turbo-16k") {
            16384
        } else if model_name.contains("gpt-3.5-turbo") {
            4096
        } else if model_name.contains("claude-3") {
            200000
        } else if model_name.contains("claude") {
            100000
        } else if model_name.contains("llama-3") {
            8192
        } else if model_name.contains("llama-2") {
            4096
        } else if model_name.contains("codellama") {
            16384
        } else if model_name.contains("deepseek-coder") {
            65536
        } else if model_name.contains("qwen")
            || model_name.contains("mistral")
            || model_name.contains("mixtral")
        {
            32768
        } else {
            8192 // Conservative default
        }
    }

    /// Calculate remaining tokens in context window
    pub fn remaining_tokens(&self, used_tokens: usize) -> usize {
        let max_tokens = self.get_max_tokens();
        max_tokens.saturating_sub(used_tokens)
    }

    /// Get the base model name (strip provider prefix)
    fn get_base_model_name(&self) -> String {
        if let Some(idx) = self.model_name.find('/') {
            self.model_name[idx + 1..].to_string()
        } else {
            self.model_name.clone()
        }
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
        assert_eq!(tokenizer.get_base_model_name(), "gpt-4");

        let tokenizer = Tokenizer::new("unknown-model");
        assert_eq!(tokenizer.get_base_model_name(), "unknown-model");
    }

    #[test]
    fn test_max_tokens() {
        let tokenizer = Tokenizer::new("gpt-4");
        assert_eq!(tokenizer.get_max_tokens(), 8192);

        let tokenizer = Tokenizer::new("gpt-4o");
        assert_eq!(tokenizer.get_max_tokens(), 128000);

        let tokenizer = Tokenizer::new("gpt-3.5-turbo");
        assert_eq!(tokenizer.get_max_tokens(), 4096);
    }
}
