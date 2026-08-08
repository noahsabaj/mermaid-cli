//! OS-keyring storage for provider API keys (`mermaid login`).
//!
//! Environment variables keep ABSOLUTE precedence — the keyring only fills
//! the gap when no env var resolves (see `utils::auth::resolve_provider_key`).
//! Keys are stored under service `"mermaid"`, account `<provider>`. Backends:
//! macOS Keychain, Windows Credential Manager, and the freedesktop Secret
//! Service over blocking dbus on Linux. Kernel keyutils was deliberately
//! rejected: kernel keyrings are session-scoped and vanish on reboot — the
//! wrong semantics for API keys.
//!
//! On a headless Linux box without a Secret Service every operation fails;
//! that is treated as "no stored key" with ONE rate-limited warning, never a
//! fatal error. `MERMAID_NO_KEYRING=1` disables the keyring entirely (broken
//! dbus setups).

use anyhow::Result;

/// Abstract credential storage so resolution logic tests against an
/// in-memory fake instead of the real OS keyring.
pub trait CredentialStore: Send + Sync {
    /// The stored key for `provider`, if any. Backend failures (no Secret
    /// Service, locked keychain) read as `None` — resolution then simply
    /// reports no key, exactly like an unset env var.
    fn get(&self, provider: &str) -> Option<String>;
    /// Store (or replace) `provider`'s key.
    ///
    /// # Errors
    ///
    /// Whatever the backend reports: no Secret Service on a headless Linux
    /// box, a locked keychain, a denied write. Unlike [`Self::get`], a write
    /// failure is surfaced rather than read as "no key" — silently discarding
    /// a key the user just typed would look like success.
    fn set(&self, provider: &str, key: &str) -> Result<()>;
    /// Delete `provider`'s key; `Ok(false)` when nothing was stored.
    ///
    /// # Errors
    ///
    /// Whatever the backend reports (unavailable, locked, denied). A key that
    /// was not there is `Ok(false)`, not an error.
    fn delete(&self, provider: &str) -> Result<bool>;
    /// Human name for confirmations ("Stored key for groq in `<label>`").
    fn label(&self) -> &'static str;
}

/// The real OS keyring, service `"mermaid"`.
pub struct KeyringStore;

/// Warn about an unusable keyring backend once per process — a headless
/// Linux box hits this on every provider resolution otherwise.
fn warn_backend_unavailable(err: &keyring::Error) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::warn!(
            error = %err,
            "OS keyring unavailable; stored provider keys are inaccessible \
             (env vars still work; set MERMAID_NO_KEYRING=1 to silence)"
        );
    });
}

impl CredentialStore for KeyringStore {
    fn get(&self, provider: &str) -> Option<String> {
        let entry = match keyring::Entry::new("mermaid", provider) {
            Ok(entry) => entry,
            Err(err) => {
                warn_backend_unavailable(&err);
                return None;
            },
        };
        match entry.get_password() {
            Ok(key) => {
                let key = key.trim().to_string();
                if key.is_empty() { None } else { Some(key) }
            },
            Err(keyring::Error::NoEntry) => None,
            Err(err) => {
                warn_backend_unavailable(&err);
                None
            },
        }
    }

    fn set(&self, provider: &str, key: &str) -> Result<()> {
        keyring::Entry::new("mermaid", provider)?.set_password(key)?;
        Ok(())
    }

    fn delete(&self, provider: &str) -> Result<bool> {
        match keyring::Entry::new("mermaid", provider)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    fn label(&self) -> &'static str {
        "the OS keyring"
    }
}

/// The `MERMAID_NO_KEYRING=1` escape hatch: reads yield nothing, writes are
/// a clear error instead of a silent no-op.
pub struct NoopStore;

impl CredentialStore for NoopStore {
    fn get(&self, _provider: &str) -> Option<String> {
        None
    }

    fn set(&self, _provider: &str, _key: &str) -> Result<()> {
        anyhow::bail!("keyring is disabled (MERMAID_NO_KEYRING is set)")
    }

    fn delete(&self, _provider: &str) -> Result<bool> {
        anyhow::bail!("keyring is disabled (MERMAID_NO_KEYRING is set)")
    }

    fn label(&self) -> &'static str {
        "keyring (disabled via MERMAID_NO_KEYRING)"
    }
}

/// The process-wide store: the OS keyring, or the no-op store when
/// `MERMAID_NO_KEYRING` is set (non-empty). Resolved once — env is
/// process-static.
#[must_use]
pub fn default_store() -> &'static dyn CredentialStore {
    static KEYRING: KeyringStore = KeyringStore;
    static NOOP: NoopStore = NoopStore;
    static DISABLED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var_os("MERMAID_NO_KEYRING").is_some_and(|v| !v.is_empty())
    });
    if *DISABLED { &NOOP } else { &KEYRING }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory store for resolution-precedence tests.
    #[derive(Default)]
    pub struct FakeStore {
        pub entries: Mutex<HashMap<String, String>>,
    }

    impl CredentialStore for FakeStore {
        fn get(&self, provider: &str) -> Option<String> {
            self.entries.lock().unwrap().get(provider).cloned()
        }

        fn set(&self, provider: &str, key: &str) -> Result<()> {
            self.entries
                .lock()
                .unwrap()
                .insert(provider.to_string(), key.to_string());
            Ok(())
        }

        fn delete(&self, provider: &str) -> Result<bool> {
            Ok(self.entries.lock().unwrap().remove(provider).is_some())
        }

        fn label(&self) -> &'static str {
            "fake store"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_store_reads_nothing_and_refuses_writes() {
        let store = NoopStore;
        assert!(store.get("groq").is_none());
        assert!(store.set("groq", "k").is_err());
        assert!(store.delete("groq").is_err());
    }

    /// Real-keyring round-trip. Ignored in CI (no Secret Service there);
    /// run locally with `cargo test -- --ignored keyring_round_trip`.
    #[test]
    #[ignore]
    fn keyring_round_trip() {
        let store = KeyringStore;
        let provider = format!("mermaid-test-{}", std::process::id());
        store.set(&provider, "s3cret").expect("set");
        assert_eq!(store.get(&provider).as_deref(), Some("s3cret"));
        assert!(store.delete(&provider).expect("delete"));
        assert!(store.get(&provider).is_none());
        assert!(!store.delete(&provider).expect("second delete"));
    }
}
