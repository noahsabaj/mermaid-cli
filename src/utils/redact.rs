//! Secret redaction for persisted/recorded text (Cause 7).
//!
//! Anything we write to disk or replay logs — the `--record` JSONL stream,
//! durable memory files, compaction summaries, surfaced upstream errors — can
//! incidentally carry a credential the model just read (`read_file .env`, an
//! API error echoing a key, a pasted token). [`redact_secrets`] scrubs the
//! common secret shapes before that text is persisted.
//!
//! Deliberately high-precision (known key prefixes, `Authorization: Bearer`,
//! and `SECRET_NAME=value` where the name matches the same patterns as the
//! env-scrubber in `providers::tool::exec`). Ordinary prose, code, and tool
//! output pass through untouched — over-redaction would make the record/replay
//! and memory features useless.

use std::sync::LazyLock;

use regex::Regex;

/// `(pattern, replacement)` pairs. `replacement` may reference capture groups
/// with `${1}` so a labelled secret keeps its label (`API_KEY=[REDACTED]`)
/// while the value is scrubbed.
static SECRET_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let p = |re: &str| Regex::new(re).expect("static redaction regex must compile");
    vec![
        // NAME=VALUE / NAME: VALUE where NAME looks credential-bearing. Mirrors
        // the `is_secret_env_name` denylist patterns in `providers::tool::exec`.
        // The value must be >= 6 chars so we don't scrub `token_count = 42`
        // (a credential-named identifier in arithmetic/config), and we consume an
        // optional closing quote so `KEY: 'value'` doesn't leave a dangling `'`.
        (
            p(
                r#"(?i)\b([A-Z0-9_]*(?:API[_-]?KEY|APIKEY|SECRET|TOKEN|PASSWORD|PASSWD|ACCESS[_-]?KEY|PRIVATE[_-]?KEY|CREDENTIALS?)[A-Z0-9_]*)\s*[:=]\s*["']?[^\s"']{6,}["']?"#,
            ),
            "${1}=[REDACTED]",
        ),
        // Authorization: Bearer <token>
        (
            p(r#"(?i)\b(Bearer)\s+[A-Za-z0-9._\-]{8,}"#),
            "${1} [REDACTED]",
        ),
        // Provider key prefixes (high-precision shapes).
        (p(r#"\bsk-(?:ant-)?[A-Za-z0-9._\-]{12,}"#), "[REDACTED]"), // OpenAI / Anthropic
        (p(r#"\bAKIA[0-9A-Z]{16}\b"#), "[REDACTED]"),               // AWS access key id
        (p(r#"\bgh[pousr]_[A-Za-z0-9]{20,}\b"#), "[REDACTED]"),     // GitHub tokens
        (p(r#"\bxox[baprs]-[A-Za-z0-9-]{10,}\b"#), "[REDACTED]"),   // Slack
        (p(r#"\bAIza[0-9A-Za-z._\-]{20,}\b"#), "[REDACTED]"),       // Google API key
    ]
});

/// Replace credential-shaped substrings of `input` with `[REDACTED]`.
pub fn redact_secrets(input: &str) -> String {
    let mut out = std::borrow::Cow::Borrowed(input);
    for (re, repl) in SECRET_PATTERNS.iter() {
        if re.is_match(&out) {
            out = std::borrow::Cow::Owned(re.replace_all(&out, *repl).into_owned());
        }
    }
    out.into_owned()
}

/// Walk a JSON value and redact every string leaf in place. The recorder uses
/// this to scrub whole structured payloads at the single write choke point.
pub fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            let red = redact_secrets(s);
            if &red != s {
                *s = red;
            }
        },
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json),
        serde_json::Value::Object(map) => map.values_mut().for_each(redact_json),
        _ => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_named_secrets() {
        assert_eq!(
            redact_secrets("OPENAI_API_KEY=sk-abcdefghijklmnop1234"),
            "OPENAI_API_KEY=[REDACTED]"
        );
        assert_eq!(
            redact_secrets("export AWS_SECRET_ACCESS_KEY: 'wJalrXUtnFEMI/K7MDENG'"),
            "export AWS_SECRET_ACCESS_KEY=[REDACTED]"
        );
        assert_eq!(redact_secrets("password = hunter2"), "password=[REDACTED]");
        // CLI-arg form — the new #93 usage context (MCP server args / stderr).
        assert_eq!(
            redact_secrets("--api-key=sk-abcdefghijklmnop1234"),
            "--api-key=[REDACTED]"
        );
    }

    #[test]
    fn redacts_known_prefixes_and_bearer() {
        assert_eq!(
            redact_secrets("Authorization: Bearer abcdef123456ghijkl"),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(
            redact_secrets("key sk-ant-api03-abcdefghijklmnop"),
            "key [REDACTED]"
        );
        assert_eq!(
            redact_secrets("AKIAIOSFODNN7EXAMPLE here"),
            "[REDACTED] here"
        );
        assert_eq!(
            redact_secrets("token ghp_0123456789abcdefghijABCDEFGHIJ012345"),
            "token [REDACTED]"
        );
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        // No false positives on normal prose / code / hashes that aren't keys.
        for ok in [
            "the quick brown fox",
            "let token_count = 42;",
            "commit a614aa9f deploys the fix",
            "see src/providers/tool/exec.rs:855",
        ] {
            assert_eq!(redact_secrets(ok), ok, "should not redact: {ok}");
        }
    }

    #[test]
    fn redact_json_scrubs_string_leaves() {
        let mut v = serde_json::json!({
            "text": "OPENAI_API_KEY=sk-abcdefghijklmnop1234",
            "count": 7,
            "nested": ["safe", "Bearer abcdef123456ghijkl"],
        });
        redact_json(&mut v);
        assert_eq!(v["text"], "OPENAI_API_KEY=[REDACTED]");
        assert_eq!(v["count"], 7);
        assert_eq!(v["nested"][0], "safe");
        assert_eq!(v["nested"][1], "Bearer [REDACTED]");
    }
}
