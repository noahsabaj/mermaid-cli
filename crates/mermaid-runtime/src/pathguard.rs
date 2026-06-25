//! Lexical path containment for the runtime crate.
//!
//! Resolves a caller- or manifest-supplied path against a trusted root,
//! collapsing `.`/`..` *without* touching the filesystem, and rejects anything
//! that escapes the root. Used by approval replay ([`crate::approval`]) and
//! checkpoint restore ([`crate::checkpoint`]) to confine writes/deletes whose
//! paths come from stored (and therefore potentially tampered) state.
//!
//! This is the runtime-crate sibling of the main crate's `path_safety` helper;
//! `mermaid-runtime` is the lower crate and cannot depend on it.

use std::path::{Component, Path, PathBuf};

use anyhow::Result;

/// Resolve `raw` against `root` and confirm it stays inside `root`.
///
/// `raw` may be relative (joined onto `root`) or absolute (taken as-is). Both
/// the candidate and the root are normalized lexically — `.` is dropped and
/// `..` pops the previous component — so traversal is resolved without symlink
/// expansion or filesystem access (the target may not exist yet, as on a fresh
/// checkout). Returns the normalized in-root path, or `Err` if it escapes.
pub(crate) fn contain_within(root: &Path, raw: &str) -> Result<PathBuf> {
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    let lexical = normalize_lexical(&candidate);
    let root = normalize_lexical(root);
    anyhow::ensure!(
        lexical.starts_with(&root),
        "path escapes the project root: {raw}"
    );
    Ok(lexical)
}

/// Collapse `.` and `..` components lexically (no filesystem access, no symlink
/// resolution).
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {},
            Component::ParentDir => {
                out.pop();
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_escape() {
        let root = std::env::temp_dir().join("mermaid_pathguard_root");
        assert!(contain_within(&root, "../escape").is_err());
    }

    #[test]
    fn rejects_absolute_outside_root() {
        let root = std::env::temp_dir().join("mermaid_pathguard_root2");
        #[cfg(unix)]
        assert!(contain_within(&root, "/etc/passwd").is_err());
        #[cfg(windows)]
        assert!(contain_within(&root, "C:\\Windows\\System32\\drivers\\etc\\hosts").is_err());
    }

    #[test]
    fn accepts_in_root_relative() {
        let root = std::env::temp_dir().join("mermaid_pathguard_root3");
        let p = contain_within(&root, "a/b.txt").unwrap();
        assert!(p.starts_with(normalize_lexical(&root)));
        assert!(p.ends_with("b.txt"));
    }

    #[test]
    fn collapses_interior_parent_within_root() {
        let root = std::env::temp_dir().join("mermaid_pathguard_root4");
        // `a/../b.txt` stays inside the root.
        let p = contain_within(&root, "a/../b.txt").unwrap();
        assert!(p.ends_with("b.txt"));
        assert!(p.starts_with(normalize_lexical(&root)));
    }
}
