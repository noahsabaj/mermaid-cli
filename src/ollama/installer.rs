use super::detector;
use super::guide;
use anyhow::Result;
use mermaid_domain::Config;
use mermaid_model::models::adapters::ollama::OllamaAdapter;
use mermaid_model::models::{BackendConfig, Model};
use std::sync::Arc;

/// List installed Ollama models via the HTTP API using the user's
/// config. Falls back to an empty list on any HTTP error.
async fn list_installed_models(config: &Config) -> Vec<String> {
    let backend = BackendConfig {
        ollama_url: config.ollama.base_url(),
        timeout_secs: 5,
        max_idle_per_host: 2,
        ollama_autostart: config.ollama.auto_start,
    };
    let autostart = backend.ollama_autostart;
    match OllamaAdapter::new("__list__", Arc::new(backend)).await {
        // This runs pre-TUI on plain console, so the autostart notice can go
        // straight to stderr — otherwise a cold-boot `mermaid` sits silent
        // for the whole server start.
        Ok(adapter) => {
            let adapter = adapter.with_status_notify(Arc::new(|ev| {
                if let mermaid_model::models::StreamEvent::Status(text) = ev {
                    eprintln!("{text}");
                }
            }));
            // The startup preflight is an intent path, not a diagnostic one:
            // it exists to get a model ready, so it keeps the ability to heal
            // a dead server when the user's config allows it.
            let adapter = if autostart {
                adapter.with_recovery(Arc::new(super::OllamaAutostart))
            } else {
                adapter
            };
            adapter.list_models().await.unwrap_or_default()
        },
        Err(_) => Vec::new(),
    }
}

/// Validate that a model exists, auto-pull if not found.
///
/// Uses the user's configured Ollama host/port (via `config`) so a remote
/// Ollama instance is honored.
///
/// # Errors
///
/// Ollama not being installed — printed as the platform install guidance
/// first — and `ollama pull` failing to run or exiting nonzero, which usually
/// means the model name is wrong. A `:cloud` model returns `Ok` without any
/// local check: it cannot be pulled and is proxied by the daemon at request
/// time. A model that is already present is `Ok` with nothing pulled.
pub async fn ensure_model(model_name: &str, config: &Config) -> Result<()> {
    // Check if Ollama is installed
    if !detector::is_installed() {
        guide::detect_and_guide();
        anyhow::bail!("Ollama is not installed. See instructions above.");
    }

    // Get the model name without provider prefix (all models route through Ollama)
    let model = model_name.strip_prefix("ollama/").unwrap_or(model_name);

    // Cloud models (`:cloud`) are proxied to ollama.com by the Ollama daemon at
    // request time — they can't be `ollama pull`ed ("pull model manifest: file
    // does not exist") and don't appear in `ollama list` unless separately
    // registered. Skip the local existence/pull check and let the chat request
    // reach the daemon, which serves the cloud model directly. (The runtime
    // `:model` switch already skips the pull the same way — see
    // reducer::ollama_pull_target.)
    if crate::ollama::is_cloud_model(model) {
        return Ok(());
    }

    // Check available models via HTTP (honors user's host/port)
    let models = list_installed_models(config).await;

    // Check if the requested model exists (exact match or implicit :latest)
    let model_exists = models
        .iter()
        .any(|m| m == model || (!model.contains(':') && *m == format!("{model}:latest")));

    if !model_exists {
        println!("Model '{model}' not found locally. Pulling from Ollama...\n");

        // Auto-pull using subprocess with inherited stdio for native progress display
        let status = std::process::Command::new("ollama")
            .arg("pull")
            .arg(model)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status();

        match status {
            Ok(exit_status) if exit_status.success() => {
                println!("\nModel '{model}' pulled successfully.\n");
            },
            Ok(_) => {
                anyhow::bail!(
                    "Failed to pull model '{model}'. Check if the model name is correct: https://ollama.com/library"
                );
            },
            Err(e) => {
                anyhow::bail!("Failed to run 'ollama pull': {e}");
            },
        }
    }

    Ok(())
}

/// The local Ollama install's models, for callers deciding whether a local
/// model is available at all.
///
/// `None` means Ollama is not installed on this machine; `Some(vec![])` means
/// it is installed but has nothing usable (unreachable with an unreadable
/// store, or running with nothing pulled). Neither is an error: Ollama is
/// Mermaid's default backend, not a prerequisite — a machine with only a
/// remote provider key configured must still be able to start. Callers that
/// genuinely need an Ollama model (`ensure_model`) still fail loudly, with
/// [`guide::detect_and_guide`].
///
/// Observes first: picking a startup default is enumeration, not intent, so
/// a stopped server whose manifest store already answers must not be woken
/// (its VRAM grab included) just to be asked the same question — the first
/// chat autostarts it anyway. The healing HTTP list survives only as the
/// fallback for an unreadable store, e.g. a fresh install before its first
/// pull.
pub async fn local_models(config: &Config) -> Option<Vec<String>> {
    if !detector::is_installed() {
        return None;
    }
    match super::observe_models(config).await {
        super::LocalModelListing::Live(models) | super::LocalModelListing::FromDisk(models) => {
            Some(models)
        },
        super::LocalModelListing::Unreachable => Some(list_installed_models(config).await),
    }
}

#[cfg(test)]
mod tests {
    /// `ensure_model` must skip the local existence/pull check for `:cloud`
    /// models (which can't be pulled), with or without the `ollama/` prefix,
    /// while still pulling ordinary local models.
    #[test]
    fn cloud_models_skip_pull_local_models_do_not() {
        let skips = |name: &str| {
            let model = name.strip_prefix("ollama/").unwrap_or(name);
            crate::ollama::is_cloud_model(model)
        };
        for cloud in [
            "nemotron-3-ultra:cloud",
            "ollama/nemotron-3-ultra:cloud",
            "minimax-m3:cloud",
            "ollama/gpt-oss:cloud",
        ] {
            assert!(skips(cloud), "{cloud} is a cloud model — must skip pull");
        }
        for local in ["qwen3:8b", "ollama/llama3.2", "phi3:latest", "mistral"] {
            assert!(!skips(local), "{local} is a local model — must still pull");
        }
    }
}
