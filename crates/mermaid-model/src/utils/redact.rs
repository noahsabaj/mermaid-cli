//! CLI-facing exports for the runtime's mandatory redaction implementation.
//!
//! The implementation lives in `mermaid-runtime` so every SQLite repository
//! enforces the same rules even when called by the daemon or SDK rather than
//! this CLI process.

pub use mermaid_runtime::{redact_json, redact_secrets, sanitize_url_for_display};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_named_secrets_and_known_shapes() {
        assert_eq!(
            redact_secrets("OPENAI_API_KEY=sk-abcdefghijklmnop1234"),
            "OPENAI_API_KEY=[REDACTED]"
        );
        assert_eq!(
            redact_secrets("Authorization: Bearer abcdef123456ghijkl"),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(
            redact_secrets("token ghp_0123456789abcdefghijABCDEFGHIJ012345"),
            "token [REDACTED]"
        );
    }

    #[test]
    fn sanitizes_url_userinfo_fragment_and_signed_query_values() {
        let sanitized = sanitize_url_for_display(
            "https://alice:hunter2@example.test/path?q=rust&X-Amz-Signature=opaque#private",
        );
        for secret in ["alice", "hunter2", "opaque", "private"] {
            assert!(!sanitized.contains(secret), "leaked {secret}: {sanitized}");
        }
        assert!(sanitized.contains("q=rust"));
        assert!(sanitized.contains("X-Amz-Signature=%5BREDACTED%5D"));

        let gcs = sanitize_url_for_display(
            "https://storage.example.test/object?GoogleAccessId=opaque-id&Signature=opaque-sig",
        );
        assert!(!gcs.contains("opaque-id"));
        assert!(!gcs.contains("opaque-sig"));
    }

    #[test]
    fn redact_json_scrubs_credential_keys_urls_and_string_leaves() {
        let mut value = serde_json::json!({
            "api_key": "opaque-value-123",
            "key": "id",
            "url": "https://user:password@example.test/a?token=opaque-token#fragment",
            "nested": ["safe", "Bearer abcdef123456ghijkl"]
        });
        redact_json(&mut value);
        assert_eq!(value["api_key"], "[REDACTED]");
        assert_eq!(value["key"], "id");
        let url = value["url"].as_str().unwrap();
        for secret in ["user", "password", "opaque-token", "fragment"] {
            assert!(!url.contains(secret), "leaked {secret}: {url}");
        }
        assert_eq!(value["nested"][0], "safe");
        assert_eq!(value["nested"][1], "Bearer [REDACTED]");
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        for ordinary in [
            "the quick brown fox",
            "let token_count = 42;",
            "commit a614aa9f deploys the fix",
            "see src/providers/tool/exec.rs:855",
        ] {
            assert_eq!(redact_secrets(ordinary), ordinary);
        }
    }
}
