use anyhow::Result;
use tiktoken_rs::{num_tokens_from_messages, ChatCompletionRequestMessage};
use std::collections::HashMap;

/// Token counting utility for various model families
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

    /// Count tokens in a single text string
    pub fn count_tokens(&self, text: &str) -> Result<usize> {
        // Extract the base model name for tokenizer selection
        let model_for_encoding = self.get_base_model_name();

        // Get the appropriate tokenizer
        match tiktoken_rs::get_bpe_from_model(&model_for_encoding) {
            Ok(bpe) => {
                // Count tokens using the BPE tokenizer
                Ok(bpe.encode_with_special_tokens(text).len())
            }
            Err(_) => {
                // Fallback to cl100k_base encoding (GPT-4/GPT-3.5-turbo)
                tiktoken_rs::cl100k_base()
                    .map(|bpe| bpe.encode_with_special_tokens(text).len())
                    .or_else(|_| Ok(text.len() / 4))
            }
        }
    }

    /// Count tokens in a chat message format
    pub fn count_chat_tokens(&self, messages: &[(String, String)]) -> Result<usize> {
        // Convert to tiktoken's ChatCompletionRequestMessage format
        let chat_messages: Vec<ChatCompletionRequestMessage> = messages
            .iter()
            .map(|(role, content)| {
                ChatCompletionRequestMessage {
                    role: role.clone(),
                    content: Some(content.clone()),
                    name: None,
                    function_call: None,
                }
            })
            .collect();

        let model_for_encoding = self.get_base_model_name();

        // Use tiktoken's chat token counter
        match num_tokens_from_messages(&model_for_encoding, &chat_messages) {
            Ok(count) => Ok(count),
            Err(_) => {
                // Fallback to GPT-3.5 encoding
                num_tokens_from_messages("gpt-3.5-turbo", &chat_messages)
                    .or_else(|_| {
                        // Last resort: simple approximation
                        let total_chars: usize = messages.iter()
                            .map(|(_, content)| content.len())
                            .sum();
                        Ok(total_chars / 4)
                    })
            }
        }
    }

    /// Get the maximum tokens for a model
    pub fn get_max_tokens(&self) -> usize {
        let model_name = self.get_base_model_name();

        // Return max tokens based on common models
        // These are approximate values for context window sizes
        if model_name.contains("gpt-4o") {
            128000  // GPT-4o
        } else if model_name.contains("gpt-4-turbo") || model_name.contains("gpt-4-1106") {
            128000  // GPT-4 Turbo
        } else if model_name.contains("gpt-4-32k") {
            32768   // GPT-4 32k
        } else if model_name.contains("gpt-4") {
            8192    // GPT-4
        } else if model_name.contains("gpt-3.5-turbo-16k") {
            16384   // GPT-3.5 Turbo 16k
        } else if model_name.contains("gpt-3.5-turbo") {
            4096    // GPT-3.5 Turbo
        } else if model_name.contains("claude-3") {
            200000  // Claude 3
        } else if model_name.contains("claude") {
            100000  // Claude 2
        } else if model_name.contains("llama-3") {
            8192    // Llama 3
        } else if model_name.contains("llama-2") {
            4096    // Llama 2
        } else if model_name.contains("codellama") {
            16384   // Code Llama
        } else if model_name.contains("deepseek-coder") {
            65536   // DeepSeek Coder
        } else if model_name.contains("qwen") {
            32768   // Qwen models
        } else if model_name.contains("mistral") || model_name.contains("mixtral") {
            32768   // Mistral/Mixtral
        } else {
            8192    // Conservative default
        }
    }

    /// Calculate remaining tokens in context window
    pub fn remaining_tokens(&self, used_tokens: usize) -> usize {
        let max_tokens = self.get_max_tokens();
        max_tokens.saturating_sub(used_tokens)
    }

    /// Get the base model name for tokenizer selection
    fn get_base_model_name(&self) -> String {
        // Remove provider prefix if present (e.g., "ollama/gpt-4" -> "gpt-4")
        let base_name = if let Some(idx) = self.model_name.find('/') {
            &self.model_name[idx + 1..]
        } else {
            &self.model_name
        };

        // Map model variations to their base tokenizer
        let model_mappings: HashMap<&str, &str> = [
            // OpenAI models
            ("gpt-4o", "gpt-4o"),
            ("gpt-4-turbo", "gpt-4-turbo"),
            ("gpt-4", "gpt-4"),
            ("gpt-3.5-turbo", "gpt-3.5-turbo"),

            // Claude models - use GPT-4 encoding as approximation
            ("claude-3", "gpt-4"),
            ("claude-3-opus", "gpt-4"),
            ("claude-3-sonnet", "gpt-4"),
            ("claude-3-haiku", "gpt-4"),

            // Llama models - use GPT-3.5 encoding as approximation
            ("llama3", "gpt-3.5-turbo"),
            ("llama2", "gpt-3.5-turbo"),
            ("codellama", "gpt-3.5-turbo"),

            // Other models - use GPT-3.5 as default
            ("deepseek", "gpt-3.5-turbo"),
            ("qwen", "gpt-3.5-turbo"),
            ("mistral", "gpt-3.5-turbo"),
            ("mixtral", "gpt-3.5-turbo"),
        ].iter().cloned().collect();

        // Find the best matching tokenizer
        for (pattern, tokenizer) in model_mappings {
            if base_name.to_lowercase().contains(pattern) {
                return tokenizer.to_string();
            }
        }

        // Default to GPT-3.5 tokenizer for unknown models
        "gpt-3.5-turbo".to_string()
    }
}

/// Count tokens in file contents (convenience function)
pub fn count_file_tokens(content: &str, model_name: &str) -> usize {
    let tokenizer = Tokenizer::new(model_name);
    tokenizer.count_tokens(content).unwrap_or_else(|_| content.len() / 4)
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
        assert!(count < text.len()); // Tokens should be less than characters
    }

    #[test]
    fn test_model_name_extraction() {
        let tokenizer = Tokenizer::new("ollama/gpt-4");
        assert_eq!(tokenizer.get_base_model_name(), "gpt-4");

        let tokenizer = Tokenizer::new("anthropic/claude-3-sonnet");
        assert_eq!(tokenizer.get_base_model_name(), "gpt-4"); // Mapped to GPT-4

        let tokenizer = Tokenizer::new("unknown-model");
        assert_eq!(tokenizer.get_base_model_name(), "gpt-3.5-turbo"); // Default
    }

    #[test]
    fn test_max_tokens() {
        let tokenizer = Tokenizer::new("gpt-4");
        assert!(tokenizer.get_max_tokens() > 100000);

        let tokenizer = Tokenizer::new("gpt-3.5-turbo");
        assert!(tokenizer.get_max_tokens() > 4000);
    }
}