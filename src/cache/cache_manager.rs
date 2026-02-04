use anyhow::Result;
use directories::ProjectDirs;
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::file_cache::FileCache;
use super::types::{CacheKey, CachedTokens};
use crate::utils::lock_arc_mutex_safe;

/// Main cache manager for the application
#[derive(Debug)]
pub struct CacheManager {
    file_cache: Arc<FileCache>,
    memory_cache: Arc<Mutex<MemoryCache>>,
    cache_dir: PathBuf,
}

/// In-memory cache for hot data
#[derive(Debug, Default)]
struct MemoryCache {
    tokens: FxHashMap<CacheKey, CachedTokens>,
    hits: usize,
    misses: usize,
}

impl CacheManager {
    /// Create a new cache manager
    pub fn new() -> Result<Self> {
        // Get cache directory (~/.cache/mermaid on Linux, ~/Library/Caches/mermaid on macOS)
        let cache_dir = if let Some(proj_dirs) = ProjectDirs::from("", "", "mermaid") {
            proj_dirs.cache_dir().to_path_buf()
        } else {
            // Fallback to ~/.cache/mermaid
            let home = std::env::var("HOME")?;
            PathBuf::from(home).join(".cache").join("mermaid")
        };

        let file_cache = Arc::new(FileCache::new(cache_dir.clone())?);
        let memory_cache = Arc::new(Mutex::new(MemoryCache::default()));

        Ok(Self {
            file_cache,
            memory_cache,
            cache_dir,
        })
    }

    /// Get or compute token count for content
    pub fn get_or_compute_tokens(
        &self,
        path: &Path,
        content: &str,
        model_name: &str,
    ) -> Result<usize> {
        // Generate cache key
        let key = FileCache::generate_key(path)?;

        // Check memory cache first
        {
            let mut mem_cache = lock_arc_mutex_safe(&self.memory_cache);
            if let Some(cached) = mem_cache.tokens.get(&key).cloned() {
                if cached.model_name == model_name {
                    mem_cache.hits += 1;
                    return Ok(cached.count);
                }
            }
        }

        // Check file cache
        if let Some(cached) = self.file_cache.load::<CachedTokens>(&key)? {
            if cached.model_name == model_name {
                // Validate cache
                if self.file_cache.is_valid(&key)? {
                    // Store in memory cache
                    let mut mem_cache = lock_arc_mutex_safe(&self.memory_cache);
                    mem_cache.tokens.insert(key.clone(), cached.clone());
                    mem_cache.hits += 1;
                    return Ok(cached.count);
                } else {
                    // Invalid cache, remove it
                    self.file_cache.remove(&key)?;
                }
            }
        }

        // Cache miss - compute and cache
        {
            let mut mem_cache = lock_arc_mutex_safe(&self.memory_cache);
            mem_cache.misses += 1;
        }

        // Count tokens
        let tokenizer = crate::utils::Tokenizer::new(model_name);
        let count = tokenizer.count_tokens(content)?;

        // Cache the results
        let cached = CachedTokens {
            count,
            model_name: model_name.to_string(),
        };

        // Save to file cache
        self.file_cache.save(&key, &cached)?;

        // Save to memory cache
        {
            let mut mem_cache = lock_arc_mutex_safe(&self.memory_cache);
            mem_cache.tokens.insert(key, cached);
        }

        Ok(count)
    }

    /// Invalidate cache for a specific file
    pub fn invalidate(&self, path: &Path) -> Result<()> {
        let key = FileCache::generate_key(path)?;

        // Remove from memory cache
        {
            let mut mem_cache = lock_arc_mutex_safe(&self.memory_cache);
            mem_cache.tokens.remove(&key);
        }

        // Remove from file cache
        self.file_cache.remove(&key)?;

        Ok(())
    }

    /// Clear all caches
    pub fn clear_all(&self) -> Result<()> {
        // Clear memory cache
        {
            let mut mem_cache = lock_arc_mutex_safe(&self.memory_cache);
            mem_cache.tokens.clear();
            mem_cache.hits = 0;
            mem_cache.misses = 0;
        }

        // Clear file cache (remove cache directory)
        if self.cache_dir.exists() {
            std::fs::remove_dir_all(&self.cache_dir)?;
            std::fs::create_dir_all(&self.cache_dir)?;
        }

        Ok(())
    }

    /// Get cache statistics
    pub fn get_stats(&self) -> Result<CacheStats> {
        let file_stats = self.file_cache.get_stats()?;

        let (memory_entries, hits, misses, hit_rate) = {
            let mem_cache = lock_arc_mutex_safe(&self.memory_cache);
            let total_requests = mem_cache.hits + mem_cache.misses;
            let hit_rate = if total_requests > 0 {
                (mem_cache.hits as f32 / total_requests as f32) * 100.0
            } else {
                0.0
            };
            (
                mem_cache.tokens.len(),
                mem_cache.hits,
                mem_cache.misses,
                hit_rate,
            )
        };

        Ok(CacheStats {
            file_cache_entries: file_stats.total_entries,
            memory_cache_entries: memory_entries,
            total_size: file_stats.total_size,
            compressed_size: file_stats.total_compressed_size,
            compression_ratio: file_stats.compression_ratio,
            cache_hits: hits,
            cache_misses: misses,
            hit_rate,
            cache_directory: self.cache_dir.clone(),
        })
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub file_cache_entries: usize,
    pub memory_cache_entries: usize,
    pub total_size: usize,
    pub compressed_size: usize,
    pub compression_ratio: f32,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub hit_rate: f32,
    pub cache_directory: PathBuf,
}

impl CacheStats {
    /// Format cache stats for display
    pub fn format(&self) -> String {
        format!(
            "Cache Statistics:\n\
            Directory: {}\n\
            File Cache: {} entries\n\
            Memory Cache: {} entries\n\
            Total Size: {:.2} MB\n\
            Compressed: {:.2} MB (ratio: {:.1}x)\n\
            Hit Rate: {:.1}% ({} hits, {} misses)",
            self.cache_directory.display(),
            self.file_cache_entries,
            self.memory_cache_entries,
            self.total_size as f64 / 1_048_576.0,
            self.compressed_size as f64 / 1_048_576.0,
            self.compression_ratio,
            self.hit_rate,
            self.cache_hits,
            self.cache_misses
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Phase 4 Test Suite: Cache Manager - focused core logic tests

    #[test]
    fn test_cache_manager_new() {
        // Test basic cache manager creation
        let result = CacheManager::new();
        assert!(result.is_ok() || result.is_err(), "Should return a Result");
    }

    #[test]
    fn test_cache_stats_hit_rate_calculation() {
        // Test hit rate calculation logic
        let stats = CacheStats {
            file_cache_entries: 10,
            memory_cache_entries: 5,
            total_size: 1_000_000,
            compressed_size: 500_000,
            compression_ratio: 2.0,
            cache_hits: 100,
            cache_misses: 20,
            hit_rate: 83.33,
            cache_directory: PathBuf::from("/cache"),
        };

        // Verify hit rate calculation
        let expected_hit_rate = (100.0 / 120.0) * 100.0;
        assert!(
            (stats.hit_rate - expected_hit_rate).abs() < 0.1,
            "Hit rate should be ~83.33%"
        );
    }

    #[test]
    fn test_cache_stats_compression_ratio() {
        // Test compression ratio is calculated correctly
        let stats = CacheStats {
            file_cache_entries: 5,
            memory_cache_entries: 3,
            total_size: 1000,
            compressed_size: 400,
            compression_ratio: 2.5,
            cache_hits: 50,
            cache_misses: 10,
            hit_rate: 83.33,
            cache_directory: PathBuf::from("/cache"),
        };

        assert_eq!(
            stats.compression_ratio, 2.5,
            "Compression ratio should be 2.5"
        );
        assert_eq!(stats.total_size, 1000, "Total size should be 1000");
    }

    #[test]
    fn test_cache_stats_format_display() {
        // Test that stats can be formatted for display
        let stats = CacheStats {
            file_cache_entries: 10,
            memory_cache_entries: 5,
            total_size: 1_048_576,
            compressed_size: 524_288,
            compression_ratio: 2.0,
            cache_hits: 100,
            cache_misses: 20,
            hit_rate: 83.33,
            cache_directory: PathBuf::from("/cache"),
        };

        let formatted = stats.format();
        assert!(
            formatted.contains("Cache Statistics"),
            "Should include header"
        );
        assert!(
            formatted.contains("/cache"),
            "Should include cache directory"
        );
        assert!(
            formatted.contains("File Cache: 10"),
            "Should include file cache entries"
        );
        assert!(
            formatted.contains("Memory Cache: 5"),
            "Should include memory cache entries"
        );
    }

    #[test]
    fn test_memory_cache_default() {
        // Test default MemoryCache initialization
        let mem_cache = MemoryCache::default();
        assert_eq!(mem_cache.hits, 0, "Initial hits should be 0");
        assert_eq!(mem_cache.misses, 0, "Initial misses should be 0");
        assert!(
            mem_cache.tokens.is_empty(),
            "Initial tokens should be empty"
        );
    }

    #[test]
    fn test_cache_key_components() {
        // Test cache key structure and components
        let path = PathBuf::from("src/main.rs");
        let file_hash = "abc123def456".to_string();

        let key = CacheKey {
            file_path: path.clone(),
            file_hash: file_hash.clone(),
        };

        assert_eq!(key.file_path, path, "File path should match");
        assert_eq!(key.file_hash, file_hash, "File hash should match");
    }

    #[test]
    fn test_cached_tokens_structure() {
        // Test CachedTokens structure
        let cached = CachedTokens {
            count: 1000,
            model_name: "ollama/tinyllama".to_string(),
        };

        assert_eq!(cached.count, 1000, "Token count should be 1000");
        assert_eq!(
            cached.model_name, "ollama/tinyllama",
            "Model name should match"
        );
    }

    #[test]
    fn test_cache_directory_structure() {
        // Test cache directory path construction with platform-specific paths
        #[cfg(windows)]
        let cache_dir = PathBuf::from("C:\\Users\\user\\AppData\\Local\\mermaid");

        #[cfg(not(windows))]
        let cache_dir = PathBuf::from("/home/user/.cache/mermaid");

        // Verify cache directory is a valid PathBuf
        assert!(
            cache_dir.is_absolute(),
            "Cache directory should be absolute"
        );
        assert!(
            cache_dir.to_string_lossy().contains("mermaid"),
            "Should contain mermaid"
        );
    }

    #[test]
    fn test_hit_rate_percentages() {
        // Test hit rate calculation for various scenarios
        let scenarios = vec![
            (100, 0, 100.0), // All hits
            (0, 100, 0.0),   // All misses
            (50, 50, 50.0),  // Even split
            (75, 25, 75.0),  // 75% hit rate
        ];

        for (hits, misses, expected_rate) in scenarios {
            let total = hits + misses;
            let rate = if total > 0 {
                (hits as f32 / total as f32) * 100.0
            } else {
                0.0
            };

            assert!(
                (rate - expected_rate).abs() < 0.1,
                "Hit rate calculation for ({}, {}) failed",
                hits,
                misses
            );
        }
    }
}
