//! Integration coverage for the `mermaid run --format ndjson` event stream
//! and the session-addressable headless flow (`--resume <id>` / `--continue`).
//!
//! Structural and model-independent: whether the underlying model call succeeds
//! or (as here) fails fast on a missing key, the stream must open with
//! `session_started` and close with `result`, one JSON object per line — and
//! the emitted session id must point at a real conversation file that a second
//! `--resume <id>` run appends to. This guards the public SDK contract
//! end-to-end through the real binary.

use std::path::{Path, PathBuf};
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

/// Run the real binary keylessly (the anthropic provider errors fast on the
/// missing key — no network), fully sandboxed: config/data under `home`, and
/// the working directory under `home/work` so the run's `.mermaid/` session
/// store never touches the repo checkout. The `MERMAID_*` overrides are what
/// isolate on Windows, where the HOME/XDG vars don't move the known-folder
/// platform dirs — before them, every Windows `cargo test` wrote
/// `last_used_model = "anthropic/pty-exit-test"` into the developer's real
/// `config.toml`.
fn run_sandboxed(home: &Path, extra_args: &[&str]) -> std::process::Output {
    run_sandboxed_with_model(home, "anthropic/pty-exit-test", extra_args)
}

fn run_sandboxed_with_model(home: &Path, model: &str, extra_args: &[&str]) -> std::process::Output {
    let work = home.join("work");
    std::fs::create_dir_all(&work).expect("create workdir");
    let mut args = vec!["--model", model];
    args.extend_from_slice(extra_args);
    Command::new(env!("CARGO_BIN_EXE_mermaid"))
        .args(&args)
        .current_dir(&work)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("MERMAID_CONFIG_DIR", sandbox_config_dir(home))
        .env("MERMAID_DATA_DIR", home.join("data").join("mermaid"))
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .expect("spawn mermaid")
}

/// Where the sandboxed run's user config lives — the dir `MERMAID_CONFIG_DIR`
/// pins, so tests can assert on exactly what the binary wrote.
fn sandbox_config_dir(home: &Path) -> PathBuf {
    home.join("config").join("mermaid")
}

/// Parse the non-empty NDJSON lines of a run's stdout.
fn ndjson_lines(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("stream line is not JSON ({e}): {line:?}"))
        })
        .collect()
}

#[test]
fn run_ndjson_stream_opens_with_session_started_and_ends_with_result() {
    let home = sandbox_dir();
    let output = run_sandboxed(&home, &["run", "--format", "ndjson", "hi"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = ndjson_lines(&stdout);
    assert!(
        lines.len() >= 2,
        "expected at least session_started + result, got: {stdout:?}"
    );
    for value in &lines {
        assert!(
            value.get("type").and_then(|t| t.as_str()).is_some(),
            "stream line missing a `type` tag: {value:?}"
        );
    }

    let first = &lines[0];
    assert_eq!(
        first["type"], "session_started",
        "first line must open the stream"
    );
    assert_eq!(
        first["protocol_version"], 1,
        "protocol version must be pinned"
    );

    let last = &lines[lines.len() - 1];
    assert_eq!(
        last["type"], "result",
        "last line must be the terminal result"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn run_emits_session_id_that_resumes_the_same_session() {
    let home = sandbox_dir();

    // First run: capture the emitted session id and confirm the file exists.
    let output = run_sandboxed(&home, &["run", "--format", "ndjson", "hi"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = ndjson_lines(&stdout);
    let session_id = lines[0]["session_id"]
        .as_str()
        .expect("session_started carries session_id")
        .to_string();
    // The id has the store's shape (YYYYMMDD_HHMMSS_mmm, 19 chars).
    assert_eq!(session_id.len(), 19, "unexpected id shape: {session_id}");
    // The terminal result repeats it.
    assert_eq!(
        lines[lines.len() - 1]["session_id"].as_str(),
        Some(session_id.as_str())
    );
    // Even this errored run persisted the session file (the emitted id must
    // never dangle).
    let conversation = home
        .join("work")
        .join(".mermaid")
        .join("conversations")
        .join(format!("{session_id}.json"));
    assert!(
        conversation.exists(),
        "session file missing: {}",
        conversation.display()
    );
    let first_len = std::fs::metadata(&conversation).expect("stat").len();

    // Second run resumes the SAME id and appends to the same file.
    let output = run_sandboxed(
        &home,
        &[
            "--resume",
            &session_id,
            "run",
            "--format",
            "ndjson",
            "again",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = ndjson_lines(&stdout);
    assert_eq!(
        lines[0]["session_id"].as_str(),
        Some(session_id.as_str()),
        "resumed run must keep the session id"
    );
    let second_len = std::fs::metadata(&conversation).expect("stat").len();
    assert!(
        second_len > first_len,
        "resumed run must append to the session file ({first_len} -> {second_len})"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn headless_resume_error_paths_are_clear() {
    let home = sandbox_dir();

    // A named-but-missing session id fails hard, naming the id.
    let output = run_sandboxed(&home, &["--resume", "19990101_000000_000", "run", "x"]);
    assert!(
        !output.status.success(),
        "missing session id must fail the run"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("19990101_000000_000"),
        "error must name the id: {stderr}"
    );

    // Bare --resume before `run` — clap's greedy value parsing swallows `run`
    // as the session id (a documented quirk of `--resume [SESSION_ID]`), so
    // this fails at parse or at load. Either way: non-zero, never a silent
    // fresh session.
    let output = run_sandboxed(&home, &["--resume", "run", "x"]);
    assert!(!output.status.success(), "bare --resume must fail headless");

    // --continue with no saved session must not silently start fresh.
    let output = run_sandboxed(&home, &["--continue", "run", "x"]);
    assert!(
        !output.status.success(),
        "--continue with no session must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no saved session"),
        "error must explain: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// The matched pair for the `last_used_model` persist gate, through the real
/// binary. Negative: a `--model` whose provider cannot be built must leave the
/// user config without a `last_used_model` — this is the write that poisoned
/// developer configs with `anthropic/pty-exit-test` before the gate. Positive:
/// a keyless loopback provider IS buildable, so the same code path must write
/// the key — proving the gate discriminates rather than never persisting, and
/// that `MERMAID_CONFIG_DIR` really is where the binary's config writes land.
/// No network beyond one refused loopback connect: the provider endpoint
/// resolves without contacting anything, and the chat then fails fast.
#[test]
fn cli_model_is_remembered_only_when_its_provider_is_buildable() {
    let home = sandbox_dir();
    let config_path = sandbox_config_dir(&home).join("config.toml");

    // Negative: unknown provider — the endpoint cannot resolve, so nothing
    // may be remembered (the run itself fails; its exit status is the
    // provider error's concern, not this test's).
    let _ = run_sandboxed_with_model(&home, "nosuch/never-persist", &["run", "x"]);
    let written = std::fs::read_to_string(&config_path).unwrap_or_default();
    assert!(
        !written.contains("last_used_model"),
        "an unbuildable provider must not be remembered: {written}"
    );

    // Positive: a keyless loopback endpoint is a real, buildable provider
    // (same rule as discovery), so the persist fires even though the chat
    // then dies on the dead port.
    std::fs::create_dir_all(sandbox_config_dir(&home)).expect("create config dir");
    std::fs::write(
        &config_path,
        "[providers.stub]\nbase_url = \"http://127.0.0.1:9/v1\"\n",
    )
    .expect("write sandbox config");
    let _ = run_sandboxed_with_model(&home, "stub/test-model", &["run", "x"]);
    let written = std::fs::read_to_string(&config_path).expect("config must exist");
    assert!(
        written.contains("last_used_model = \"stub/test-model\""),
        "a buildable provider must be remembered in the sandbox: {written}"
    );

    let _ = std::fs::remove_dir_all(&home);
}
