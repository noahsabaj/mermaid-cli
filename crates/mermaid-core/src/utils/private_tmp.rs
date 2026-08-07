//! Per-user private scratch directory for short-lived sensitive files (Cause 7).
//!
//! Screenshots and clipboard images used to be written to fixed, predictable
//! names directly in the shared system temp dir (`/tmp/mermaid-screenshot-N.png`,
//! `/tmp/mermaid-clipboard-paste.png`). On a multi-user host another local user
//! could read those frames, or pre-create / symlink the path to redirect the
//! write. Routing them through a `0700` directory under the app data dir closes
//! that window — only the owning user can traverse it (#11, #33).

use std::path::PathBuf;

/// Return (creating if needed) a `0700` per-user scratch directory under the app
/// data dir. Callers write transient sensitive files here instead of the shared
/// system temp dir.
pub fn private_temp_dir() -> std::io::Result<PathBuf> {
    // Straight to the runtime crate, not through `crate::runtime` — that module
    // is a re-export facade, and routing through it made `utils` look like it
    // depended on a sibling module when the real dependency is the external
    // crate. Naming the crate keeps `utils` a leaf.
    let base = mermaid_runtime::data_dir()
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .join("tmp");
    std::fs::create_dir_all(&base)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort tighten to owner-only every time — cheap, and self-heals a
        // dir that was somehow created with looser bits.
        let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
    }
    Ok(base)
}
