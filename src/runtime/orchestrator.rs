use anyhow::Result;
use colored::Colorize;
use std::path::PathBuf;

use crate::{
    app::{load_config, Config},
    cli::{handle_command, Cli},
    context::ContextLoader,
    models::{ModelFactory, ProjectContext},
    ollama::ensure_model as ensure_ollama_model,
    proxy::{count_mermaid_processes, ensure_proxy, is_proxy_running, stop_proxy},
    session::SessionState,
    tui::{run_ui, App},
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
        let config = if let Some(config_path) = &cli.config {
            let toml_str = std::fs::read_to_string(config_path)?;
            toml::from_str::<Config>(&toml_str)?
        } else {
            match load_config() {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("⚠️  Failed to load config: {}. Using defaults.", e);
                    Config::default()
                }
            }
        };

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
        // Handle subcommands
        if let Some(command) = &self.cli.command {
            if handle_command(command).await? {
                return Ok(()); // Command handled, exit
            }
            // Continue to chat for Commands::Chat
        }

        // Determine model to use (CLI arg > session > config)
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
                eprintln!("⚠️  Failed to save initial session: {}", e);
            }
        }

        println!("🧜‍♀️ Starting Mermaid with model: {}", model_id.green());

        // Ensure LiteLLM proxy is running (unless --no-auto-proxy is set)
        if !is_proxy_running().await {
            ensure_proxy(self.cli.no_auto_proxy).await?;
            self.proxy_started_by_us = !self.cli.no_auto_proxy;
        }

        // Ensure Ollama model is available (auto-install if needed)
        ensure_ollama_model(&model_id, self.cli.no_auto_install).await?;

        // Create model instance with config for authentication
        let model = match ModelFactory::create(&model_id, Some(&self.config)).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("❌ Failed to initialize model: {}", e);
                eprintln!("   Make sure the model is available and properly configured.");
                std::process::exit(1);
            }
        };

        // Set up project context
        let project_path = self
            .cli
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));

        // Load project context
        let context = self.load_project_context(&project_path)?;

        // Create app instance with model and context
        let app = App::new(model, context);

        // Run the TUI
        let result = run_ui(app).await;

        // Note: Session is saved by the UI when changes happen (e.g., model switching)
        // We don't save here to avoid overwriting UI's changes with stale data

        // Cleanup
        self.cleanup().await?;

        result
    }

    /// Load project context
    fn load_project_context(&self, project_path: &PathBuf) -> Result<ProjectContext> {
        let loader = ContextLoader::new()?;

        println!("📂 Loading project context from: {}", project_path.display());

        let context = loader.load_context(project_path)?;

        println!(
            "📊 Loaded {} files (~{} tokens)",
            context.files.len(),
            context.token_count
        );

        Ok(context)
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
                println!("🛑 Stopping LiteLLM proxy (no other Mermaid instances running)...");
                stop_proxy().await?;
            }
        }

        Ok(())
    }
}