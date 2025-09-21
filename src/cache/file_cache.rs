use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use super::types::{CacheEntry, CacheKey, CacheMetadata};

/// File-level cache operations
#[derive(Debug)]
pub struct FileCache {
    cache_dir: std::path::PathBuf,
}

impl FileCache {
    /// Create a new file cache
    pub fn new(cache_dir: std::path::PathBuf) -> Result<Self> {
        // Ensure cache directory exists
        fs::create_dir_all(&cache_dir)?;
        Ok(Self { cache_dir })
    }

    /// Compute SHA256 hash of a file
    pub fn hash_file(path: &Path) -> Result<String> {
        let content = fs::read(path)?;
        let mut hasher = Sha256::new();
        hasher.update(&content);
        let result = hasher.finalize();
        Ok(format!("{:x}", result))
    }

    /// Generate cache key for a file
    pub fn generate_key(path: &Path) -> Result<CacheKey> {
        let file_hash = Self::hash_file(path)?;
        Ok(CacheKey {
            file_path: path.to_path_buf(),
            file_hash,
        })
    }

    /// Save data to cache with compression
    pub fn save<T>(&self, key: &CacheKey, data: &T) -> Result<()>
    where
        T: serde::Serialize,
    {
        // Serialize data
        let serialized = bincode::serialize(data)?;
        let original_size = serialized.len();

        // Compress data
        let compressed = lz4::block::compress(&serialized, None, true)?;
        let compressed_size = compressed.len();

        // Create metadata
        let metadata = CacheMetadata {
            created_at: SystemTime::now(),
            last_accessed: SystemTime::now(),
            file_size: original_size as u64,
            compressed_size,
            compression_ratio: original_size as f32 / compressed_size as f32,
        };

        // Create cache entry
        let entry = CacheEntry {
            key: key.clone(),
            data: compressed,
            metadata,
        };

        // Generate cache file path
        let cache_path = self.cache_path(key);

        // Ensure parent directory exists
        if let Some(parent) = cache_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write to file
        let entry_data = bincode::serialize(&entry)?;
        fs::write(cache_path, entry_data)?;

        Ok(())
    }

    /// Load data from cache
    pub fn load<T>(&self, key: &CacheKey) -> Result<Option<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        let cache_path = self.cache_path(key);

        // Check if cache file exists
        if !cache_path.exists() {
            return Ok(None);
        }

        // Read cache entry
        let entry_data = fs::read(&cache_path)?;
        let mut entry: CacheEntry<Vec<u8>> = bincode::deserialize(&entry_data)?;

        // Update last accessed time
        entry.metadata.last_accessed = SystemTime::now();

        // Decompress data
        let decompressed = lz4::block::decompress(&entry.data, Some(entry.metadata.file_size as i32))?;

        // Deserialize data
        let data: T = bincode::deserialize(&decompressed)?;

        Ok(Some(data))
    }

    /// Check if cache entry is valid (file hasn't changed)
    pub fn is_valid(&self, key: &CacheKey) -> Result<bool> {
        // Check if file still exists
        if !key.file_path.exists() {
            return Ok(false);
        }

        // Check if hash matches
        let current_hash = Self::hash_file(&key.file_path)?;
        Ok(current_hash == key.file_hash)
    }

    /// Remove cache entry
    pub fn remove(&self, key: &CacheKey) -> Result<()> {
        let cache_path = self.cache_path(key);
        if cache_path.exists() {
            fs::remove_file(cache_path)?;
        }
        Ok(())
    }

    /// Generate cache file path for a key
    fn cache_path(&self, key: &CacheKey) -> std::path::PathBuf {
        // Use first 2 chars of hash for directory sharding
        let hash_prefix = &key.file_hash[..2];
        let cache_name = format!("{}_{}.cache",
            key.file_path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown"),
            &key.file_hash[..8]
        );

        self.cache_dir.join(hash_prefix).join(cache_name)
    }

    /// Get cache statistics
    pub fn get_stats(&self) -> Result<CacheStats> {
        let mut total_entries = 0;
        let mut total_size = 0;
        let mut total_compressed_size = 0;

        // Walk cache directory
        for entry in fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                for cache_file in fs::read_dir(entry.path())? {
                    let cache_file = cache_file?;
                    let metadata = cache_file.metadata()?;
                    total_entries += 1;
                    total_compressed_size += metadata.len() as usize;
                    // Estimate original size (we'd need to read entries for exact)
                    total_size += (metadata.len() as f32 * 3.0) as usize;
                }
            }
        }

        Ok(CacheStats {
            total_entries,
            total_size,
            total_compressed_size,
            compression_ratio: if total_compressed_size > 0 {
                total_size as f32 / total_compressed_size as f32
            } else {
                1.0
            },
            cache_dir: self.cache_dir.clone(),
        })
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub total_entries: usize,
    pub total_size: usize,
    pub total_compressed_size: usize,
    pub compression_ratio: f32,
    pub cache_dir: std::path::PathBuf,
}