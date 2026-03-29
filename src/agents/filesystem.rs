use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use std::fs;
use std::path::{Path, PathBuf};

/// Read a file from the filesystem
pub fn read_file(path: &str) -> Result<String> {
    let path = normalize_path_for_read(path)?;

    // Security check: block sensitive files but allow reading outside project
    validate_path_for_read(&path)?;

    fs::read_to_string(&path).with_context(|| format!("Failed to read file: {}", path.display()))
}

/// Read a file from the filesystem asynchronously (for parallel operations)
pub async fn read_file_async(path: String) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        read_file(&path)
    })
    .await
    .context("Failed to spawn blocking task for file read")?
}

/// Check if a file is a binary format that should be base64-encoded
pub fn is_binary_file(path: &str) -> bool {
    let path = Path::new(path);
    if let Some(ext) = path.extension() {
        let ext_str = ext.to_string_lossy().to_lowercase();
        matches!(
            ext_str.as_str(),
            "pdf" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tiff"
        )
    } else {
        false
    }
}

/// Read a binary file and encode it as base64
pub fn read_binary_file(path: &str) -> Result<String> {
    let path = normalize_path_for_read(path)?;

    // Security check: block sensitive files but allow reading outside project
    validate_path_for_read(&path)?;

    let bytes = fs::read(&path)
        .with_context(|| format!("Failed to read binary file: {}", path.display()))?;

    Ok(general_purpose::STANDARD.encode(&bytes))
}

/// Write content to a file atomically with timestamped backup
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

    // Create timestamped backup if file exists
    if path.exists() {
        create_timestamped_backup(&path)?;
    }

    // Atomic write: write to temporary file, then rename
    let temp_path = format!("{}.tmp.{}", path.display(), std::process::id());
    let temp_path = std::path::PathBuf::from(&temp_path);

    // Write to temporary file
    fs::write(&temp_path, content).with_context(|| {
        format!("Failed to write to temporary file: {}", temp_path.display())
    })?;

    // Atomically rename temp file to target
    fs::rename(&temp_path, &path).with_context(|| {
        format!(
            "Failed to finalize write to: {} (temp file: {})",
            path.display(),
            temp_path.display()
        )
    })?;

    Ok(())
}

/// Create a timestamped backup of a file
/// Format: file.txt.backup.2025-10-20-01-45-32
fn create_timestamped_backup(path: &std::path::Path) -> Result<()> {
    let timestamp = chrono::Local::now().format("%Y-%m-%d-%H-%M-%S");
    let backup_path = format!("{}.backup.{}", path.display(), timestamp);

    fs::copy(path, &backup_path).with_context(|| {
        format!(
            "Failed to create backup of: {} to {}",
            path.display(),
            backup_path
        )
    })?;

    Ok(())
}

/// Edit a file by replacing a unique occurrence of old_string with new_string
/// Returns a unified diff showing the changes
pub fn edit_file(path: &str, old_string: &str, new_string: &str) -> Result<String> {
    let path = normalize_path(path)?;

    // Security check
    validate_path(&path)?;

    // Read current content
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read file for editing: {}", path.display()))?;

    // Check that old_string occurs exactly once
    let match_count = content.matches(old_string).count();
    if match_count == 0 {
        anyhow::bail!(
            "old_string not found in {}. Make sure the text matches exactly, including whitespace and indentation.",
            path.display()
        );
    }
    if match_count > 1 {
        anyhow::bail!(
            "old_string appears {} times in {}. It must be unique. Include more surrounding context to make it unique.",
            match_count,
            path.display()
        );
    }

    // Perform the replacement
    let new_content = content.replacen(old_string, new_string, 1);

    // Create timestamped backup
    create_timestamped_backup(&path)?;

    // Atomic write: write to temporary file, then rename
    let temp_path = format!("{}.tmp.{}", path.display(), std::process::id());
    let temp_path = std::path::PathBuf::from(&temp_path);

    fs::write(&temp_path, &new_content).with_context(|| {
        format!("Failed to write to temporary file: {}", temp_path.display())
    })?;

    fs::rename(&temp_path, &path).with_context(|| {
        format!(
            "Failed to finalize edit to: {} (temp file: {})",
            path.display(),
            temp_path.display()
        )
    })?;

    // Generate diff
    let diff = generate_diff(&content, &new_content, old_string, new_string);
    Ok(diff)
}

/// Generate a unified diff showing the changed lines with context
fn generate_diff(old_content: &str, new_content: &str, old_string: &str, new_string: &str) -> String {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    let removed_count = old_string.lines().count();
    let added_count = new_string.lines().count();

    // Find where the change starts in the old content
    let prefix_len = old_content[..old_content.find(old_string).unwrap_or(0)].len();
    let change_start_line = old_content[..prefix_len].matches('\n').count();

    let context_lines = 3;
    let diff_start = change_start_line.saturating_sub(context_lines);
    let new_diff_end = (change_start_line + added_count + context_lines).min(new_lines.len());

    let mut output = String::new();
    output.push_str(&format!("Added {} lines, removed {} lines\n", added_count, removed_count));

    // Context before
    for i in diff_start..change_start_line {
        if i < old_lines.len() {
            output.push_str(&format!("{:>4}   {}\n", i + 1, old_lines[i]));
        }
    }

    // Removed lines
    for i in 0..removed_count {
        let line_num = change_start_line + i;
        if line_num < old_lines.len() {
            output.push_str(&format!("{:>4} - {}\n", line_num + 1, old_lines[line_num]));
        }
    }

    // Added lines
    for i in 0..added_count {
        let line_num = change_start_line + i;
        if line_num < new_lines.len() {
            output.push_str(&format!("{:>4} + {}\n", line_num + 1, new_lines[line_num]));
        }
    }

    // Context after
    let context_after_start = change_start_line + added_count;
    for i in context_after_start..new_diff_end {
        if i < new_lines.len() {
            output.push_str(&format!("{:>4}   {}\n", i + 1, new_lines[i]));
        }
    }

    output
}

/// Delete a file with timestamped backup (for recovery)
pub fn delete_file(path: &str) -> Result<()> {
    let path = normalize_path(path)?;

    // Security check
    validate_path(&path)?;

    // Create timestamped backup before deletion
    if path.exists() {
        create_timestamped_backup(&path)?;
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

/// Normalize a path for reading (allows absolute paths anywhere)
fn normalize_path_for_read(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);

    if path.is_absolute() {
        // For absolute paths, return as-is (user has specified exact location)
        Ok(path.to_path_buf())
    } else {
        // For relative paths, resolve from current directory
        let current_dir = std::env::current_dir()?;
        Ok(current_dir.join(path))
    }
}

/// Normalize a path (resolve relative paths) - strict version for writes
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

/// Check if a path component or filename matches a sensitive pattern.
///
/// Uses path-component matching (not substring) to avoid false positives
/// like ".environment.ts" matching ".env". Checks both directory components
/// and file extensions.
fn is_sensitive_path(path: &Path) -> bool {
    // Directory components that are always sensitive
    let sensitive_dirs = [".ssh", ".aws", ".gnupg"];

    // Filenames/extensions that are sensitive
    let sensitive_filenames = [
        ".npmrc",
        ".pypirc",
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        "id_dsa",
        "credentials.json",
        "secrets.yaml",
        "secrets.yml",
        "token.json",
    ];

    // File extensions that are sensitive
    let sensitive_extensions = ["pem", "key"];

    let path_str = path.to_string_lossy();

    // Check for .git/config specifically (substring is fine here, it's unique)
    if path_str.contains(".git/config") || path_str.contains(".git\\config") {
        return true;
    }

    for component in path.components() {
        let name = component.as_os_str().to_string_lossy();

        // Check sensitive directories
        for dir in &sensitive_dirs {
            if name == *dir {
                return true;
            }
        }

        // Check .env files: match ".env" exactly or ".env.*" (like .env.local, .env.production)
        // but NOT files that merely contain "env" (like .environment.ts)
        if name == ".env" || name.starts_with(".env.") {
            return true;
        }

        // Check sensitive filenames
        for filename in &sensitive_filenames {
            if name == *filename {
                return true;
            }
        }
    }

    // Check sensitive extensions
    if let Some(ext) = path.extension() {
        let ext_str = ext.to_string_lossy().to_lowercase();
        for sensitive_ext in &sensitive_extensions {
            if ext_str == *sensitive_ext {
                return true;
            }
        }
    }

    false
}

/// Validate that a path is safe to read from (blocks sensitive files only)
fn validate_path_for_read(path: &Path) -> Result<()> {
    if is_sensitive_path(path) {
        anyhow::bail!(
            "Security error: attempted to access potentially sensitive file: {}",
            path.display()
        );
    }
    Ok(())
}

/// Validate that a path is safe to write to (strict - must be in project)
fn validate_path(path: &Path) -> Result<()> {
    let current_dir = std::env::current_dir()?;

    // Resolve the path to handle .. and .
    // For non-existent paths, walk up to find the first existing ancestor
    let canonical = if path.exists() {
        path.canonicalize()?
    } else {
        // Walk up the path to find the first existing ancestor
        let mut ancestors_to_join = Vec::new();
        let mut current = path;

        while let Some(parent) = current.parent() {
            if let Some(name) = current.file_name() {
                ancestors_to_join.push(name.to_os_string());
            }
            if parent.as_os_str().is_empty() {
                // Reached the root of a relative path
                break;
            }
            if parent.exists() {
                // Found existing ancestor - canonicalize it and join the rest
                let mut result = parent.canonicalize()?;
                for component in ancestors_to_join.iter().rev() {
                    result = result.join(component);
                }
                return validate_canonical_path(&result, &current_dir);
            }
            current = parent;
        }

        // No existing ancestor found - use current_dir as base
        let mut result = current_dir.canonicalize().unwrap_or_else(|_| current_dir.clone());
        for component in ancestors_to_join.iter().rev() {
            result = result.join(component);
        }
        result
    };

    validate_canonical_path(&canonical, &current_dir)
}

/// Helper to validate a canonical path against the current directory
fn validate_canonical_path(canonical: &Path, current_dir: &Path) -> Result<()> {
    // Canonicalize current_dir for consistent comparison (Windows adds \\?\ prefix)
    let current_dir_canonical = current_dir.canonicalize().unwrap_or_else(|_| current_dir.to_path_buf());

    // Ensure the path is within the current directory
    if !canonical.starts_with(&current_dir_canonical) {
        anyhow::bail!(
            "Security error: attempted to access path outside of project directory: {}",
            canonical.display()
        );
    }

    // Check for sensitive files using shared path-component matcher
    if is_sensitive_path(canonical) {
        anyhow::bail!(
            "Security error: attempted to access potentially sensitive file: {}",
            canonical.display()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Phase 2 Test Suite: Filesystem Operations - 10 comprehensive tests

    #[test]
    fn test_read_file_valid() {
        // Test reading an existing file in the current project
        let result = read_file("Cargo.toml");
        assert!(
            result.is_ok(),
            "Should successfully read valid file from project"
        );
        let content = result.unwrap();
        assert!(
            content.contains("[package]") || !content.is_empty(),
            "Content should be reasonable"
        );
    }

    #[test]
    fn test_read_file_not_found() {
        let result = read_file("this_file_definitely_does_not_exist_12345.txt");
        assert!(result.is_err(), "Should fail to read non-existent file");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to read file"),
            "Error message should indicate read failure, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_write_and_read_roundtrip() {
        // Test actual write + read roundtrip in target/ (always within project)
        let test_path = "target/test_write_roundtrip.txt";
        let content = "Hello, Mermaid!";
        let result = write_file(test_path, content);
        assert!(result.is_ok(), "Write should succeed in target/");

        let read_back = read_file(test_path);
        assert!(read_back.is_ok(), "Should read back written file");
        assert_eq!(read_back.unwrap(), content);

        // Cleanup
        let _ = fs::remove_file(test_path);
        // Also clean up backup file
        let _ = fs::remove_file(format!("{}.backup", test_path));
    }

    #[test]
    fn test_delete_file_not_found() {
        let result = delete_file("this_definitely_should_not_exist_xyz123.txt");
        assert!(result.is_err(), "Should fail to delete non-existent file");
    }

    #[test]
    fn test_create_directory_simple() {
        let dir_path = "target/test_dir_creation";

        let result = create_directory(dir_path);
        assert!(result.is_ok(), "Should successfully create directory");

        let full_path = Path::new(dir_path);
        assert!(full_path.exists(), "Directory should exist");
        assert!(full_path.is_dir(), "Should be a directory");

        // Cleanup
        fs::remove_dir(dir_path).ok();
    }

    #[test]
    fn test_create_nested_directories_all() {
        let nested_path = "target/level1/level2/level3";

        let result = create_directory(nested_path);
        assert!(
            result.is_ok(),
            "Should create nested directories: {}",
            result.unwrap_err()
        );

        let full_path = Path::new(nested_path);
        assert!(full_path.exists(), "Nested directory should exist");
        assert!(full_path.is_dir(), "Should be a directory");

        // Cleanup
        fs::remove_dir_all("target/level1").ok();
    }

    #[test]
    fn test_path_validation_blocks_dotenv() {
        let result = read_file(".env");
        assert!(result.is_err(), "Should reject .env file access");
        let error = result.unwrap_err().to_string();
        assert!(error.contains("Security"), "Error should mention Security: {}", error);
    }

    #[test]
    fn test_path_validation_blocks_dotenv_variants() {
        // .env.local, .env.production should be blocked
        assert!(is_sensitive_path(Path::new("/project/.env.local")));
        assert!(is_sensitive_path(Path::new("/project/.env.production")));
        // But .environment.ts should NOT be blocked (path-component matching)
        assert!(!is_sensitive_path(Path::new("/project/src/.environment.ts")));
        assert!(!is_sensitive_path(Path::new("/project/src/environment.rs")));
    }

    #[test]
    fn test_path_validation_blocks_ssh_keys() {
        let result = read_file(".ssh/id_rsa");
        assert!(result.is_err(), "Should reject .ssh/id_rsa access");
        let error = result.unwrap_err().to_string();
        assert!(error.contains("Security"), "Error should mention Security: {}", error);
    }

    #[test]
    fn test_path_validation_blocks_aws_credentials() {
        let result = read_file(".aws/credentials");
        assert!(result.is_err(), "Should reject .aws/credentials access");
        let error = result.unwrap_err().to_string();
        assert!(error.contains("Security"), "Error should mention Security: {}", error);
    }

    #[test]
    fn test_path_validation_blocks_new_sensitive_patterns() {
        // Verify the expanded blocklist
        assert!(is_sensitive_path(Path::new("/home/user/credentials.json")));
        assert!(is_sensitive_path(Path::new("/project/secrets.yaml")));
        assert!(is_sensitive_path(Path::new("/project/server.pem")));
        assert!(is_sensitive_path(Path::new("/project/private.key")));
        assert!(is_sensitive_path(Path::new("/project/token.json")));
        assert!(is_sensitive_path(Path::new("/home/user/.gnupg/pubring.kbx")));
    }
}
