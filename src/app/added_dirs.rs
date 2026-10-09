//! Added working roots: `--add-dir`, `[workspace] additional_dirs`, and
//! `/add-dir`.
//!
//! An added root carries the project root's trust, so every entry is
//! canonicalized here, once, before anything relies on it: the path tools
//! compare canonical paths, and a root that is itself a symlink would
//! otherwise be judged by where the link sits rather than where it points.
//! A missing or non-directory entry is an error, never skipped: a session
//! that silently lost a root the user asked for would gate edits they expected
//! to run.
//!
//! The project config cannot reach this: its allowlist omits `workspace`
//! (`project_config::PROJECT_ALLOWED_TOP_LEVEL`), so a cloned repository can
//! never add a root. Only the user file, a `--profile`, `-c` and `--add-dir`
//! can.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Resolve one added root. `~` and `~/…` expand to the home directory; any
/// other relative path resolves against `base`. The result is the canonical
/// directory.
///
/// # Errors
///
/// A path that does not exist, cannot be canonicalized, or is not a
/// directory, with a message naming the path.
pub fn resolve_added_dir(base: &Path, raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("an added directory needs a path".to_string());
    }
    let expanded = expand_home(raw);
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    let real = std::fs::canonicalize(&candidate)
        .map_err(|e| format!("cannot add directory '{raw}': {e}"))?;
    if !real.is_dir() {
        return Err(format!(
            "cannot add directory '{raw}': {} is not a directory",
            real.display()
        ));
    }
    Ok(real)
}

/// Fold the configured roots and the `--add-dir` flags into
/// `config.workspace.additional_dirs` as canonical, de-duplicated paths.
/// Config entries resolve relative to `project_dir`; flags are resolved
/// relative to the process's current directory, as the shell that typed them
/// would.
///
/// # Errors
///
/// The first entry that does not resolve (see [`resolve_added_dir`]).
pub fn apply_added_dirs(
    config: &mut mermaid_domain::Config,
    project_dir: &Path,
    flags: &[PathBuf],
) -> Result<()> {
    let mut resolved: Vec<PathBuf> = Vec::new();
    for raw in &config.workspace.additional_dirs {
        let dir = resolve_added_dir(project_dir, &raw.to_string_lossy())
            .map_err(|e| anyhow::anyhow!("[workspace] additional_dirs: {e}"))?;
        push_unique(&mut resolved, dir);
    }
    let shell_cwd = std::env::current_dir()?;
    for raw in flags {
        let dir = resolve_added_dir(&shell_cwd, &raw.to_string_lossy())
            .map_err(|e| anyhow::anyhow!("--add-dir: {e}"))?;
        push_unique(&mut resolved, dir);
    }
    config.workspace.additional_dirs = resolved;
    Ok(())
}

fn push_unique(dirs: &mut Vec<PathBuf>, dir: PathBuf) {
    if !dirs.contains(&dir) {
        dirs.push(dir);
    }
}

fn expand_home(raw: &str) -> PathBuf {
    let rest = match raw.strip_prefix('~') {
        Some("") => Some(""),
        Some(rest) if rest.starts_with('/') || rest.starts_with('\\') => Some(&rest[1..]),
        _ => None,
    };
    match (rest, directories::BaseDirs::new()) {
        (Some(rest), Some(dirs)) => dirs.home_dir().join(rest),
        _ => PathBuf::from(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("mermaid_added_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn resolves_relative_paths_against_the_base_and_canonicalizes() {
        let base = temp_base("rel");
        std::fs::create_dir_all(base.join("project")).unwrap();
        std::fs::create_dir_all(base.join("lib")).unwrap();
        let got = resolve_added_dir(&base.join("project"), "../lib").unwrap();
        assert_eq!(got, std::fs::canonicalize(base.join("lib")).unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_paths_and_files_are_errors_naming_the_path() {
        let base = temp_base("missing");
        let err = resolve_added_dir(&base, "nope").unwrap_err();
        assert!(err.contains("'nope'"), "{err}");
        std::fs::write(base.join("file.txt"), "x").unwrap();
        let err = resolve_added_dir(&base, "file.txt").unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        assert!(resolve_added_dir(&base, "  ").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn apply_canonicalizes_dedupes_and_fails_on_a_missing_entry() {
        let base = temp_base("apply");
        std::fs::create_dir_all(base.join("project")).unwrap();
        std::fs::create_dir_all(base.join("lib")).unwrap();
        let lib = std::fs::canonicalize(base.join("lib")).unwrap();

        let mut config = mermaid_domain::Config::default();
        config.workspace.additional_dirs = vec![PathBuf::from("../lib")];
        apply_added_dirs(
            &mut config,
            &base.join("project"),
            std::slice::from_ref(&lib),
        )
        .unwrap();
        assert_eq!(config.workspace.additional_dirs, vec![lib]);

        let mut config = mermaid_domain::Config::default();
        let err = apply_added_dirs(
            &mut config,
            &base.join("project"),
            &[base.join("does-not-exist")],
        )
        .unwrap_err();
        assert!(err.to_string().starts_with("--add-dir:"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
