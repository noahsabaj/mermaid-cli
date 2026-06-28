//! Provider API-key resolution.
//!
//! Mermaid's auth surface is uniform across providers: an API key lives in
//! an environment variable, with the option to override the variable name
//! per-provider in `config.toml`. There's no in-config secret storage —
//! keys never sit on disk in plaintext. The Ollama cloud key follows the same
//! rule: it is read from `OLLAMA_API_KEY` and never persisted (#88).

/// Resolve an API key from the environment.
///
/// `default_env` is the env var the built-in registry expects (e.g.
/// `"GROQ_API_KEY"`). `override_env` is an optional per-provider
/// override from `config.toml`'s `[providers.<name>] api_key_env = ...`.
/// When set, it takes precedence — a user who's already standardized on
/// `LLM_API_KEY` for everything can point all their providers at it.
///
/// Empty values are treated as unset (matches the existing
/// `get_cloud_api_key` semantics).
pub fn resolve_api_key(default_env: &str, override_env: Option<&str>) -> Option<String> {
    let env_var = override_env.unwrap_or(default_env);
    match std::env::var(env_var) {
        Ok(key) if !key.is_empty() => Some(key),
        _ => None,
    }
}

/// Resolve an API key with a legacy fallback env var.
///
/// If `override_env` is set, it remains authoritative and no fallback
/// is attempted. Without an override, `default_env` is checked first
/// and `fallback_env` is accepted only when the default is unset.
pub fn resolve_api_key_with_fallback(
    default_env: &str,
    fallback_env: &str,
    override_env: Option<&str>,
) -> Option<String> {
    if override_env.is_some() {
        return resolve_api_key(default_env, override_env);
    }
    resolve_api_key(default_env, None).or_else(|| resolve_api_key(fallback_env, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a unique env var name per test so concurrent test runs
    /// don't step on each other's process-global env state. `temp_env`
    /// restores the prior value after the closure, but a collision with
    /// a concurrent test from another module on a shared name would
    /// still race — unique names are belt-and-braces.
    fn unique_env(prefix: &str) -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        format!(
            "{}_{}_{}",
            prefix,
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        )
    }

    #[test]
    fn returns_none_when_env_var_unset() {
        let var = unique_env("MERMAID_TEST_AUTH_UNSET");
        temp_env::with_var_unset(&var, || {
            assert_eq!(resolve_api_key(&var, None), None);
        });
    }

    #[test]
    fn returns_value_when_env_var_set() {
        let var = unique_env("MERMAID_TEST_AUTH_SET");
        temp_env::with_var(&var, Some("secret-value"), || {
            assert_eq!(
                resolve_api_key(&var, None),
                Some("secret-value".to_string())
            );
        });
    }

    #[test]
    fn empty_string_treated_as_unset() {
        let var = unique_env("MERMAID_TEST_AUTH_EMPTY");
        temp_env::with_var(&var, Some(""), || {
            assert_eq!(resolve_api_key(&var, None), None);
        });
    }

    #[test]
    fn override_env_takes_precedence_over_default() {
        let default_var = unique_env("MERMAID_TEST_AUTH_DEFAULT");
        let override_var = unique_env("MERMAID_TEST_AUTH_OVERRIDE");
        temp_env::with_vars(
            [
                (default_var.as_str(), Some("default-key")),
                (override_var.as_str(), Some("override-key")),
            ],
            || {
                let resolved = resolve_api_key(&default_var, Some(&override_var));
                assert_eq!(resolved, Some("override-key".to_string()));
            },
        );
    }

    #[test]
    fn override_env_unset_falls_through_to_none() {
        // When the override env name is provided but unset, we DON'T fall
        // back to the default — the user explicitly asked for a different
        // var and got nothing. Better to fail loudly than silently use a
        // key the user thought they'd disabled.
        let default_var = unique_env("MERMAID_TEST_AUTH_DEFAULT2");
        let override_var = unique_env("MERMAID_TEST_AUTH_OVERRIDE2");
        temp_env::with_vars(
            [
                (default_var.as_str(), Some("default-key")),
                (override_var.as_str(), None),
            ],
            || {
                let resolved = resolve_api_key(&default_var, Some(&override_var));
                assert_eq!(resolved, None);
            },
        );
    }

    #[test]
    fn fallback_env_used_only_when_default_is_unset() {
        let default_var = unique_env("MERMAID_TEST_AUTH_FALLBACK_DEFAULT");
        let fallback_var = unique_env("MERMAID_TEST_AUTH_FALLBACK_LEGACY");
        temp_env::with_vars(
            [
                (default_var.as_str(), None),
                (fallback_var.as_str(), Some("legacy-key")),
            ],
            || {
                let resolved = resolve_api_key_with_fallback(&default_var, &fallback_var, None);
                assert_eq!(resolved, Some("legacy-key".to_string()));
            },
        );

        temp_env::with_vars(
            [
                (default_var.as_str(), Some("default-key")),
                (fallback_var.as_str(), Some("legacy-key")),
            ],
            || {
                let resolved = resolve_api_key_with_fallback(&default_var, &fallback_var, None);
                assert_eq!(resolved, Some("default-key".to_string()));
            },
        );
    }

    #[test]
    fn fallback_env_is_ignored_when_override_is_set() {
        let default_var = unique_env("MERMAID_TEST_AUTH_OVERRIDE_DEFAULT3");
        let fallback_var = unique_env("MERMAID_TEST_AUTH_OVERRIDE_FALLBACK3");
        let override_var = unique_env("MERMAID_TEST_AUTH_OVERRIDE3");
        temp_env::with_vars(
            [
                (default_var.as_str(), Some("default-key")),
                (fallback_var.as_str(), Some("legacy-key")),
                (override_var.as_str(), None),
            ],
            || {
                let resolved =
                    resolve_api_key_with_fallback(&default_var, &fallback_var, Some(&override_var));
                assert_eq!(resolved, None);
            },
        );
    }
}
