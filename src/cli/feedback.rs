//! `mermaid feedback` — a LOCAL diagnostic bundle for bug reports.
//!
//! Collects the doctor report, a names-and-booleans config summary, the
//! always-on trace ring, the log tail, and recent session IDS into one
//! markdown (or JSON) document. Defense in depth against secret leakage:
//! the ring redacts at capture, the log file redacts at write, the summary
//! carries no values (only names and `key_source` labels), and the whole
//! rendered document passes through `redact_secrets` once more before it
//! leaves the process. Nothing is uploaded, ever — the bundle is written to
//! the current directory (0600) or stdout for the user to review and share.

use anyhow::Result;
use std::path::Path;

use crate::app::Config;
use crate::models::PROVIDER_REGISTRY;

use super::OutputFormat;
use super::commands::DoctorReport;

/// Lines tailed from `~/.mermaid/mermaid.log`. The 10 MiB rotation bounds the
/// read, so read-then-tail is safe.
const LOG_TAIL_LINES: usize = 200;
/// Recent conversation IDS (ids only — never message content).
const RECENT_SESSION_IDS: usize = 10;

/// The full bundle. Serializes directly for `--format json`; the markdown
/// renderer walks the same fields.
#[derive(Debug, serde::Serialize)]
struct FeedbackReport {
    generated_at: String,
    cli_version: String,
    os: String,
    arch: String,
    config: ConfigSummary,
    doctor: DoctorReport,
    trace_ring: Vec<String>,
    log_tail: Vec<String>,
    recent_sessions: Vec<String>,
}

/// Names and booleans ONLY: no URLs, no headers, no env values — a provider
/// is `{name, key_source}`, an MCP server is its name.
#[derive(Debug, serde::Serialize)]
struct ConfigSummary {
    default_model: String,
    safety_mode: String,
    network: String,
    filesystem: String,
    checkpoint_on_mutation: bool,
    providers: Vec<ProviderKeyStatus>,
    mcp_servers: Vec<String>,
    enabled_plugins: usize,
}

/// One provider row: is a key resolvable for it right now?
#[derive(Debug, serde::Serialize)]
struct ProviderKeyStatus {
    name: String,
    /// Where the key resolves from: `"env"`, `"keyring"`, or `"none"`.
    key_source: &'static str,
}

/// Entry point for `mermaid feedback`.
pub(crate) async fn run_feedback(
    config: &Config,
    cwd: &Path,
    cli_model: Option<&str>,
    stdout: bool,
    format: OutputFormat,
) -> Result<()> {
    // Also serves as a live self-check: this event lands in the trace ring
    // before the snapshot below, so a healthy pipeline never renders an
    // empty ring section.
    tracing::info!(target: "mermaid_cli::feedback", "building diagnostic bundle");
    let report = build_feedback_report(config, cwd, cli_model).await;
    let (rendered, extension) = match format {
        OutputFormat::Json | OutputFormat::Ndjson => {
            (serde_json::to_string_pretty(&report)?, "json")
        },
        OutputFormat::Markdown | OutputFormat::Text => (render_markdown(&report), "md"),
    };
    // Final defense-in-depth pass over the WHOLE rendered document: even a
    // secret that slipped into a doctor message or session title is scrubbed
    // before it can reach disk or stdout.
    let rendered = crate::utils::redact_secrets(&rendered);
    if stdout {
        println!("{rendered}");
        return Ok(());
    }
    let path = cwd.join(format!(
        "mermaid-feedback-{}.{extension}",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    ));
    crate::runtime::write_atomic_with_mode(&path, rendered.as_bytes(), 0o600)?;
    println!(
        "wrote {} (local only - review before sharing)",
        path.display()
    );
    Ok(())
}

/// Gather every section. Failures degrade to empty sections — a diagnostic
/// tool must not itself fail on the broken machine it is diagnosing.
async fn build_feedback_report(
    config: &Config,
    cwd: &Path,
    cli_model: Option<&str>,
) -> FeedbackReport {
    FeedbackReport {
        generated_at: chrono::Local::now().to_rfc3339(),
        cli_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        config: summarize_config(config),
        doctor: super::commands::build_doctor_report(config, cwd, cli_model).await,
        trace_ring: crate::utils::trace_ring()
            .map(|ring| ring.snapshot())
            .unwrap_or_default(),
        log_tail: read_log_tail(),
        recent_sessions: recent_session_ids(cwd),
    }
}

/// Build the names-and-booleans summary (see [`ConfigSummary`]).
fn summarize_config(config: &Config) -> ConfigSummary {
    // Built-in registry providers first, then user-defined [providers]
    // entries; key presence via the same env resolution the factory uses.
    let mut providers = Vec::new();
    for profile in PROVIDER_REGISTRY {
        let override_env = config
            .providers
            .get(profile.name)
            .and_then(|c| c.api_key_env.as_deref());
        providers.push(ProviderKeyStatus {
            name: profile.name.to_string(),
            key_source: crate::utils::provider_key_source(
                profile.name,
                profile.api_key_env,
                override_env,
            ),
        });
    }
    for (name, provider) in &config.providers {
        if providers.iter().any(|p| &p.name == name) {
            continue;
        }
        // Custom providers: api_key_env is their default (keyring may fill).
        let key_source = match provider.api_key_env.as_deref() {
            Some(env) => crate::utils::provider_key_source(name, env, None),
            None => "none",
        };
        providers.push(ProviderKeyStatus {
            name: name.clone(),
            key_source,
        });
    }
    providers.sort_by(|a, b| a.name.cmp(&b.name));
    let mut mcp_servers: Vec<String> = config.mcp_servers.keys().cloned().collect();
    mcp_servers.sort();
    let enabled_plugins = crate::runtime::RuntimeStore::open_default()
        .ok()
        .and_then(|store| store.plugins().list().ok())
        .map(|plugins| plugins.iter().filter(|p| p.enabled).count())
        .unwrap_or(0);
    ConfigSummary {
        default_model: format!(
            "{}/{}",
            config.default_model.provider, config.default_model.name
        ),
        safety_mode: format!("{:?}", config.safety.mode).to_lowercase(),
        network: format!("{:?}", config.safety.network).to_lowercase(),
        filesystem: format!("{:?}", config.safety.filesystem).to_lowercase(),
        checkpoint_on_mutation: config.safety.checkpoint_on_mutation,
        providers,
        mcp_servers,
        enabled_plugins,
    }
}

/// Last [`LOG_TAIL_LINES`] lines of the (already-redacted-at-write) log file.
fn read_log_tail() -> Vec<String> {
    let Some(path) = crate::utils::log_file_path() else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = raw.lines().collect();
    lines
        .iter()
        .skip(lines.len().saturating_sub(LOG_TAIL_LINES))
        .map(|l| l.to_string())
        .collect()
}

/// Newest [`RECENT_SESSION_IDS`] conversation ids for this project — ids
/// only, never titles or messages (titles can quote user content).
fn recent_session_ids(cwd: &Path) -> Vec<String> {
    let Ok(manager) = crate::session::ConversationManager::new(cwd) else {
        return Vec::new();
    };
    let mut metas = manager.list_conversation_metas().unwrap_or_default();
    metas.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
    metas
        .into_iter()
        .take(RECENT_SESSION_IDS)
        .map(|m| m.id)
        .collect()
}

/// Render the bundle as one markdown document.
fn render_markdown(report: &FeedbackReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "# Mermaid Feedback Bundle\n");
    let _ = writeln!(
        out,
        "Generated {} by mermaid v{} on {}/{}.",
        report.generated_at, report.cli_version, report.os, report.arch
    );
    let _ = writeln!(
        out,
        "\nThis file was written locally and is never uploaded. Review before sharing.\n"
    );

    let cfg = &report.config;
    let _ = writeln!(out, "## Config\n");
    let _ = writeln!(out, "- model: {}", cfg.default_model);
    let _ = writeln!(
        out,
        "- safety: mode={} network={} filesystem={} checkpoint_on_mutation={}",
        cfg.safety_mode, cfg.network, cfg.filesystem, cfg.checkpoint_on_mutation
    );
    let with_keys: Vec<String> = cfg
        .providers
        .iter()
        .filter(|p| p.key_source != "none")
        .map(|p| format!("{} ({})", p.name, p.key_source))
        .collect();
    let _ = writeln!(
        out,
        "- providers with keys: {}",
        if with_keys.is_empty() {
            "(none)".to_string()
        } else {
            with_keys.join(", ")
        }
    );
    let _ = writeln!(
        out,
        "- mcp servers: {}",
        if cfg.mcp_servers.is_empty() {
            "(none)".to_string()
        } else {
            cfg.mcp_servers.join(", ")
        }
    );
    let _ = writeln!(out, "- enabled plugins: {}", cfg.enabled_plugins);

    let _ = writeln!(out, "\n## Doctor\n");
    let _ = writeln!(out, "```json");
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&report.doctor).unwrap_or_default()
    );
    let _ = writeln!(out, "```");

    let _ = writeln!(out, "\n## Recent sessions (ids only)\n");
    if report.recent_sessions.is_empty() {
        let _ = writeln!(out, "(none)");
    }
    for id in &report.recent_sessions {
        let _ = writeln!(out, "- {id}");
    }

    let _ = writeln!(
        out,
        "\n## Trace ring ({} events, oldest first)\n",
        report.trace_ring.len()
    );
    let _ = writeln!(out, "```");
    for line in &report.trace_ring {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, "```");

    let _ = writeln!(
        out,
        "\n## Log tail (last {} lines)\n",
        report.log_tail.len()
    );
    let _ = writeln!(out, "```");
    for line in &report.log_tail {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, "```");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> FeedbackReport {
        FeedbackReport {
            generated_at: "2026-01-02T03:04:05+00:00".to_string(),
            cli_version: "0.0.0-test".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            config: ConfigSummary {
                default_model: "ollama/test".to_string(),
                safety_mode: "ask".to_string(),
                network: "allow".to_string(),
                filesystem: "unrestricted".to_string(),
                checkpoint_on_mutation: true,
                providers: vec![
                    ProviderKeyStatus {
                        name: "anthropic".to_string(),
                        key_source: "env",
                    },
                    ProviderKeyStatus {
                        name: "openai".to_string(),
                        key_source: "none",
                    },
                ],
                mcp_servers: vec!["context7".to_string()],
                enabled_plugins: 2,
            },
            doctor: sample_doctor(),
            trace_ring: vec!["2026-01-02 INFO mermaid_cli: ring line".to_string()],
            log_tail: vec!["log line one".to_string()],
            recent_sessions: vec!["20260101_120000_000".to_string()],
        }
    }

    fn sample_doctor() -> DoctorReport {
        use super::super::commands::{DoctorCheck, DoctorRuntime};
        DoctorReport {
            ok: true,
            cwd: "/project/demo".to_string(),
            active_profile: None,
            active_model: Some("ollama/test".to_string()),
            model_error: None,
            model_capabilities: None,
            safety_mode: "ask".to_string(),
            checkpoint_on_mutation: true,
            prompt_customized: false,
            ollama: DoctorCheck {
                status: "ok",
                message: "reachable".to_string(),
            },
            remote_providers: vec!["anthropic".to_string()],
            provider_problems: Vec::new(),
            project_instructions: DoctorCheck {
                status: "info",
                message: "none".to_string(),
            },
            tools: vec!["read/edit/write files".to_string()],
            runtime: DoctorRuntime {
                daemon: DoctorCheck {
                    status: "info",
                    message: "not attached".to_string(),
                },
                local_store: DoctorCheck {
                    status: "ok",
                    message: "ready".to_string(),
                },
            },
            next_steps: Vec::new(),
        }
    }

    #[test]
    fn markdown_carries_every_section() {
        let md = render_markdown(&sample_report());
        assert!(md.starts_with("# Mermaid Feedback Bundle"));
        assert!(md.contains("never uploaded"));
        assert!(md.contains("## Config"));
        assert!(md.contains("- model: ollama/test"));
        assert!(md.contains("providers with keys: anthropic"));
        assert!(!md.contains("openai,"), "keyless providers are not listed");
        assert!(md.contains("## Doctor"));
        assert!(md.contains("## Recent sessions"));
        assert!(md.contains("20260101_120000_000"));
        assert!(md.contains("## Trace ring (1 events"));
        assert!(md.contains("ring line"));
        assert!(md.contains("## Log tail (last 1 lines)"));
    }

    #[test]
    fn summary_never_serializes_secret_values() {
        // A summary built from a config whose provider rows exist must carry
        // only names + booleans; serialize and scan for shapes that would
        // indicate a value leak (URLs, env assignments).
        let json = serde_json::to_string(&sample_report().config).unwrap();
        assert!(json.contains("\"key_source\":\"env\""));
        assert!(!json.contains("http"), "no URLs in the summary: {json}");
        assert!(!json.contains("="), "no env assignments: {json}");
    }

    #[test]
    fn log_tail_keeps_only_the_last_lines() {
        // Boundary check on the tailing arithmetic (skip = len - N).
        let lines: Vec<String> = (0..LOG_TAIL_LINES + 50).map(|i| format!("l{i}")).collect();
        let tail: Vec<&String> = lines
            .iter()
            .skip(lines.len().saturating_sub(LOG_TAIL_LINES))
            .collect();
        assert_eq!(tail.len(), LOG_TAIL_LINES);
        assert_eq!(tail[0], &format!("l{}", 50));
        // And the short-file case: no underflow, everything kept.
        let short = ["a".to_string(), "b".to_string()];
        let kept: Vec<&String> = short
            .iter()
            .skip(short.len().saturating_sub(LOG_TAIL_LINES))
            .collect();
        assert_eq!(kept.len(), 2);
    }
}
