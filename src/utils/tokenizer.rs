use anyhow::Result;

/// Token counting utility using character-based estimation.
/// Uses ~4 characters per token as a reasonable approximation.
///
/// Stateless. Only counting is implemented; context-window budget is
/// enforced via `MAX_CONTEXT_TOKENS` / `CONTEXT_RESERVE_TOKENS`
/// constants and the user's `num_ctx` Ollama option. The old
/// per-model-family table (`get_max_tokens` / `remaining_tokens`) and
/// the `base_model_name` field were never consulted by the budget
/// logic and have been deleted.
pub struct Tokenizer;

impl Tokenizer {
    /// Create a new tokenizer. The `model_name` parameter is accepted
    /// for call-site compatibility but ignored — the count is a pure
    /// function of text length.
    pub fn new(_model_name: &str) -> Self {
        Self
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
    fn test_count_chat_tokens() {
        let tokenizer = Tokenizer::new("any-model");
        let messages = vec![
            ("user".to_string(), "Hello".to_string()),
            ("assistant".to_string(), "Hi there".to_string()),
        ];
        let count = tokenizer.count_chat_tokens(&messages).unwrap();
        assert!(count > 0);
    }
}
