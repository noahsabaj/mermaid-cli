/// Token counting and caching logic
///
/// Uses character-based estimation (~4 chars per token) for simplicity.
/// For accurate token counts, Ollama provides real counts in API responses.
use anyhow::{Context, Result};
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

/// Manages token counting with caching support
pub struct TokenCounter {
    cache_manager: Option<Arc<crate::cache::CacheManager>>,
}

impl TokenCounter {
    pub fn new(cache_manager: Option<Arc<crate::cache::CacheManager>>) -> Self {
        Self { cache_manager }
    }

    /// Estimate tokens from text (~4 chars per token)
    fn estimate_tokens(text: &str) -> usize {
        (text.len() + 3) / 4
    }

    /// Load a single file
    pub fn load_file(&self, path: &Path) -> Result<String> {
        fs::read_to_string(path).with_context(|| format!("Failed to read file: {}", path.display()))
    }

    /// Load a file incrementally with token counting
    ///
    /// This method streams the file in lines, counting tokens as it reads.
    /// If the token budget is exceeded mid-read, it stops reading and returns
    /// what has been accumulated so far.
    ///
    /// Returns: (content, tokens_used)
    pub fn load_file_with_token_limit(
        &self,
        path: &Path,
        token_budget: usize,
    ) -> Result<(String, usize)> {
        let file =
            File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("Failed to stat file: {}", path.display()))?;

        // For small files, use regular loading (more efficient)
        if metadata.len() < 100 * 1024 {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read file: {}", path.display()))?;
            let tokens = Self::estimate_tokens(&content);
            return Ok((content, tokens));
        }

        // For large files, use incremental loading with token counting
        let mut reader = BufReader::new(file);
        let mut content = String::new();
        let mut total_tokens = 0;
        let mut line_buffer = String::with_capacity(1024);

        loop {
            line_buffer.clear();
            let bytes_read = reader
                .read_line(&mut line_buffer)
                .with_context(|| format!("Failed to read file: {}", path.display()))?;

            if bytes_read == 0 {
                break; // EOF
            }

            // Estimate tokens in this line
            let line_tokens = Self::estimate_tokens(&line_buffer);

            // Check if adding this line would exceed budget
            if total_tokens + line_tokens > token_budget {
                break;
            }

            content.push_str(&line_buffer);
            total_tokens += line_tokens;
        }

        Ok((content, total_tokens))
    }

    /// Load file content and token count with caching
    pub fn load_file_cached(&self, path: &Path, token_budget: usize) -> Result<(String, usize)> {
        // Try to get cached token count first
        if let Some(ref cache) = self.cache_manager {
            if let Ok(cached_tokens) = cache.get_or_compute_tokens(path, "", "estimate") {
                if cached_tokens <= token_budget {
                    let content = self.load_file(path)?;
                    return Ok((content, cached_tokens));
                }
            }
        }

        // Not in cache or exceeded budget, load incrementally
        let (content, tokens) = self.load_file_with_token_limit(path, token_budget)?;

        // Cache the result for next time
        if let Some(ref cache) = self.cache_manager {
            let _ = cache.get_or_compute_tokens(path, &content, "estimate");
        }

        Ok((content, tokens))
    }

    /// Get the token count for content
    #[allow(dead_code)]
    pub fn count_tokens(&self, content: &str) -> usize {
        Self::estimate_tokens(content)
    }
}
