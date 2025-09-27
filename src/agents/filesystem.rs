use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Read a file from the filesystem
pub fn read_file(path: &str) -> Result<String> {
    let path = normalize_path(path)?;

    // Security check: ensure path is within current directory
    validate_path(&path)?;

    fs::read_to_string(&path).with_context(|| format!("Failed to read file: {}", path.display()))
}

/// Write content to a file
pub fn write_file(path: &str, content: &str) -> Result<()> {
    let path = normalize_path(path)?;

    // Security check
    validate_path(&path)?;

    // Create parent directories if they don't exist
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create parent directories for: {}",
                path.display()
            )
        })?;
    }

    // Create backup if file exists
    if path.exists() {
        let backup_path = format!("{}.backup", path.display());
        fs::copy(&path, &backup_path)
            .with_context(|| format!("Failed to create backup of: {}", path.display()))?;
    }

    fs::write(&path, content).with_context(|| format!("Failed to write file: {}", path.display()))
}

/// Delete a file
pub fn delete_file(path: &str) -> Result<()> {
    let path = normalize_path(path)?;

    // Security check
    validate_path(&path)?;

    // Create backup before deletion
    if path.exists() {
        let backup_path = format!("{}.deleted", path.display());
        fs::copy(&path, &backup_path).with_context(|| {
            format!(
                "Failed to create backup before deletion: {}",
                path.display()
            )
        })?;
    }

    fs::remove_file(&path).with_context(|| format!("Failed to delete file: {}", path.display()))
}

/// Create a directory
pub fn create_directory(path: &str) -> Result<()> {
    let path = normalize_path(path)?;

    // Security check
    validate_path(&path)?;

    fs::create_dir_all(&path)
        .with_context(|| format!("Failed to create directory: {}", path.display()))
}

/// Check if a path exists
pub fn path_exists(path: &str) -> Result<bool> {
    let path = normalize_path(path)?;

    // Security check
    validate_path(&path)?;

    Ok(path.exists())
}

/// Normalize a path (resolve relative paths)
fn normalize_path(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);

    if path.is_absolute() {
        // For absolute paths, ensure they're within the current directory
        let current_dir = std::env::current_dir()?;
        if !path.starts_with(&current_dir) {
            anyhow::bail!("Access denied: path outside of project directory");
        }
        Ok(path.to_path_buf())
    } else {
        // For relative paths, resolve from current directory
        let current_dir = std::env::current_dir()?;
        Ok(current_dir.join(path))
    }
}

/// Validate that a path is safe to access
fn validate_path(path: &Path) -> Result<()> {
    let current_dir = std::env::current_dir()?;

    // Resolve the path to handle .. and .
    let canonical = if path.exists() {
        path.canonicalize()?
    } else {
        // For non-existent paths, canonicalize the parent
        if let Some(parent) = path.parent() {
            if parent.exists() {
                let parent_canonical = parent.canonicalize()?;
                parent_canonical.join(path.file_name().unwrap_or_default())
            } else {
                path.to_path_buf()
            }
        } else {
            path.to_path_buf()
        }
    };

    // Ensure the path is within the current directory
    if !canonical.starts_with(&current_dir) {
        anyhow::bail!(
            "Security error: attempted to access path outside of project directory: {}",
            path.display()
        );
    }

    // Check for sensitive files
    let sensitive_patterns = [
        ".ssh",
        ".aws",
        ".env",
        "id_rsa",
        "id_ed25519",
        ".git/config",
        ".npmrc",
        ".pypirc",
    ];

    let path_str = path.to_string_lossy();
    for pattern in &sensitive_patterns {
        if path_str.contains(pattern) {
            anyhow::bail!(
                "Security error: attempted to access potentially sensitive file: {}",
                path.display()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_file_operations() {
        let temp_dir = TempDir::new().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        // Test write and read
        let test_file = "test.txt";
        let content = "Hello, Mermaid!";

        write_file(test_file, content).unwrap();
        let read_content = read_file(test_file).unwrap();
        assert_eq!(read_content, content);

        // Test file exists
        assert!(path_exists(test_file).unwrap());

        // Test delete
        delete_file(test_file).unwrap();
        assert!(!path_exists(test_file).unwrap());

        // Test create directory
        create_directory("test_dir").unwrap();
        assert!(path_exists("test_dir").unwrap());
    }

    #[test]
    fn test_path_validation() {
        let temp_dir = TempDir::new().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        // This should fail - trying to access parent directory
        assert!(read_file("../sensitive_file").is_err());

        // This should fail - trying to access absolute path outside project
        assert!(read_file("/etc/passwd").is_err());
    }
}
