use anyhow::Result;
use colored::Colorize;
use std::path::PathBuf;

use crate::{
    app::{load_config, Config},
    cli::{handle_command, Cli},
    context::ContextManager,
    models::ModelFactory,
    ollama::ensure_model as ensure_ollama_model,
    proxy::{count_mermaid_processes, ensure_proxy, is_proxy_running, stop_proxy},
    session::{select_conversation, ConversationManager, SessionState},
    tui::{run_ui, App},
    utils::{log_error, log_info, log_progress, log_warn},
};

/// Main runtime orchestrator
pub struct Orchestrator {
    cli: Cli,
    config: Config,
    session: SessionState,
    proxy_started_by_us: bool,
}

impl Orchestrator {
    /// Create a new orchestrator from CLI args
    pub fn new(cli: Cli) -> Result<Self> {
        // Load configuration
        let mut config = if let Some(config_path) = &cli.config {
            let toml_str = std::fs::read_to_string(config_path)?;
            toml::from_str::<Config>(&toml_str)?
        } else {
            match load_config() {
                Ok(cfg) => cfg,
                Err(e) => {
                    // Check if this is an environment variable parsing issue
                    let err_msg = format!("{:?}", e);
                    if err_msg.contains("MERMAID_") && err_msg.contains("environment variable") {
                        log_warn(
                            "CONFIG",
                            "Ignoring invalid MERMAID_* environment variables. Using config file and defaults.".to_string(),
                        );
                    } else {
                        log_warn(
                            "CONFIG",
                            format!("Config load failed: {}. Using defaults.", e),
                        );
                    }
                    Config::default()
                },
            }
        };

        // Apply CLI overrides for Ollama offloading
        if cli.num_gpu.is_some() {
            config.ollama.num_gpu = cli.num_gpu;
        }
        if cli.num_thread.is_some() {
            config.ollama.num_thread = cli.num_thread;
        }
        if cli.num_ctx.is_some() {
            config.ollama.num_ctx = cli.num_ctx;
        }
        if cli.numa.is_some() {
            config.ollama.numa = cli.numa;
        }

        // Load session state
        let session = SessionState::load().unwrap_or_default();

        Ok(Self {
            cli,
            config,
            session,
            proxy_started_by_us: false,
        })
    }

    /// Run the orchestrator
    pub async fn run(mut self) -> Result<()> {
        // Progress tracking for startup
        let total_steps = 7; // Total startup steps
        let mut current_step = 0;

        // Handle subcommands
        current_step += 1;
        log_progress(current_step, total_steps, "Processing commands");
        if let Some(command) = &self.cli.command {
            if handle_command(command).await? {
                return Ok(()); // Command handled, exit
            }
            // Continue to chat for Commands::Chat
        }

        // Determine model to use (CLI arg > session > config)
        current_step += 1;
        log_progress(current_step, total_steps, "Configuring model");
        let (model_id, should_save_session) = if let Some(model) = &self.cli.model {
            // CLI argument overrides session
            (model.clone(), true)
        } else if let Some(last_model) = self.session.get_model() {
            // Use saved session model (don't re-save it)
            (last_model.to_string(), false)
        } else {
            // No session, use config default
            (
                format!(
                    "{}/{}",
                    self.config.default_model.provider, self.config.default_model.name
                ),
                true,
            )
        };

        // Only update session if model came from CLI or config (not from session itself)
        if should_save_session {
            self.session.set_model(model_id.clone());
            if let Err(e) = self.session.save() {
                log_warn("WARNING", format!("Failed to save initial session: {}", e));
            }
        }

        log_info(
            "MERMAID",
            format!("Starting Mermaid with model: {}", model_id.green()),
        );

        // Ensure LiteLLM proxy is running ONLY for API models (not Ollama)
        current_step += 1;
        if requires_proxy(&model_id) {
            log_progress(current_step, total_steps, "Checking LiteLLM proxy");
            if !is_proxy_running().await {
                ensure_proxy(self.cli.no_auto_proxy).await?;
                self.proxy_started_by_us = !self.cli.no_auto_proxy;
            }
        } else {
            log_progress(current_step, total_steps, "Using direct Ollama connection");
        }

        // Ensure Ollama model is available (auto-install if needed)
        current_step += 1;
        log_progress(current_step, total_steps, "Checking model availability");
        ensure_ollama_model(&model_id, self.cli.no_auto_install).await?;

        // Create model instance with config for authentication and optional backend override
        current_step += 1;
        log_progress(current_step, total_steps, "Initializing model");
        let model = match ModelFactory::create_with_backend(
            &model_id,
            Some(&self.config),
            self.cli.backend.as_deref(),
        )
        .await
        {
            Ok(m) => m,
            Err(e) => {
                log_error("ERROR", format!("Failed to initialize model: {}", e));
                log_error(
                    "",
                    "Make sure the model is available and properly configured.",
                );
                std::process::exit(1);
            },
        };

        // Set up project context
        let project_path = self.cli.path.clone().unwrap_or_else(|| PathBuf::from("."));

        // Load project structure asynchronously (file paths only, no contents)
        current_step += 1;
        log_progress(current_step, total_steps, "Loading project structure");
        let context_manager = self.load_project_structure(&project_path).await?;

        // Build context with file tree (for immediate injection into model)
        current_step += 1;
        log_progress(current_step, total_steps, "Starting UI");
        let context = context_manager.build_context();
        let mut app = App::new(model, context, model_id.clone());
        app.set_context_manager(context_manager);

        // Handle --resume or --continue flags
        if self.cli.resume || self.cli.continue_conversation {
            let conversation_manager = ConversationManager::new(&project_path)?;
            let conversations = conversation_manager.list_conversations()?;

            if self.cli.continue_conversation {
                // Continue the last conversation
                if let Some(last_conv) = conversation_manager.load_last_conversation()? {
                    log_info(
                        "CONTINUE",
                        format!("Continuing last conversation: {}", last_conv.title.green()),
                    );
                    app.load_conversation(last_conv);
                } else {
                    log_info("INFO", "No previous conversations found in this directory");
                }
            } else if self.cli.resume {
                // Show selection UI for resuming a conversation
                if !conversations.is_empty() {
                    if let Some(selected) = select_conversation(conversations)? {
                        log_info(
                            "RESUME",
                            format!("Resuming conversation: {}", selected.title.green()),
                        );
                        app.load_conversation(selected);
                    }
                } else {
                    log_info("INFO", "No previous conversations found in this directory");
                }
            }
        }

        // Run the TUI
        let result = run_ui(app).await;

        // Note: Session is saved by the UI when changes happen (e.g., model switching)
        // We don't save here to avoid overwriting UI's changes with stale data

        // Cleanup
        self.cleanup().await?;

        result
    }

    /// Load project structure asynchronously (file paths only)
    async fn load_project_structure(
        &self,
        project_path: &PathBuf,
    ) -> Result<ContextManager> {
        log_info(
            "FILES",
            format!("Loading project structure from: {}", project_path.display()),
        );

        let mut manager = ContextManager::new(project_path);
        manager.reload().await?;

        log_info(
            "STATS",
            format!(
                "Found {} files in project",
                manager.total_files()
            ),
        );

        Ok(manager)
    }

    /// Cleanup on exit
    async fn cleanup(&self) -> Result<()> {
        // Stop proxy if we started it and:
        // 1. User requested --stop-proxy-on-exit, OR
        // 2. We auto-started it AND no other mermaid instances are running
        if self.proxy_started_by_us {
            let should_stop = if self.cli.stop_proxy_on_exit {
                true
            } else {
                // Check if other mermaid processes are running
                let mermaid_count = count_mermaid_processes();
                mermaid_count <= 1 // Only us or no processes
            };

            if should_stop {
                log_info(
                    "STOP",
                    "Stopping LiteLLM proxy (no other Mermaid instances running)...",
                );
                stop_proxy().await?;
            }
        }

        Ok(())
    }
}

/// Check if a model requires the LiteLLM proxy
///
/// Ollama models use direct connection (no proxy needed).
/// All other models (OpenAI, Anthropic, etc.) require the proxy.
fn requires_proxy(model_id: &str) -> bool {
    !model_id.starts_with("ollama/")
}
