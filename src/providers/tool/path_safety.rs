//! Shared path-containment helpers.
//!
//! `resolve_path_safe` confines a caller-supplied path to the project root and
//! is used by the filesystem tools. `resolve_path_within` reports containment
//! *without* rejecting, so `execute_command` can feed an out-of-project
//! `working_dir` into the policy gate (escalating it from auto-allow to
//! Ask/Classify) instead of running it unconstrained.

use std::path::{Path, PathBuf};

/// Resolve a caller-supplied path against `workdir`, enforcing the
/// "absolute paths outside the project are blocked" contract advertised in the
/// filesystem tool schemas.
///
/// Rules (F10):
/// - Relative paths → joined onto `workdir`; `..` components are resolved then
///   containment-checked (not rejected outright).
/// - Existing targets → canonicalized through symlinks.
/// - Non-existent targets (`write_file` / `create_directory`) → the nearest
///   existing ancestor is canonicalized (resolving any symlinked parent) and
///   the remaining components re-attached. Escapes → `Err`.
pub(crate) fn resolve_path_safe(workdir: &Path, raw: &str) -> Result<PathBuf, String> {
    let (resolved, within) = resolve_path_within(workdir, raw)?;
    if within {
        Ok(resolved)
    } else {
        Err(format!(
            "path '{}' is outside the project directory '{}'",
            raw,
            workdir.display()
        ))
    }
}

/// Like [`resolve_path_safe`], but returns `(resolved_path, is_within_workdir)`
/// instead of erroring on escape. `Err` only when the path can't be resolved at
/// all (workdir not canonicalizable, or no existing ancestor). Lets the caller
/// decide what to do with an out-of-project path rather than rejecting it.
pub(crate) fn resolve_path_within(workdir: &Path, raw: &str) -> Result<(PathBuf, bool), String> {
    let p = PathBuf::from(raw);
    let candidate = if p.is_absolute() { p } else { workdir.join(&p) };

    // Canonical project root. If the workdir itself can't canonicalize we
    // cannot make a sound containment decision — fail closed rather than
    // falling back to a weaker lexical check.
    let root = std::fs::canonicalize(workdir).map_err(|e| {
        format!(
            "cannot canonicalize project dir '{}': {}",
            workdir.display(),
            e
        )
    })?;

    // Resolve the target THROUGH symlinks. For an existing target `canonicalize`
    // gives the real location; for a not-yet-existing target we canonicalize the
    // nearest existing ancestor (resolving any symlinked parent) and re-attach
    // the remaining components. This closes both the symlink-follow/TOCTOU gap
    // and the symlinked-parent-on-create gap.
    let resolved = match std::fs::canonicalize(&candidate) {
        Ok(real) => real,
        Err(_) => resolve_via_existing_ancestor(&candidate)?,
    };

    let within = resolved.starts_with(&root);
    Ok((resolved, within))
}

/// Compute the workdir-relative, lexically-normalized form of `raw`, erroring if
/// it escapes `workdir`. The result is the `rel` argument for the confined
/// fd-based helpers ([`mermaid_runtime::open_beneath`] et al.): those resolve it
/// beneath a directory fd for `workdir` under `RESOLVE_BENEATH`, so the bytes
/// hit the same inode the kernel confined — closing the check-then-write TOCTOU
/// that a by-path `std::fs` call leaves open. `resolve_path_safe` remains the
/// canonical containment gate; this is the lexical companion that names the path
/// relative to the root.
pub(crate) fn relative_within(workdir: &Path, raw: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(raw);
    let candidate = if p.is_absolute() { p } else { workdir.join(&p) };
    let normalized = lexical_normalize(&candidate);
    let root = lexical_normalize(workdir);
    match normalized.strip_prefix(&root) {
        Ok(rel) => Ok(rel.to_path_buf()),
        Err(_) => Err(format!(
            "path '{}' is outside the project directory '{}'",
            raw,
            workdir.display()
        )),
    }
}

/// Resolve a not-yet-existing target by canonicalizing its nearest existing
/// ancestor (resolving any symlinked parent directory) and re-joining the
/// remaining path components lexically. Rejects paths with no canonicalizable
/// ancestor.
fn resolve_via_existing_ancestor(candidate: &Path) -> Result<PathBuf, String> {
    let normalized = lexical_normalize(candidate);
    let mut ancestor = normalized.as_path();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(ancestor) {
            let mut out = real;
            for comp in tail.iter().rev() {
                out.push(comp);
            }
            return Ok(out);
        }
        let Some(file) = ancestor.file_name() else {
            return Err(format!(
                "cannot resolve path '{}': no existing ancestor directory",
                candidate.display()
            ));
        };
        tail.push(file.to_os_string());
        match ancestor.parent() {
            Some(parent) => ancestor = parent,
            None => {
                return Err(format!(
                    "cannot resolve path '{}': no existing ancestor directory",
                    candidate.display()
                ));
            },
        }
    }
}

/// Normalize a path lexically (no filesystem access), collapsing `.` and
/// resolving `..` without symlink expansion. Used when a target doesn't exist
/// yet so `canonicalize` would fail but we still want to reject `..`-escapes.
fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                // Drop the last segment if one exists; otherwise keep the `..`
                // (can only happen on relative paths, which the caller has
                // already joined against workdir).
                if !out.pop() {
                    out.push("..");
                }
            },
            Component::CurDir => {},
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_within_reports_containment() {
        let root = std::env::temp_dir().join(format!("mermaid_pw_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();

        // In-project subdir → within.
        let (p, within) = resolve_path_within(&root, "sub").unwrap();
        assert!(within, "subdir should be within the project");
        assert!(p.ends_with("sub"));

        // Parent escape → resolves but not contained.
        let (_p, within) = resolve_path_within(&root, "..").unwrap();
        assert!(!within, "parent dir should be outside the project");

        // resolve_path_safe rejects the escape, accepts the subdir.
        assert!(resolve_path_safe(&root, "..").is_err());
        assert!(resolve_path_safe(&root, "sub").is_ok());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn relative_within_names_path_relative_to_root() {
        let root = std::env::temp_dir().join(format!("mermaid_rel_{}", std::process::id()));

        // In-project paths come back relative to the root.
        assert_eq!(
            relative_within(&root, "sub/file.txt").unwrap(),
            PathBuf::from("sub/file.txt")
        );
        // Interior `..` that stays inside is collapsed.
        assert_eq!(
            relative_within(&root, "a/../b.txt").unwrap(),
            PathBuf::from("b.txt")
        );
        // An absolute path that happens to be inside the root is re-relativized.
        assert_eq!(
            relative_within(&root, root.join("c.txt").to_str().unwrap()).unwrap(),
            PathBuf::from("c.txt")
        );
        // Escapes are rejected.
        assert!(relative_within(&root, "../escape").is_err());
        #[cfg(unix)]
        assert!(relative_within(&root, "/etc/passwd").is_err());
    }
}
