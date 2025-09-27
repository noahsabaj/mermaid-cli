use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

/// A lazily-loaded project context that loads files on demand
#[derive(Debug, Clone)]
pub struct LazyProjectContext {
    /// Root path of the project
    pub root_path: String,
    /// Detected project type (Rust, Python, JavaScript, etc.)
    pub project_type: String,
    /// List of all file paths (loaded immediately)
    pub file_paths: Arc<Vec<PathBuf>>,
    /// Lazily loaded file contents
    pub files: Arc<RwLock<HashMap<String, String>>>,
    /// Running token count (updated as files are loaded)
    pub token_count: Arc<AtomicUsize>,
    /// Files that have been requested for loading
    pub loading_queue: Arc<Mutex<Vec<PathBuf>>>,
    /// Cache manager for persistent caching
    pub cache: Option<Arc<crate::cache::CacheManager>>,
}

impl LazyProjectContext {
    /// Create a new lazy project context with just file paths
    pub fn new(root_path: String, file_paths: Vec<PathBuf>) -> Self {
        let cache = crate::cache::CacheManager::new().ok().map(Arc::new);

        Self {
            root_path: root_path.clone(),
            project_type: detect_project_type(&root_path),
            file_paths: Arc::new(file_paths),
            files: Arc::new(RwLock::new(HashMap::new())),
            token_count: Arc::new(AtomicUsize::new(0)),
            loading_queue: Arc::new(Mutex::new(Vec::new())),
            cache,
        }
    }

    /// Get a file's content, loading it if necessary
    pub async fn get_file(&self, path: &str) -> Result<Option<String>> {
        // Check if already loaded
        {
            let files = self.files.read().await;
            if let Some(content) = files.get(path) {
                return Ok(Some(content.clone()));
            }
        }

        // Not loaded, need to load it
        let full_path = if path.starts_with(&self.root_path) {
            PathBuf::from(path)
        } else {
            PathBuf::from(&self.root_path).join(path)
        };

        // Load the file
        if full_path.exists() {
            let content = tokio::fs::read_to_string(&full_path).await?;

            // Count tokens
            if let Some(ref cache) = self.cache {
                if let Ok(tokens) = cache.get_or_compute_tokens(&full_path, &content, "cl100k_base")
                {
                    self.token_count.fetch_add(tokens, Ordering::Relaxed);
                }
            }

            // Store in memory
            let mut files = self.files.write().await;
            files.insert(path.to_string(), content.clone());

            Ok(Some(content))
        } else {
            Ok(None)
        }
    }

    /// Load a batch of files in the background
    pub async fn load_files_batch(&self, paths: Vec<String>) -> Result<()> {
        use futures::future::join_all;

        let futures = paths.into_iter().map(|path| {
            let self_clone = self.clone();
            async move {
                let _ = self_clone.get_file(&path).await;
            }
        });

        join_all(futures).await;
        Ok(())
    }

    /// Get the list of all file paths (instant)
    pub fn get_file_list(&self) -> Vec<String> {
        self.file_paths
            .iter()
            .filter_map(|p| {
                p.strip_prefix(&self.root_path)
                    .ok()
                    .and_then(|p| p.to_str())
                    .map(|s| s.to_string())
            })
            .collect()
    }

    /// Get current loaded file count
    pub async fn loaded_file_count(&self) -> usize {
        self.files.read().await.len()
    }

    /// Get total file count
    pub fn total_file_count(&self) -> usize {
        self.file_paths.len()
    }

    /// Check if all files are loaded
    pub async fn is_fully_loaded(&self) -> bool {
        self.loaded_file_count().await >= self.total_file_count()
    }

    /// Convert to regular ProjectContext (for compatibility)
    pub async fn to_project_context(&self) -> crate::models::ProjectContext {
        let files = self.files.read().await;
        let mut context = crate::models::ProjectContext::new(self.root_path.clone());
        context.project_type = Some(self.project_type.clone());
        context.token_count = self.token_count.load(Ordering::Relaxed);

        for (path, content) in files.iter() {
            context.add_file(path.clone(), content.clone());
        }

        context
    }
}

/// Detect project type from root path
fn detect_project_type(root_path: &str) -> String {
    let path = Path::new(root_path);

    // Check for various project files
    if path.join("Cargo.toml").exists() {
        "Rust".to_string()
    } else if path.join("package.json").exists() {
        "JavaScript/TypeScript".to_string()
    } else if path.join("requirements.txt").exists() || path.join("setup.py").exists() {
        "Python".to_string()
    } else if path.join("go.mod").exists() {
        "Go".to_string()
    } else if path.join("pom.xml").exists() || path.join("build.gradle").exists() {
        "Java".to_string()
    } else if path.join("*.csproj").exists() || path.join("*.sln").exists() {
        "C#/.NET".to_string()
    } else if path.join("Gemfile").exists() {
        "Ruby".to_string()
    } else if path.join("composer.json").exists() {
        "PHP".to_string()
    } else {
        "Unknown".to_string()
    }
}

/// Priority files to load first for better UX
pub fn get_priority_files(root_path: &str) -> Vec<String> {
    vec![
        "README.md",
        "readme.md",
        "README.rst",
        "README.txt",
        "CLAUDE.md", // Mermaid's own instructions file
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        ".gitignore",
        "LICENSE",
    ]
    .into_iter()
    .filter_map(|f| {
        let path = Path::new(root_path).join(f);
        if path.exists() {
            Some(f.to_string())
        } else {
            None
        }
    })
    .collect()
}
