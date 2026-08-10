//! Mandatory secret redaction at durable runtime boundaries.
//!
//! The single implementation used by the CLI, the SQLite repositories, and
//! everything between. Values remain unmodified while executing; cloned
//! arguments, outcomes, labels, and URLs are scrubbed immediately before
//! they cross a persistence or display boundary. It lives in this bottom
//! crate (rather than `mermaid-runtime`, which re-exports it) so redaction
//! is available below the store -- the crate stack points one way.

use std::sync::LazyLock;

use regex::Regex;

static SECRET_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let pattern = |source: &str| Regex::new(source).expect("static redaction regex must compile");
    vec![
        (
            pattern(
                r#"(?i)\b([A-Z0-9_]*(?:API[_-]?KEY|APIKEY|SECRET|TOKEN|PASSWORD|PASSWD|ACCESS[_-]?KEY|PRIVATE[_-]?KEY|CREDENTIALS?))\b\s*[:=]\s*(?:"(?:\\.|[^"\\\r\n])*"?|'(?:\\.|[^'\\\r\n])*'?|[^\s"']+)"#,
            ),
            "${1}=[REDACTED]",
        ),
        (
            pattern(r#"(?i)\b(Bearer)\s+[A-Za-z0-9._~+/\-]+={0,}"#),
            "${1} [REDACTED]",
        ),
        (
            pattern(
                r#"(?i)\b((?:Proxy-)?Authorization)\b\s*[:=]\s*((?:[A-Za-z][A-Za-z0-9._~+/-]*\s+)?)[^\r\n]+"#,
            ),
            "${1}: ${2}[REDACTED]",
        ),
        (
            pattern(r#"\bsk-(?:ant-)?[A-Za-z0-9._\-]{12,}"#),
            "[REDACTED]",
        ),
        (pattern(r#"\bAKIA[0-9A-Z]{16}\b"#), "[REDACTED]"),
        (pattern(r#"\bgh[pousr]_[A-Za-z0-9]{20,}\b"#), "[REDACTED]"),
        (pattern(r#"\bxox[baprs]-[A-Za-z0-9-]{10,}\b"#), "[REDACTED]"),
        (pattern(r#"\bAIza[0-9A-Za-z._\-]{20,}\b"#), "[REDACTED]"),
        (pattern(r#"\bgsk_[A-Za-z0-9]{20,}\b"#), "[REDACTED]"),
        (
            pattern(r#"\bsk_(?:live|test)_[A-Za-z0-9]{16,}\b"#),
            "[REDACTED]",
        ),
        (
            pattern(r#"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"#),
            "[REDACTED]",
        ),
        (
            pattern(
                r#"(?is)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)"#,
            ),
            "[REDACTED PRIVATE KEY]",
        ),
        (pattern(r#"(://[^/:@\s]+:)[^/@\s]{3,}@"#), "${1}[REDACTED]@"),
        (
            pattern(
                r#"(?i)([?&](?:access[_-]?key|access[_-]?token|api[_-]?key|apikey|auth|authorization|client[_-]?secret|code|credential|googleaccessid|id[_-]?token|jwt|key|key-pair-id|password|policy|refresh[_-]?token|secret|session[_-]?token|sig|signature|token|x-amz-credential|x-amz-signature)=)[^&#\s]+"#,
            ),
            "${1}[REDACTED]",
        ),
    ]
});

static HTTP_URL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)https?://[^\s<>"']+"#).expect("static URL redaction regex must compile")
});

const BARE_CREDENTIAL_KEYS: &[&str] = &[
    "authorization",
    "auth",
    "password",
    "token",
    "secret",
    "apikey",
];

fn key_names_credential(key: &str) -> bool {
    let normalized = normalize_credential_key(key);
    if BARE_CREDENTIAL_KEYS.iter().any(|bare| normalized == *bare) {
        return true;
    }

    let segments: Vec<_> = normalized
        .split('_')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.iter().any(|segment| {
        matches!(
            *segment,
            "password" | "passwd" | "secret" | "credential" | "credentials" | "apikey"
        )
    }) {
        return true;
    }
    if segments
        .windows(2)
        .any(|pair| matches!(pair, ["api" | "access" | "private", "key"]))
    {
        return true;
    }

    // A singular terminal `token` conventionally names the credential itself
    // (`access_token`, `idToken`, `nullable_token`). Plural or qualified metric
    // fields such as `max_tokens`, `prompt_tokens`, and `token_count` describe
    // accounting, not secrets; replacing their numbers with a string would
    // also corrupt typed persisted payloads such as replay headers.
    segments.last() == Some(&"token")
}

fn normalize_credential_key(key: &str) -> String {
    let mut normalized = String::with_capacity(key.len());
    let mut previous_was_lower_or_digit = false;
    for ch in key.chars() {
        if matches!(ch, '-' | ' ' | '.') {
            if !normalized.ends_with('_') {
                normalized.push('_');
            }
            previous_was_lower_or_digit = false;
        } else {
            if ch.is_ascii_uppercase() && previous_was_lower_or_digit {
                normalized.push('_');
            }
            normalized.extend(ch.to_lowercase());
            previous_was_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        }
    }
    normalized
}

/// Replace credential-shaped substrings while preserving surrounding context.
pub fn redact_secrets(input: &str) -> String {
    let mut output = redact_secret_shapes(input);
    if HTTP_URL_PATTERN.is_match(&output) {
        output = std::borrow::Cow::Owned(
            HTTP_URL_PATTERN
                .replace_all(&output, |captures: &regex::Captures<'_>| {
                    sanitize_url_match(&captures[0])
                })
                .into_owned(),
        );
    }
    output.into_owned()
}

fn redact_secret_shapes(input: &str) -> std::borrow::Cow<'_, str> {
    let mut output = std::borrow::Cow::Borrowed(input);
    for (regex, replacement) in SECRET_PATTERNS.iter() {
        if regex.is_match(&output) {
            output = std::borrow::Cow::Owned(regex.replace_all(&output, *replacement).into_owned());
        }
    }
    output
}

fn sanitize_url_match(raw: &str) -> String {
    let core = raw.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
    let trailing = &raw[core.len()..];
    let Ok(url) = url::Url::parse(core) else {
        return raw.to_string();
    };
    format!("{}{trailing}", sanitize_parsed_url(url))
}

/// Redact every string leaf of a cloned structured payload in place.
pub fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(string) => {
            let redacted = if url::Url::parse(string)
                .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
            {
                sanitize_url_for_display(string)
            } else {
                redact_secrets(string)
            };
            if &redacted != string {
                *string = redacted;
            }
        },
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json),
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key_names_credential(key) && !value.is_null() {
                    *value = serde_json::Value::String("[REDACTED]".to_string());
                } else {
                    redact_json(value);
                }
            }
        },
        _ => {},
    }
}

/// Redact a serialized JSON payload. Malformed input is treated as ordinary
/// text and still receives shape-based scrubbing; persistence never fails open.
#[must_use]
pub fn redact_json_text(input: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(mut value) => {
            redact_json(&mut value);
            serde_json::to_string(&value).unwrap_or_else(|_| redact_secrets(input))
        },
        Err(_) => redact_secrets(input),
    }
}

/// Sanitize a URL for display or storage without changing the transport URL.
#[must_use]
pub fn sanitize_url_for_display(input: &str) -> String {
    let Ok(url) = url::Url::parse(input) else {
        return redact_secrets(input);
    };
    sanitize_parsed_url(url)
}

fn sanitize_parsed_url(mut url: url::Url) -> String {
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);

    let path = url.path().to_string();
    let redacted_path = redact_secret_shapes(&path);
    if redacted_path != path {
        url.set_path(&redacted_path);
    }

    if url.query().is_some() {
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(key, value)| {
                let value = if sensitive_query_key(&key) || redact_secrets(&value) != value {
                    "[REDACTED]".to_string()
                } else {
                    value.into_owned()
                };
                (key.into_owned(), value)
            })
            .collect();
        url.set_query(None);
        if !pairs.is_empty() {
            url.query_pairs_mut().extend_pairs(pairs);
        }
    }

    url.to_string()
}

fn sensitive_query_key(key: &str) -> bool {
    let normalized = normalize_credential_key(key);
    key_names_credential(key)
        || matches!(
            normalized.as_str(),
            "auth"
                | "authorization"
                | "code"
                | "googleaccessid"
                | "google_access_id"
                | "jwt"
                | "key"
                | "key_pair_id"
                | "policy"
                | "sig"
                | "signature"
        )
        || normalized.ends_with("_signature")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Over-redaction is the quieter failure: a redactor that mangles ordinary
    /// prose corrupts every log and transcript it touches. These four shapes
    /// are the ones that bait a secret scanner — a variable named `token`, a
    /// hex-looking commit sha, and a `path:line` pair with a colon in it.
    ///
    /// Migrated from `mermaid-model`'s `utils/redact.rs`, which was a pure
    /// re-export facade whose tests exercised this file from a crate that only
    /// forwarded to it. It was the only negative assertion in either suite.
    #[test]
    fn ordinary_text_is_left_untouched() {
        for ordinary in [
            "the quick brown fox",
            "let token_count = 42;",
            "commit a614aa9f deploys the fix",
            "see src/providers/tool/exec.rs:855",
        ] {
            assert_eq!(redact_secrets(ordinary), ordinary);
        }
    }

    #[test]
    fn serialized_payload_redaction_covers_signed_urls_and_fetched_secrets() {
        let input = serde_json::json!({
            "url": "https://user:password@example.test/a?GoogleAccessId=opaque-id&Signature=opaque-signature#fragment",
            "model_content": "OPENAI_API_KEY=sk-abcdefghijklmnop1234; see (https://alice:hunter2@example.test/page?q=1#private-state).",
            "authorization": "opaque-bearer-value"
        })
        .to_string();
        let redacted = redact_json_text(&input);
        for secret in [
            "user",
            "password",
            "opaque-id",
            "opaque-signature",
            "fragment",
            "sk-abcdefghijklmnop1234",
            "opaque-bearer-value",
            "alice",
            "hunter2",
            "private-state",
        ] {
            assert!(!redacted.contains(secret), "leaked {secret}: {redacted}");
        }
        assert!(redacted.contains("REDACTED"));
    }

    #[test]
    fn structured_credentials_are_redacted_regardless_of_value_type_or_length() {
        let mut value = serde_json::json!({
            "password": "abc",
            "token": 12345,
            "nested": {
                "client_secret": true,
                "accessToken": 67890,
                "apiKey": false
            },
            "nullable_token": null,
            "ordinary": 12345,
            "max_tokens": 4096,
            "promptTokens": 2048,
            "token_count": 3,
            "key": "Enter"
        });
        redact_json(&mut value);
        assert_eq!(value["password"], "[REDACTED]");
        assert_eq!(value["token"], "[REDACTED]");
        assert_eq!(value["nested"]["client_secret"], "[REDACTED]");
        assert_eq!(value["nested"]["accessToken"], "[REDACTED]");
        assert_eq!(value["nested"]["apiKey"], "[REDACTED]");
        assert!(value["nullable_token"].is_null());
        assert_eq!(value["ordinary"], 12345);
        assert_eq!(value["max_tokens"], 4096);
        assert_eq!(value["promptTokens"], 2048);
        assert_eq!(value["token_count"], 3);
        assert_eq!(value["key"], "Enter");
    }

    #[test]
    fn embedded_short_basic_and_private_key_secrets_are_fully_redacted() {
        let input = "password=abc\nAuthorization: Basic dXNlcjphYmM=\n\
                     -----BEGIN PRIVATE KEY-----\n\
                     cHJpdmF0ZS1tYXRlcmlhbA==\n\
                     -----END PRIVATE KEY-----";
        let redacted = redact_secrets(input);
        for secret in ["password=abc", "dXNlcjphYmM=", "cHJpdmF0ZS1tYXRlcmlhbA=="] {
            assert!(!redacted.contains(secret), "leaked {secret}: {redacted}");
        }
        assert!(redacted.contains("password=[REDACTED]"));
        assert!(redacted.contains("Authorization: Basic [REDACTED]"));
        assert!(redacted.contains("[REDACTED PRIVATE KEY]"));
        assert!(!redacted.contains("-----END PRIVATE KEY-----"));

        let truncated =
            redact_secrets("before\n-----BEGIN EC PRIVATE KEY-----\ncHJpdmF0ZS1tYXRlcmlhbA==");
        assert_eq!(truncated, "before\n[REDACTED PRIVATE KEY]");

        let uppercase_url =
            redact_secrets("see HTTPS://alice:pw@example.test/path?token=abc#private-state");
        for secret in ["alice", "pw", "abc", "private-state"] {
            assert!(
                !uppercase_url.contains(secret),
                "uppercase URL leaked {secret}: {uppercase_url}"
            );
        }
    }

    #[test]
    fn quoted_assignments_and_authorization_schemes_are_fully_redacted() {
        let input = "password=\"alpha beta\"\n\
                     client_secret='gamma delta'\n\
                     Authorization: Digest username=\"alice\", response=\"digest-secret\"\n\
                     Proxy-Authorization: Custom opaque proxy credential\n\
                     Authorization is required for this operation.";
        let redacted = redact_secrets(input);

        for secret in [
            "alpha beta",
            "gamma delta",
            "alice",
            "digest-secret",
            "opaque proxy credential",
        ] {
            assert!(!redacted.contains(secret), "leaked {secret}: {redacted}");
        }
        assert!(redacted.contains("password=[REDACTED]"));
        assert!(redacted.contains("client_secret=[REDACTED]"));
        assert!(redacted.contains("Authorization: Digest [REDACTED]"));
        assert!(redacted.contains("Proxy-Authorization: Custom [REDACTED]"));
        assert!(redacted.contains("Authorization is required for this operation."));
    }

    #[test]
    fn provider_signed_url_credentials_are_sanitized() {
        for url in [
            "https://storage.example/object?X-Goog-Credential=acct%2Fscope&X-Goog-Signature=abcdef&X-Goog-Date=20260721T000000Z",
            "https://s3.example/object?X-Amz-Credential=acct%2Fscope&X-Amz-Signature=abcdef&X-Amz-Security-Token=session-token&response-content-type=text%2Fplain",
        ] {
            let sanitized = sanitize_url_for_display(url);
            assert!(!sanitized.contains("acct"), "{sanitized}");
            assert!(!sanitized.contains("abcdef"), "{sanitized}");
            assert!(!sanitized.contains("session-token"), "{sanitized}");
            assert!(sanitized.contains("REDACTED"), "{sanitized}");
        }
    }

    #[test]
    fn camel_case_query_credentials_are_sanitized_without_hiding_ordinary_values() {
        let sanitized = sanitize_url_for_display(
            "https://example.test/callback?accessToken=opaque-access&clientSecret=opaque-client&ordinary=visible",
        );

        assert!(!sanitized.contains("opaque-access"), "{sanitized}");
        assert!(!sanitized.contains("opaque-client"), "{sanitized}");
        assert!(sanitized.contains("ordinary=visible"), "{sanitized}");
    }

    #[test]
    fn exact_url_paths_and_short_bearer_values_are_shape_redacted() {
        let token = "sk-abcdefghijklmnop1234";
        let sanitized = sanitize_url_for_display(&format!("https://example.test/download/{token}"));
        assert!(!sanitized.contains(token), "URL path leaked: {sanitized}");
        assert!(sanitized.contains("REDACTED"));
        assert_eq!(
            redact_secrets("Authorization: Bearer abc"),
            "Authorization: Bearer [REDACTED]"
        );
    }
}
