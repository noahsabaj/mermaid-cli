//! Shared path-containment helpers.
//!
//! `resolve_in_roots` confines a caller-supplied path to the project root or
//! the per-session scratchpad and reports which root it landed in; it is the
//! containment gate for the filesystem tools. `resolve_path_within` reports
//! containment *without* rejecting, so `execute_command` can feed an
//! out-of-project `working_dir` into the policy gate (escalating it from
//! auto-allow to Ask/Classify) instead of running it unconstrained.

use std::path::{Path, PathBuf};

/// Resolve a caller-supplied path against `workdir` and report
/// `(resolved_path, is_within_workdir)` instead of erroring on escape. `Err`
/// only when the path can't be resolved at all (workdir not canonicalizable,
/// or no existing ancestor). Lets the caller decide what to do with an
/// out-of-project path rather than rejecting it.
///
/// Resolution rules (F10):
/// - Relative paths → joined onto `workdir`; `..` components are resolved then
///   containment-checked (not rejected outright).
/// - Existing targets → canonicalized through symlinks.
/// - Non-existent targets (`write_file` / `create_directory`) → the nearest
///   existing ancestor is canonicalized (resolving any symlinked parent) and
///   the remaining components re-attached.
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

/// The set of roots a filesystem tool may operate within: the project workdir
/// plus, when the session has one materialized, the per-session scratchpad
/// (`ExecContext::scratchpad`). Borrowed views so callers can build one per
/// call without cloning either path.
pub(crate) struct AllowedRoots<'a> {
    pub workdir: &'a Path,
    pub scratchpad: Option<&'a Path>,
}

impl<'a> AllowedRoots<'a> {
    pub(crate) fn new(workdir: &'a Path, scratchpad: Option<&'a Path>) -> Self {
        Self {
            workdir,
            scratchpad,
        }
    }
}

/// Which allowed root a resolved path landed in -- or neither of them.
///
/// Three states rather than a bool, and every filesystem tool matches on it:
/// a tool cannot consume a resolution without deciding what an out-of-project
/// path means for it. The resolver used to answer a three-way question with
/// `Ok`/`Err`, and once `Err` stopped being the answer for external paths,
/// `read_file` read any path on disk without a gate because nothing forced it
/// to look at the third case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathContainment {
    /// Inside the project workdir.
    Project,
    /// Inside the per-session scratchpad: session-private and ephemeral.
    Scratchpad,
    /// Anywhere else on the filesystem.
    External,
}

/// A caller-supplied path resolved into whichever allowed root contains it.
#[derive(Debug)]
pub(crate) struct ResolvedInRoot {
    /// Canonicalized absolute target (through symlinks; not-yet-existing
    /// targets resolve via the nearest existing ancestor).
    pub abs: PathBuf,
    /// Lexically-normalized path relative to `root` — the `rel` argument for
    /// the confined `*_beneath` helpers.
    pub rel: PathBuf,
    /// The root the path landed in, owned so blocking jobs can take it.
    pub root: PathBuf,
    /// Where the path landed. Callers match on this; see [`PathContainment`].
    pub containment: PathContainment,
}

/// The canonical path resolver for the filesystem tools: resolve `raw`
/// into the project workdir, the session scratchpad, or an external filesystem
/// path. Relative paths resolve relative to `workdir`; absolute paths resolve
/// to their target location.
pub(crate) fn resolve_in_roots(
    roots: &AllowedRoots<'_>,
    raw: &str,
) -> Result<ResolvedInRoot, String> {
    let project = resolve_path_within(roots.workdir, raw);
    if let Ok((abs, true)) = &project {
        return Ok(ResolvedInRoot {
            abs: abs.clone(),
            rel: relative_within(roots.workdir, raw)?,
            root: roots.workdir.to_path_buf(),
            containment: PathContainment::Project,
        });
    }
    if let Some(scratch) = roots.scratchpad
        && Path::new(raw).is_absolute()
        && relative_within(scratch, raw).is_ok()
    {
        let (abs, within) = resolve_path_within(scratch, raw)?;
        if within {
            return Ok(ResolvedInRoot {
                abs,
                rel: relative_within(scratch, raw)?,
                root: scratch.to_path_buf(),
                containment: PathContainment::Scratchpad,
            });
        }
        // A symlink planted inside the scratchpad escapes the scratch root:
        // refuse rather than allowing an ungated/redirected write.
        return Err(format!(
            "path '{raw}' is inside scratchpad but symlinks outside it"
        ));
    }
    let p = PathBuf::from(raw);
    let candidate = if p.is_absolute() {
        p
    } else {
        roots.workdir.join(&p)
    };
    let (root, rel, abs) = resolve_external_target(&candidate)?;
    Ok(ResolvedInRoot {
        abs,
        rel,
        root,
        containment: PathContainment::External,
    })
}

/// Compute the workdir-relative, lexically-normalized form of `raw`, erroring if
/// it escapes `workdir`. The result is the `rel` argument for the confined
/// fd-based helpers ([`mermaid_runtime::open_beneath`] et al.): those resolve it
/// beneath a directory fd for `workdir` under `RESOLVE_BENEATH`, so the bytes
/// hit the same inode the kernel confined — closing the check-then-write TOCTOU
/// that a by-path `std::fs` call leaves open. `resolve_in_roots` remains the
/// canonical containment gate; this is the lexical companion that names the path
/// relative to the root.
pub(crate) fn relative_within(workdir: &Path, raw: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(raw);
    let candidate = if p.is_absolute() { p } else { workdir.join(&p) };
    let normalized = mermaid_runtime::normalize_lexical(&candidate);
    let root = mermaid_runtime::normalize_lexical(workdir);
    match normalized.strip_prefix(&root) {
        Ok(rel) => Ok(rel.to_path_buf()),
        Err(_) => Err(format!(
            "path '{}' is outside the project directory '{}'",
            raw,
            workdir.display()
        )),
    }
}

/// Resolve an external target candidate into `(root, rel, abs)`.
/// `root` is the nearest existing directory ancestor (or parent directory for an existing file),
/// and `rel` is the path relative to that `root`.
fn resolve_external_target(candidate: &Path) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let normalized = mermaid_runtime::normalize_lexical(candidate);
    let mut ancestor = normalized.as_path();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let (real_root, tail) = loop {
        if let Ok(real) = std::fs::canonicalize(ancestor) {
            break (real, tail);
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
    };
    let mut abs = real_root.clone();
    let mut rel = PathBuf::new();
    for comp in tail.iter().rev() {
        abs.push(comp);
        rel.push(comp);
    }
    let (root, rel) = if rel.as_os_str().is_empty() {
        if let Some(parent) = real_root.parent() {
            let file = real_root
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            (parent.to_path_buf(), file)
        } else {
            (real_root, PathBuf::from("."))
        }
    } else {
        (real_root, rel)
    };
    Ok((root, rel, abs))
}

/// Resolve a not-yet-existing target by canonicalizing its nearest existing
/// ancestor (resolving any symlinked parent directory) and re-joining the
/// remaining path components lexically. Rejects paths with no canonicalizable
/// ancestor.
fn resolve_via_existing_ancestor(candidate: &Path) -> Result<PathBuf, String> {
    resolve_external_target(candidate).map(|(_, _, abs)| abs)
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

        // resolve_in_roots resolves both in-project subdir and parent dir.
        let roots = AllowedRoots::new(&root, None);
        assert!(resolve_in_roots(&roots, "..").is_ok());
        assert!(resolve_in_roots(&roots, "sub").is_ok());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_in_roots_dual_root_table() {
        let base = std::env::temp_dir().join(format!("mermaid_rir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        let scratch = base.join("scratch");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let roots = AllowedRoots::new(&project, Some(&scratch));

        // Relative paths land in the project root.
        let r = resolve_in_roots(&roots, "sub/file.txt").unwrap();
        assert_eq!(r.containment, PathContainment::Project);
        assert_eq!(r.rel, PathBuf::from("sub/file.txt"));
        assert_eq!(r.root, project);

        // An absolute path inside the scratchpad lands there, relative to it.
        let raw = scratch.join("nested/notes.txt");
        let r = resolve_in_roots(&roots, raw.to_str().unwrap()).unwrap();
        assert_eq!(r.containment, PathContainment::Scratchpad);
        assert_eq!(r.rel, PathBuf::from("nested/notes.txt"));
        assert_eq!(r.root, scratch);

        // Outside both roots resolves, and says so: the caller decides what an
        // external path means for it.
        let outside = base.join("elsewhere/file.txt");
        let r = resolve_in_roots(&roots, outside.to_str().unwrap()).unwrap();
        assert_eq!(r.containment, PathContainment::External);
        assert!(r.abs.ends_with(Path::new("elsewhere").join("file.txt")));

        // A `..` escape from the project must not fall into the scratchpad —
        // relative paths never match the scratch root.
        let r = resolve_in_roots(&roots, "../scratch/sneaky.txt").unwrap();
        assert_eq!(r.containment, PathContainment::External);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_in_roots_rejects_a_symlink_inside_the_scratchpad_that_escapes() {
        // Adversarial: a symlink planted INSIDE the scratchpad that points at
        // the project (or anywhere outside the scratch root) must not let an
        // absolute scratch-looking path retarget out of it. The containment
        // gate canonicalizes THROUGH the symlink, so the resolved target no
        // longer sits beneath the scratch root and the write is refused —
        // the scratchpad carve-out can never be turned into a project/$HOME
        // write.
        let base = std::env::temp_dir().join(format!("mermaid_sym_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        let scratch = base.join("scratch");
        let outside = base.join("outside");
        std::fs::create_dir_all(project.join("secrets")).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir_all(outside.join("home")).unwrap();
        let roots = AllowedRoots::new(&project, Some(&scratch));

        // Symlink inside the scratchpad aimed at the project directory.
        std::os::unix::fs::symlink(&project, scratch.join("to_project")).unwrap();
        // Symlink inside the scratchpad aimed at an unrelated outside dir.
        std::os::unix::fs::symlink(&outside, scratch.join("to_outside")).unwrap();

        // Writing "into" the scratchpad but through the escaping symlink must
        // be rejected (never resolves as `Scratchpad`, never silently rewrites
        // the project).
        for tail in ["to_project/secrets/leak.txt", "to_outside/home/leak.txt"] {
            let raw = scratch.join(tail);
            let res = resolve_in_roots(&roots, raw.to_str().unwrap());
            assert!(
                res.is_err(),
                "symlink escape {tail:?} must be rejected, got {res:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&base);
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
