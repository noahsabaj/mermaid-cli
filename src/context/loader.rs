/// Context loader orchestration
///
/// Orchestrates file collection, token counting, and project detection
/// to build a complete project context.
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::sync::{Arc, Mutex};
use tiktoken_rs::{cl100k_base, CoreBPE};

use super::file_collector::{CollectorConfig, FileCollector};
use super::project_detector::{FileLoader, ProjectDetector};
use super::token_counter::TokenCounter;
use crate::models::ProjectContext;

// Static string slices for configuration (zero-allocation)
const DEFAULT_PRIORITY_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "jsx", "tsx", "go", "java", "cpp", "c", "h", "hpp", "cs", "rb", "php",
    "swift", "kt", "scala", "r", "sql", "sh", "yaml", "yml", "toml", "json", "xml", "html", "css",
    "scss", "md", "txt",
];

const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    "*.log", "*.tmp", "*.cache", "*.pyc", "*.pyo", "*.pyd", "*.so", "*.dylib", "*.dll", "*.exe",
    "*.o", "*.a", "*.lib", "*.png", "*.jpg", "*.jpeg", "*.gif", "*.bmp", "*.ico", "*.svg", "*.pdf",
    "*.zip", "*.tar", "*.gz", "*.rar", "*.7z",
];

/// Thread-safe state for tracking loading progress
#[derive(Debug, Clone)]
struct LoadingState {
    files_loaded: usize,
    tokens_used: usize,
}

impl LoadingState {
    fn new() -> Self {
        Self {
            files_loaded: 0,
            tokens_used: 0,
        }
    }

    /// Check and update counters atomically
    /// Returns true if the file should be processed (limits not exceeded)
    fn try_add_file(&mut self, tokens: usize, max_files: usize, max_tokens: usize) -> bool {
        if self.files_loaded >= max_files {
            return false;
        }

        if self.tokens_used + tokens > max_tokens {
            return false;
        }

        self.files_loaded += 1;
        self.tokens_used += tokens;
        true
    }
}

/// Configuration for the context loader
#[derive(Debug, Clone)]
pub struct LoaderConfig {
    /// Maximum file size to load (in bytes)
    pub max_file_size: usize,
    /// Maximum number of files to include
    pub max_files: usize,
    /// Maximum total context size in tokens
    pub max_context_tokens: usize,
    /// File extensions to prioritize
    pub priority_extensions: Vec<&'static str>,
    /// Additional patterns to ignore
    pub ignore_patterns: Vec<&'static str>,
}

impl Default for LoaderConfig {
    fn default() -> Self {
        Self {
            max_file_size: 1024 * 1024, // 1MB
            max_files: 100,
            max_context_tokens: 50000,
            priority_extensions: DEFAULT_PRIORITY_EXTENSIONS.to_vec(),
            ignore_patterns: DEFAULT_IGNORE_PATTERNS.to_vec(),
        }
    }
}

/// Loads project context from the filesystem
pub struct ContextLoader {
    config: LoaderConfig,
    tokenizer: CoreBPE,
    cache_manager: Option<Arc<crate::cache::CacheManager>>,
}

impl ContextLoader {
    /// Create a new context loader with default config
    pub fn new() -> Result<Self> {
        Ok(Self {
            config: LoaderConfig::default(),
            tokenizer: cl100k_base()?,
            cache_manager: crate::cache::CacheManager::new().ok().map(Arc::new),
        })
    }

    /// Create with custom config
    pub fn with_config(config: LoaderConfig) -> Result<Self> {
        Ok(Self {
            config,
            tokenizer: cl100k_base()?,
            cache_manager: crate::cache::CacheManager::new().ok().map(Arc::new),
        })
    }

    /// Load project context from the given path (alias for compatibility)
    pub fn load(&self, root_path: &std::path::Path) -> Result<ProjectContext> {
        self.load_context(root_path)
    }

    /// Load only the project structure without file contents (fast)
    pub fn load_structure(
        &self,
        root_path: &std::path::Path,
    ) -> Result<crate::models::LazyProjectContext> {
        let collector_config = CollectorConfig {
            max_file_size: self.config.max_file_size,
            max_files: self.config.max_files,
            priority_extensions: self.config.priority_extensions.clone(),
            ignore_patterns: self.config.ignore_patterns.clone(),
        };
        let collector = FileCollector::new(collector_config);
        let files = collector.collect_files(root_path)?;

        let lazy_context =
            crate::models::LazyProjectContext::new(root_path.to_string_lossy().to_string(), files);

        Ok(lazy_context)
    }

    /// Load project context from the given path
    pub fn load_context(&self, root_path: &std::path::Path) -> Result<ProjectContext> {
        let mut context = ProjectContext::new(root_path.to_string_lossy().to_string());

        // Detect project type
        context.project_type = ProjectDetector::detect_project_type(root_path);

        // Collect files
        let collector_config = CollectorConfig {
            max_file_size: self.config.max_file_size,
            max_files: self.config.max_files,
            priority_extensions: self.config.priority_extensions.clone(),
            ignore_patterns: self.config.ignore_patterns.clone(),
        };
        let collector = FileCollector::new(collector_config);
        let files = collector.collect_files(root_path)?;

        // Use Mutex-protected state for thread-safe tracking
        let loading_state = Arc::new(Mutex::new(LoadingState::new()));
        let token_counter = TokenCounter::new(self.tokenizer.clone(), self.cache_manager.clone());

        // Configuration for convenient access
        let max_files = self.config.max_files;
        let max_tokens = self.config.max_context_tokens;

        // Process files in parallel
        let loaded_contents: Vec<(String, String, usize)> = files
            .par_iter()
            .filter_map(|file_path| {
                // Determine token budget for this file
                let remaining_budget = {
                    let state = loading_state.lock().unwrap();
                    max_tokens.saturating_sub(state.tokens_used)
                };

                if remaining_budget == 0 {
                    return None;
                }

                // Load file with caching
                let (content, tokens) = token_counter
                    .load_file_cached(file_path, remaining_budget)
                    .ok()?;

                // Try to add file with mutex protection
                let mut state = loading_state.lock().unwrap();
                if !state.try_add_file(tokens, max_files, max_tokens) {
                    return None;
                }

                let relative_path = file_path
                    .strip_prefix(root_path)
                    .unwrap_or(file_path)
                    .to_string_lossy()
                    .to_string();

                Some((relative_path, content, tokens))
            })
            .collect();

        // Add all loaded files to context
        let mut actual_total_tokens = 0;
        for (path, content, tokens) in loaded_contents {
            context.add_file(path, content);
            actual_total_tokens += tokens;
        }

        context.token_count = actual_total_tokens;

        // Auto-include important files
        ProjectDetector::auto_include_important_files(&mut context, root_path, self);

        Ok(context)
    }
}

impl FileLoader for ContextLoader {
    fn load_file(&self, path: &std::path::Path) -> Result<String> {
        std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read file: {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_detect_project_type() {
        let temp_dir = TempDir::new().unwrap();
        let loader = ContextLoader::new().unwrap();

        // Test Rust project
        File::create(temp_dir.path().join("Cargo.toml")).unwrap();
        assert_eq!(
            ProjectDetector::detect_project_type(temp_dir.path()),
            Some("rust".to_string())
        );

        // Test Python project
        File::create(temp_dir.path().join("requirements.txt")).unwrap();
        assert_eq!(
            ProjectDetector::detect_project_type(temp_dir.path()),
            Some("rust".to_string()) // Cargo.toml takes precedence
        );
    }

    #[test]
    fn test_load_context() {
        let temp_dir = TempDir::new().unwrap();
        let loader = ContextLoader::new().unwrap();

        // Create some test files
        let mut cargo_file = File::create(temp_dir.path().join("Cargo.toml")).unwrap();
        writeln!(cargo_file, "[package]\nname = \"test\"").unwrap();

        let src_dir = temp_dir.path().join("src");
        std::fs::create_dir(&src_dir).unwrap();

        let mut main_file = File::create(src_dir.join("main.rs")).unwrap();
        writeln!(main_file, "fn main() {{\n    println!(\"Hello\");\n}}").unwrap();

        // Load context
        let context = loader.load_context(temp_dir.path()).unwrap();

        assert_eq!(context.project_type, Some("rust".to_string()));
        assert!(context.files.contains_key("Cargo.toml"));
        assert!(context.files.contains_key("src/main.rs"));
        assert!(context.token_count > 0);
    }

    #[test]
    fn test_loading_state_atomicity() {
        let mut state = LoadingState::new();

        assert!(state.try_add_file(10, 100, 1000));
        assert_eq!(state.files_loaded, 1);
        assert_eq!(state.tokens_used, 10);

        state.files_loaded = 100;
        assert!(!state.try_add_file(5, 100, 1000));
        assert_eq!(state.files_loaded, 100);

        let mut state2 = LoadingState::new();
        state2.tokens_used = 990;
        assert!(!state2.try_add_file(100, 100, 1000));
        assert_eq!(state2.tokens_used, 990);
    }

    #[test]
    fn test_concurrent_file_loading_safety() {
        use std::thread;

        let state = Arc::new(Mutex::new(LoadingState::new()));
        let mut handles = vec![];

        for _ in 0..10 {
            let state_clone = Arc::clone(&state);
            let handle = thread::spawn(move || {
                let mut state = state_clone.lock().unwrap();
                state.try_add_file(100, 100, 500)
            });
            handles.push(handle);
        }

        let results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert_eq!(results.iter().filter(|&&r| r).count(), 5);
        assert_eq!(results.iter().filter(|&&r| !r).count(), 5);

        let final_state = state.lock().unwrap();
        assert_eq!(final_state.files_loaded, 5);
        assert_eq!(final_state.tokens_used, 500);
    }
}
