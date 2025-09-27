use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

/// Events that we care about for the file system
#[derive(Debug, Clone)]
pub enum FileEvent {
    Created(Vec<PathBuf>),
    Modified(Vec<PathBuf>),
    Deleted(Vec<PathBuf>),
}

/// A file system watcher that monitors changes in a directory
pub struct FileSystemWatcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<Result<Event, notify::Error>>,
}

impl FileSystemWatcher {
    /// Create a new file system watcher for the given path
    pub fn new(path: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })?;

        // Watch the path recursively
        watcher.watch(path, RecursiveMode::Recursive)?;

        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// Check for any file system events (non-blocking)
    pub fn check_events(&self) -> Vec<FileEvent> {
        let mut events = Vec::new();

        // Process all available events
        while let Ok(Ok(event)) = self.rx.try_recv() {
            match event.kind {
                EventKind::Create(_) => {
                    if !event.paths.is_empty() {
                        events.push(FileEvent::Created(event.paths));
                    }
                },
                EventKind::Modify(modify_kind) => {
                    // Filter out metadata-only changes
                    use notify::event::ModifyKind;
                    match modify_kind {
                        ModifyKind::Data(_) | ModifyKind::Any => {
                            if !event.paths.is_empty() {
                                events.push(FileEvent::Modified(event.paths));
                            }
                        },
                        _ => {}, // Ignore metadata changes
                    }
                },
                EventKind::Remove(_) => {
                    if !event.paths.is_empty() {
                        events.push(FileEvent::Deleted(event.paths));
                    }
                },
                _ => {}, // Ignore other events
            }
        }

        events
    }

    /// Check if a path should be ignored (e.g., hidden files, git files, etc.)
    pub fn should_ignore_path(path: &Path) -> bool {
        // Ignore hidden files and directories
        if let Some(name) = path.file_name() {
            if let Some(name_str) = name.to_str() {
                if name_str.starts_with('.') {
                    return true;
                }
            }
        }

        // Ignore common build/cache directories
        if let Some(parent) = path.parent() {
            if let Some(parent_name) = parent.file_name() {
                if let Some(parent_str) = parent_name.to_str() {
                    match parent_str {
                        "target" | "node_modules" | "__pycache__" | ".git" | "dist" | "build"
                        | ".venv" | "venv" => return true,
                        _ => {},
                    }
                }
            }
        }

        // Only watch text files and common code files
        if let Some(ext) = path.extension() {
            if let Some(ext_str) = ext.to_str() {
                match ext_str {
                    // Allow common text and code files
                    "txt" | "md" | "rs" | "toml" | "yaml" | "yml" | "json" | "js" | "ts"
                    | "jsx" | "tsx" | "py" | "go" | "java" | "c" | "cpp" | "h" | "hpp" | "sh"
                    | "bash" | "zsh" | "fish" | "html" | "css" | "scss" | "xml" | "vue"
                    | "svelte" => false,
                    // Ignore everything else (binaries, images, etc.)
                    _ => true,
                }
            } else {
                true // Ignore files without valid UTF-8 extensions
            }
        } else {
            false // Allow files without extensions (like LICENSE, README)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_should_ignore_path() {
        assert!(FileSystemWatcher::should_ignore_path(Path::new(
            ".gitignore"
        )));
        assert!(FileSystemWatcher::should_ignore_path(Path::new(
            "node_modules/package.json"
        )));
        assert!(FileSystemWatcher::should_ignore_path(Path::new(
            "image.png"
        )));

        assert!(!FileSystemWatcher::should_ignore_path(Path::new("main.rs")));
        assert!(!FileSystemWatcher::should_ignore_path(Path::new(
            "README.md"
        )));
        assert!(!FileSystemWatcher::should_ignore_path(Path::new(
            "config.toml"
        )));
    }

    #[tokio::test]
    async fn test_file_watcher_events() {
        let temp_dir = TempDir::new().unwrap();
        let watcher = FileSystemWatcher::new(temp_dir.path()).unwrap();

        // Create a file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "hello").unwrap();

        // Give the watcher time to detect the change
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let events = watcher.check_events();
        assert!(!events.is_empty(), "Should have detected file creation");
    }
}
