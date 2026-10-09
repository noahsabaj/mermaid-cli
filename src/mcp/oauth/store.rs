//! OS-keyring storage for MCP OAuth tokens (`mermaid mcp login`).
//!
//! One record per server, under service `"mermaid"` and account
//! `mcp-oauth:<server>` — the same keyring `mermaid login` uses for provider
//! keys, through the same [`CredentialStore`] seam. The record is JSON,
//! base64url-encoded so it holds no whitespace: `KeyringStore::get` trims
//! what it reads, and a scope string split across two entries would lose the
//! space at the cut.
//!
//! Windows Credential Manager caps a secret at 2560 bytes, and `keyring`
//! writes a password there as UTF-16, so one entry holds at most 1280
//! characters. A token record with a large JWT access token and a refresh
//! token is bigger than that, so a long record is split: the head entry
//! holds `chunks:<n>` and the parts sit in `mcp-oauth:<server>#1..n`.

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mermaid_model::utils::CredentialStore;
use serde::{Deserialize, Serialize};

/// Characters per keyring entry; under the 1280 a Windows entry can hold.
const CHUNK_CHARS: usize = 1000;
/// A record never needs more; a corrupt head naming more is refused.
const MAX_CHUNKS: usize = 64;
const CHUNK_PREFIX: &str = "chunks:";

/// How the client proves itself at the token endpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClientAuth {
    /// A public client: `client_id` in the body, PKCE does the rest.
    #[default]
    None,
    /// HTTP Basic with `client_id:client_secret` (RFC 6749 section 2.3.1).
    ClientSecretBasic,
    /// `client_id` and `client_secret` in the form body.
    ClientSecretPost,
}

/// Everything a later process needs to use and refresh a server's tokens
/// without repeating discovery. No `Debug`: it holds secrets.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct StoredTokens {
    /// The configured MCP `url` at sign-in. Tokens are bound to it: a config
    /// that now points somewhere else must not receive them.
    pub server_url: String,
    /// The RFC 8707 `resource` indicator the tokens were issued for.
    pub resource: String,
    pub issuer: String,
    pub token_endpoint: String,
    pub client_id: String,
    /// Only for a dynamically registered confidential client. A
    /// pre-registered client's secret stays in its env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub client_auth: ClientAuth,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds; `None` when the server gave no `expires_in`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

fn account(server: &str) -> String {
    format!("mcp-oauth:{server}")
}

fn chunk_account(server: &str, index: usize) -> String {
    format!("mcp-oauth:{server}#{index}")
}

/// The stored tokens for `server`, if any. A missing, unreadable or corrupt
/// record reads as `None`: the user signs in again, nothing crashes.
pub(crate) fn load(store: &dyn CredentialStore, server: &str) -> Option<StoredTokens> {
    let head = store.get(&account(server))?;
    let encoded = match head.strip_prefix(CHUNK_PREFIX) {
        None => head,
        Some(count) => {
            let count: usize = count
                .parse()
                .ok()
                .filter(|n| (1..=MAX_CHUNKS).contains(n))?;
            let mut joined = String::new();
            for index in 1..=count {
                joined.push_str(&store.get(&chunk_account(server, index))?);
            }
            joined
        },
    };
    let bytes = URL_SAFE_NO_PAD.decode(encoded.as_bytes()).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(tokens) => Some(tokens),
        Err(e) => {
            tracing::warn!("MCP OAuth: stored tokens for '{server}' do not parse: {e}");
            None
        },
    }
}

/// Store (or replace) `server`'s tokens.
///
/// # Errors
///
/// Whatever the keyring reports for a write: no Secret Service on a headless
/// Linux box, a locked keychain, `MERMAID_NO_KEYRING`.
pub(crate) fn save(store: &dyn CredentialStore, server: &str, tokens: &StoredTokens) -> Result<()> {
    let old_chunks = chunk_count(store, server);
    let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(tokens)?);
    let new_chunks = if encoded.len() <= CHUNK_CHARS {
        store.set(&account(server), &encoded)?;
        0
    } else {
        // base64url is ASCII, so byte offsets are char boundaries.
        let parts: Vec<&str> = encoded
            .as_bytes()
            .chunks(CHUNK_CHARS)
            .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
            .collect();
        if parts.len() > MAX_CHUNKS {
            return Err(anyhow!(
                "MCP OAuth tokens for '{server}' are too large to store"
            ));
        }
        for (i, part) in parts.iter().enumerate() {
            store.set(&chunk_account(server, i + 1), part)?;
        }
        // The head last: until it is written, a reader still sees the old
        // record's count and the decode of mixed parts fails closed.
        store.set(&account(server), &format!("{CHUNK_PREFIX}{}", parts.len()))?;
        parts.len()
    };
    for index in (new_chunks + 1)..=old_chunks {
        let _ = store.delete(&chunk_account(server, index));
    }
    Ok(())
}

/// Delete `server`'s tokens; `Ok(false)` when none were stored.
///
/// # Errors
///
/// Whatever the keyring reports for a delete (unavailable, locked, denied).
pub(crate) fn delete(store: &dyn CredentialStore, server: &str) -> Result<bool> {
    let chunks = chunk_count(store, server);
    let removed = store
        .delete(&account(server))
        .with_context(|| format!("delete MCP OAuth tokens for '{server}'"))?;
    for index in 1..=chunks {
        let _ = store.delete(&chunk_account(server, index));
    }
    Ok(removed)
}

fn chunk_count(store: &dyn CredentialStore, server: &str) -> usize {
    store
        .get(&account(server))
        .and_then(|head| head.strip_prefix(CHUNK_PREFIX)?.parse().ok())
        .filter(|n| *n <= MAX_CHUNKS)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory keyring that, like Windows, refuses an entry over 1280
    /// characters.
    #[derive(Default)]
    pub struct MemStore {
        pub entries: Mutex<HashMap<String, String>>,
    }

    impl CredentialStore for MemStore {
        fn get(&self, account: &str) -> Option<String> {
            self.entries.lock().unwrap().get(account).cloned()
        }

        fn set(&self, account: &str, value: &str) -> Result<()> {
            anyhow::ensure!(value.len() <= 1280, "entry too long for Windows");
            self.entries
                .lock()
                .unwrap()
                .insert(account.to_string(), value.to_string());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<bool> {
            Ok(self.entries.lock().unwrap().remove(account).is_some())
        }

        fn label(&self) -> &'static str {
            "memory store"
        }
    }

    pub fn tokens(access: &str) -> StoredTokens {
        StoredTokens {
            server_url: "http://127.0.0.1/mcp".to_string(),
            resource: "http://127.0.0.1/mcp".to_string(),
            issuer: "http://127.0.0.1".to_string(),
            token_endpoint: "http://127.0.0.1/token".to_string(),
            client_id: "client-1".to_string(),
            client_secret: None,
            client_auth: ClientAuth::None,
            access_token: access.to_string(),
            refresh_token: Some("refresh-1".to_string()),
            expires_at: None,
            scope: Some("files:read files:write".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn short_record_round_trips_in_one_entry() {
        let store = MemStore::default();
        save(&store, "s", &tokens("abc")).unwrap();
        assert_eq!(store.entries.lock().unwrap().len(), 1);
        let back = load(&store, "s").expect("stored");
        assert_eq!(back.access_token, "abc");
        assert_eq!(back.scope.as_deref(), Some("files:read files:write"));
    }

    #[test]
    fn long_record_is_split_and_shrinking_removes_stale_parts() {
        let store = MemStore::default();
        let big = "x".repeat(4000);
        save(&store, "s", &tokens(&big)).unwrap();
        let entries = store.entries.lock().unwrap().len();
        assert!(entries > 2, "split into parts: {entries}");
        assert_eq!(load(&store, "s").unwrap().access_token, big);
        // Replacing with a short record leaves exactly one entry.
        save(&store, "s", &tokens("short")).unwrap();
        assert_eq!(store.entries.lock().unwrap().len(), 1);
        assert_eq!(load(&store, "s").unwrap().access_token, "short");
    }

    #[test]
    fn delete_removes_every_part() {
        let store = MemStore::default();
        save(&store, "s", &tokens(&"y".repeat(3000))).unwrap();
        assert!(delete(&store, "s").unwrap());
        assert!(store.entries.lock().unwrap().is_empty());
        assert!(!delete(&store, "s").unwrap());
        assert!(load(&store, "s").is_none());
    }

    #[test]
    fn corrupt_record_reads_as_none() {
        let store = MemStore::default();
        store.set("mcp-oauth:s", "not base64 json!").unwrap();
        assert!(load(&store, "s").is_none());
        store.set("mcp-oauth:s", "chunks:3").unwrap();
        assert!(load(&store, "s").is_none(), "missing parts");
    }
}
