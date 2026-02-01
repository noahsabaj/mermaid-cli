/// Unified context management
///
/// Single module that handles:
/// - File collection and caching
/// - Change detection for dynamic reloading
/// - Token counting and content loading
/// - Project context building

use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tiktoken_rs::{cl100k_base, CoreBPE};

use super::file_collector::{CollectorConfig, FileCollector};
use super::token_counter::TokenCounter;
use crate::models::{LazyProjectContext, ProjectContext};
use crate::utils::MutexExt;

// Default file extensions to prioritize
const DEFAULT_PRIORITY_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "jsx", "tsx", "go", "java", "cpp", "c", "h", "hpp", "cs", "rb", "php",
    "swift", "kt", "scala", "r", "sql", "sh", "yaml", "yml", "toml", "json", "xml", "html", "css",
    "scss", "md", "txt",
];

// Default patterns to ignore
const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    "*.log", "*.tmp", "*.cache", "*.pyc", "*.pyo", "*.pyd", "*.so", "*.dylib", "*.dll", "*.exe",
    "*.o", "*.a", "*.lib", "*.png", "*.jpg", "*.jpeg", "*.gif", "*.bmp", "*.ico", "*.svg", "*.pdf",
    "*.zip", "*.tar", "*.gz", "*.rar", "*.7z",
];

/// Configuration for context loading
#[derive(Debug, Clone)]
pub struct ContextConfig {
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

impl Default for ContextConfig {
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

/// Unified context manager for project files
///
/// Combines file collection, change detection, and content loading into a single interface.
#[derive(Clone)]
pub struct Context {
    /// Root path of the project
    root_path: PathBuf,
    /// Configuration
    config: ContextConfig,
    /// Tokenizer for counting tokens
    tokenizer: CoreBPE,
    /// Cache manager for token caching
    cache_manager: Option<Arc<crate::cache::CacheManager>>,
    /// Last computed hash of the file tree
    last_file_hash: Option<u64>,
    /// Last time context was loaded
    last_load_time: Option<u64>,
    /// Cached file list from last load
    cached_files: Vec<PathBuf>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("root_path", &self.root_path)
            .field("config", &self.config)
            .field("last_file_hash", &self.last_file_hash)
            .field("last_load_time", &self.last_load_time)
            .field("cached_files", &self.cached_files.len())
            .finish()
    }
}

impl Context {
    /// Create a new context manager for the given project path
    pub fn new(root_path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            root_path: root_path.as_ref().to_path_buf(),
            config: ContextConfig::default(),
            tokenizer: cl100k_base()?,
            cache_manager: crate::cache::CacheManager::new().ok().map(Arc::new),
            last_file_hash: None,
            last_load_time: None,
            cached_files: Vec::new(),
        })
    }

    /// Create with custom config
    pub fn with_config(root_path: impl AsRef<Path>, config: ContextConfig) -> Result<Self> {
        Ok(Self {
            root_path: root_path.as_ref().to_path_buf(),
            config,
            tokenizer: cl100k_base()?,
            cache_manager: crate::cache::CacheManager::new().ok().map(Arc::new),
            last_file_hash: None,
            last_load_time: None,
            cached_files: Vec::new(),
        })
    }

    /// Load full project context from a path (one-shot, static method)
    ///
    /// This is the main entry point for loading a complete project context
    /// with file contents and token counting.
    pub async fn load(root_path: impl AsRef<Path>) -> Result<ProjectContext> {
        let ctx = Self::new(&root_path)?;
        ctx.load_full_context().await
    }

    /// Load full context with file contents and token counting
    pub async fn load_full_context(&self) -> Result<ProjectContext> {
        let mut context = ProjectContext::new(self.root_path.to_string_lossy().to_string());

        // Collect files
        let collector = self.create_collector();
        let files = collector.collect_files(&self.root_path).await?;

        // Use Mutex-protected state for thread-safe tracking
        let loading_state = Arc::new(Mutex::new(LoadingState::new()));
        let token_counter = TokenCounter::new(self.tokenizer.clone(), self.cache_manager.clone());

        // Configuration for convenient access
        let max_files = self.config.max_files;
        let max_tokens = self.config.max_context_tokens;

        // Process files sequentially (I/O bound, parallelism overhead not worth it)
        let loaded_contents: Vec<(String, String, usize)> = files
            .iter()
            .filter_map(|file_path| {
                // Determine token budget for this file
                let remaining_budget = {
                    let state = loading_state.lock_mut_safe();
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
                let mut state = loading_state.lock_mut_safe();
                if !state.try_add_file(tokens, max_files, max_tokens) {
                    return None;
                }

                let relative_path = file_path
                    .strip_prefix(&self.root_path)
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

        Ok(context)
    }

    /// Load only project structure (file paths, no contents) - fast
    pub async fn load_structure(&self) -> Result<LazyProjectContext> {
        let collector = self.create_collector();
        let files = collector.collect_files(&self.root_path).await?;

        let lazy_context =
            LazyProjectContext::new(self.root_path.to_string_lossy().to_string(), files);

        Ok(lazy_context)
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

    /// Force a reload of the project structure (file paths only)
    pub async fn reload(&mut self) -> Result<()> {
        // Collect files from the project
        let collector = self.create_collector();
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

    /// Build a ProjectContext with the current file tree (paths only, no contents)
    ///
    /// This creates a context with:
    /// - Root path
    /// - Complete file tree (file paths only, not contents)
    /// - No file contents loaded (those load on demand)
    pub fn build_context(&self) -> ProjectContext {
        let mut context = ProjectContext::new(self.root_path.to_string_lossy().to_string());

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

    /// Create a file collector with current config
    fn create_collector(&self) -> FileCollector {
        let collector_config = CollectorConfig {
            max_file_size: self.config.max_file_size,
            max_files: self.config.max_files,
            priority_extensions: self.config.priority_extensions.clone(),
            ignore_patterns: self.config.ignore_patterns.clone(),
        };
        FileCollector::new(collector_config)
    }

    /// Compute a hash of the current file tree for change detection
    async fn compute_file_hash(&self) -> Result<u64> {
        let collector = self.create_collector();
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


#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_context_creation() {
        let temp_dir = TempDir::new().unwrap();
        let ctx = Context::new(temp_dir.path()).unwrap();

        assert_eq!(ctx.root_path, temp_dir.path());
        assert_eq!(ctx.total_files(), 0);
        assert!(ctx.needs_reload().await);
    }

    #[tokio::test]
    async fn test_file_tree_change_detection() {
        let temp_dir = TempDir::new().unwrap();
        let mut ctx = Context::new(temp_dir.path()).unwrap();

        // Initial load
        ctx.reload().await.unwrap();
        let initial_hash = ctx.last_file_hash;

        // No changes - should not need reload
        assert!(!ctx.needs_reload().await);

        // Add a file - should need reload
        let test_file = temp_dir.path().join("test.py");
        fs::write(&test_file, "print('test')").unwrap();

        assert!(ctx.needs_reload().await);

        // Reload and verify hash changed
        ctx.reload().await.unwrap();
        assert_ne!(ctx.last_file_hash, initial_hash);
    }

    #[tokio::test]
    async fn test_project_context_building() {
        let temp_dir = TempDir::new().unwrap();

        // Create some test files
        fs::write(temp_dir.path().join("main.py"), "print('hello')").unwrap();
        fs::write(temp_dir.path().join("lib.py"), "def helper(): pass").unwrap();
        fs::write(temp_dir.path().join("requirements.txt"), "requests\n").unwrap();

        let mut ctx = Context::new(temp_dir.path()).unwrap();
        ctx.reload().await.unwrap();

        let context = ctx.build_context();
        assert_eq!(
            context.root_path,
            temp_dir.path().to_string_lossy().to_string()
        );
        assert_eq!(context.files.len(), 3);
    }

    #[tokio::test]
    async fn test_load_full_context() {
        let temp_dir = TempDir::new().unwrap();

        // Create some test files
        let mut cargo_file = File::create(temp_dir.path().join("Cargo.toml")).unwrap();
        writeln!(cargo_file, "[package]\nname = \"test\"").unwrap();

        let src_dir = temp_dir.path().join("src");
        std::fs::create_dir(&src_dir).unwrap();

        let mut main_file = File::create(src_dir.join("main.rs")).unwrap();
        writeln!(main_file, "fn main() {{\n    println!(\"Hello\");\n}}").unwrap();

        // Load full context (static method)
        let context = Context::load(temp_dir.path()).await.unwrap();

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
