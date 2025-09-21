mod cache_manager;
mod file_cache;
mod types;

pub use cache_manager::{CacheManager, CacheStats};
pub use file_cache::FileCache;
pub use types::{CacheEntry, CacheKey, CacheMetadata};

/// Initialize the cache system
pub fn init() -> anyhow::Result<CacheManager> {
    CacheManager::new()
}