#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::data_dir;

pub const DAEMON_TOKEN_ENV: &str = "MERMAID_DAEMON_TOKEN";

/// Default lifetime for a freshly minted pairing token. Tokens expire so a
/// leaked or forgotten token can't be replayed indefinitely; `--ttl-days 0`
/// opts out for long-lived automation.
pub const DEFAULT_PAIRING_TTL_DAYS: i64 = 30;

/// RFC3339 expiry `ttl_days` from now, or `None` when `ttl_days <= 0`
/// (never expires). Shared by the daemon `pair` command and the local CLI so
/// both honor the same TTL semantics.
pub fn pairing_expiry_from_now(ttl_days: i64) -> Option<String> {
    (ttl_days > 0).then(|| (chrono::Utc::now() + chrono::Duration::days(ttl_days)).to_rfc3339())
}

/// Clamp a *client-supplied* pairing TTL so a daemon socket caller can't mint a
/// never-expiring token by sending `ttl_days <= 0`: non-positive input becomes
/// the default TTL, positive values pass through. The local `mermaid pair` CLI
/// deliberately does **not** call this — its `--ttl-days 0` "never expires"
/// opt-out is an owner-only choice with no privilege boundary (#65).
pub fn clamp_pairing_ttl_days(ttl_days: i64) -> i64 {
    if ttl_days <= 0 {
        DEFAULT_PAIRING_TTL_DAYS
    } else {
        ttl_days
    }
}

pub fn daemon_socket_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("mermaidd.sock"))
}

pub fn generate_pairing_token() -> Result<(String, String)> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|err| anyhow::anyhow!("failed to generate pairing token: {}", err))?;
    let token = format!("mermaid_{}", general_purpose::URL_SAFE_NO_PAD.encode(bytes));
    let hash = hash_pairing_token(&token);
    Ok((token, hash))
}

pub fn hash_pairing_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    crate::hex_lower(&digest)
}

pub fn request_daemon_json(mut body: serde_json::Value) -> Result<serde_json::Value> {
    if body.get("auth").is_none()
        && let Ok(token) = std::env::var(DAEMON_TOKEN_ENV)
        && !token.trim().is_empty()
    {
        body["auth"] = serde_json::json!({ "token": token });
    }
    request_daemon_text(&body.to_string())
}

pub fn request_daemon_text(line: &str) -> Result<serde_json::Value> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let socket = daemon_socket_path()?;
        let mut stream = UnixStream::connect(&socket)
            .with_context(|| format!("failed to connect to {}", socket.display()))?;
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        let mut response = String::new();
        let mut reader = BufReader::new(stream);
        reader.read_line(&mut response)?;
        let value: serde_json::Value =
            serde_json::from_str(response.trim()).context("daemon returned invalid JSON")?;
        if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
            anyhow::bail!(
                "{}",
                value
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("daemon request failed")
            );
        }
        Ok(value)
    }

    #[cfg(not(unix))]
    {
        let _ = line;
        anyhow::bail!("daemon IPC currently supports Unix sockets only")
    }
}

pub fn snapshot_field_from_daemon<T: DeserializeOwned>(field: &str) -> Result<T> {
    let value = request_daemon_json(serde_json::json!({ "command": "snapshot" }))?;
    let field_value = value
        .get(field)
        .cloned()
        .with_context(|| format!("daemon snapshot missing `{}`", field))?;
    serde_json::from_value(field_value)
        .with_context(|| format!("daemon snapshot field `{}` had unexpected shape", field))
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn pairing_token_hash_is_stable_and_not_plaintext() {
        let hash = hash_pairing_token("mermaid_test");
        assert_eq!(hash, hash_pairing_token("mermaid_test"));
        assert_ne!(hash, "mermaid_test");
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn generated_pairing_token_hash_matches_token() {
        let (token, hash) = generate_pairing_token().expect("token");
        assert!(token.starts_with("mermaid_"));
        assert_eq!(hash, hash_pairing_token(&token));
    }

    #[test]
    fn clamp_pairing_ttl_days_forces_expiry_for_non_positive() {
        assert_eq!(clamp_pairing_ttl_days(0), DEFAULT_PAIRING_TTL_DAYS);
        assert_eq!(clamp_pairing_ttl_days(-5), DEFAULT_PAIRING_TTL_DAYS);
        assert_eq!(clamp_pairing_ttl_days(7), 7);
        // The #65 property: a clamped non-positive ttl yields a NON-NULL expiry,
        // exactly as the daemon `pair` handler composes the two helpers.
        assert!(pairing_expiry_from_now(clamp_pairing_ttl_days(0)).is_some());
        assert!(pairing_expiry_from_now(clamp_pairing_ttl_days(-1)).is_some());
    }
}
