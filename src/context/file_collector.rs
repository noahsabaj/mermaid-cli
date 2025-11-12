/// File collection and walking logic
///
/// Responsible for discovering and collecting files from the project directory
/// with support for prioritization, filtering, and backpressure.
use anyhow::Result;
use ignore::{DirEntry, WalkBuilder};
use std::fs;
use std::path::{Path, PathBuf};

/// Configuration for file collection
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    /// Maximum file size to load (in bytes)
    pub max_file_size: usize,
    /// Maximum number of files to include
    pub max_files: usize,
    /// File extensions to prioritize
    pub priority_extensions: Vec<&'static str>,
    /// Additional patterns to ignore
    pub ignore_patterns: Vec<&'static str>,
}

/// Collects files from a project directory
pub struct FileCollector {
    config: CollectorConfig,
}

impl FileCollector {
    pub fn new(config: CollectorConfig) -> Self {
        Self { config }
    }

    /// Collect all relevant files from the project
    /// Includes backpressure: stops early if file limit is reached
    pub async fn collect_files(&self, root_path: &Path) -> Result<Vec<PathBuf>> {
        let root_path = root_path.to_path_buf();
        let config = self.config.clone();

        // Run file collection in blocking thread pool (ignore crate is sync-only)
        tokio::task::spawn_blocking(move || {
            Self::collect_files_sync(&config, &root_path)
        })
        .await?
    }

    /// Synchronous file collection (called from spawn_blocking)
    fn collect_files_sync(config: &CollectorConfig, root_path: &Path) -> Result<Vec<PathBuf>> {
        let mut priority_files = Vec::new();
        let mut other_files = Vec::new();

        // Build walker with ignore patterns
        let mut walker = WalkBuilder::new(root_path);
        walker
            .standard_filters(true) // Respect .gitignore, .ignore, etc.
            .hidden(false) // Include hidden files like .env.example
            .parents(false)
            .ignore(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true);

        // Add custom ignore patterns
        for pattern in &config.ignore_patterns {
            walker.add_custom_ignore_filename(pattern);
        }

        // Walk the directory with backpressure
        // Stop early if we've collected enough files (saves time on huge repos)
        let file_limit_threshold = config.max_files * 2; // Collect 2x limit to account for prioritization

        for result in walker.build() {
            let entry = result?;

            if !Self::should_include_entry(&entry) {
                continue;
            }

            let path = entry.path();
            if path.is_file() {
                // Check file size
                if let Ok(metadata) = fs::metadata(path) {
                    if metadata.len() > config.max_file_size as u64 {
                        continue;
                    }
                }

                // Prioritize certain extensions
                if let Some(ext) = path.extension() {
                    let ext_str = ext.to_string_lossy().to_lowercase();
                    if config
                        .priority_extensions
                        .iter()
                        .any(|&e| e == ext_str.as_str())
                    {
                        priority_files.push(path.to_path_buf());
                    } else {
                        other_files.push(path.to_path_buf());
                    }
                } else {
                    other_files.push(path.to_path_buf());
                }

                // Check if we've collected enough files (backpressure)
                if priority_files.len() + other_files.len() >= file_limit_threshold {
                    break;
                }
            }
        }

        // Combine priority files first (prioritization still works)
        let mut files = Vec::new();
        files.extend(priority_files);
        files.extend(other_files);

        Ok(files)
    }

    /// Check if a directory entry should be included
    fn should_include_entry(entry: &DirEntry) -> bool {
        let path = entry.path();

        // Skip directories we don't want to traverse
        if path.is_dir() {
            let dir_name = path.file_name().unwrap_or_default().to_string_lossy();

            // Common directories to skip
            let skip_dirs = [
                "node_modules",
                "target",
                "dist",
                "build",
                ".git",
                ".svn",
                ".hg",
                "venv",
                ".venv",
                "env",
                ".env",
                "__pycache__",
                ".pytest_cache",
                ".mypy_cache",
                ".tox",
                "vendor",
                "bower_components",
                ".idea",
                ".vscode",
                "coverage",
                ".coverage",
                "htmlcov",
                ".gradle",
                ".cargo",
            ];

            return !skip_dirs.iter().any(|&skip| dir_name == skip);
        }

        true
    }
}
