/// Context manager for dynamic context reloading
///
/// This module handles:
/// - Detecting when the project file tree has changed
/// - Reloading context only when necessary
/// - Caching file tree state
/// - Determining which files to include in prompt context

use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::file_collector::{CollectorConfig, FileCollector};
use crate::models::ProjectContext;

/// Manages project context reloading and change detection
#[derive(Debug, Clone)]
pub struct ContextManager {
    /// Root path of the project
    root_path: PathBuf,
    /// Last computed hash of the file tree
    last_file_hash: Option<u64>,
    /// Last time context was loaded
    last_load_time: Option<u64>,
    /// Cached file list from last load
    cached_files: Vec<PathBuf>,
    /// Collector config for loading files
    collector_config: CollectorConfig,
}

impl ContextManager {
    /// Create a new context manager for the given project path
    pub fn new(root_path: impl AsRef<Path>) -> Self {
        Self {
            root_path: root_path.as_ref().to_path_buf(),
            last_file_hash: None,
            last_load_time: None,
            cached_files: Vec::new(),
            collector_config: CollectorConfig {
                max_file_size: 1024 * 1024, // 1MB
                max_files: 100,
                priority_extensions: vec![
                    "rs", "py", "js", "ts", "jsx", "tsx", "go", "java", "cpp", "c", "h", "hpp",
                    "cs", "rb", "php", "swift", "kt", "scala", "r", "sql", "sh", "yaml", "yml",
                    "toml", "json", "xml", "html", "css", "scss", "md", "txt",
                ],
                ignore_patterns: vec![
                    "*.log", "*.tmp", "*.cache", "*.pyc", "*.pyo", "*.pyd", "*.so", "*.dylib",
                    "*.dll", "*.exe", "*.o", "*.a", "*.lib", "*.png", "*.jpg", "*.jpeg", "*.gif",
                    "*.bmp", "*.ico", "*.svg", "*.pdf", "*.zip", "*.tar", "*.gz", "*.rar", "*.7z",
                ],
            },
        }
    }

    /// Check if the file tree has changed since last load
    pub async fn needs_reload(&self) -> bool {
        match self.compute_file_hash().await {
            Ok(current_hash) => {
                if let Some(last_hash) = self.last_file_hash {
                    current_hash != last_hash
                } else {
                    // Never loaded before, needs initial load
                    true
                }
            }
            Err(_) => false, // Error computing hash, don't reload
        }
    }

    /// Reload the project context if needed
    pub async fn reload_if_needed(&mut self) -> Result<bool> {
        if self.needs_reload().await {
            self.reload().await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Force a reload of the project context
    pub async fn reload(&mut self) -> Result<()> {
        // Collect files from the project
        let collector = FileCollector::new(self.collector_config.clone());
        let files = collector.collect_files(&self.root_path).await?;

        // Compute hash from the files we just collected (avoid re-scanning)
        let hash = self.compute_hash_from_files(&files)?;

        // Update cached state
        self.cached_files = files;
        self.last_file_hash = Some(hash);
        self.last_load_time = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );

        Ok(())
    }

    /// Build a ProjectContext with the current file tree
    ///
    /// This creates a context with:
    /// - Root path and project type
    /// - Complete file tree (file paths only, not contents)
    /// - No file contents loaded (those load on demand)
    pub fn build_context(&self) -> ProjectContext {
        let mut context = ProjectContext::new(self.root_path.to_string_lossy().to_string());
        context.project_type = detect_project_type(&self.root_path);

        // Add all file paths to context (for file tree structure in prompt)
        for file_path in &self.cached_files {
            if let Ok(rel_path) = file_path.strip_prefix(&self.root_path) {
                if let Some(path_str) = rel_path.to_str() {
                    // Add file path (empty content, just for tree structure)
                    context.add_file(path_str.to_string(), String::new());
                }
            }
        }

        context
    }

    /// Get the list of currently cached file paths
    pub fn get_file_list(&self) -> Vec<String> {
        self.cached_files
            .iter()
            .filter_map(|p| {
                p.strip_prefix(&self.root_path)
                    .ok()
                    .and_then(|p| p.to_str())
                    .map(|s| s.to_string())
            })
            .collect()
    }

    /// Get the total number of files in the project
    pub fn total_files(&self) -> usize {
        self.cached_files.len()
    }

    /// Compute a hash of the current file tree for change detection
    /// This scans the filesystem directly to detect changes
    async fn compute_file_hash(&self) -> Result<u64> {
        // Scan the filesystem to get current state (not cached files)
        let collector = FileCollector::new(self.collector_config.clone());
        let current_files = collector.collect_files(&self.root_path).await?;

        self.compute_hash_from_files(&current_files)
    }

    /// Compute hash from a given list of files
    fn compute_hash_from_files(&self, files: &[PathBuf]) -> Result<u64> {
        let mut hasher = DefaultHasher::new();

        // Hash all file paths (sorted for consistency)
        let mut file_paths: Vec<_> = files
            .iter()
            .filter_map(|p| {
                p.strip_prefix(&self.root_path)
                    .ok()
                    .and_then(|p| p.to_str())
            })
            .collect();
        file_paths.sort();

        for path in file_paths {
            path.hash(&mut hasher);
        }

        Ok(hasher.finish())
    }
}

/// Detect project type from root path
fn detect_project_type(root_path: &Path) -> Option<String> {
    if root_path.join("Cargo.toml").exists() {
        Some("Rust".to_string())
    } else if root_path.join("package.json").exists() {
        Some("JavaScript/TypeScript".to_string())
    } else if root_path.join("requirements.txt").exists() || root_path.join("setup.py").exists() {
        Some("Python".to_string())
    } else if root_path.join("go.mod").exists() {
        Some("Go".to_string())
    } else if root_path.join("pom.xml").exists() || root_path.join("build.gradle").exists() {
        Some("Java".to_string())
    } else if root_path.join("Gemfile").exists() {
        Some("Ruby".to_string())
    } else if root_path.join("composer.json").exists() {
        Some("PHP".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_context_manager_creation() {
        let temp_dir = TempDir::new().unwrap();
        let manager = ContextManager::new(temp_dir.path());

        assert_eq!(manager.root_path, temp_dir.path());
        assert_eq!(manager.total_files(), 0);
        assert!(manager.needs_reload().await);
    }

    #[tokio::test]
    async fn test_file_tree_change_detection() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = ContextManager::new(temp_dir.path());

        // Initial load
        manager.reload().await.unwrap();
        let initial_hash = manager.last_file_hash;

        // No changes - should not need reload
        assert!(!manager.needs_reload().await);

        // Add a file - should need reload
        let test_file = temp_dir.path().join("test.py");
        fs::write(&test_file, "print('test')").unwrap();

        assert!(manager.needs_reload().await);

        // Reload and verify hash changed
        manager.reload().await.unwrap();
        assert_ne!(manager.last_file_hash, initial_hash);
    }

    #[tokio::test]
    async fn test_project_context_building() {
        let temp_dir = TempDir::new().unwrap();

        // Create some test files including a requirements.txt to mark it as a Python project
        fs::write(temp_dir.path().join("main.py"), "print('hello')").unwrap();
        fs::write(temp_dir.path().join("lib.py"), "def helper(): pass").unwrap();
        fs::write(temp_dir.path().join("requirements.txt"), "requests\n").unwrap();

        let mut manager = ContextManager::new(temp_dir.path());
        manager.reload().await.unwrap();

        let context = manager.build_context();
        assert_eq!(context.root_path, temp_dir.path().to_string_lossy().to_string());
        assert_eq!(context.project_type, Some("Python".to_string()));
        assert_eq!(context.files.len(), 3); // All three files added to context (main.py, lib.py, requirements.txt)
    }
}
