//! Config keys Mermaid once read and no longer does, and the cleanup that
//! deletes them from the user's file.
//!
//! An unknown key is normally a spelling error, so the loader's warning says
//! "check for a typo". A key on this list is not one: it is a section that an
//! older Mermaid read (and that `mermaid init` used to write out in full), so
//! the warning instead names the release that dropped it and how to get rid of
//! it. `mermaid clean-config`, and the y/N offer at TUI startup, delete exactly
//! these keys from the user file — a backup first, comments, order and every
//! other key kept — and never touch a key that is merely unrecognized.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item};

/// A dotted config path and the release that stopped reading it. A path
/// covers everything beneath it: `plan` also covers `[plan.permissions]`.
pub(crate) struct RemovedKey {
    pub(crate) path: &'static str,
    pub(crate) version: &'static str,
}

/// Every config key a released Mermaid read and the current one ignores,
/// found by walking the `Config` schema at each release tag. Renamed keys
/// that migrate on load (`model_profiles`) are not here — they still work.
pub(crate) const REMOVED_KEYS: &[RemovedKey] = &[
    RemovedKey {
        path: "plan",
        version: "0.28.0",
    },
    RemovedKey {
        path: "compaction.tool_output_max_chars",
        version: "0.28.0",
    },
    RemovedKey {
        path: "computer_use",
        version: "0.26.0",
    },
    RemovedKey {
        path: "non_interactive",
        version: "0.26.0",
    },
    RemovedKey {
        path: "ollama.cloud_api_key",
        version: "0.12.0",
    },
    RemovedKey {
        path: "anthropic",
        version: "0.5.0",
    },
    RemovedKey {
        path: "openai",
        version: "0.5.0",
    },
    RemovedKey {
        path: "mode",
        version: "0.5.0",
    },
    RemovedKey {
        path: "behavior",
        version: "0.5.0",
    },
    RemovedKey {
        path: "default_model.system_prompt",
        version: "0.5.0",
    },
    RemovedKey {
        path: "context",
        version: "0.4.1",
    },
    RemovedKey {
        path: "ui.show_line_numbers",
        version: "0.4.1",
    },
    RemovedKey {
        path: "ui.show_sidebar",
        version: "0.4.1",
    },
    RemovedKey {
        path: "ui.syntax_theme",
        version: "0.4.1",
    },
    RemovedKey {
        path: "litellm",
        version: "0.3.0",
    },
];

/// The removed key an ignored config path falls under, if any. `path` is a
/// dotted path as `serde_ignored` reports it, relative to one layer's table.
pub(crate) fn removed_key_for(path: &str) -> Option<&'static RemovedKey> {
    REMOVED_KEYS.iter().find(|key| {
        path == key.path
            || path
                .strip_prefix(key.path)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// One removed key found in the user file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundKey {
    /// Where it sits in the file: `plan`, or `profiles.ci.plan` inside a
    /// `--profile` overlay.
    pub path: String,
    /// The release that stopped reading it.
    pub version: &'static str,
}

/// What a cleanup did.
#[derive(Debug)]
pub struct Cleanup {
    /// The config file that was checked.
    pub config_path: PathBuf,
    /// The keys deleted (empty: nothing to do, and nothing was written).
    pub removed: Vec<FoundKey>,
    /// Where the untouched original was copied, when anything was deleted.
    pub backup: Option<PathBuf>,
}

/// Delete every removed key from `doc` — at the top level and inside each
/// `[profiles.<name>]` overlay — and report what went.
fn strip_removed_keys(doc: &mut DocumentMut) -> Vec<FoundKey> {
    let mut found = strip_from(doc.as_table_mut(), "");
    if let Some(profiles) = doc.get_mut("profiles").and_then(Item::as_table_like_mut) {
        for (name, overlay) in profiles.iter_mut() {
            if let Some(overlay) = overlay.as_table_like_mut() {
                let prefix = format!("profiles.{}.", name.get());
                found.extend(strip_from(overlay, &prefix));
            }
        }
    }
    found
}

fn strip_from(table: &mut dyn toml_edit::TableLike, prefix: &str) -> Vec<FoundKey> {
    let mut found = Vec::new();
    for key in REMOVED_KEYS {
        let segments: Vec<&str> = key.path.split('.').collect();
        if remove_path(table, &segments) {
            found.push(FoundKey {
                path: format!("{prefix}{}", key.path),
                version: key.version,
            });
        }
    }
    found
}

/// Remove the item at `segments` below `table`. Parents are never created and
/// are left in place when they empty out — an empty `[ui]` is harmless.
fn remove_path(table: &mut dyn toml_edit::TableLike, segments: &[&str]) -> bool {
    match segments {
        [] => false,
        [last] => table.remove(last).is_some(),
        [first, rest @ ..] => table
            .get_mut(first)
            .and_then(Item::as_table_like_mut)
            .is_some_and(|child| remove_path(child, rest)),
    }
}

/// The removed keys in the user config file, without changing it. An absent
/// file has none.
///
/// # Errors
///
/// Reading the file, or a file that is not valid TOML.
pub fn find_removed_config_keys(config_path: &Path) -> Result<Vec<FoundKey>> {
    if !config_path.exists() {
        return Ok(Vec::new());
    }
    let mut doc = read_document(config_path)?;
    Ok(strip_removed_keys(&mut doc))
}

fn read_document(config_path: &Path) -> Result<DocumentMut> {
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;
    raw.parse::<DocumentMut>()
        .with_context(|| format!("Failed to parse {}", config_path.display()))
}

/// Delete the removed keys from the user config file at `config_path`.
///
/// The original is first copied, byte for byte, to the first free name among
/// `config.toml.bak`, `config.toml.bak.2`, … (an earlier backup is never
/// overwritten), then the edited file replaces it atomically. Comments, key
/// order and every key not on [`REMOVED_KEYS`] stay exactly as they were. A
/// file with nothing to remove is not written at all.
///
/// # Errors
///
/// Reading or parsing the file, writing the backup, or writing the result.
pub fn clean_removed_config_keys_at(config_path: &Path) -> Result<Cleanup> {
    super::config::with_persist_lock(|| {
        let mut cleanup = Cleanup {
            config_path: config_path.to_path_buf(),
            removed: Vec::new(),
            backup: None,
        };
        if !config_path.exists() {
            return Ok(cleanup);
        }
        let original = std::fs::read(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        let mut doc = read_document(config_path)?;
        cleanup.removed = strip_removed_keys(&mut doc);
        if cleanup.removed.is_empty() {
            return Ok(cleanup);
        }
        let backup = free_backup_path(config_path);
        super::config::write_config_bytes(&backup, &original)
            .with_context(|| format!("Failed to write backup {}", backup.display()))?;
        cleanup.backup = Some(backup);
        super::config::write_config_bytes(config_path, doc.to_string().as_bytes())?;
        Ok(cleanup)
    })
}

/// [`clean_removed_config_keys_at`] on the user config file.
///
/// # Errors
///
/// Resolving the config path, plus [`clean_removed_config_keys_at`]'s.
pub fn clean_removed_config_keys() -> Result<Cleanup> {
    clean_removed_config_keys_at(&super::config::get_config_path()?)
}

fn free_backup_path(config_path: &Path) -> PathBuf {
    let mut name = config_path.as_os_str().to_owned();
    name.push(".bak");
    let first = PathBuf::from(&name);
    if !first.exists() {
        return first;
    }
    (2u32..)
        .map(|n| {
            let mut numbered = name.clone();
            numbered.push(format!(".{n}"));
            PathBuf::from(numbered)
        })
        .find(|candidate| !candidate.exists())
        .unwrap_or(first)
}

/// The lines `mermaid clean-config` prints for a finished cleanup.
#[must_use]
pub fn cleanup_report(cleanup: &Cleanup) -> String {
    let path = cleanup.config_path.display();
    if cleanup.removed.is_empty() {
        return format!("No removed config keys in {path}. Nothing to do.");
    }
    let mut report = format!("Deleted from {path}:\n{}", key_list(&cleanup.removed));
    if let Some(backup) = &cleanup.backup {
        report.push_str(&format!("\nBackup of the old file: {}", backup.display()));
    }
    report
}

fn key_list(keys: &[FoundKey]) -> String {
    let width = keys.iter().map(|k| k.path.len()).max().unwrap_or(0);
    keys.iter()
        .map(|k| format!("  {:width$}  (removed in {})", k.path, k.version))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Before the TUI starts: when the user config holds removed keys and both
/// stdin and stderr are a terminal, list them and ask `[y/N]`. Yes runs the
/// cleanup; anything else changes nothing (the load's warnings then repeat
/// the `mermaid clean-config` hint). Headless runs, pipes and scripts never
/// reach this. Never fails: a problem is printed and startup continues.
pub fn offer_removed_key_cleanup() {
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return;
    }
    let Ok(config_path) = super::config::get_config_path() else {
        return;
    };
    // A file that does not parse is reported by the load that follows.
    let Ok(found) = find_removed_config_keys(&config_path) else {
        return;
    };
    if found.is_empty() {
        return;
    }
    let mut stderr = std::io::stderr();
    let _ = write!(
        stderr,
        "mermaid: {} has settings this version no longer uses:\n{}\n\
         Delete them now? The old file is kept as a backup. [y/N]: ",
        config_path.display(),
        key_list(&found)
    );
    let _ = stderr.flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return;
    }
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return;
    }
    match clean_removed_config_keys_at(&config_path) {
        Ok(cleanup) => {
            if let Some(backup) = &cleanup.backup {
                eprintln!(
                    "mermaid: deleted {} key(s); backup at {}",
                    cleanup.removed.len(),
                    backup.display()
                );
            }
        },
        Err(e) => eprintln!(
            "mermaid: could not clean {}: {}",
            config_path.display(),
            mermaid_model::utils::redact_secrets(&format!("{e:#}"))
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mermaid-removed-keys-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Every entry must really be ignored by today's schema — a key that came
    /// back must leave the list, or the cleanup would delete a live setting.
    #[test]
    fn every_removed_key_is_unknown_to_the_current_schema() {
        for key in REMOVED_KEYS {
            let mut table = toml::Table::new();
            let segments: Vec<&str> = key.path.split('.').collect();
            let (last, parents) = segments.split_last().unwrap();
            let mut cursor = &mut table;
            for parent in parents {
                cursor = cursor
                    .entry((*parent).to_string())
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                    .as_table_mut()
                    .unwrap();
            }
            cursor.insert((*last).to_string(), toml::Value::Boolean(true));
            let mut ignored = Vec::new();
            let _: mermaid_domain::Config =
                serde_ignored::deserialize(toml::Value::Table(table), |p| {
                    ignored.push(p.to_string());
                })
                .unwrap_or_else(|e| panic!("{} fails to load: {e}", key.path));
            assert_eq!(ignored, vec![key.path.to_string()], "{}", key.path);
        }
    }

    #[test]
    fn removed_key_for_matches_the_key_and_what_is_below_it_only() {
        assert_eq!(removed_key_for("plan").unwrap().version, "0.28.0");
        assert_eq!(removed_key_for("plan.permissions").unwrap().path, "plan");
        assert!(removed_key_for("planner").is_none());
        assert!(removed_key_for("ui.theme").is_none());
        assert!(removed_key_for("typo_key").is_none());
        assert_eq!(
            removed_key_for("ollama.cloud_api_key").unwrap().version,
            "0.12.0"
        );
    }

    const OLD_CONFIG: &str = r#"# my settings
last_used_model = "ollama/qwen3:8b"
mystery = 1 # a spelling error stays

[default_model]
provider = "ollama" # keep me
name = "qwen3:8b"

# plan mode settings
[plan]
auto_approve = false

[plan.permissions]
web = true

[ui]
theme = "dark"
syntax_theme = "monokai"

[computer_use]
auto_screenshot = true

[profiles.ci]
non_interactive = { output_format = "json" }

[profiles.ci.default_model]
name = "x"
"#;

    #[test]
    fn cleanup_deletes_only_removed_keys_and_keeps_the_rest_verbatim() {
        let dir = scratch("clean");
        let path = dir.join("config.toml");
        std::fs::write(&path, OLD_CONFIG).unwrap();

        let cleanup = clean_removed_config_keys_at(&path).unwrap();
        let paths: Vec<&str> = cleanup.removed.iter().map(|k| k.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "plan",
                "computer_use",
                "ui.syntax_theme",
                "profiles.ci.non_interactive"
            ]
        );
        let backup = cleanup.backup.clone().unwrap();
        assert_eq!(backup, dir.join("config.toml.bak"));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), OLD_CONFIG);

        let cleaned = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            cleaned,
            r#"# my settings
last_used_model = "ollama/qwen3:8b"
mystery = 1 # a spelling error stays

[default_model]
provider = "ollama" # keep me
name = "qwen3:8b"

[ui]
theme = "dark"

[profiles.ci]

[profiles.ci.default_model]
name = "x"
"#
        );

        // Running again finds nothing, writes nothing, and makes no backup.
        let again = clean_removed_config_keys_at(&path).unwrap();
        assert!(again.removed.is_empty());
        assert!(again.backup.is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), cleaned);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_earlier_backup_is_never_overwritten() {
        let dir = scratch("backup");
        let path = dir.join("config.toml");
        std::fs::write(dir.join("config.toml.bak"), "older").unwrap();
        std::fs::write(&path, "[plan]\nmodel = \"x\"\n").unwrap();

        let cleanup = clean_removed_config_keys_at(&path).unwrap();
        assert_eq!(cleanup.backup.unwrap(), dir.join("config.toml.bak.2"));
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml.bak")).unwrap(),
            "older"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_reports_without_writing_and_an_absent_file_is_clean() {
        let dir = scratch("find");
        let path = dir.join("config.toml");
        assert!(find_removed_config_keys(&path).unwrap().is_empty());
        std::fs::write(&path, OLD_CONFIG).unwrap();
        assert_eq!(find_removed_config_keys(&path).unwrap().len(), 4);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), OLD_CONFIG);
        assert!(!dir.join("config.toml.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_names_the_keys_and_the_backup() {
        let cleanup = Cleanup {
            config_path: PathBuf::from("/c/config.toml"),
            removed: vec![FoundKey {
                path: "plan".into(),
                version: "0.28.0",
            }],
            backup: Some(PathBuf::from("/c/config.toml.bak")),
        };
        let report = cleanup_report(&cleanup);
        assert!(report.contains("plan  (removed in 0.28.0)"), "{report}");
        assert!(report.contains("/c/config.toml.bak"), "{report}");
        let none = Cleanup {
            config_path: PathBuf::from("/c/config.toml"),
            removed: Vec::new(),
            backup: None,
        };
        assert!(cleanup_report(&none).contains("Nothing to do"));
    }
}
