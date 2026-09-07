//! Named output styles (`/output-style`): voice/format presets that modify the
//! system prompt.
//!
//! Discovery mirrors skills: built-ins (from `mermaid_domain::prompts`) plus
//! `output-styles/<name>.md` files under the user config dir (all projects)
//! and `<git-root>/.mermaid/output-styles/` (this project wins on a name
//! clash). Loading is startup-only plus the in-session `/output-style`
//! switch — styles are authored artifacts, not live state.

use std::path::{Path, PathBuf};

use mermaid_domain::{ActiveStyle, OutputStyleSummary};

/// File layout: `<dir>/output-styles/<name>.md`.
const STYLES_DIR: &str = "output-styles";

/// User-global styles directory, if the config dir resolves.
fn user_styles_dir() -> Option<PathBuf> {
    crate::app::get_config_dir()
        .ok()
        .map(|dir| dir.join(STYLES_DIR))
}

/// Project styles directory, if `cwd` sits inside a git repository.
fn project_styles_dir(cwd: &Path) -> Option<PathBuf> {
    crate::app::memory::find_git_root(cwd).map(|root| root.join(".mermaid").join(STYLES_DIR))
}

/// One discovered custom file, before project-shadowing is applied.
struct DiscoveredFile {
    name: String,
    path: PathBuf,
    source: &'static str,
}

fn discover_dir(root: Option<PathBuf>, source: &'static str) -> Vec<DiscoveredFile> {
    let Some(root) = root else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !mermaid_domain::prompts::is_valid_style_name(stem) {
            tracing::warn!(path = %path.display(), "output style filename is not a valid style name; skipping");
            continue;
        }
        out.push(DiscoveredFile {
            name: stem.to_string(),
            path,
            source,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Read one style file: frontmatter (`name`, `description`,
/// `keep-coding-instructions`, default keep) plus body. `None` when the file
/// is missing, unreadable, empty-bodied, or oversize — discovery is tolerant
/// so one broken file never hides the rest.
fn read_style_file(path: &Path) -> Option<(String, String, bool, String)> {
    let cap = mermaid_model::constants::MAX_OUTPUT_STYLE_BYTES;
    let (bytes, truncated) = match mermaid_model::utils::read_file_capped(path, cap) {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "output style: skipping unreadable file");
            return None;
        },
    };
    if truncated {
        tracing::warn!(path = %path.display(), "output style exceeds the {cap}-byte cap; skipping");
        return None;
    }
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let parsed = mermaid_domain::prompts::parse_style_file(&raw);
    if parsed.body.trim().is_empty() {
        tracing::warn!(path = %path.display(), "output style has no body; skipping");
        return None;
    }
    let name = parsed.name.unwrap_or_default();
    let description = parsed.description.unwrap_or_else(|| {
        parsed
            .body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("Custom output style")
            .to_string()
    });
    Some((
        name,
        description,
        parsed.keep_coding_instructions,
        parsed.body,
    ))
}

/// Every selectable style: `default`, the built-ins, then custom files with
/// project shadowing user on a name clash. Never errors — an unreadable root
/// simply contributes nothing.
#[must_use]
pub fn list_styles(cwd: &Path) -> Vec<OutputStyleSummary> {
    let mut entries = vec![OutputStyleSummary {
        name: mermaid_domain::prompts::DEFAULT_OUTPUT_STYLE.to_string(),
        description: "The stock system prompt".to_string(),
        custom: false,
        source: "builtin".to_string(),
    }];
    for builtin in mermaid_domain::prompts::BUILTIN_OUTPUT_STYLES {
        entries.push(OutputStyleSummary {
            name: builtin.name.to_string(),
            description: builtin.description.to_string(),
            custom: false,
            source: "builtin".to_string(),
        });
    }
    let mut seen = std::collections::HashSet::new();
    for file in discover_dir(project_styles_dir(cwd), "project")
        .into_iter()
        .chain(discover_dir(user_styles_dir(), "user"))
    {
        if !seen.insert(file.name.clone()) {
            continue;
        }
        let (_name, description, _keep, _body) = match read_style_file(&file.path) {
            Some(parsed) => parsed,
            None => continue,
        };
        entries.push(OutputStyleSummary {
            name: file.name,
            description,
            custom: true,
            source: file.source.to_string(),
        });
    }
    entries
}

/// Resolve one style name to its prompt body: built-ins need no filesystem;
/// custom files read project-first, then user. Returns
/// `(body, keep_coding_instructions, custom, source)`, or `None` when no
/// built-in or readable file carries the name.
#[must_use]
pub fn load_style_body(cwd: &Path, name: &str) -> Option<(String, bool, bool, &'static str)> {
    if let Some(builtin) = mermaid_domain::prompts::builtin_output_style(name) {
        return Some((builtin.body.to_string(), true, false, "builtin"));
    }
    for file in discover_dir(project_styles_dir(cwd), "project")
        .into_iter()
        .chain(discover_dir(user_styles_dir(), "user"))
    {
        if file.name != name {
            continue;
        }
        let (frontmatter_name, _description, keep, body) = read_style_file(&file.path)?;
        if !frontmatter_name.is_empty() && frontmatter_name != name {
            tracing::warn!(path = %file.path.display(), "output style frontmatter name disagrees with the filename; using the filename");
        }
        return Some((body, keep, true, file.source));
    }
    None
}

/// Resolve `config.output.style` into `config.active_style` at startup, and
/// report where the selection came from. Unknown names warn and fall back to
/// `default` (fail-closed and visible, never a silent stock prompt).
pub fn resolve_and_apply(
    cwd: &Path,
    flags: &mermaid_domain::SessionFlags,
    config: &mut mermaid_domain::Config,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let name = config.output.style.trim().to_string();
    if name.is_empty() || name == mermaid_domain::prompts::DEFAULT_OUTPUT_STYLE {
        config.output.style = mermaid_domain::prompts::DEFAULT_OUTPUT_STYLE.to_string();
        config.active_style = ActiveStyle::none();
        return warnings;
    }
    if !mermaid_domain::prompts::is_valid_style_name(&name) {
        warnings.push(format!(
            "unknown output style '{name}' — using default (bare `/output-style` lists styles)"
        ));
        config.output.style = mermaid_domain::prompts::DEFAULT_OUTPUT_STYLE.to_string();
        config.active_style = ActiveStyle::none();
        return warnings;
    }
    let source = style_source(cwd, flags);
    match load_style_body(cwd, &name) {
        Some((body, keep, custom, _)) => {
            config.active_style = ActiveStyle {
                body,
                keep_coding_instructions: keep,
                custom,
                source: source.to_string(),
            };
        },
        None => {
            warnings.push(format!(
                "unknown output style '{name}' — using default (bare `/output-style` lists styles)"
            ));
            config.output.style = mermaid_domain::prompts::DEFAULT_OUTPUT_STYLE.to_string();
            config.active_style = ActiveStyle::none();
        },
    }
    warnings
}

/// Which layer provided the merged `output.style`: the session flag (or a
/// `-c output.style=` override) wins, then the project file, then the user
/// file. Read directly from the two files (the merged `Config` no longer says
/// where a value came from).
fn style_source(cwd: &Path, flags: &mermaid_domain::SessionFlags) -> &'static str {
    if flags
        .output_style
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
        || flags
            .overrides
            .iter()
            .any(|raw| raw.split('=').next().map(str::trim) == Some("output.style"))
    {
        return "session";
    }
    if let Some(root) = crate::app::memory::find_git_root(cwd)
        && let Ok(table) =
            crate::app::config::read_config_table(&root.join(".mermaid").join("config.toml"))
        && table
            .get("output")
            .and_then(|output| output.get("style"))
            .and_then(|style| style.as_str())
            .is_some_and(|style| !style.trim().is_empty())
    {
        return "project";
    }
    if let Ok(dir) = crate::app::get_config_dir()
        && let Ok(table) = crate::app::config::read_config_table(&dir.join("config.toml"))
        && table
            .get("output")
            .and_then(|output| output.get("style"))
            .and_then(|style| style.as_str())
            .is_some_and(|style| !style.trim().is_empty())
    {
        return "user";
    }
    "default"
}

/// One-line startup summary for a non-default style. `None` for `default` so
/// a stock startup stays quiet — silence means the stock prompt.
#[must_use]
pub fn output_style_notice(config: &mermaid_domain::Config) -> Option<String> {
    let name = config.output.style.trim();
    if name.is_empty() || name == mermaid_domain::prompts::DEFAULT_OUTPUT_STYLE {
        return None;
    }
    let kind = if config.active_style.custom {
        "custom"
    } else {
        "built-in"
    };
    Some(format!(
        "Output style: {name} ({kind}, from {}).",
        config.active_style.source
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mermaid-output-styles-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_style(dir: &Path, name: &str, content: &str) -> PathBuf {
        let styles = dir.join(STYLES_DIR);
        std::fs::create_dir_all(&styles).unwrap();
        let path = styles.join(format!("{name}.md"));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn list_styles_has_default_and_builtins_without_any_files() {
        let dir = temp_dir("bare");
        let entries = list_styles(&dir);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"default"));
        for builtin in ["proactive", "concise", "explanatory", "learning"] {
            assert!(names.contains(&builtin), "missing {builtin}: {names:?}");
        }
        assert!(entries.iter().all(|entry| !entry.custom));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_styles_discovers_user_files() {
        let dir = temp_dir("user-file");
        temp_env::with_var(
            crate::app::config::CONFIG_DIR_ENV,
            Some(dir.to_str().unwrap()),
            || {
                write_style(
                    &dir,
                    "terse",
                    "---\ndescription: Very short\n---\n\nBe brief.\n",
                );
                let entries = list_styles(std::path::Path::new("/nonexistent-cwd"));
                let terse = entries
                    .iter()
                    .find(|entry| entry.name == "terse")
                    .expect("listed");
                assert!(terse.custom);
                assert_eq!(terse.source, "user");
                assert_eq!(terse.description, "Very short");
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_style_body_prefers_builtin_without_touching_files() {
        let dir = temp_dir("builtin");
        let (body, keep, custom, source) = load_style_body(&dir, "concise").expect("builtin");
        assert!(body.contains("concise"));
        assert!(keep);
        assert!(!custom);
        assert_eq!(source, "builtin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_style_body_reads_custom_files_project_first() {
        let dir = temp_dir("custom");
        temp_env::with_var(
            crate::app::config::CONFIG_DIR_ENV,
            Some(dir.to_str().unwrap()),
            || {
                write_style(&dir, "terse", "User body.\n");
                let (body, keep, custom, source) = load_style_body(&dir, "terse").expect("custom");
                assert_eq!(body, "User body.");
                assert!(keep, "keeping the base prompt is the default");
                assert!(custom);
                assert_eq!(source, "user");
                assert!(load_style_body(&dir, "missing").is_none());
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_styles_shadow_user_files() {
        let dir = temp_dir("shadow");
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let project_styles = repo.join(".mermaid").join(STYLES_DIR);
        std::fs::create_dir_all(&project_styles).unwrap();
        std::fs::write(project_styles.join("terse.md"), "Project body.\n").unwrap();
        temp_env::with_var(
            crate::app::config::CONFIG_DIR_ENV,
            Some(dir.to_str().unwrap()),
            || {
                write_style(&dir, "terse", "User body.\n");
                let sub = repo.join("src");
                std::fs::create_dir_all(&sub).unwrap();
                let (body, _, custom, source) = load_style_body(&sub, "terse").expect("shadowed");
                assert_eq!(body, "Project body.");
                assert!(custom);
                assert_eq!(source, "project");
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_and_apply_falls_back_on_unknown_names() {
        let dir = temp_dir("fallback");
        let mut config = mermaid_domain::Config::default();
        config.output.style = "Bogus Name!".to_string();
        let warnings =
            resolve_and_apply(&dir, &mermaid_domain::SessionFlags::default(), &mut config);
        assert_eq!(config.output.style, "default");
        assert!(config.active_style.body.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Bogus Name!"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
