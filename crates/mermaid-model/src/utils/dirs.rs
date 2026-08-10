//! Where mermaid's durable data lives.
//!
//! The one resolver for the app data directory, in the bottom crate so both
//! the store (`mermaid-runtime`) and the temp-dir policy here can share it
//! without a dependency cycle.

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

/// Environment override for [`data_dir`] -- point the store somewhere else
/// wholesale (test isolation), instead of the platform location.
///
/// The data-dir twin of `app::config::CONFIG_DIR_ENV`, and it exists for the
/// same reason: `ProjectDirs` resolves a Windows known folder that no
/// environment variable redirects, so tests spawning the real binary wrote
/// checkpoints and process rows into the developer's own store.
pub const DATA_DIR_ENV: &str = "MERMAID_DATA_DIR";

/// The app data dir: [`DATA_DIR_ENV`] when set, else the platform location, or
/// `~/.local/share/mermaid` when the platform has none.
///
/// # Errors
///
/// Only the fallback path failing, when neither `HOME` nor `USERPROFILE` is
/// set. The directory is not created or checked for here.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(proj_dirs) = ProjectDirs::from("", "", "mermaid") {
        return Ok(proj_dirs.data_dir().to_path_buf());
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("could not determine home directory")?;
    Ok(PathBuf::from(home).join(".local/share/mermaid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The matched pair for the override: set, it redirects `data_dir()`
    /// wholesale (the isolation the integration suites rely on so a test run
    /// can never write the developer's real runtime store -- Windows resolves
    /// the platform location through a known folder no HOME/XDG var moves);
    /// empty, it is "unset", so a stray `MERMAID_DATA_DIR=` in a shell profile
    /// cannot silently point the store at the current directory.
    #[test]
    fn data_dir_env_override_wins_and_empty_is_unset() {
        let sandbox = std::env::temp_dir().join("mermaid-data-dir-override-test");
        temp_env::with_var(
            DATA_DIR_ENV,
            Some(sandbox.to_str().expect("utf8 temp path")),
            || {
                assert_eq!(data_dir().expect("override resolves"), sandbox);
            },
        );
        temp_env::with_var(DATA_DIR_ENV, Some(""), || {
            let resolved = data_dir().expect("platform dir resolves");
            assert_ne!(resolved, PathBuf::from(""), "empty must not become cwd");
            assert!(resolved.is_absolute(), "got {}", resolved.display());
        });
    }
}
