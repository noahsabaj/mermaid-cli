//! `/autocompact`: show or change when automatic compaction starts.
//!
//! The threshold is a context size in tokens, set for the current model or for
//! every model, in the user config or the project config. The command only
//! parses here; the effect layer writes the file and the reducer applies the
//! result.

use crate::config::CompactionConfig;

/// The usage line, shown for a bad argument.
pub const USAGE: &str =
    "Usage: /autocompact [<tokens>|off|on|reset] [global|project] [current-model|all-models]";

/// What `/autocompact` changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoCompactSetting {
    /// Compact when the context reaches this many tokens.
    Tokens(usize),
    /// Turn automatic compaction off (every model).
    Off,
    /// Turn automatic compaction on (every model).
    On,
    /// Remove the value, so the default (or a lower-priority value) applies.
    Reset,
}

/// The config file a change goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFile {
    /// The user config file (every project).
    User,
    /// `<git-root>/.mermaid/config.toml`.
    Project,
}

/// One `/autocompact` change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoCompactChange {
    pub setting: AutoCompactSetting,
    pub file: ConfigFile,
    /// The model the change is for; `None` for every model.
    pub model_id: Option<String>,
}

/// Parse the argument of `/autocompact` for the model in use. `Ok(None)` is a
/// bare command, which shows the current threshold.
///
/// # Errors
///
/// The message to show for an argument that is not a valid change.
pub fn parse(arg: &str, model_id: &str) -> Result<Option<AutoCompactChange>, String> {
    let mut setting = None;
    let mut file = None;
    let mut all_models = None;
    for word in arg.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        match lower.as_str() {
            "global" | "user" => set_once(&mut file, ConfigFile::User)?,
            "project" => set_once(&mut file, ConfigFile::Project)?,
            "all-models" | "all" => set_once(&mut all_models, true)?,
            "current-model" | "model" => set_once(&mut all_models, false)?,
            "off" => set_once(&mut setting, AutoCompactSetting::Off)?,
            "on" => set_once(&mut setting, AutoCompactSetting::On)?,
            "reset" => set_once(&mut setting, AutoCompactSetting::Reset)?,
            _ => match parse_tokens(&lower) {
                Some(tokens) => set_once(&mut setting, AutoCompactSetting::Tokens(tokens))?,
                None => return Err(format!("'{word}' is not a token count.\n{USAGE}")),
            },
        }
    }
    let Some(setting) = setting else {
        return if file.is_none() && all_models.is_none() {
            Ok(None)
        } else {
            Err(USAGE.to_string())
        };
    };
    if let AutoCompactSetting::Tokens(tokens) = setting
        && tokens < crate::MIN_AUTO_THRESHOLD_TOKENS
    {
        return Err(format!(
            "The smallest threshold is {} tokens.",
            crate::format_compact_count(crate::MIN_AUTO_THRESHOLD_TOKENS)
        ));
    }
    let every_model = matches!(setting, AutoCompactSetting::Off | AutoCompactSetting::On);
    if every_model && all_models == Some(false) {
        return Err("`off` and `on` apply to all models.".to_string());
    }
    Ok(Some(AutoCompactChange {
        setting,
        file: file.unwrap_or(ConfigFile::User),
        model_id: (!every_model && !all_models.unwrap_or(false)).then(|| model_id.to_string()),
    }))
}

fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), String> {
    if slot.is_some() {
        return Err(USAGE.to_string());
    }
    *slot = Some(value);
    Ok(())
}

/// A token count: `250000`, `250,000`, `250_000`, `250k` or `1.05m`.
fn parse_tokens(word: &str) -> Option<usize> {
    let digits: String = word.chars().filter(|c| *c != ',' && *c != '_').collect();
    let (number, scale) = match digits.strip_suffix('m') {
        Some(n) => (n, 1_000_000_usize),
        None => match digits.strip_suffix('k') {
            Some(n) => (n, 1_000),
            None => (digits.as_str(), 1),
        },
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() || !fraction.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut value = whole.parse::<usize>().ok()?.checked_mul(scale)?;
    let mut place = scale;
    for digit in fraction.chars() {
        place /= 10;
        if place == 0 {
            return None;
        }
        value = value.checked_add(place * digit.to_digit(10)? as usize)?;
    }
    Some(value)
}

/// Where the threshold that applies to `model_id` comes from.
#[must_use]
pub fn source(config: &CompactionConfig, model_id: &str) -> &'static str {
    if config
        .auto_threshold_tokens_per_model
        .contains_key(model_id)
    {
        "set for this model"
    } else if config.auto_threshold_tokens.is_some() {
        "set for all models"
    } else {
        "default"
    }
}

/// The status line for `model_id`, whose window is `window` when known.
#[must_use]
pub fn status(config: &CompactionConfig, model_id: &str, window: Option<usize>) -> String {
    if !config.auto_enabled {
        return "Auto-compact is off. `/autocompact on` turns it on.".to_string();
    }
    let policy = config.policy_for(model_id);
    let source = source(config, model_id);
    let mut line = match (policy.auto_threshold_tokens, window) {
        (Some(tokens), Some(window)) => format!(
            "Auto-compact for {model_id}: at {} tokens ({source}). The window is {} tokens.",
            crate::format_compact_count(tokens),
            crate::format_compact_count(window),
        ),
        (Some(tokens), None) => format!(
            "Auto-compact for {model_id}: at {} tokens ({source}).",
            crate::format_compact_count(tokens),
        ),
        (None, Some(window)) => format!(
            "Auto-compact for {model_id}: at {} tokens ({}% of the {} window, {source}).",
            crate::format_compact_count(policy.trigger_tokens(window)),
            policy.auto_threshold_percent,
            crate::format_compact_count(window),
        ),
        (None, None) => format!(
            "Auto-compact for {model_id}: at {}% of the window ({source}).",
            policy.auto_threshold_percent,
        ),
    };
    if let (Some(tokens), Some(window)) = (policy.auto_threshold_tokens, window)
        && tokens >= window
    {
        line.push_str(
            "\nThis is not smaller than the window, so Mermaid compacts when the free space \
             gets too small for a reply.",
        );
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "openai/gpt-5.6";

    fn change(arg: &str) -> AutoCompactChange {
        parse(arg, MODEL).expect("parses").expect("a change")
    }

    #[test]
    fn a_bare_number_sets_the_current_model_in_the_user_file() {
        assert_eq!(
            change("250000"),
            AutoCompactChange {
                setting: AutoCompactSetting::Tokens(250_000),
                file: ConfigFile::User,
                model_id: Some(MODEL.to_string()),
            }
        );
    }

    #[test]
    fn scope_words_come_in_any_order() {
        let expected = AutoCompactChange {
            setting: AutoCompactSetting::Tokens(1_050_000),
            file: ConfigFile::Project,
            model_id: None,
        };
        assert_eq!(change("1.05m project all-models"), expected);
        assert_eq!(change("all project 1,050,000"), expected);
        assert_eq!(
            change("current-model global 250k").model_id.as_deref(),
            Some(MODEL)
        );
    }

    #[test]
    fn off_and_on_are_for_every_model() {
        assert_eq!(change("off").model_id, None);
        assert_eq!(change("on project").file, ConfigFile::Project);
        assert!(parse("off current-model", MODEL).is_err());
    }

    #[test]
    fn reset_follows_the_scope() {
        assert_eq!(change("reset").model_id.as_deref(), Some(MODEL));
        assert_eq!(change("reset all-models").model_id, None);
    }

    #[test]
    fn token_counts_take_suffixes_and_separators() {
        for (word, tokens) in [
            ("250000", 250_000),
            ("250,000", 250_000),
            ("250_000", 250_000),
            ("250k", 250_000),
            ("1.05m", 1_050_000),
            ("1m", 1_000_000),
            ("62.5k", 62_500),
        ] {
            assert_eq!(parse_tokens(word), Some(tokens), "{word}");
        }
    }

    #[test]
    fn nothing_to_change_shows_the_status() {
        assert_eq!(parse("", MODEL), Ok(None));
        assert_eq!(parse("  ", MODEL), Ok(None));
    }

    #[test]
    fn bad_arguments_are_refused() {
        for arg in [
            "lots",
            "250000 300000",
            "project",
            "on off",
            "-5k",
            "10000",
            "1.5",
            "100.0001k",
        ] {
            assert!(parse(arg, MODEL).is_err(), "{arg}");
        }
    }

    #[test]
    fn the_status_names_the_threshold_and_its_source() {
        let mut config = CompactionConfig::default();
        assert_eq!(
            status(&config, MODEL, Some(1_050_000)),
            "Auto-compact for openai/gpt-5.6: at 892.5k tokens (85% of the 1M window, default)."
        );
        config.auto_threshold_tokens = Some(250_000);
        assert!(status(&config, MODEL, None).contains("250k tokens (set for all models)"));
        config
            .auto_threshold_tokens_per_model
            .insert(MODEL.to_string(), 2_000_000);
        let line = status(&config, MODEL, Some(1_050_000));
        assert!(line.contains("2M tokens (set for this model)"), "{line}");
        assert!(line.contains("not smaller than the window"), "{line}");
        config.auto_enabled = false;
        assert!(status(&config, MODEL, None).starts_with("Auto-compact is off"));
    }
}
