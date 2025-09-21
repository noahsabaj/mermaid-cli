use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

/// Key for cache entries
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey {
    pub file_path: PathBuf,
    pub file_hash: String,
}

/// Metadata for cache entries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMetadata {
    pub created_at: SystemTime,
    pub last_accessed: SystemTime,
    pub file_size: u64,
    pub compressed_size: usize,
    pub compression_ratio: f32,
}

/// Cache entry containing parsed data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry<T> {
    pub key: CacheKey,
    pub data: T,
    pub metadata: CacheMetadata,
}

/// Cached AST symbols
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSymbols {
    pub symbols: Vec<crate::context::Symbol>,
    pub references: Vec<crate::context::SymbolReference>,
}

/// Cached token count
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTokens {
    pub count: usize,
    pub model_name: String,
}