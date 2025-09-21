use anyhow::Result;
use directories::ProjectDirs;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::file_cache::FileCache;
use super::types::{CacheKey, CachedSymbols, CachedTokens};
use crate::context::{Symbol, SymbolReference, TreeParser};

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
    symbols: HashMap<CacheKey, CachedSymbols>,
    tokens: HashMap<CacheKey, CachedTokens>,
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

    /// Get or compute symbols for a file
    pub fn get_or_compute_symbols(
        &self,
        path: &Path,
        content: &str,
        parser: &mut TreeParser,
    ) -> Result<(Vec<Symbol>, Vec<SymbolReference>)> {
        // Generate cache key
        let key = FileCache::generate_key(path)?;

        // Check memory cache first
        {
            let mut mem_cache = self.memory_cache.lock().unwrap();
            if let Some(cached) = mem_cache.symbols.get(&key).cloned() {
                mem_cache.hits += 1;
                return Ok((cached.symbols, cached.references));
            }
        }

        // Check file cache
        if let Some(cached) = self.file_cache.load::<CachedSymbols>(&key)? {
            // Validate cache
            if self.file_cache.is_valid(&key)? {
                // Store in memory cache
                let mut mem_cache = self.memory_cache.lock().unwrap();
                mem_cache.symbols.insert(key.clone(), cached.clone());
                mem_cache.hits += 1;
                return Ok((cached.symbols, cached.references));
            } else {
                // Invalid cache, remove it
                self.file_cache.remove(&key)?;
            }
        }

        // Cache miss - compute and cache
        {
            let mut mem_cache = self.memory_cache.lock().unwrap();
            mem_cache.misses += 1;
        }

        // Parse the file
        let symbols = parser.parse_file(path, content)?;
        let references = parser.find_references(path, content).unwrap_or_default();

        // Cache the results
        let cached = CachedSymbols {
            symbols: symbols.clone(),
            references: references.clone(),
        };

        // Save to file cache
        self.file_cache.save(&key, &cached)?;

        // Save to memory cache
        {
            let mut mem_cache = self.memory_cache.lock().unwrap();
            mem_cache.symbols.insert(key, cached);
        }

        Ok((symbols, references))
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
            let mut mem_cache = self.memory_cache.lock().unwrap();
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
                    let mut mem_cache = self.memory_cache.lock().unwrap();
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
            let mut mem_cache = self.memory_cache.lock().unwrap();
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
            let mut mem_cache = self.memory_cache.lock().unwrap();
            mem_cache.tokens.insert(key, cached);
        }

        Ok(count)
    }

    /// Parse multiple files with caching
    pub fn parse_files_cached(
        &self,
        files: &[PathBuf],
    ) -> Vec<(PathBuf, Vec<Symbol>, Vec<SymbolReference>)> {
        files
            .par_iter()
            .filter_map(|file| {
                // Read file content
                match std::fs::read_to_string(file) {
                    Ok(content) => {
                        // Create a parser for this thread
                        match TreeParser::new() {
                            Ok(mut parser) => {
                                // Get or compute symbols with caching
                                match self.get_or_compute_symbols(file, &content, &mut parser) {
                                    Ok((symbols, references)) => {
                                        Some((file.clone(), symbols, references))
                                    }
                                    Err(e) => {
                                        eprintln!("Failed to parse {}: {}", file.display(), e);
                                        None
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("Failed to create parser for {}: {}", file.display(), e);
                                None
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to read {}: {}", file.display(), e);
                        None
                    }
                }
            })
            .collect()
    }

    /// Invalidate cache for a specific file
    pub fn invalidate(&self, path: &Path) -> Result<()> {
        let key = FileCache::generate_key(path)?;

        // Remove from memory cache
        {
            let mut mem_cache = self.memory_cache.lock().unwrap();
            mem_cache.symbols.remove(&key);
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
            let mut mem_cache = self.memory_cache.lock().unwrap();
            mem_cache.symbols.clear();
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
            let mem_cache = self.memory_cache.lock().unwrap();
            let total_requests = mem_cache.hits + mem_cache.misses;
            let hit_rate = if total_requests > 0 {
                (mem_cache.hits as f32 / total_requests as f32) * 100.0
            } else {
                0.0
            };
            (
                mem_cache.symbols.len() + mem_cache.tokens.len(),
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
            📁 Directory: {}\n\
            📊 File Cache: {} entries\n\
            💾 Memory Cache: {} entries\n\
            📦 Total Size: {:.2} MB\n\
            🗜️  Compressed: {:.2} MB (ratio: {:.1}x)\n\
            ✅ Hit Rate: {:.1}% ({} hits, {} misses)",
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