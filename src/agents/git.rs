use anyhow::{Context, Result};
use git2::{Repository, StatusOptions, DiffOptions};
use std::path::Path;

/// Get git diff for the current repository
pub fn get_diff(path: Option<&str>) -> Result<String> {
    let repo = Repository::open_from_env()
        .context("Failed to open git repository. Is this a git repo?")?;

    let mut diff_options = DiffOptions::new();

    if let Some(path) = path {
        diff_options.pathspec(path);
    }

    // Get diff between HEAD and working directory
    let head = repo.head()?.peel_to_tree()?;
    let diff = repo.diff_tree_to_workdir_with_index(Some(&head), Some(&mut diff_options))?;

    let mut output = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        output.push_str(std::str::from_utf8(line.content()).unwrap_or("<invalid UTF-8>"));
        true
    })?;

    if output.is_empty() {
        output = "No changes detected".to_string();
    }

    Ok(output)
}

/// Get git status for the current repository
pub fn get_status() -> Result<String> {
    let repo = Repository::open_from_env()
        .context("Failed to open git repository. Is this a git repo?")?;

    let mut status_options = StatusOptions::new();
    status_options.include_untracked(true);
    status_options.include_ignored(false);

    let statuses = repo.statuses(Some(&mut status_options))?;

    let mut output = String::new();
    output.push_str("Git Status:\n");
    output.push_str("-----------\n");

    let mut has_changes = false;

    for entry in statuses.iter() {
        let status = entry.status();
        let path = entry.path().unwrap_or("<unknown>");

        let status_str = if status.is_wt_new() {
            format!("  new file: {}", path)
        } else if status.is_wt_modified() {
            format!("  modified: {}", path)
        } else if status.is_wt_deleted() {
            format!("  deleted:  {}", path)
        } else if status.is_wt_renamed() {
            format!("  renamed:  {}", path)
        } else if status.is_index_new() {
            format!("  staged:   {}", path)
        } else if status.is_index_modified() {
            format!("  staged:   {}", path)
        } else if status.is_index_deleted() {
            format!("  staged:   {}", path)
        } else if status.is_conflicted() {
            format!("  conflict: {}", path)
        } else {
            continue;
        };

        output.push_str(&status_str);
        output.push('\n');
        has_changes = true;
    }

    if !has_changes {
        output.push_str("  (working directory clean)\n");
    }

    // Add branch info
    if let Ok(head) = repo.head() {
        if let Some(name) = head.shorthand() {
            output.push_str(&format!("\nOn branch: {}\n", name));
        }
    }

    Ok(output)
}

/// Commit changes with a message
pub fn commit(message: &str, files: &[String]) -> Result<()> {
    let repo = Repository::open_from_env()
        .context("Failed to open git repository. Is this a git repo?")?;

    let mut index = repo.index()?;

    // Add specified files to the index
    if !files.is_empty() {
        for file in files {
            index.add_path(Path::new(file))
                .with_context(|| format!("Failed to add file to index: {}", file))?;
        }
    } else {
        // Add all modified files
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
    }

    index.write()?;

    // Get the tree for the index
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    // Get parent commit
    let parent_commit = match repo.head() {
        Ok(head) => Some(head.peel_to_commit()?),
        Err(_) => None, // First commit
    };

    // Get signature
    let signature = repo.signature()
        .or_else(|_| git2::Signature::now("Mermaid AI", "mermaid@ai.local"))?;

    // Create the commit
    if let Some(parent) = parent_commit.as_ref() {
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[parent],
        )?;
    } else {
        // Initial commit
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[],
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs;

    #[test]
    fn test_git_operations() {
        let temp_dir = TempDir::new().unwrap();

        // Initialize a git repo
        let repo = Repository::init(temp_dir.path()).unwrap();

        // Create a test file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "Hello, Git!").unwrap();

        // Set working directory to temp dir
        std::env::set_current_dir(temp_dir.path()).unwrap();

        // Test status
        let status = get_status().unwrap();
        assert!(status.contains("new file"));

        // Test commit
        commit("Initial commit", &[]).unwrap();

        // Test status after commit
        let status = get_status().unwrap();
        assert!(status.contains("working directory clean"));

        // Test current branch
        let branch = current_branch().unwrap();
        assert!(branch == "main" || branch == "master");
    }
}