//! Smoke coverage for `mermaid feedback` through the real binary: the bundle
//! renders, states its local-only contract, lands with owner-only permissions
//! in file mode, and never carries a planted secret (the ring redacts at
//! capture, the summary is names-only, and the final whole-document
//! `redact_secrets` pass backstops both).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn sandbox_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("mermaid-feedback-{}-{}", std::process::id(), nonce));
    std::fs::create_dir_all(&dir).expect("create sandbox dir");
    dir
}

/// Run the real binary fully sandboxed: config/data under `home`, working
/// directory under `home/work` so the bundle file never lands in the checkout.
/// The `MERMAID_*` overrides are what isolate on Windows, where the HOME/XDG
/// vars don't move the known-folder platform dirs.
fn run_sandboxed(home: &Path, extra_args: &[&str]) -> std::process::Output {
    let work = home.join("work");
    std::fs::create_dir_all(&work).expect("create workdir");
    Command::new(env!("CARGO_BIN_EXE_mermaid"))
        .args(extra_args)
        .current_dir(&work)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("MERMAID_CONFIG_DIR", home.join("config").join("mermaid"))
        .env("MERMAID_DATA_DIR", home.join("data").join("mermaid"))
        // A planted credential-shaped env var: it must never surface in the
        // bundle (the summary reports key PRESENCE only).
        .env("OPENAI_API_KEY", "sk-plantedsecret1234567890abcd")
        .output()
        .expect("spawn mermaid")
}

#[test]
fn feedback_stdout_renders_the_bundle_without_secrets() {
    let home = sandbox_dir();
    let output = run_sandboxed(&home, &["feedback", "--stdout"]);
    assert!(
        output.status.success(),
        "feedback --stdout must exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("# Mermaid Feedback Bundle"), "{stdout}");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "bundle must carry the CLI version"
    );
    assert!(
        stdout.contains("never uploaded"),
        "the local-only contract must be stated in the bundle"
    );
    assert!(stdout.contains("## Doctor"), "{stdout}");
    assert!(stdout.contains("## Log tail"), "{stdout}");
    assert!(
        !stdout.contains("sk-plantedsecret1234567890abcd"),
        "a planted key must never surface in the bundle"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn feedback_writes_an_owner_only_file_in_cwd() {
    let home = sandbox_dir();
    let output = run_sandboxed(&home, &["feedback"]);
    assert!(
        output.status.success(),
        "feedback must exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("review before sharing"),
        "file mode must print the review reminder: {stdout}"
    );
    let work = home.join("work");
    let bundle = std::fs::read_dir(&work)
        .expect("read workdir")
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("mermaid-feedback-") && n.ends_with(".md"))
        })
        .expect("bundle file written to cwd");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&bundle)
            .expect("stat bundle")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "bundle must be owner-only");
    }
    let text = std::fs::read_to_string(&bundle).expect("read bundle");
    assert!(text.contains("# Mermaid Feedback Bundle"));
    assert!(!text.contains("sk-plantedsecret1234567890abcd"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn feedback_json_format_emits_valid_json() {
    let home = sandbox_dir();
    let output = run_sandboxed(&home, &["feedback", "--stdout", "--format", "json"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("json bundle parses");
    assert_eq!(
        parsed["cli_version"],
        serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string())
    );
    assert!(parsed["doctor"].is_object(), "doctor section embedded");
    // The planted key resolves from env for openai — source only, no value.
    let openai = parsed["config"]["providers"]
        .as_array()
        .expect("providers array")
        .iter()
        .find(|p| p["name"] == "openai")
        .expect("openai row");
    assert_eq!(
        openai["key_source"],
        serde_json::Value::String("env".into())
    );
    assert!(!stdout.contains("sk-plantedsecret1234567890abcd"));
    let _ = std::fs::remove_dir_all(&home);
}
