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
            && handle_command(command).await?
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

        // Check Ollama availability (all models route through Ollama)
        current_step += 1;
        log_progress(current_step, total_steps, "Checking Ollama availability");
        let ollama_check =
            check_ollama_available(&self.config.ollama.host, self.config.ollama.port).await;

        if !ollama_check.available {
            log_error("OLLAMA", &ollama_check.message);
            anyhow::bail!("{}", ollama_check.message);
        }

        // Validate model exists
        current_step += 1;
        log_progress(current_step, total_steps, "Checking model availability");
        ensure_ollama_model(&model_id).await?;

        // Persist model if CLI flag was used
        if cli_model_provided && let Err(e) = persist_last_model(&model_id) {
            log_warn("CONFIG", format!("Failed to persist model choice: {}", e));
        }

        // Create model instance with config for authentication
        current_step += 1;
        log_progress(current_step, total_steps, "Initializing model");
        let model = ModelFactory::create(
            &model_id,
            Some(&self.config),
        )
        .await
        .map_err(|e| {
            log_error("ERROR", format!("Failed to initialize model: {}", e));
            anyhow::anyhow!(
                "Failed to initialize model: {}. Make sure the model is available and properly configured.",
                e
            )
        })?;

        // Set up project path for session management
        let project_path = self.cli.path.clone().unwrap_or_else(|| PathBuf::from("."));

        // Start UI - LLM explores codebase via tools, no context injection
        current_step += 1;
        log_progress(current_step, total_steps, "Starting UI");
        let base_config = ModelConfig::from_app_config(&self.config, &model_id);
        let mut app = App::new(model, model_id.clone(), base_config);

        // Start MCP servers in background (non-blocking — TUI renders immediately)
        if !self.config.mcp_servers.is_empty() {
            let server_count = self.config.mcp_servers.len();
            log_info("MCP", format!("Starting {} MCP server(s) in background...", server_count));
            mark_mcp_init_started();

            let mcp_configs = self.config.mcp_servers.clone();
            app.mcp_init_task = Some(tokio::spawn(async move {
                let manager = McpServerManager::start(&mcp_configs).await;
                if manager.has_servers() {
                    let tools =
                        crate::models::tools::mcp_tools_to_ollama(manager.get_all_tools());
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
