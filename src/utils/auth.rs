//! Provider API-key resolution.
//!
//! Mermaid's auth surface is uniform across providers: an API key lives in
//! an environment variable, with the option to override the variable name
//! per-provider in `config.toml`. There's no in-config secret storage —
//! keys never sit on disk in plaintext via this helper. (The legacy
//! `cloud_api_key` field on `[ollama]` predates this and writes the key
//! to disk; that path stays for backward compat but is not what new
//! providers should use.)

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
}
