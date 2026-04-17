use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

use crate::{
    agents::mark_mcp_init_started,
    app::{Config, load_config, persist_last_model},
    cli::{Cli, handle_command},
    mcp::McpServerManager,
    models::{ModelConfig, ModelFactory},
    ollama::ensure_model as ensure_ollama_model,
    session::{ConversationManager, select_conversation},
    tui::{App, McpInitResult, run_ui},
    utils::{check_ollama_available, log_error, log_info, log_progress, log_warn},
};

/// Main runtime orchestrator
pub struct Orchestrator {
    cli: Cli,
    config: Config,
}

impl Orchestrator {
    /// Create a new orchestrator from CLI args
    pub fn new(cli: Cli) -> Result<Self> {
        // Load configuration (single file + defaults)
        let config = match load_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                log_warn(
                    "CONFIG",
                    format!("Config load failed: {:#}. Using defaults.", e),
                );
                Config::default()
            },
        };

        Ok(Self { cli, config })
    }

    /// Run the orchestrator
    pub async fn run(self) -> Result<()> {
        // Progress tracking for startup
        let total_steps = 6; // Total startup steps
        let mut current_step = 0;

        // Handle subcommands
        current_step += 1;
        log_progress(current_step, total_steps, "Processing commands");
        if let Some(command) = &self.cli.command
            && handle_command(command, &self.config).await?
        {
            return Ok(()); // Command handled, exit
        }
        // Continue to chat for Commands::Chat

        // Determine model to use (CLI arg > last_used > default_model)
        current_step += 1;
        log_progress(current_step, total_steps, "Configuring model");

        let cli_model_provided = self.cli.model.is_some();
        let model_id =
            crate::app::resolve_model_id(self.cli.model.as_deref(), &self.config).await?;

        log_info(
            "MERMAID",
            format!("Starting Mermaid with model: {}", model_id),
        );

        // For remote providers (anthropic, openai, etc.) we skip the
        // Ollama health check and the local model-pull step. The provider
        // adapter will surface API errors at chat time. Bare names default
        // to Ollama, matching legacy behavior.
        let model_uses_ollama = is_ollama_model(&model_id);

        // Check Ollama availability (only when the chosen model routes
        // through Ollama).
        current_step += 1;
        log_progress(current_step, total_steps, "Checking Ollama availability");
        if model_uses_ollama {
            let ollama_check =
                check_ollama_available(&self.config.ollama.host, self.config.ollama.port).await;

            if !ollama_check.available {
                log_error("OLLAMA", &ollama_check.message);
                anyhow::bail!("{}", ollama_check.message);
            }
        }

        // Validate model exists (Ollama-only — remote adapters surface
        // 404 themselves on first chat call).
        current_step += 1;
        log_progress(current_step, total_steps, "Checking model availability");
        if model_uses_ollama {
            ensure_ollama_model(&model_id, &self.config).await?;
        }

        // Create model instance with config for authentication.
        // (Step 5d: persist moved to AFTER successful create — see below.)
        current_step += 1;
        log_progress(current_step, total_steps, "Initializing model");
        let model = ModelFactory::create(&model_id, Some(&self.config))
            .await
            .map_err(|e| {
                log_error("ERROR", format!("Failed to initialize model: {}", e));
                anyhow::anyhow!(actionable_init_error(&model_id, e))
            })?;

        // Persist model AFTER successful creation — never persist a
        // broken model to last_used_model. (Step 5d fix.)
        if cli_model_provided && let Err(e) = persist_last_model(&model_id) {
            log_warn("CONFIG", format!("Failed to persist model choice: {}", e));
        }

        // Set up project path for session management
        let project_path = self.cli.path.clone().unwrap_or_else(|| PathBuf::from("."));

        // Start UI - LLM explores codebase via tools, no context injection
        current_step += 1;
        log_progress(current_step, total_steps, "Starting UI");
        let mut base_config = ModelConfig::from_app_config(&self.config, &model_id);
        // CLI `--reasoning` overrides the config-file default for this
        // session. Slash command + Alt+T can still change it at runtime.
        if let Some(level) = self.cli.reasoning {
            base_config.reasoning = level;
        }
        let mut app = App::new(model, model_id.clone(), base_config);

        // Step 5h: discover MERMAID.md for project-level instructions.
        // Walks UP from cwd to git root or $HOME. Silent if absent —
        // most projects won't have one and we don't want log noise.
        {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            if let Some(loaded) = crate::app::instructions::find_mermaid_md(&cwd)
                .and_then(|p| crate::app::instructions::load_from_path(&p))
            {
                log_info(
                    "INSTRUCTIONS",
                    format!(
                        "Loaded MERMAID.md ({} bytes{}) from {}",
                        loaded.byte_len,
                        if loaded.truncated { ", truncated" } else { "" },
                        loaded.path.display()
                    ),
                );
                app.instructions = Some(loaded);
            }
        }

        // Start MCP servers in background (non-blocking — TUI renders immediately)
        if !self.config.mcp_servers.is_empty() {
            let server_count = self.config.mcp_servers.len();
            log_info(
                "MCP",
                format!("Starting {} MCP server(s) in background...", server_count),
            );
            mark_mcp_init_started();

            let mcp_configs = self.config.mcp_servers.clone();
            app.mcp_init_task = Some(tokio::spawn(async move {
                let manager = McpServerManager::start(&mcp_configs).await;
                if manager.has_servers() {
                    let tools = crate::models::tools::mcp_tools_to_ollama(manager.get_all_tools());
                    log_info("MCP", format!("{} MCP tool(s) available", tools.len()));
                    McpInitResult {
                        tools,
                        manager: Some(Arc::new(manager)),
                    }
                } else {
                    McpInitResult {
                        tools: Vec::new(),
                        manager: None,
                    }
                }
            }));
        }

        // Handle session loading
        // Default: start fresh (no history)
        // --continue: resume last conversation
        // --sessions: show picker to choose a previous conversation
        if self.cli.continue_session || self.cli.sessions {
            let conversation_manager = ConversationManager::new(&project_path)?;

            if self.cli.sessions {
                // Show selection UI for choosing a conversation
                let conversations = conversation_manager.list_conversations()?;
                if !conversations.is_empty() {
                    if let Some(selected) = select_conversation(conversations)? {
                        log_info(
                            "RESUME",
                            format!("Resuming conversation: {}", selected.title),
                        );
                        app.load_conversation(selected);
                    }
                } else {
                    log_info("INFO", "No previous conversations found in this directory");
                }
            } else {
                // --continue: resume last conversation
                if let Some(last_conv) = conversation_manager.load_last_conversation()? {
                    log_info("RESUME", format!("Resuming: {}", last_conv.title));
                    app.load_conversation(last_conv);
                } else {
                    log_info("INFO", "No previous conversation to continue");
                }
            }
        }

        // Run the TUI
        run_ui(app).await
    }
}

/// Whether the model_id resolves to the Ollama backend. Bare names
/// (no `/`) default to Ollama; explicit `ollama/...` is also Ollama;
/// anything with a non-ollama provider prefix is remote.
fn is_ollama_model(model_id: &str) -> bool {
    match model_id.split_once('/') {
        Some((provider, _)) => provider.eq_ignore_ascii_case("ollama"),
        None => true,
    }
}

/// Build a startup error message that tells the user how to escape.
/// The wrapped error from `ModelFactory::create` already explains WHAT
/// failed (auth, 404, network); this wrapper appends WHAT TO DO so a
/// user with a stale or unauthenticated `last_used_model` doesn't have
/// to dig through docs to recover. Used by both startup paths
/// (interactive `Orchestrator::run` + non-interactive
/// `run_non_interactive` in main.rs).
pub fn actionable_init_error(model_id: &str, source: impl std::fmt::Display) -> String {
    format!(
        "Failed to initialize model '{}': {}\n\n\
         To recover, try one of:\n  \
         - List available models: mermaid list\n  \
         - Switch to a different model: mermaid --model <name>\n  \
         - Use a local Ollama model (no API key needed): mermaid --model ollama/<name>",
        model_id, source
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The user's failing model id must appear in the error so they
    /// know which one to swap out. Without this, "Failed to initialize
    /// model" with no name is useless — the user can't tell whether
    /// they picked the wrong model or hit a global config issue.
    #[test]
    fn actionable_init_error_names_failing_model() {
        let msg = actionable_init_error(
            "anthropic/claude-opus-4-7",
            "Authentication error: ANTHROPIC_API_KEY not set",
        );
        assert!(
            msg.contains("anthropic/claude-opus-4-7"),
            "model id missing from error: {}",
            msg
        );
    }

    /// Recommend `mermaid list` so the user can see what they CAN
    /// actually use right now.
    #[test]
    fn actionable_init_error_recommends_mermaid_list() {
        let msg = actionable_init_error("foo/bar", "404 not found");
        assert!(
            msg.contains("mermaid list"),
            "expected `mermaid list` recovery hint in: {}",
            msg
        );
    }

    /// Recommend `mermaid --model <name>` so the user has a one-line
    /// command to escape the trap.
    #[test]
    fn actionable_init_error_recommends_explicit_model_flag() {
        let msg = actionable_init_error("foo/bar", "404 not found");
        assert!(
            msg.contains("--model"),
            "expected `--model` recovery hint in: {}",
            msg
        );
    }
}
