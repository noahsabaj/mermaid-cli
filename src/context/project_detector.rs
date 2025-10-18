/// Project type detection logic
///
/// Responsible for detecting the project type based on configuration files
/// and auto-including important files for that project type.
use crate::models::ProjectContext;
use std::path::Path;

/// Detects and manages project-specific logic
pub struct ProjectDetector;

impl ProjectDetector {
    /// Detect the project type based on configuration files
    pub fn detect_project_type(root_path: &Path) -> Option<String> {
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
    pub fn auto_include_important_files(
        context: &mut ProjectContext,
        root_path: &Path,
        loader: &impl FileLoader,
    ) {
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
                if let Ok(content) = loader.load_file(&file_path) {
                    context.included_files.push(file_name.to_string());
                    context.add_file(file_name.to_string(), content);
                }
            }
        }
    }
}

/// Trait for file loading (allows abstraction over different loaders)
pub trait FileLoader {
    fn load_file(&self, path: &Path) -> anyhow::Result<String>;
}
