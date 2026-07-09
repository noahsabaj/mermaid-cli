//! Integration coverage for the `mermaid run --format ndjson` event stream.
//!
//! Structural and model-independent: whether the underlying model call succeeds
//! or (as here) fails fast on a missing key, the stream must open with
//! `session_started` and close with `result`, one JSON object per line. This
//! guards the public SDK contract end-to-end through the real binary.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn sandbox_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mermaid-ndjson-{}-{}", std::process::id(), nonce));
    std::fs::create_dir_all(&dir).expect("create sandbox dir");
    dir
}

#[test]
fn run_ndjson_stream_opens_with_session_started_and_ends_with_result() {
    let home = sandbox_dir();
    // Keyless model id → the anthropic provider errors fast on a missing key,
    // so the run terminates without a network call. The assertions below hold
    // regardless of success/failure: only the stream's shape is under test.
    let output = Command::new(env!("CARGO_BIN_EXE_mermaid"))
        .args([
            "--model",
            "anthropic/pty-exit-test",
            "run",
            "--format",
            "ndjson",
            "hi",
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .expect("spawn mermaid");

    let _ = std::fs::remove_dir_all(&home);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        lines.len() >= 2,
        "expected at least session_started + result, got: {stdout:?}"
    );

    // Every line is a JSON object carrying a `type` tag.
    for line in &lines {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stream line is not JSON ({e}): {line:?}"));
        assert!(
            value.get("type").and_then(|t| t.as_str()).is_some(),
            "stream line missing a `type` tag: {line:?}"
        );
    }

    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(
        first["type"], "session_started",
        "first line must open the stream"
    );
    assert_eq!(
        first["protocol_version"], 1,
        "protocol version must be pinned"
    );

    let last: serde_json::Value = serde_json::from_str(lines[lines.len() - 1]).unwrap();
    assert_eq!(
        last["type"], "result",
        "last line must be the terminal result"
    );
}
