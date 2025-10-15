use anyhow::{Context, Result};
use ignore::{DirEntry, WalkBuilder};
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tiktoken_rs::{cl100k_base, CoreBPE};

use crate::models::ProjectContext;

// Static string slices for configuration (zero-allocation)
const DEFAULT_PRIORITY_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "jsx", "tsx", "go", "java", "cpp", "c", "h", "hpp",
    "cs", "rb", "php", "swift", "kt", "scala", "r", "sql", "sh",
    "yaml", "yml", "toml", "json", "xml", "html", "css", "scss", "md", "txt",
];

const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    "*.log", "*.tmp", "*.cache", "*.pyc", "*.pyo", "*.pyd",
    "*.so", "*.dylib", "*.dll", "*.exe", "*.o", "*.a", "*.lib",
    "*.png", "*.jpg", "*.jpeg", "*.gif", "*.bmp", "*.ico", "*.svg", "*.pdf",
    "*.zip", "*.tar", "*.gz", "*.rar", "*.7z",
];

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
}

impl ContextLoader {
    /// Create a new context loader with default config
    pub fn new() -> Result<Self> {
        Ok(Self {
            config: LoaderConfig::default(),
            tokenizer: cl100k_base()?,
        })
    }

    /// Create with custom config
    pub fn with_config(config: LoaderConfig) -> Result<Self> {
        Ok(Self {
            config,
            tokenizer: cl100k_base()?,
        })
    }

    /// Load project context from the given path (alias for compatibility)
    pub fn load(&self, root_path: &Path) -> Result<ProjectContext> {
        self.load_context(root_path)
    }

    /// Load only the project structure without file contents (fast)
    pub fn load_structure(&self, root_path: &Path) -> Result<crate::models::LazyProjectContext> {
        // Collect all file paths (without loading content)
        let files = self.collect_files(root_path)?;

        // Create lazy context with just paths
        let lazy_context =
            crate::models::LazyProjectContext::new(root_path.to_string_lossy().to_string(), files);

        Ok(lazy_context)
    }

    /// Load project context from the given path
    pub fn load_context(&self, root_path: &Path) -> Result<ProjectContext> {
        let mut context = ProjectContext::new(root_path.to_string_lossy().to_string());

        // Detect project type
        context.project_type = self.detect_project_type(root_path);

        // Collect all files using the ignore crate
        let files = self.collect_files(root_path)?;

        // Use atomic counters for thread-safe tracking
        let total_tokens = Arc::new(AtomicUsize::new(0));
        let loaded_files = Arc::new(AtomicUsize::new(0));

        // Create a shared tokenizer for all threads
        let tokenizer = Arc::new(self.tokenizer.clone());

        // Process files in parallel and collect results
        let loaded_contents: Vec<(String, String, usize)> = files
            .par_iter()
            .filter_map(|file_path| {
                // Check if we've hit the file limit
                if loaded_files.load(Ordering::Relaxed) >= self.config.max_files {
                    return None;
                }

                // Try to load the file
                if let Ok(content) = self.load_file(file_path) {
                    // Count tokens using the shared tokenizer
                    let tokens = tokenizer.encode_with_special_tokens(&content).len();

                    // Check if adding this file would exceed token limit
                    let current_total = total_tokens.load(Ordering::Relaxed);
                    if current_total + tokens > self.config.max_context_tokens {
                        return None;
                    }

                    // Update counters atomically
                    total_tokens.fetch_add(tokens, Ordering::Relaxed);
                    loaded_files.fetch_add(1, Ordering::Relaxed);

                    let relative_path = file_path
                        .strip_prefix(root_path)
                        .unwrap_or(file_path)
                        .to_string_lossy()
                        .to_string();

                    Some((relative_path, content, tokens))
                } else {
                    None
                }
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
        self.auto_include_important_files(&mut context, root_path);

        Ok(context)
    }

    /// Collect all relevant files from the project
    fn collect_files(&self, root_path: &Path) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
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
        for pattern in &self.config.ignore_patterns {
            walker.add_custom_ignore_filename(pattern);
        }

        // Walk the directory
        for result in walker.build() {
            let entry = result?;

            if !self.should_include_entry(&entry) {
                continue;
            }

            let path = entry.path();
            if path.is_file() {
                // Check file size
                if let Ok(metadata) = fs::metadata(path) {
                    if metadata.len() > self.config.max_file_size as u64 {
                        continue;
                    }
                }

                // Prioritize certain extensions
                if let Some(ext) = path.extension() {
                    let ext_str = ext.to_string_lossy().to_lowercase();
                    if self.config.priority_extensions.iter().any(|&e| e == ext_str.as_str()) {
                        priority_files.push(path.to_path_buf());
                    } else {
                        other_files.push(path.to_path_buf());
                    }
                } else {
                    other_files.push(path.to_path_buf());
                }
            }
        }

        // Combine priority files first
        files.extend(priority_files);
        files.extend(other_files);

        Ok(files)
    }

    /// Check if a directory entry should be included
    fn should_include_entry(&self, entry: &DirEntry) -> bool {
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

    /// Load a single file
    fn load_file(&self, path: &Path) -> Result<String> {
        fs::read_to_string(path).with_context(|| format!("Failed to read file: {}", path.display()))
    }

    /// Detect the project type based on configuration files
    fn detect_project_type(&self, root_path: &Path) -> Option<String> {
        let checks = [
            ("Cargo.toml", "rust"),
            ("package.json", "javascript"),
            ("requirements.txt", "python"),
            ("setup.py", "python"),
            ("pyproject.toml", "python"),
            ("go.mod", "go"),
            ("pom.xml", "java"),
            ("build.gradle", "java"),
            ("composer.json", "php"),
            ("Gemfile", "ruby"),
            ("mix.exs", "elixir"),
            ("project.clj", "clojure"),
            ("build.sbt", "scala"),
            ("Package.swift", "swift"),
            ("tsconfig.json", "typescript"),
        ];

        for (file, project_type) in &checks {
            if root_path.join(file).exists() {
                return Some(project_type.to_string());
            }
        }

        None
    }

    /// Auto-include important files based on project type
    fn auto_include_important_files(&self, context: &mut ProjectContext, root_path: &Path) {
        let important_files = match context.project_type.as_deref() {
            Some("rust") => vec!["Cargo.toml", "src/main.rs", "src/lib.rs"],
            Some("javascript") | Some("typescript") => {
                vec![
                    "package.json",
                    "index.js",
                    "index.ts",
                    "src/index.js",
                    "src/index.ts",
                ]
            },
            Some("python") => vec![
                "requirements.txt",
                "setup.py",
                "main.py",
                "app.py",
                "__init__.py",
            ],
            Some("go") => vec!["go.mod", "main.go"],
            _ => vec!["README.md", "README.txt", "readme.md"],
        };

        for file_name in important_files {
            let file_path = root_path.join(file_name);
            if file_path.exists() && !context.files.contains_key(file_name) {
                if let Ok(content) = self.load_file(&file_path) {
                    context.included_files.push(file_name.to_string());
                    context.add_file(file_name.to_string(), content);
                }
            }
        }
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
            loader.detect_project_type(temp_dir.path()),
            Some("rust".to_string())
        );

        // Test Python project
        File::create(temp_dir.path().join("requirements.txt")).unwrap();
        assert_eq!(
            loader.detect_project_type(temp_dir.path()),
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
        fs::create_dir(&src_dir).unwrap();

        let mut main_file = File::create(src_dir.join("main.rs")).unwrap();
        writeln!(main_file, "fn main() {{\n    println!(\"Hello\");\n}}").unwrap();

        // Load context
        let context = loader.load_context(temp_dir.path()).unwrap();

        assert_eq!(context.project_type, Some("rust".to_string()));
        assert!(context.files.contains_key("Cargo.toml"));
        assert!(context.files.contains_key("src/main.rs"));
        assert!(context.token_count > 0);
    }
}
