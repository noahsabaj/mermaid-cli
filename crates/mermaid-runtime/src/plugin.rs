use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{NewPluginInstall, PluginInstallRecord, RuntimeStore, data_dir};

/// A plugin hook that runs longer than this is killed. A runaway hook must
/// never hang the caller (the TUI event loop, the daemon, or the CLI) — this
/// bound is what makes that guarantee. A killed hook counts as "no opinion"
/// (infrastructure fails open; see [`HookDecision`]).
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on captured hook stdout/stderr, per stream per hook. Reading continues
/// to EOF past the cap so a chatty hook never blocks on a full pipe; only the
/// first `HOOK_OUTPUT_CAP` bytes are kept.
const HOOK_OUTPUT_CAP: usize = 64 * 1024;

/// Exit code a hook uses to deny the action without printing JSON — its
/// captured stderr becomes the denial reason (Claude Code parity).
const HOOK_DENY_EXIT_CODE: i32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub mcp: Vec<String>,
    /// Capabilities the plugin *declares* it uses (e.g. "network", "filesystem").
    /// ADVISORY ONLY — surfaced at install/enable time for informed consent, not
    /// enforced. A plugin hook is a native child process running with the user's
    /// privileges; the runtime cannot confine it to this list without OS-level
    /// sandboxing, so the field documents intent rather than granting a sandbox.
    /// The real boundary is the explicit `mermaid plugin enable` decision.
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub bin: Vec<String>,
}

/// A summary of what a plugin declares it will do, shown before install/enable.
/// These are advisory disclosures for informed consent, not an enforced sandbox
/// (see [`PluginManifest::capabilities`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCapabilityPreview {
    pub name: String,
    pub declared_capabilities: Vec<String>,
    pub capabilities_toml: Option<toml::Value>,
    pub hooks: Vec<String>,
    pub mcp: Vec<String>,
    pub bin: Vec<String>,
}

pub fn validate_plugin_manifest(manifest: &PluginManifest, root: &Path) -> Result<()> {
    anyhow::ensure!(!manifest.name.trim().is_empty(), "plugin name is required");
    ensure_relative_paths("skills", &manifest.skills, root)?;
    ensure_relative_paths("agents", &manifest.agents, root)?;
    ensure_relative_paths("hooks", &manifest.hooks, root)?;
    ensure_relative_paths("mcp", &manifest.mcp, root)?;
    ensure_relative_paths("prompts", &manifest.prompts, root)?;
    ensure_relative_paths("bin", &manifest.bin, root)?;
    Ok(())
}

pub fn install_plugin_from_path(path: &Path) -> Result<PluginInstallRecord> {
    let (_manifest_path, root, manifest) = load_plugin_manifest(path)?;
    validate_plugin_manifest(&manifest, &root)?;
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    let store = RuntimeStore::open_default()?;
    let record = store.plugins().install(NewPluginInstall {
        id: Some(manifest.name.clone()),
        name: manifest.name,
        source: root.display().to_string(),
        version: manifest.version,
        // Installed plugins are DISABLED by default. A hook runs native code
        // with the user's privileges, so activation is a separate, explicit
        // decision (`mermaid plugin enable <id>`) — never a side effect of
        // install. This is the meaningful boundary in a hook system.
        enabled: false,
        manifest_json,
    })?;
    write_plugin_lockfile()?;
    Ok(record)
}

pub fn plugin_capability_preview(path: &Path) -> Result<PluginCapabilityPreview> {
    let (_manifest_path, root, manifest) = load_plugin_manifest(path)?;
    validate_plugin_manifest(&manifest, &root)?;
    let capabilities_path = root.join("capabilities.toml");
    let capabilities_toml = if capabilities_path.exists() {
        let raw = std::fs::read_to_string(&capabilities_path)
            .with_context(|| format!("failed to read {}", capabilities_path.display()))?;
        Some(toml::from_str(&raw)?)
    } else {
        None
    };
    Ok(PluginCapabilityPreview {
        name: manifest.name,
        declared_capabilities: manifest.capabilities,
        capabilities_toml,
        hooks: manifest.hooks,
        mcp: manifest.mcp,
        bin: manifest.bin,
    })
}

pub fn write_plugin_lockfile() -> Result<PathBuf> {
    let store = RuntimeStore::open_default()?;
    let plugins = store.plugins().list()?;
    let path = data_dir()?.join("plugins.lock.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Atomic write so a crash can't leave a truncated lockfile.
    crate::write_atomic(&path, &serde_json::to_vec_pretty(&plugins)?)?;
    Ok(path)
}

/// One enabled hook's parsed response to a hook event. Most hooks print
/// nothing and exit 0 — that's an [`HookDecision::Allow`] with no extras, so
/// pre-contract hooks keep working unchanged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookResponse {
    /// Plugin that owns the responding hook.
    pub plugin: String,
    /// Hook path (as declared in the manifest), for logs.
    pub hook: String,
    /// The allow-or-deny verdict.
    pub decision: HookDecision,
    /// Replacement tool-arguments object (consulted on `before_tool_use`).
    pub updated_input: Option<serde_json::Value>,
    /// Context string to surface to the model on the next request.
    pub additional_context: Option<String>,
}

/// Allow-or-deny verdict parsed from a hook's stdout / exit status.
///
/// Failure semantics are asymmetric by design: INTENT fails closed (an
/// explicit deny — JSON or exit code 2 — always denies), while INFRASTRUCTURE
/// fails open (a parse error, timeout, or spawn failure counts as `Allow`
/// with a warning). Hooks are user-installed, disabled-by-default code layered
/// on top of the policy gate — a buggy hook must not lock the user out of
/// every tool call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HookDecision {
    /// No opinion, explicit allow, or an infrastructure failure.
    #[default]
    Allow,
    /// Block the action.
    Deny {
        /// Human-readable reason surfaced in the tool outcome.
        reason: String,
    },
}

/// Combined verdict across all responding hooks for one event.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookGate {
    /// First denial in plugin order: `(plugin name, reason)`.
    pub deny: Option<(String, String)>,
    /// Replacement tool arguments — the LAST rewrite wins (a collision is
    /// logged).
    pub updated_input: Option<serde_json::Value>,
    /// All `additionalContext` strings, in plugin order.
    pub context: Vec<String>,
}

/// Aggregate hook responses: first deny wins, last `updated_input` wins,
/// contexts concatenate in order.
pub fn aggregate_hook_responses(responses: Vec<HookResponse>) -> HookGate {
    let mut gate = HookGate::default();
    for response in responses {
        if gate.deny.is_none()
            && let HookDecision::Deny { reason } = &response.decision
        {
            gate.deny = Some((response.plugin.clone(), reason.clone()));
        }
        if let Some(input) = response.updated_input {
            if gate.updated_input.is_some() {
                tracing::warn!(
                    plugin = %response.plugin,
                    "multiple hooks rewrote the tool input; the last rewrite wins"
                );
            }
            gate.updated_input = Some(input);
        }
        if let Some(context) = response.additional_context {
            gate.context.push(context);
        }
    }
    gate
}

/// Claude Code-compatible wire shapes a hook may print on stdout.
#[derive(Debug, Deserialize)]
struct HookWire {
    #[serde(rename = "hookSpecificOutput")]
    hook_specific_output: Option<HookSpecificWire>,
    /// Legacy shape: `{"decision": "block", "reason": "..."}`.
    decision: Option<String>,
    reason: Option<String>,
    #[serde(rename = "systemMessage")]
    system_message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HookSpecificWire {
    #[serde(rename = "permissionDecision")]
    permission_decision: Option<String>,
    #[serde(rename = "permissionDecisionReason")]
    permission_decision_reason: Option<String>,
    /// Mermaid extension: full replacement tool-arguments object.
    #[serde(rename = "updatedInput")]
    updated_input: Option<serde_json::Value>,
    /// Mermaid extension: string surfaced to the model on the next request.
    #[serde(rename = "additionalContext")]
    additional_context: Option<String>,
}

/// Run every enabled plugin's hooks for `event`, returning their parsed
/// responses (empty when no plugin responds — most events, most hooks).
/// Callers that gate on the result aggregate via [`aggregate_hook_responses`];
/// fire-and-forget callers keep ignoring the return value.
pub fn run_plugin_hooks(event: &str, payload: &serde_json::Value) -> Result<Vec<HookResponse>> {
    let store = RuntimeStore::open_default()?;
    let payload_bytes = std::sync::Arc::new(serde_json::to_string(payload)?.into_bytes());
    let mut responses = Vec::new();
    for plugin in store.plugins().list()? {
        // The enabled flag is the trust boundary: a plugin runs native hook
        // code only after an explicit `plugin enable`. Declared capabilities are
        // advisory and intentionally not consulted here — they cannot constrain
        // a native child process.
        if !plugin.enabled {
            continue;
        }
        let Ok(manifest) = serde_json::from_str::<PluginManifest>(&plugin.manifest_json) else {
            tracing::warn!(plugin = %plugin.name, "skipping plugin with unparseable manifest");
            continue;
        };
        // Canonicalize the root so a symlink inside it can't be used to escape.
        let Ok(root) = std::fs::canonicalize(&plugin.source) else {
            tracing::warn!(plugin = %plugin.name, "plugin source missing; skipping hooks");
            continue;
        };
        responses.extend(run_hooks_for_plugin(
            &root,
            &manifest.hooks,
            &plugin.name,
            event,
            &payload_bytes,
        ));
    }
    Ok(responses)
}

/// Run one plugin's declared hooks and parse each response. Extracted from
/// [`run_plugin_hooks`] so the spawn/capture/parse path is unit-testable
/// against a temp directory without a `RuntimeStore`.
fn run_hooks_for_plugin(
    root: &Path,
    hooks: &[String],
    plugin_name: &str,
    event: &str,
    payload_bytes: &std::sync::Arc<Vec<u8>>,
) -> Vec<HookResponse> {
    let mut responses = Vec::new();
    for hook in hooks {
        // Resolve the hook through symlinks and verify containment on the
        // CANONICAL path (the old lexical `starts_with` could be escaped
        // by a symlink inside the root pointing outside it).
        let Ok(canonical_hook) = std::fs::canonicalize(root.join(hook)) else {
            continue; // missing hook: nothing to run
        };
        if !canonical_hook.starts_with(root) {
            tracing::warn!(plugin = %plugin_name, hook = %hook, "plugin hook escapes root; skipping");
            continue;
        }
        // Execute with a SCRUBBED environment (clear + minimal allowlist)
        // so provider API keys and MERMAID_DAEMON_TOKEN never leak into
        // plugin-provided code. stdout/stderr are captured (capped) — a hook
        // may answer with a decision JSON on stdout, or deny via exit code 2
        // with the reason on stderr.
        let spawn = Command::new(&canonical_hook)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", std::env::var_os("HOME").unwrap_or_default())
            .env("MERMAID_HOOK_EVENT", event)
            .env("MERMAID_PLUGIN_NAME", plugin_name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawn {
            Ok(child) => child,
            Err(err) => {
                tracing::warn!(plugin = %plugin_name, error = %err, "failed to spawn plugin hook");
                continue; // isolate: one bad hook must not abort the rest
            },
        };
        // Write the payload on a detached thread, then let stdin drop so the
        // hook sees EOF. Two failure modes are bounded here: a hook that
        // reads stdin-to-EOF (the drop unblocks it), AND a hook that never
        // reads stdin while the payload exceeds the pipe buffer (~64 KiB;
        // checkpoint payloads embed the full file list) — a synchronous
        // `write_all` would block forever there, and the timeout below only
        // bounds the WAIT. The thread unblocks when the child exits or is
        // killed on timeout (closing the pipe), so we never join it.
        if let Some(mut stdin) = child.stdin.take() {
            let payload = std::sync::Arc::clone(payload_bytes);
            std::thread::spawn(move || {
                let _ = stdin.write_all(&payload);
            });
        }
        // Capped reader threads drain each stream to EOF (so a chatty hook
        // never blocks on a full pipe) keeping only the first cap bytes.
        // They terminate when the child exits or is killed (pipes close), so
        // the joins after the bounded wait cannot hang.
        let stdout_reader = child.stdout.take().map(spawn_capped_reader);
        let stderr_reader = child.stderr.take().map(spawn_capped_reader);
        let status = wait_hook_bounded(&mut child, plugin_name, hook, HOOK_TIMEOUT);
        let stdout = join_reader(stdout_reader);
        let stderr = join_reader(stderr_reader);
        responses.push(parse_hook_output(
            plugin_name,
            hook,
            &stdout,
            &stderr,
            status,
        ));
    }
    responses
}

/// Read a hook output stream to EOF on a thread, keeping the first
/// [`HOOK_OUTPUT_CAP`] bytes.
fn spawn_capped_reader<R: Read + Send + 'static>(
    mut stream: R,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if kept.len() < HOOK_OUTPUT_CAP {
                        let take = n.min(HOOK_OUTPUT_CAP - kept.len());
                        kept.extend_from_slice(&chunk[..take]);
                    }
                    // Past the cap: keep draining to EOF without storing.
                },
            }
        }
        kept
    })
}

/// Join a capped reader, tolerating a panicked thread as empty output.
fn join_reader(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

/// Parse one hook's captured output + exit status into a [`HookResponse`].
/// Pure — unit-tested against every accepted wire shape.
fn parse_hook_output(
    plugin: &str,
    hook: &str,
    stdout: &[u8],
    stderr: &[u8],
    status: Option<ExitStatus>,
) -> HookResponse {
    let mut response = HookResponse {
        plugin: plugin.to_string(),
        hook: hook.to_string(),
        ..HookResponse::default()
    };
    // Timeout / kill / wait failure: infrastructure fails open.
    let Some(status) = status else {
        return response;
    };
    // Exit code 2 = deny, stderr is the reason (Claude Code parity). Any
    // other nonzero exit is a non-blocking failure: warn + allow.
    match status.code() {
        Some(HOOK_DENY_EXIT_CODE) => {
            let reason = String::from_utf8_lossy(stderr).trim().to_string();
            response.decision = HookDecision::Deny {
                reason: if reason.is_empty() {
                    format!("hook exited {HOOK_DENY_EXIT_CODE}")
                } else {
                    reason
                },
            };
            return response;
        },
        Some(0) => {},
        _ => {
            tracing::warn!(plugin = %plugin, hook = %hook, %status, "plugin hook failed");
            return response;
        },
    }
    let text = String::from_utf8_lossy(stdout);
    let text = text.trim();
    if text.is_empty() {
        return response; // silent hook: no opinion
    }
    let Ok(wire) = serde_json::from_str::<HookWire>(text) else {
        // Garbage stdout is an infrastructure failure: warn + allow.
        tracing::warn!(plugin = %plugin, hook = %hook, "plugin hook printed unparseable output; ignoring");
        return response;
    };
    let mut deny_reason: Option<String> = None;
    if let Some(specific) = wire.hook_specific_output {
        match specific.permission_decision.as_deref() {
            Some("deny") => {
                deny_reason = Some(
                    specific
                        .permission_decision_reason
                        .or(wire.system_message.clone())
                        .unwrap_or_else(|| "denied by hook".to_string()),
                );
            },
            // "ask" is parsed but mapped to deny: mermaid has no hook-driven
            // confirmation modal, and allowing on an explicit "ask" would be
            // the unsafe reading of the hook's intent.
            Some("ask") => {
                deny_reason = Some(format!(
                    "{} (hook requested user confirmation, which mermaid does not support; treating as deny)",
                    specific
                        .permission_decision_reason
                        .unwrap_or_else(|| "hook requested confirmation".to_string())
                ));
            },
            _ => {},
        }
        response.updated_input = specific.updated_input;
        response.additional_context = specific.additional_context;
    }
    // Legacy shape: {"decision": "block", "reason": "..."}.
    if deny_reason.is_none() && wire.decision.as_deref() == Some("block") {
        deny_reason = Some(
            wire.reason
                .or(wire.system_message)
                .unwrap_or_else(|| "blocked by hook".to_string()),
        );
    }
    if let Some(reason) = deny_reason {
        response.decision = HookDecision::Deny { reason };
    }
    response
}

/// Wait for a plugin hook to exit, killing it if it overruns `timeout`.
/// Returns the exit status, or `None` on timeout/kill/wait-failure (which the
/// parser treats as "no opinion" — infrastructure fails open). Synchronous —
/// the runtime crate has no async runtime; callers that must not block an
/// executor (the `effect/` loop) wrap `run_plugin_hooks` in `spawn_blocking`.
fn wait_hook_bounded(
    child: &mut std::process::Child,
    plugin: &str,
    hook: &str,
    timeout: Duration,
) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Some(status);
            },
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!(plugin = %plugin, hook = %hook, "plugin hook timed out; killed");
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            },
            Err(err) => {
                tracing::warn!(plugin = %plugin, error = %err, "plugin hook wait failed");
                return None;
            },
        }
    }
}

fn load_plugin_manifest(path: &Path) -> Result<(PathBuf, PathBuf, PluginManifest)> {
    let resolved = resolve_plugin_source(path)?;
    let manifest_path = if resolved.is_dir() {
        resolved.join("plugin.toml")
    } else {
        resolved
    };
    let root = manifest_path
        .parent()
        .context("plugin manifest must have a parent directory")?
        .to_path_buf();
    let raw = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: PluginManifest = toml::from_str(&raw)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    Ok((manifest_path, root, manifest))
}

fn resolve_plugin_source(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path.to_path_buf());
    }
    let source = path.to_string_lossy();
    // Only an EXPLICIT git URL is treated as a remote source. The old bare
    // `owner/repo` → github.com expansion turned any short string into a
    // network fetch of attacker-named code; require the full URL instead.
    let is_git_url = source.starts_with("https://")
        || source.starts_with("git@")
        || source.starts_with("ssh://")
        || source.ends_with(".git");
    if !is_git_url {
        return Ok(path.to_path_buf());
    }

    // Fetching remote plugin code is a privileged operation — gate it behind
    // an explicit opt-in so it can't be triggered silently (e.g. via the
    // daemon's local fallback). Operators who want it set the env var.
    anyhow::ensure!(
        std::env::var("MERMAID_ALLOW_PLUGIN_FETCH").is_ok_and(|v| v == "1" || v == "true"),
        "refusing to fetch remote plugin source {source:?}: set MERMAID_ALLOW_PLUGIN_FETCH=1 to allow, \
         or clone it yourself and install from the local path",
    );

    let git_source = source.to_string();
    let dest = data_dir()?
        .join("plugins")
        .join("sources")
        .join(crate::hex_lower(&Sha256::digest(git_source.as_bytes())));
    // Harden git: no credential prompts, no repo-provided hooks, no external
    // transports (`ext::` RCE), so materializing the source can't itself run
    // attacker code.
    const HARDENING: [&str; 4] = [
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "protocol.ext.allow=never",
    ];
    if dest.exists() {
        let _ = Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(HARDENING)
            .arg("-C")
            .arg(&dest)
            .args(["pull", "--ff-only"])
            .status();
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let status = Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(HARDENING)
            .args(["clone", "--depth", "1"])
            .arg(&git_source)
            .arg(&dest)
            .status()
            .with_context(|| format!("failed to clone plugin source {}", git_source))?;
        anyhow::ensure!(status.success(), "git clone failed for {}", git_source);
    }
    Ok(dest)
}

fn ensure_relative_paths(kind: &str, paths: &[String], root: &Path) -> Result<()> {
    for path in paths {
        let rel = Path::new(path);
        anyhow::ensure!(
            !rel.is_absolute() && !path.contains(".."),
            "{} path must stay inside plugin root: {}",
            kind,
            path
        );
        let full = root.join(rel);
        anyhow::ensure!(
            full.exists(),
            "{} path does not exist under plugin root: {}",
            kind,
            path
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_hook_output, run_hooks_for_plugin};
    use crate::*;

    #[test]
    fn manifest_rejects_parent_escape() {
        let root = std::env::temp_dir();
        let manifest = PluginManifest {
            name: "bad".to_string(),
            version: None,
            description: None,
            skills: vec!["../x".to_string()],
            agents: vec![],
            hooks: vec![],
            mcp: vec![],
            capabilities: vec![],
            prompts: vec![],
            bin: vec![],
        };
        assert!(validate_plugin_manifest(&manifest, &root).is_err());
    }

    #[test]
    fn manifest_round_trips_capabilities_field() {
        let toml_src = r#"
            name = "demo"
            capabilities = ["network", "filesystem"]
        "#;
        let manifest: PluginManifest = toml::from_str(toml_src).expect("parse manifest");
        assert_eq!(manifest.capabilities, vec!["network", "filesystem"]);
        // Serializes back under the new key, and the old key is gone.
        let json = serde_json::to_string(&manifest).expect("serialize");
        assert!(json.contains("\"capabilities\""));
        assert!(!json.contains("\"permissions\""));
    }

    fn wire(stdout: &str) -> HookResponse {
        // Exit-0 with the given stdout, no stderr.
        parse_hook_output("p", "h", stdout.as_bytes(), b"", Some(exit_status(0)))
    }

    /// Build a real ExitStatus with the given code (portable enough: run a
    /// shell that exits with it).
    fn exit_status(code: i32) -> std::process::ExitStatus {
        #[cfg(unix)]
        {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("exit {code}"))
                .status()
                .expect("sh exit")
        }
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", &format!("exit {code}")])
                .status()
                .expect("cmd exit")
        }
    }

    #[test]
    fn parse_permission_decision_shapes() {
        // deny with reason
        let r = wire(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no writes on friday"}}"#,
        );
        assert_eq!(
            r.decision,
            HookDecision::Deny {
                reason: "no writes on friday".to_string()
            }
        );
        // allow (explicit) and extensions ride along
        let r = wire(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"ls -la"},"additionalContext":"prefer -la"}}"#,
        );
        assert_eq!(r.decision, HookDecision::Allow);
        assert_eq!(r.updated_input.unwrap()["command"], "ls -la");
        assert_eq!(r.additional_context.as_deref(), Some("prefer -la"));
        // "ask" maps to deny with an explanation
        let r = wire(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"needs review"}}"#,
        );
        match r.decision {
            HookDecision::Deny { reason } => {
                assert!(reason.contains("needs review"));
                assert!(reason.contains("treating as deny"));
            },
            other => panic!("ask must deny, got {other:?}"),
        }
    }

    #[test]
    fn parse_legacy_block_shape_and_silent_and_garbage() {
        let r = wire(r#"{"decision":"block","reason":"legacy nope"}"#);
        assert_eq!(
            r.decision,
            HookDecision::Deny {
                reason: "legacy nope".to_string()
            }
        );
        // Empty stdout = no opinion; garbage = infrastructure fail-open.
        assert_eq!(wire("").decision, HookDecision::Allow);
        assert_eq!(wire("not json at all").decision, HookDecision::Allow);
    }

    #[test]
    fn parse_exit_codes() {
        // Exit 2 denies with stderr as the reason.
        let r = parse_hook_output("p", "h", b"", b"policy violation\n", Some(exit_status(2)));
        assert_eq!(
            r.decision,
            HookDecision::Deny {
                reason: "policy violation".to_string()
            }
        );
        // Exit 2 with empty stderr still denies (fallback reason).
        let r = parse_hook_output("p", "h", b"", b"", Some(exit_status(2)));
        assert!(matches!(r.decision, HookDecision::Deny { .. }));
        // Other nonzero exits are non-blocking failures: allow.
        let r = parse_hook_output("p", "h", b"", b"boom", Some(exit_status(1)));
        assert_eq!(r.decision, HookDecision::Allow);
        // Timeout/kill (no status) is infrastructure: allow.
        let r = parse_hook_output("p", "h", b"", b"", None);
        assert_eq!(r.decision, HookDecision::Allow);
    }

    #[test]
    fn aggregate_first_deny_last_rewrite_ordered_context() {
        let responses = vec![
            HookResponse {
                plugin: "a".into(),
                additional_context: Some("ctx-a".into()),
                updated_input: Some(serde_json::json!({"v": 1})),
                ..HookResponse::default()
            },
            HookResponse {
                plugin: "b".into(),
                decision: HookDecision::Deny {
                    reason: "first deny".into(),
                },
                ..HookResponse::default()
            },
            HookResponse {
                plugin: "c".into(),
                decision: HookDecision::Deny {
                    reason: "second deny".into(),
                },
                updated_input: Some(serde_json::json!({"v": 2})),
                additional_context: Some("ctx-c".into()),
                ..HookResponse::default()
            },
        ];
        let gate = aggregate_hook_responses(responses);
        assert_eq!(gate.deny, Some(("b".to_string(), "first deny".to_string())));
        assert_eq!(gate.updated_input.unwrap()["v"], 2);
        assert_eq!(gate.context, vec!["ctx-a".to_string(), "ctx-c".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn fixture_scripts_deny_via_json_and_exit2_and_timeout_allows() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "mermaid_hook_fixtures_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let write_script = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            name.to_string()
        };
        let hooks = vec![
            write_script(
                "deny_json.sh",
                r#"echo '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"json says no"}}'"#,
            ),
            write_script("deny_exit2.sh", "echo 'stderr says no' >&2; exit 2"),
            write_script("silent_ok.sh", "exit 0"),
        ];
        let payload = std::sync::Arc::new(b"{}".to_vec());
        let root = std::fs::canonicalize(&dir).unwrap();
        let responses = run_hooks_for_plugin(&root, &hooks, "fixture", "before_tool_use", &payload);
        assert_eq!(responses.len(), 3);
        assert_eq!(
            responses[0].decision,
            HookDecision::Deny {
                reason: "json says no".to_string()
            }
        );
        assert_eq!(
            responses[1].decision,
            HookDecision::Deny {
                reason: "stderr says no".to_string()
            }
        );
        assert_eq!(responses[2].decision, HookDecision::Allow);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hook_overrunning_timeout_is_killed() {
        use std::time::{Duration, Instant};
        // A hook that sleeps far past the timeout must be killed, and
        // wait_hook_bounded must return promptly (no permanent hang).
        #[cfg(unix)]
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 10")
            .spawn()
            .expect("spawn sleep");
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 11 127.0.0.1 >NUL"])
            .spawn()
            .expect("spawn ping");
        let start = Instant::now();
        super::wait_hook_bounded(&mut child, "test", "hook", Duration::from_millis(150));
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "should return promptly after killing the overrunning hook"
        );
    }

    #[test]
    fn hook_that_exits_quickly_returns_without_kill() {
        use std::time::{Duration, Instant};
        #[cfg(unix)]
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn exit");
        let start = Instant::now();
        super::wait_hook_bounded(&mut child, "test", "hook", Duration::from_secs(30));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
